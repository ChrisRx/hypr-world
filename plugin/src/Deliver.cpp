#include "Deliver.hpp"

#include <hyprland/src/desktop/state/WindowState.hpp>
#include <hyprland/src/desktop/view/Popup.hpp>
#include <hyprland/src/desktop/view/WLSurface.hpp>
#include <hyprland/src/desktop/view/Window.hpp>
#include <hyprland/src/output/Monitor.hpp>
#include <hyprland/src/protocols/core/Compositor.hpp>

namespace World::Deliver {

PHLWINDOW findWindow(uint64_t address) {
    if (!Desktop::windowState())
        return nullptr;

    for (const auto& window : Desktop::windowState()->windows()) {
        if (reinterpret_cast<uint64_t>(window.get()) != address)
            continue;

        if (!Desktop::View::validMapped(window) || !window->wlSurface() || !window->wlSurface()->resource())
            return nullptr;

        return window;
    }

    return nullptr;
}

Vector2D windowLocal(const PHLWINDOW& window, double u, double v, const Vector2D& buffer) {
    const auto   monitor = window->m_monitor.lock();
    const double scale   = monitor && monitor->m_scale > 0 ? monitor->m_scale : 1.0;

    // The renderer still adds the floating offset with the position ignored.
    return Vector2D{u * buffer.x / scale, v * buffer.y / scale} - window->m_floatingOffset;
}

std::optional<STarget> popupAt(const PHLWINDOW& window, const Vector2D& local) {
    // X11 windows have no xdg popups.
    if (window->m_isX11 || !window->m_popupHead)
        return std::nullopt;

    // By global position, as Hyprland's pointer path does. The main surface
    // box is the captured image's extent.
    const Vector2D origin = window->getWindowMainSurfaceBox().pos();
    const Vector2D global = origin + local;

    const auto popup = window->m_popupHead->at(global, true);
    if (!popup || !popup->wlSurface())
        return std::nullopt;
    const auto surface = popup->wlSurface()->resource();
    if (!surface)
        return std::nullopt;

    // The popup in window-local coordinates, then any subsurface of it.
    const Vector2D popupOrigin = popup->coordsGlobal() - origin;
    const auto [sub, subLocal] = surface->at(local - popupOrigin, true);
    if (sub)
        return STarget{sub, local - subLocal, 1.0};
    return STarget{surface, popupOrigin, 1.0};
}

std::optional<STarget> resolve(const PHLWINDOW& window, const Vector2D& local) {
    const auto main = window->wlSurface() ? window->wlSurface()->resource() : nullptr;
    if (!main)
        return std::nullopt;

    // X11 windows have no xdg popups or hit-testable subsurfaces here (and
    // Hyprland's own windowSurfaceAt asserts on them): the main surface, in
    // the X server's scale.
    if (window->m_isX11)
        return STarget{main, {}, window->m_X11SurfaceScaledBy};

    if (const auto popup = popupAt(window, local))
        return popup;

    // A subsurface, or the main surface, by input region. Off every input
    // region (a client-side shadow, a surface smaller than its box) Hyprland
    // falls back to the main surface, and so does this.
    const auto [sub, subLocal] = main->at(local, true);
    if (sub)
        return STarget{sub, local - subLocal, 1.0};

    return STarget{main, {}, 1.0};
}

} // namespace World::Deliver
