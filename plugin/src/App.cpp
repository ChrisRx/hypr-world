#include "App.hpp"

#include "Feed.hpp"
#include "Input.hpp"
#include "Server.hpp"
#include "Util.hpp"

#include <hyprland/src/config/shared/actions/ConfigActions.hpp>
#include <hyprland/src/config/supplementary/executor/Executor.hpp>
#include <hyprland/src/debug/log/Logger.hpp>
#include <hyprland/src/desktop/Workspace.hpp>
#include <hyprland/src/desktop/state/FocusState.hpp>
#include <hyprland/src/desktop/state/GlobalWindowController.hpp>
#include <hyprland/src/desktop/state/WindowState.hpp>
#include <hyprland/src/desktop/view/LayerSurface.hpp>
#include <hyprland/src/desktop/view/Window.hpp>
#include <hyprland/src/event/EventBus.hpp>
#include <hyprland/src/helpers/MiscFunctions.hpp>
#include <hyprland/src/managers/fullscreen/FullscreenController.hpp>
#include <hyprland/src/output/Monitor.hpp>
#include <hyprland/src/protocols/LayerShell.hpp>
#include <hyprland/src/render/Renderer.hpp>
#include <hyprland/src/state/MonitorState.hpp>
#include <hyprland/src/state/WorkspaceState.hpp>

#include <chrono>
#include <csignal>
#include <cstdlib>
#include <format>
#include <optional>
#include <string_view>
#include <utility>
#include <vector>

#include <unistd.h>

