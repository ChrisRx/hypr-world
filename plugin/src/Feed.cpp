#include "Feed.hpp"

#include "App.hpp"
#include "Layers.hpp"
#include "Server.hpp"
#include "Share.hpp"
#include "Util.hpp"

#include <hyprland/src/desktop/Workspace.hpp>
#include <hyprland/src/desktop/state/WindowState.hpp>
#include <hyprland/src/desktop/view/Popup.hpp>
#include <hyprland/src/desktop/view/WLSurface.hpp>
#include <hyprland/src/desktop/view/Window.hpp>
#include <hyprland/src/managers/fullscreen/FullscreenController.hpp>
#include <hyprland/src/output/Monitor.hpp>
#include <hyprland/src/protocols/core/Compositor.hpp>
#include <hyprland/src/state/WorkspaceState.hpp>

#include <algorithm>
#include <chrono>
#include <format>
#include <map>
#include <optional>
#include <string>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>

namespace World::Feed {

namespace {

// Every frame at 60 Hz, so the app follows Hyprland's animations.
constexpr auto kInterval = std::chrono::milliseconds(16);
// Out of view nothing the app shows is seen, so the feed only keeps up.
constexpr auto kHiddenInterval = std::chrono::milliseconds(250);

struct SWindow {
    int64_t workspace = 0;
    long    x = 0, y = 0, w = 0, h = 0;
    size_t  stack = 0;
    char    kind  = 't';

