#pragma once

// Where a point on a window's captured image lands: the surface under it and
// the point in that surface's coordinates.
//
// The image hyprland-toplevel-export-v1 gives is exactly the window's main
// surface box at its current size times the monitor scale: no borders, never
// flipped, popups and subsurfaces drawn at their offsets. So a point (u, v)
// on it is the window-local point (u * bw / s, v * bh / s), and from there
// the lookup is the one Hyprland's own pointer path does.

#include <hyprland/src/desktop/DesktopTypes.hpp>
#include <hyprland/src/helpers/math/Math.hpp>

#include <cstdint>
#include <optional>

class CWLSurfaceResource;

namespace World::Deliver {

// The window whose address (as `hyprctl clients` prints it) is `address`,
// if it is still mapped and has a surface.
PHLWINDOW findWindow(uint64_t address);

// The window-local logical point for (u, v) on its captured image of
// `buffer` pixels. Not clamped: a drag may aim past the edges.
Vector2D windowLocal(const PHLWINDOW& window, double u, double v, const Vector2D& buffer);

// A surface and how window-local points map into it:
// local = (windowLocal - offset) * scale.
struct STarget {
    SP<CWLSurfaceResource> surface;
    Vector2D               offset;
    double                 scale = 1.0;

    Vector2D local(const Vector2D& windowLocal) const {
        return (windowLocal - offset) * scale;
    }
};

// The surface under a window-local point: popups topmost first, then
// subsurfaces by input region, then the main surface. Resolved fresh every
// time, never cached: popups come and go.
std::optional<STarget> resolve(const PHLWINDOW& window, const Vector2D& windowLocal);

// The popup (or a subsurface of one) under a window-local point, if any: the
// one thing there can be past the window's own box.
std::optional<STarget> popupAt(const PHLWINDOW& window, const Vector2D& windowLocal);

} // namespace World::Deliver
