// hypr-world glue plugin: lets the app, a separate Wayland client, take the
// keyboard and mouse while it is open.

#include "App.hpp"
#include "Feed.hpp"
#include "Input.hpp"
#include "Layers.hpp"
#include "Protocol.hpp"
#include "Scanout.hpp"
#include "Server.hpp"
#include "Share.hpp"
#include "Util.hpp"

#include <hyprland/src/Compositor.hpp>
#include <hyprland/src/plugins/PluginAPI.hpp>

extern "C" {
#include <lua.h>
}

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <format>
#include <fstream>
#include <stdexcept>
#include <string>
#include <string_view>
#include <unordered_map>
#include <utility>
#include <vector>

namespace World {

namespace {

HANDLE              g_handle = nullptr;
SP<SHyprCtlCommand> g_hyprctl;

const char* stateName() {
    switch (Input::state()) {
        case Input::EState::Off: return "off";
        case Input::EState::Look: return "look";
        case Input::EState::Type: return "type";
    }
    return "?";
}

// aim <0xaddress> <u> <v> <bw> <bh>, or aim - for no window.
void onAim(const std::vector<std::string_view>& tokens) {
    if (tokens.size() == 2 && tokens[1] == "-") {
        Input::aimNone();
        return;
    }
    if (tokens.size() != 6)
        return;

    const auto address = Protocol::address(tokens[1]);
    const auto u       = Protocol::number<double>(tokens[2]);
    const auto v       = Protocol::number<double>(tokens[3]);
    const auto bw      = Protocol::number<double>(tokens[4]);
    const auto bh      = Protocol::number<double>(tokens[5]);

    if (!address || !u || !v || !bw || !bh || *bw <= 0 || *bh <= 0)
        return;

    Input::aim(*address, *u, *v, Vector2D{*bw, *bh});
}

// pace [<0xaddress> <ms>]...: each window the app shows, and the least time
// between its frames. A line with anything malformed in it is ignored.
void onPace(const std::vector<std::string_view>& tokens) {
    if (tokens.size() % 2 != 1)
        return;

    std::unordered_map<uint64_t, uint32_t> pace;
    for (size_t i = 1; i < tokens.size(); i += 2) {
        const auto address = Protocol::address(tokens[i]);
        const auto ms      = Protocol::number<uint32_t>(tokens[i + 1]);
        if (!address || !ms)
            return;
        pace[*address] = *ms;
    }
    Share::setPace(std::move(pace));
}

// hello <version>: the app's protocol. The plugin answers with its own either
// way, and the app exits on a mismatch.
void onHello(const std::vector<std::string_view>& tokens) {
    const auto version = tokens.size() == 2 ? Protocol::number<int>(tokens[1]) : std::nullopt;
    if (version != Protocol::kVersion)
        notify(g_handle,
               std::format("the app speaks protocol {}, this plugin {}; rebuild and reload both from the same tree", version ? std::to_string(*version) : "?",
                           Protocol::kVersion),
               3000, true);
    Server::send(std::format("hello {}", Protocol::kVersion));
}

// One line from the app. Unknown verbs are ignored.
void onLine(std::string_view line) {
    guarded("app message", [&] {
        Input::appSeen();

        const auto tokens = Protocol::tokens(line);
        if (tokens.empty())
            return;

        const auto verb = tokens[0];

        if (verb == "hello")
            onHello(tokens);
        else if (verb == "backend")
            App::setBackend();
        else if (verb == "grab")
            Input::enterLook("app");
        else if (verb == "release")
            Input::leave("app");
        else if (verb == "aim")
            onAim(tokens);
        else if (verb == "type")
            Input::enterType("app");
        else if (verb == "look")
            Input::leaveType("app");
        else if (verb == "live")
            App::setLive(Protocol::addresses(tokens));
        else if (verb == "layers")
            Layers::setSeen(Protocol::addresses(tokens));
        else if (verb == "workspace" && tokens.size() == 2) {
            if (const auto id = Protocol::number<int64_t>(tokens[1]))
                App::showWorkspace(*id);
        } else if (verb == "alpha" && tokens.size() == 2) {
            if (const auto opacity = Protocol::number<float>(tokens[1]))
                App::fade(*opacity);
        } else if (verb == "hide")
            App::hidden();
        else if (verb == "samples") {
            // samples <fourcc> <modifier>...: all of them hex, or none taken.
            const auto values = Protocol::addresses(tokens);
            if (values.size() >= 2 && values.size() == tokens.size() - 1)
                Share::addFormat(static_cast<uint32_t>(values[0]), {values.begin() + 1, values.end()});
        } else if (verb == "done" && tokens.size() == 2) {
            if (const auto id = Protocol::number<uint64_t>(tokens[1]))
                Share::done(*id);
        } else if (verb == "pace")
            onPace(tokens);
        else if (verb == "armed" && tokens.size() == 2)
            Input::setArmed(tokens[1] == "1");
        else if (verb == "close" && tokens.size() == 2) {
            if (const auto address = Protocol::address(tokens[1]))
                App::close(*address);
        } else if (verb == "terminate" && tokens.size() == 2) {
            if (const auto address = Protocol::address(tokens[1]))
                App::terminate(*address);
        }
        // "ping" only feeds the watchdog, done above.
    });
}

void onConnection(bool connected) {
    try {
        Input::appConnected(connected);
        App::appConnected(connected);
        Share::appConnected(connected);
        Feed::appConnected(connected);
    } catch (...) {}
}

// hl.plugin.hypr_world.toggle(): the world key.
int luaToggle(lua_State*) {
    try {
        App::toggle();
    } catch (...) {}
    return 0;
}

// A string as JSON, quoted: control characters escaped, so it stays on one
// line.
void jsonString(std::string& out, std::string_view s) {
    out += '"';
    for (const unsigned char c : s) {
        if (c == '"' || c == '\\')
            out += {'\\', static_cast<char>(c)};
        else if (c < 0x20)
            out += std::format("\\u{:04x}", c);
        else
            out += static_cast<char>(c);
    }
    out += '"';
}

// The app's settings as one JSON object: a nested table becomes a nested
// object. Only string keys, and string, number and boolean values; anything
// else, and a number JSON cannot hold, is skipped. Keys are sorted, so the
// same table is the same text.
std::string readSettings(lua_State* L, int table, int depth = 0) {
    std::vector<std::pair<std::string, std::string>> members;
    lua_pushnil(L);
    while (lua_next(L, table) != 0) {
        // Key at -2, value at -1. lua_tostring would turn a number key into a
        // string in place and derail lua_next, so only string keys are read.
        if (lua_type(L, -2) == LUA_TSTRING) {
            size_t      len = 0;
            const char* str = nullptr;
            std::string value;
            switch (lua_type(L, -1)) {
                case LUA_TTABLE:
                    if (depth < 4)
                        value = readSettings(L, lua_gettop(L), depth + 1);
                    break;
                case LUA_TBOOLEAN: value = lua_toboolean(L, -1) ? "true" : "false"; break;
                case LUA_TNUMBER:
                    if (const double n = lua_tonumber(L, -1); std::isfinite(n))
                        value = std::format("{}", n);
                    break;
                case LUA_TSTRING:
                    str = lua_tolstring(L, -1, &len);
                    jsonString(value, {str, len});
                    break;
                default: break;
            }
            if (!value.empty()) {
                str = lua_tolstring(L, -2, &len);
                members.emplace_back(std::string{str, len}, std::move(value));
            }
        }
        lua_pop(L, 1);
    }

    std::ranges::sort(members);
    std::string out = "{";
    for (const auto& [key, value] : members) {
        if (out.size() > 1)
            out += ',';
        jsonString(out, key);
        out += ':';
        out += value;
    }
    return out + '}';
}

// hl.plugin.hypr_world.config({ world = { ... } }): the app's settings,
// passed on to the app as they are. Read without luaL_error, which would
// longjmp past C++ destructors.
int luaConfig(lua_State* L) {
    if (!lua_istable(L, 1))
        return 0;

    const int top = lua_gettop(L);
    try {
        std::string settings = "{}";
        lua_getfield(L, 1, "world");
        if (lua_istable(L, -1))
            settings = readSettings(L, lua_gettop(L));
        lua_pop(L, 1);

        App::setSettings(std::move(settings));
    } catch (...) { lua_settop(L, top); }
    return 0;
}

// `hyprctl hypr-world` prints the state; `hyprctl hypr-world off` is the
// escape hatch from another terminal or an SSH session.
std::string onHyprctl(eHyprCtlOutputFormat, std::string request) {
    try {
        const auto tokens = Protocol::tokens(request);

        if (tokens.size() > 1 && tokens[1] == "off") {
            Input::leave("hyprctl");
            return "ok";
        }

        return std::format("state {}, app {}, {}, {}, {}\n", stateName(), Server::connected() ? "connected" : "not connected", App::describe(),
                           Share::describe(), Scanout::describe());
    } catch (...) { return "error\n"; }
}

// A config that asks for the plugin only while it is not loaded has
// Hyprland unload it on one reload and load it on the next, and every load
// reloads the config again, each inside the last, until the stack runs out.
// The load times are kept in the instance's runtime directory, which an
// unload leaves, and a load that comes too soon after the last few is refused.
constexpr size_t kLoopLoads  = 5;
constexpr auto   kLoopWindow = std::chrono::seconds{2};

bool loadingInALoop() {
    using std::chrono::nanoseconds;

    const auto path = g_pCompositor->m_instancePath + "/hypr-world.loads";
    const auto now  = std::chrono::steady_clock::now().time_since_epoch();

    std::vector<int64_t> recent;
    std::ifstream        in{path};
    for (int64_t loaded; in >> loaded;) {
        if (now - nanoseconds{loaded} < kLoopWindow)
            recent.push_back(loaded);
    }
    recent.push_back(std::chrono::duration_cast<nanoseconds>(now).count());

    std::ofstream out{path, std::ios::trunc};
    for (const auto loaded : recent)
        out << loaded << '\n';
    return recent.size() >= kLoopLoads;
}

void teardown() {
    // Input back first, then Hyprland's suspension, then the socket.
    Input::shutdown();
    // The buffers held for the app go back to their clients, and the commit
    // listeners go, before the code they call is unloaded.
    Share::shutdown();
    Scanout::shutdown();
    Feed::shutdown();
    App::shutdown();
    Server::stop();

    if (g_hyprctl) {
        HyprlandAPI::unregisterHyprCtlCommand(g_handle, g_hyprctl);
        g_hyprctl.reset();
    }
}

} // namespace

} // namespace World

