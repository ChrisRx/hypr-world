#pragma once

// What the app is told about the desktop it shows: its monitor and work
// area, that monitor's workspaces, and their windows with their live 2D
// boxes. Polled while the app is connected and sent as changes, so the app
// follows Hyprland's animations.

#include <hyprland/src/helpers/memory/Memory.hpp>
#include <hyprland/src/helpers/signal/Signal.hpp>

#include <functional>
#include <utility>
#include <vector>

class CWLSurfaceResource;

namespace World::Feed {

void init();

void shutdown();

// The app connected (everything goes out afresh) or went away.
void appConnected(bool connected);

// Sends what changed now rather than at the next tick: the app is about to
// be shown and needs the desktop as it is.
void refresh();

// Whether a surface's last commit brought something new to show: a buffer,
// damage, or the buffer cropped or moved. Not one that only asked for a
// frame callback or set a region. Hyprland applies a commit to m_current
// before it tells the commit's listeners, so they can ask this.
bool showsSomething(const SP<CWLSurfaceResource>& surface);

// Surfaces watched for commits, with their listeners.
using SWatched = std::vector<std::pair<WP<CWLSurfaceResource>, CHyprSignalListener>>;

// `root` and its subsurfaces watched in `watched`, after those gone are
// dropped: `drew` runs on each of their commits that shows something. True
// if any of them was new: a surface that appears has something new to show.
bool watchTree(SWatched& watched, const SP<CWLSurfaceResource>& root, std::function<void()> drew);

} // namespace World::Feed