    bool operator==(const SWindow&) const = default;
};

// What the app was last told.
struct SSent {
    std::string                           monitor;
    std::map<int64_t, std::string>        workspaces;
    std::unordered_map<uint64_t, SWindow> windows;
    std::optional<bool>                   shown;
};

SSent               g_sent;
SP<CEventLoopTimer> g_timer;

// Every surface each window draws (its own and its subsurfaces, its popups
// and theirs), with a listener on its commits: what changed since the last
// tick, so the app copies only those again.
std::unordered_map<uint64_t, SWatched> g_watched;
std::unordered_set<uint64_t>           g_damaged;

void watch(const PHLWINDOW& window, uint64_t address) {
    auto&      surfaces = g_watched[address];
    const auto tree     = [&](const SP<Desktop::View::CWLSurface>& surface) {
        if (watchTree(surfaces, surface ? surface->resource() : nullptr, [address] { g_damaged.insert(address); }))
            g_damaged.insert(address);
    };

    tree(window->wlSurface());
    if (window->m_popupHead)
        window->m_popupHead->breadthfirst(
            [&](SP<Desktop::View::CPopup> popup, void*) {
                if (popup)
                    tree(popup->wlSurface());
            },
            nullptr);
}

// monitor <w> <h> <scale> <wx> <wy> <ww> <wh>: the work area is the monitor
// without exclusive zones such as the bar, monitor-local.
void sendMonitor(const PHLMONITOR& monitor) {
    const auto box  = monitor->logicalBox();
    const auto work = monitor->logicalBoxMinusReserved();

    auto line = std::format("monitor {} {} {} {} {} {} {}", px(box.w), px(box.h), monitor->m_scale, px(work.x - box.x), px(work.y - box.y), px(work.w), px(work.h));
    if (line != g_sent.monitor) {
        Server::send(line);
        g_sent.monitor = std::move(line);
    }
}

void sendWorkspaces(const PHLMONITOR& monitor) {
    std::map<int64_t, std::string> now;
    for (const auto& workspace : State::workspaceState()->workspaces()) {
        if (!workspace->m_isSpecialWorkspace && workspace->m_monitor.lock() == monitor)
            now.emplace(workspace->m_id, workspace->m_name);
    }

    for (const auto& [id, name] : now) {
        if (const auto it = g_sent.workspaces.find(id); it == g_sent.workspaces.end() || it->second != name)
            Server::send(std::format("ws {} {}", id, name));
    }
    for (const auto& [id, name] : g_sent.workspaces) {
        if (!now.contains(id))
            Server::send(std::format("ws-gone {}", id));
    }
    g_sent.workspaces = std::move(now);
}

// window <addr> <ws> <x> <y> <w> <h> <stack> <kind>: the current, animated
// box, so the app moves with Hyprland. The stack is the place in
// Hyprland's window list, its stacking order.
void sendWindows(const PHLMONITOR& monitor) {
    const auto origin = monitor->logicalBox().pos();
    const auto app    = App::window();

    std::unordered_map<uint64_t, SWindow> now;
    size_t                                stack = 0;

    for (const auto& window : Desktop::windowState()->windows()) {
        ++stack;
        if (!Desktop::View::validMapped(window) || window->isHidden() || window == app)
            continue;

        const auto& workspace = window->m_workspace;
        if (!workspace || workspace->m_isSpecialWorkspace || workspace->m_monitor.lock() != monitor)
            continue;

        const auto pos  = window->position(Desktop::View::IGeometric::GEOMETRIC_CURRENT) - origin;
        const auto size = window->size(Desktop::View::IGeometric::GEOMETRIC_CURRENT);
        const char kind = Fullscreen::controller()->isFullscreen(window) ? 'F' : window->m_isFloating ? 'f' : 't';

        const auto address = reinterpret_cast<uint64_t>(window.get());
        now.emplace(address, SWindow{.workspace = workspace->m_id, .x = px(pos.x), .y = px(pos.y), .w = px(size.x), .h = px(size.y), .stack = stack, .kind = kind});
        watch(window, address);
    }

    for (const auto& [address, w] : now) {
        if (const auto it = g_sent.windows.find(address); it == g_sent.windows.end() || it->second != w)
            Server::send(std::format("window 0x{:x} {} {} {} {} {} {} {}", address, w.workspace, w.x, w.y, w.w, w.h, w.stack, w.kind));
    }
    for (const auto& [address, w] : g_sent.windows) {
        if (!now.contains(address))
            Server::send(std::format("closed 0x{:x}", address));
    }

    // damaged <addr>...: the windows that drew something since the last tick.
    std::erase_if(g_watched, [&](const auto& w) { return !now.contains(w.first); });
    std::string damaged;
    for (const uint64_t address : g_damaged) {
        if (now.contains(address))
            damaged += std::format(" 0x{:x}", address);
    }
    g_damaged.clear();
    if (!damaged.empty())
        Server::send("damaged" + damaged);

    g_sent.windows = std::move(now);
}

void tick() {
    const auto monitor = App::monitor();
    if (!monitor)
        return;

    const bool shown = App::inView();

    sendMonitor(monitor);
    sendWorkspaces(monitor);
    sendWindows(monitor);
    Layers::tick(monitor, shown);
    Share::tick(shown);

    if (g_sent.shown != shown) {
        Server::send(shown ? "shown" : "hidden");
        g_sent.shown = shown;
    }
}

void onTimer(SP<CEventLoopTimer> self, void*) {
    if (!Server::connected())
        return;

    refresh();
    self->updateTimeout(App::inView() ? kInterval : kHiddenInterval);
}

} // namespace

bool showsSomething(const SP<CWLSurfaceResource>& surface) {
    const auto& changed = surface->m_current.updated.bits;
    return changed.buffer || changed.damage || changed.viewport || changed.offset;
}

bool watchTree(SWatched& watched, const SP<CWLSurfaceResource>& root, std::function<void()> drew) {
    std::erase_if(watched, [](const auto& w) { return w.first.expired(); });
    if (!root)
        return false;

    bool added = false;
    root->breadthfirst(
        [&](SP<CWLSurfaceResource> surface, const Vector2D&, void*) {
            if (!surface || std::ranges::any_of(watched, [&](const auto& w) { return w.first.lock() == surface; }))
                return;
            watched.emplace_back(surface, surface->m_events.commit.listen([drew, weak = WP<CWLSurfaceResource>(surface)] {
                if (const auto committed = weak.lock(); committed && showsSomething(committed))
                    drew();
            }));
            added = true;
        },
        nullptr);
    return added;
}

void refresh() {
    if (!Server::connected())
        return;

    guarded("feed", tick);
}

void init() {
    // Disarmed until the app connects, so an idle compositor stays idle.
    g_timer = makeShared<CEventLoopTimer>(std::nullopt, onTimer, nullptr);
    g_pEventLoopManager->addTimer(g_timer);
}

// Nothing sent or watched: what the app has goes out afresh.
static void reset() {
    g_sent = {};
    g_watched.clear();
    g_damaged.clear();
    Layers::reset();
}

void shutdown() {
    removeTimer(g_timer);
    reset();
}

void appConnected(bool connected) {
    reset();

    if (g_timer)
        g_timer->updateTimeout(connected ? std::optional<Time::steady_dur>{std::chrono::milliseconds(0)} : std::nullopt);
}

} // namespace World::Feed
