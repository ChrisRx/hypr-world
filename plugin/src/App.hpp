#pragma once

// The app's window and what the app asks of the compositor.
//
// The app lives on its own special workspace, special:hypr-world, fullscreen.
// A special workspace sits over whatever regular workspace the monitor shows,
// so the app can switch that workspace underneath itself and stay in view.
// The world key shows it (launching the app first if it is not running) and
// hands it the input; pressing it again gives the input back and hides it.
//
// The app shows the workspace underneath it. The plugin tells it which one
// that is and which of its windows to show (`workspace <id> <addr>`) on every
// workspace change and when the app comes into view.

#include <hyprland/src/plugins/PluginAPI.hpp>

#include <cstdint>
#include <string>
#include <vector>

namespace World::App {

void init(HANDLE handle);

// Event bus listeners; registered last in PLUGIN_INIT, like Input's.
void startListeners();

void shutdown();

// The app's settings (`world` in hl.plugin.hypr_world.config) as one JSON
// object, passed on to the app now and on every reconnect.
void setSettings(std::string settings);

// The world key.
void toggle();

// For `hyprctl hypr-world`: whether the app is in view, and the app the world
// key launches.
std::string describe();

// The monitor the app is on: the one showing it, else the focused one.
PHLMONITOR monitor();

// The app's own window, once it is mapped.
PHLWINDOW window();

// The app's special workspace is shown, or the app is a backend's
// (setBackend): then it is always in view.
bool inView();

// The connected app runs outside this compositor, with this one as its
// backend (plugin/nested.sh backend): its window is in another session, so
// it counts as in view, and the world key leaves it alone. Until it goes.
void setBackend();

// The app connected (its process id is Server::peerPid()) or went away.
void appConnected(bool connected);

// The flight in and out: the app fades its window in and out over the
// desktop with this (`alpha`), and says `hide` when it has flown back into
// the active frame. Then the input goes back to Hyprland and the app out of
// view.
void fade(float opacity);
void hidden();

// The windows the app displays. While the app is in view they are kept out
// of Hyprland's suspension, which hidden windows otherwise get, so their
// clients keep drawing at full rate.
void setLive(const std::vector<uint64_t>& addresses);

// Those of them still about.
std::vector<PHLWINDOW> live();

// Show workspace `id` underneath the app.
void showWorkspace(int64_t id);

// close <addr>: the window the app's rocket hit, closed as Hyprland's own
// close does, by asking its client, which may still ask about unsaved work.
// Never the app's own window.
void close(uint64_t address);

// terminate <addr>: a window hit again soon after a close that left it
// open: its app told to end (SIGTERM, as Hyprland's signalwindow), which
// ends every window of that app, and lets it save first if it handles it.
void terminate(uint64_t address);

} // namespace World::App
