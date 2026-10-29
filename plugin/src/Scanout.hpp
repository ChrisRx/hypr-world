#pragma once

// The app's frames straight to the display. Hyprland puts a fullscreen
// window's buffer on the display, skipping its own compositing, only when
// that window is the monitor's solitary client, and the app never is: it
// lives on a special workspace, which CMonitor::isSolitaryBlocked refuses
// before it looks at anything else. A hook on CMonitor::recheckSolitary,
// which Hyprland runs at the start of every monitor frame, picks the app
// when Hyprland's own checks found nobody and the app covers the monitor
// alone, the way those checks would have it for a regular fullscreen window. Direct scanout then still has its own say
// (CMonitor::isDSBlocked): render:direct_scanout on, no capture or screen
// share pending, a dmabuf of the monitor's size. Hyprland's hooks only work
// on x86_64; where the hook cannot go in, the app stays composited.

#include <hyprland/src/plugins/PluginAPI.hpp>

#include <string>

namespace World::Scanout {

// Hooks recheckSolitary if it can. Never throws: without the hook the app
// is only composited, as it always was.
void init(HANDLE handle);

void shutdown();

// For `hyprctl hypr-world`: whether the hook is in, and whether the app is
// its monitor's solitary client now.
std::string describe();

} // namespace World::Scanout
