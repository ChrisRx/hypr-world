#pragma once

// The app's monitor's layer surfaces (the wallpaper, the bar), which the
// app shows in every frame as they are on the desktop. Hyprland draws a
// layer into a snapshot whenever it drew something new, and the pixels go to
// the app in a file attached to a `layer` line.

#include <hyprland/src/desktop/DesktopTypes.hpp>

#include <cstdint>
#include <vector>

namespace World::Layers {

// From the feed's tick: what came, went or drew something, at most once a
// second each. Out of view, or not among the layers the app sees, only a
// layer the app has never had is sent.
void tick(const PHLMONITOR& monitor, bool inView);

// The app's `layers` line: the layers it sees in a frame on screen.
void setSeen(const std::vector<uint64_t>& addresses);

// The app connected or went away: everything goes out afresh.
void reset();

} // namespace World::Layers