using namespace World;

APICALL EXPORT std::string PLUGIN_API_VERSION() {
    return HYPRLAND_API_VERSION;
}

APICALL EXPORT PLUGIN_DESCRIPTION_INFO PLUGIN_INIT(HANDLE handle) {
    g_handle = handle;

    if (std::string{__hyprland_api_get_hash()} != __hyprland_api_get_client_hash())
        throw std::runtime_error("[hypr-world] built against a different Hyprland");

    if (loadingInALoop()) {
        notify(g_handle, "loaded and unloaded over and over, not loading again: does the config load it on every reload?", 15000, true);
        throw std::runtime_error("[hypr-world] load loop");
    }

    try {
        if (!HyprlandAPI::addLuaFunction(g_handle, "hypr_world", "toggle", luaToggle) ||
            !HyprlandAPI::addLuaFunction(g_handle, "hypr_world", "config", luaConfig))
            throw std::runtime_error("[hypr-world] cannot register hl.plugin.hypr_world.*");

        g_hyprctl = HyprlandAPI::registerHyprCtlCommand(g_handle, SHyprCtlCommand{.name = "hypr-world", .exact = false, .fn = onHyprctl});

        Input::init(g_handle);
        App::init(g_handle);
        // Optional: without it the app is only composited.
        Scanout::init(g_handle);
        Feed::init();

        if (!Server::start(g_pCompositor->m_instancePath + "/hypr-world.sock", {.line = onLine, .connection = onConnection}))
            throw std::runtime_error("[hypr-world] cannot start the socket");

        // Last: see Input::startListeners.
        Input::startListeners();
        App::startListeners();
    } catch (const std::exception&) {
        teardown();
        throw;
    } catch (...) {
        teardown();
        throw std::runtime_error("[hypr-world] init failed");
    }

    return {"hypr-world", "Input and focus glue for the hypr-world app", "ChrisRx", "0.1.0"};
}

APICALL EXPORT void PLUGIN_EXIT() {
    teardown();
}