namespace World::App {

namespace {

using Clock = std::chrono::steady_clock;

constexpr const char* kSpecial = "special:hypr-world";

// The app the world key launches when none is running, fixed when the plugin
// is built (CMakeLists.txt, HYPR_WORLD_APP).
constexpr const char* kApp = HYPR_WORLD_APP;

// Long enough for a cold start of the app (Bevy, the GPU), short enough that
// a launch that went wrong gets noticed.
constexpr auto kLaunchTimeout = std::chrono::seconds(20);

// Backstop for re-asserting live windows; the workspace event covers the
// usual case right away.
constexpr auto kTick = std::chrono::milliseconds(500);

// The flight in and out takes 0.55 s. Past these, the app is assumed stuck
// and the plugin does without: shows the app as it is, or hides it.
constexpr auto kFadeTimeout  = std::chrono::seconds(1);
constexpr auto kLeaveTimeout = std::chrono::seconds(2);

HANDLE      g_handle = nullptr;

std::string g_settings = "{}";

// The world key was pressed before the app's window existed (it was being
// launched): shown when the window maps, if it does by then.
std::optional<Clock::time_point> g_showWhenReady;

// Shown at no opacity, waiting until then for the app to fade itself in.
std::optional<Clock::time_point> g_awaitingFade;

// The app window's opacity, as last set.
float g_opacity = 1.F;

// Asked to leave: the app is flying out and will say `hide` by then.
std::optional<Clock::time_point> g_leaving;

std::vector<PHLWINDOWREF> g_live;

// The window the world key was pressed from: what the app shows first when
// it comes into view.
PHLWINDOWREF g_cameFrom;

struct SListeners {
    CHyprSignalListener windowOpen, workspaceActive;
};
SListeners g_listeners;

SP<CEventLoopTimer> g_tick;

// Deferred work: compositor state changes never run from inside a listener
// or a Lua call (the world key runs from a Lua keybind).
uint64_t g_laterShow      = 0;
uint64_t g_laterWorkspace = 0;
uint64_t g_laterRedirect  = 0;

// Windows that opened on the app's special workspace while it was in view,
// to be moved to the workspace underneath it.
std::vector<PHLWINDOWREF> g_redirect;

void check(const Config::Actions::ActionResult& result, const char* what) {
    if (!result)
        Log::logger->log(Log::ERR, "[hypr-world] {}: {}", what, result.error().message);
}

// World::later, with anything `work` throws logged as the app's.
void later(uint64_t& slot, void (*work)()) {
    World::later(slot, [work] { guarded("app", work); });
}

// Whether `deadline` is set and past; then it is cleared.
bool passed(std::optional<Clock::time_point>& deadline, Clock::time_point now) {
    if (!deadline || now <= *deadline)
        return false;
    deadline.reset();
    return true;
}

// special:hypr-world, created on `monitor` if it does not exist yet (with no
// monitor, only looked up).
PHLWORKSPACE specialWorkspace(const PHLMONITOR& monitor) {
    const auto [id, name, isAutoID] = getWorkspaceIDNameFromString(kSpecial);
    if (id == WORKSPACE_INVALID)
        return nullptr;

    if (auto workspace = State::workspaceState()->query().id(id).run())
        return workspace;
    if (!monitor)
        return nullptr;
    return State::workspaceState()->create(id, monitor->m_id, name);
}

// The monitor showing the app's special workspace, if any is.
PHLMONITOR shownOn() {
    const auto workspace = specialWorkspace(nullptr);
    if (!workspace)
        return nullptr;

    for (const auto& monitor : State::monitorState()->monitors()) {
        if (monitor->m_activeSpecialWorkspace == workspace)
            return monitor;
    }
    return nullptr;
}

void assertLive() {
    if (!shownOn())
        return;

    // setSuspended does nothing for a window already in that state.
    for (const auto& ref : g_live) {
        if (const auto window = ref.lock())
            window->setSuspended(false);
    }
}

// Hyprland's own suspension back for every window (hidden ones suspended).
void restoreSuspension() {
    if (Desktop::globalWindowController())
        Desktop::globalWindowController()->updateSuspendedStates();
}

// No live windows any more, and those there were suspended again.
void clearLive() {
    if (!g_live.empty()) {
        g_live.clear();
        restoreSuspension();
    }
}

// Tells the app the workspace underneath it and the window to show from it:
// `preferred` when that is on the workspace, else the window Hyprland would
// focus there. Never the app's own window.
void report(const PHLWORKSPACE& workspace, PHLWINDOW preferred = nullptr) {
    if (!Server::connected() || !workspace || workspace->m_isSpecialWorkspace)
        return;

    auto window = preferred && Desktop::View::validMapped(preferred) && preferred->m_workspace == workspace ? preferred : workspace->getFocusCandidate();
    if (window && window == App::window())
        window = nullptr;

    if (window)
        Server::send(std::format("workspace {} 0x{:x}", workspace->m_id, reinterpret_cast<uintptr_t>(window.get())));
    else
        Server::send(std::format("workspace {} -", workspace->m_id));
}

// The real bar crosses over with the app's own picture of it (Layers.hpp).
// Hyprland draws the top layer over every window, the app's too, fades it
// out under the app's fullscreen window, and brings it back when the
// workspace underneath changes. So while the app is in view the bar
// shows as much as the app does not, and not at all where a fullscreen
// window covers it on the desktop underneath: the end of a flight either way
// is the desktop, bar and all, or the app with nothing over it.
void crossBars() {
    const auto monitor = shownOn();
    if (!monitor)
        return;

    const auto  workspace = monitor->m_activeWorkspace;
    const auto  covering  = workspace ? Fullscreen::controller()->getFullscreenWindow(workspace, true) : nullptr;
    const bool  covered   = covering && Fullscreen::controller()->getFullscreenModes(covering).internal != Fullscreen::FSMODE_MAXIMIZED;
    const float alpha     = covered ? 0.F : 1.F - g_opacity;

    bool changed = false;
    for (const auto& ref : monitor->m_layerSurfaceLayers[ZWLR_LAYER_SHELL_V1_LAYER_TOP]) {
        const auto layer = ref.lock();
        // One Hyprland raised over fullscreen windows stays as it has it.
        if (!layer || layer->m_aboveFullscreen)
            continue;

        auto& fade = layer->alpha()[Desktop::View::LS_ALPHA_FADE];
        if (fade->value() == alpha && fade->goal() == alpha)
            continue;
        fade->setValueAndWarp(alpha);
        changed = true;
    }
    if (changed)
        g_pHyprRenderer->damageMonitor(monitor);
}

void onWorkspaceChanged() {
    // Hyprland's suspension and its bar's fade just ran again for the change.
    assertLive();
    crossBars();

    if (const auto monitor = App::monitor())
        report(monitor->m_activeWorkspace);
}

// The app window's own fade factor, which only map and unmap animate
// otherwise; the others are Hyprland's (focus, fullscreen, moves). The bar
// fades the other way.
void setOpacity(float opacity) {
    g_opacity = std::clamp(opacity, 0.F, 1.F);
    crossBars();

    const auto window = App::window();
    if (!window)
        return;

    window->alpha(Desktop::View::WINDOW_ALPHA_FADE)->setValueAndWarp(g_opacity);
    g_pHyprRenderer->damageWindow(window);
}

// Puts the app's window on special:hypr-world, fullscreen, shows that over
// the current workspace, and hands the app the input.
void showNow() {
    const auto window = App::window();
    if (!window) {
        // Launched but not mapped yet; the window open listener comes back.
        g_showWhenReady = Clock::now() + kLaunchTimeout;
        return;
    }
    g_showWhenReady.reset();

    const auto monitor = Desktop::focusState()->monitor();
    const auto special = specialWorkspace(monitor);
    if (!monitor || !special)
        return;

    if (window->m_workspace != special)
        check(Config::Actions::moveToWorkspace(special, true, window), "moving the app to its workspace");

    if (!Fullscreen::controller()->isFullscreen(window, Fullscreen::FSMODE_FULLSCREEN))
        Fullscreen::controller()->setFullscreenMode(window, Fullscreen::FSMODE_FULLSCREEN, Fullscreen::FSMODE_FULLSCREEN, true);

    // A game, so render:direct_scanout = 2, scanout for games only, takes
    // the app too.
    if (window->getContentType() != NContentType::CONTENT_TYPE_GAME)
        window->setContentType(NContentType::CONTENT_TYPE_GAME);

    // Clear until the app fades itself in over the desktop (the flight in),
    // so the frame it last drew never shows.
    setOpacity(0.F);

    if (monitor->m_activeSpecialWorkspace != special)
        monitor->setSpecialWorkspace(special);

    // At once, and over a desktop that is neither dimmed nor blurred: the
    // app does its own coming in.
    special->m_alpha->setValueAndWarp(1.F);
    special->m_renderOffset->setValueAndWarp(Vector2D{});
    monitor->m_specialDim->setValueAndWarp(0.F);
    monitor->m_specialBlur->setValueAndWarp(0.F);
    // Opening it faded the bar out; the app is still clear.
    crossBars();

    // The app's window is on the special workspace, so this focus leaves it
    // open (focusing a window anywhere else would close it).
    check(Config::Actions::focus(window), "focusing the app");

    report(monitor->m_activeWorkspace, g_cameFrom.lock());
    Input::enterLook("toggle");
    assertLive();

    g_leaving.reset();
    g_awaitingFade = Clock::now() + kFadeTimeout;

    // The desktop as it is now, `shown` with it, then in.
    Feed::refresh();
    Server::send("enter");
}

void hideNow() {
    if (const auto monitor = shownOn()) {
        const auto special = monitor->m_activeSpecialWorkspace;
        monitor->setSpecialWorkspace(nullptr);
        // Gone at once: the app has already faded itself out.
        if (special)
            special->m_alpha->setValueAndWarp(0.F);
    }

    // Back to opaque for whatever shows it next.
    setOpacity(1.F);
    restoreSuspension();
}

// The app's output, which Hyprland would send nowhere: in $HYPR_WORLD_DIR, as
// its pipe is, when that is set (a nested test session, plugin/nested.sh),
// else in $XDG_RUNTIME_DIR.
std::string logPath() {
    for (const char* name : {"HYPR_WORLD_DIR", "XDG_RUNTIME_DIR"}) {
        if (const char* dir = std::getenv(name); dir && *dir)
            return std::string(dir) + "/hypr-world.log";
    }
    return "/tmp/hypr-world.log";
}

// One word for the /bin/sh -c the executor runs a command with.
std::string shellQuoted(std::string_view word) {
    std::string out = "'";
    for (const char c : word) {
        if (c == '\'')
            out += "'\\''";
        else
            out += c;
    }
    return out + "'";
}

void launch() {
    if (access(kApp, X_OK) != 0) {
        notify(g_handle, std::format("no app at {}: cargo build --release", kApp), 6000, true);
        return;
    }

    // Its log from this launch alone.
    const auto command = std::format("exec {} >{} 2>&1", shellQuoted(kApp), shellQuoted(logPath()));
    if (!Config::Supplementary::executor()->spawnRaw(command)) {
        notify(g_handle, std::format("could not launch the app: {}", kApp), 4000, true);
        return;
    }

    g_showWhenReady = Clock::now() + kLaunchTimeout;
    notify(g_handle, "launching hypr-world", 4000);
}

void onTick(SP<CEventLoopTimer> self, void*) {
    try {
        const auto now = Clock::now();
        if (passed(g_showWhenReady, now))
            notify(g_handle, std::format("the app did not show up; see {}", logPath()), 4000);
        if (passed(g_awaitingFade, now))
            setOpacity(1.F);
        if (passed(g_leaving, now))
            hidden();
        assertLive();
        // A fullscreen change underneath fades the bar on its own.
        crossBars();
    } catch (...) {}
    self->updateTimeout(kTick);
}

// A window opens on the focused workspace, which while the app is in view
// is its special one, under the app. Each that did goes to the workspace
// underneath instead, the one the crosshair last rested on, so an app
// launched from the app (rofi, a bind) hangs on that frame. Silently: the
// workspace does not change, and the app keeps the focus.
void redirectNow() {
    auto windows = std::exchange(g_redirect, {});

    const auto monitor = shownOn();
    const auto special = specialWorkspace(nullptr);
    const auto app     = window();
    if (!monitor || !special || !monitor->m_activeWorkspace)
        return;

    PHLWINDOW moved;
    for (const auto& ref : windows) {
        const auto window = ref.lock();
        if (!window || !Desktop::View::validMapped(window) || window == app || window->m_workspace != special)
            continue;
        check(Config::Actions::moveToWorkspace(monitor->m_activeWorkspace, true, window), "moving a new window under the app");
        moved = window;
    }
    if (!moved)
        return;

    // The new window took the focus, and the move hands it on to whatever is
    // left on the special workspace: give it back to the app, as showNow does.
    if (app && Desktop::focusState()->window() != app)
        check(Config::Actions::focus(app), "focusing the app");

    // And type into what was opened, the last of them, as it would have the
    // keyboard in 2D.
    Input::typeInto(moved, "opened");
}

} // namespace

void init(HANDLE handle) {
    g_handle = handle;

    g_tick = makeShared<CEventLoopTimer>(kTick, onTick, nullptr);
    g_pEventLoopManager->addTimer(g_tick);
}

void startListeners() {
    auto& events = Event::bus()->m_events;

    // The launched app's window, when it maps, and any other that opens
    // under the app while it is in view (redirectNow).
    g_listeners.windowOpen = events.window.open.listen([](PHLWINDOW window) {
        try {
            if (!window)
                return;
            if (g_showWhenReady && window->getPID() == Server::peerPid())
                later(g_laterShow, showNow);
            else if (shownOn() && window->m_workspace && window->m_workspace == specialWorkspace(nullptr)) {
                g_redirect.emplace_back(window);
                later(g_laterRedirect, redirectNow);
            }
        } catch (...) {}
    });

    // A workspace change: the app follows it, and Hyprland's suspension, re-run
    // right after this event, would suspend the app's hidden windows again.
    g_listeners.workspaceActive = events.workspace.active.listen([](PHLWORKSPACE) {
        try {
            later(g_laterWorkspace, onWorkspaceChanged);
        } catch (...) {}
    });
}

void shutdown() {
    g_listeners = {};
    g_redirect.clear();

    for (uint64_t* slot : {&g_laterShow, &g_laterWorkspace, &g_laterRedirect})
        cancelLater(*slot);
    removeTimer(g_tick);

    clearLive();
    g_showWhenReady.reset();
    g_cameFrom.reset();
}

// settings <json>: the whole of them, one line.
static void sendSettings() {
    if (Server::connected())
        Server::send("settings " + g_settings);
}

void setSettings(std::string settings) {
    // A config reload with nothing new for the app.
    if (settings == g_settings)
        return;

    g_settings = std::move(settings);
    sendSettings();
}

// The app's window: the mapped window of the connected app's process.
PHLWINDOW window() {
    const int pid = Server::peerPid();
    if (pid <= 0 || !Desktop::windowState())
        return nullptr;

    for (const auto& window : Desktop::windowState()->windows()) {
        if (Desktop::View::validMapped(window) && window->getPID() == pid)
            return window;
    }
    return nullptr;
}

PHLMONITOR monitor() {
    if (const auto monitor = shownOn())
        return monitor;
    return Desktop::focusState()->monitor();
}

bool inView() {
    return shownOn() != nullptr;
}

std::string describe() {
    return std::format("{}, launches {}", shownOn() ? "in view" : "out of view", kApp);
}

void toggle() {
    if (!Server::connected()) {
        // A launch still starting up is waited for, not repeated.
        if (g_showWhenReady)
            notify(g_handle, "the app is starting", 4000);
        else {
            g_cameFrom = Desktop::focusState()->window();
            launch();
        }
        return;
    }

    // In view or holding input: the app flies back into the active frame
    // and says `hide` once it is there. Pressed again on the way out, it
    // turns round. Otherwise into the app.
    if (Input::state() != Input::EState::Off || shownOn()) {
        if (g_leaving) {
            g_leaving.reset();
            Server::send("enter");
            return;
        }
        g_leaving = Clock::now() + kLeaveTimeout;
        Server::send("leave");
        return;
    }

    g_cameFrom = Desktop::focusState()->window();
    later(g_laterShow, showNow);
}

void appConnected(bool connected) {
    if (connected) {
        sendSettings();

        // Launched by the world key: show it as soon as its window is there.
        if (g_showWhenReady)
            later(g_laterShow, showNow);
        return;
    }

    clearLive();

    // The app quit: its special workspace is empty now, and still open over
    // the desktop.
    g_showWhenReady.reset();
    g_awaitingFade.reset();
    g_leaving.reset();
    later(g_laterShow, hideNow);
}

void fade(float opacity) {
    g_awaitingFade.reset();
    setOpacity(opacity);
}

void hidden() {
    g_leaving.reset();
    Input::leave("toggle");
    later(g_laterShow, hideNow);
}

void setLive(const std::vector<uint64_t>& addresses) {
    g_live.clear();

    if (Desktop::windowState()) {
        for (const auto& window : Desktop::windowState()->windows()) {
            for (const uint64_t address : addresses) {
                if (reinterpret_cast<uint64_t>(window.get()) == address)
                    g_live.emplace_back(window);
            }
        }
    }

    // Windows dropped from the set get Hyprland's suspension back; the rest
    // are taken out of it again.
    restoreSuspension();
    assertLive();
}

std::vector<PHLWINDOW> live() {
    std::vector<PHLWINDOW> windows;
    for (const auto& ref : g_live) {
        if (const auto window = ref.lock())
            windows.emplace_back(window);
    }
    return windows;
}

void showWorkspace(int64_t id) {
    // changeWorkspace keeps the app's special workspace open on top.
    check(Config::Actions::changeWorkspace(std::to_string(id)), "changing workspace");
}

// The mapped window at `address`, never the app's own.
static PHLWINDOW hitWindow(uint64_t address) {
    for (const auto& window : Desktop::windowState()->windows()) {
        if (reinterpret_cast<uint64_t>(window.get()) == address)
            return Desktop::View::validMapped(window) && window != App::window() ? window : nullptr;
    }
    return nullptr;
}

void close(uint64_t address) {
    if (const auto window = hitWindow(address))
        check(Config::Actions::closeWindow(window), "closing a window");
}

void terminate(uint64_t address) {
    if (const auto window = hitWindow(address))
        check(Config::Actions::signalWindow(SIGTERM, window), "ending a window's app");
}

} // namespace World::App
