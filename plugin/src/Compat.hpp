#pragma once

// What differs between the Hyprland releases the plugin builds against
// (0.56 and 0.57): headers that moved, and APIs that changed shape. Each
// accessor answers as the 0.56 member it stands in for did.
// HYPR_WORLD_HYPRLAND is the version CMake found, 0.57.0 as 5700.

#ifndef HYPR_WORLD_HYPRLAND
#error "HYPR_WORLD_HYPRLAND is set by CMakeLists.txt"
#endif

#define HYPR_WORLD_HYPRLAND_AT_LEAST(major, minor) (HYPR_WORLD_HYPRLAND >= (major) * 10000 + (minor) * 100)

#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
#include <hyprland/src/desktop/view/window/Window.hpp>
#include <hyprland/src/desktop/view/window/WindowPresentation.hpp>
#include <hyprland/src/keybinds/Manager.hpp>
#include <hyprland/src/workspace/HLWorkspace.hpp>
#else
#include <hyprland/src/desktop/Workspace.hpp>
#include <hyprland/src/desktop/view/Window.hpp>
#include <hyprland/src/managers/KeybindManager.hpp>
#endif

#include <hyprland/src/desktop/view/LayerSurface.hpp>
#include <hyprland/src/desktop/view/Popup.hpp>
#include <hyprland/src/desktop/view/WLSurface.hpp>
#include <hyprland/src/managers/input/InputManager.hpp>
#include <hyprland/src/plugins/PluginAPI.hpp>
#include <hyprland/src/protocols/XDGShell.hpp>
#include <hyprland/src/protocols/core/Compositor.hpp>

#include <cstdint>
#include <functional>
#include <optional>
#include <string>
#include <string_view>
#include <sys/types.h>
#include <utility>

namespace World::Compat {

// --- hyprctl ----------------------------------------------------------------

#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
using HyprCtlCommand = IPC::Socket1::SCommand;
#else
using HyprCtlCommand = SHyprCtlCommand;
#endif

// A hyprctl command for every request that starts with `name`, answered with
// what `reply` makes of the whole request.
inline HyprCtlCommand hyprctlCommand(std::string name, std::function<std::string(const std::string&)> reply) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return {.name    = std::move(name),
            .match   = IPC::Socket1::COMMAND_MATCH_PREFIX,
            .handler = [reply = std::move(reply)](const IPC::Socket1::SRequest& request) -> IPC::Socket1::SResponse { return reply(request.command); }};
#else
    return {.name = std::move(name), .exact = false, .fn = [reply = std::move(reply)](eHyprCtlOutputFormat, std::string request) { return reply(request); }};
#endif
}

// --- keyboard ---------------------------------------------------------------

#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
inline constexpr uint32_t kModCtrl = static_cast<uint32_t>(::Input::ModifierMask{::Input::HL_MODIFIER_CTRL});
inline constexpr uint32_t kModAlt  = static_cast<uint32_t>(::Input::ModifierMask{::Input::HL_MODIFIER_ALT});
inline constexpr uint32_t kModMeta = static_cast<uint32_t>(::Input::ModifierMask{::Input::HL_MODIFIER_META});
#else
inline constexpr uint32_t kModCtrl = HL_MODIFIER_CTRL;
inline constexpr uint32_t kModAlt  = HL_MODIFIER_ALT;
inline constexpr uint32_t kModMeta = HL_MODIFIER_META;
#endif

// The modifiers held on every keyboard, as a wl_keyboard mask.
inline uint32_t modifiers() {
    return g_pInputManager ? static_cast<uint32_t>(g_pInputManager->getModsFromAllKBs()) : 0;
}

// The binds on keys still held shadowed, as Hyprland does after a bind fires.
inline void shadowBinds() {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    if (Keybinds::mgr())
        Keybinds::mgr()->shadowBinds();
#else
    if (g_pKeybindManager)
        g_pKeybindManager->shadowKeybinds();
#endif
}

// --- windows ----------------------------------------------------------------

inline pid_t pid(const PHLWINDOW& window) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return window->backend().pid();
#else
    return window->getPID();
#endif
}

inline bool isX11(const PHLWINDOW& window) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return window->backend().isX11();
#else
    return window->m_isX11;
#endif
}

// What an X11 window's surface is scaled by (xwayland:force_zero_scaling).
inline double x11Scale(const PHLWINDOW& window) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return window->backend().surfaceScale();
#else
    return window->m_X11SurfaceScaledBy;
#endif
}

inline bool isFloating(const PHLWINDOW& window) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return window->isFloating();
#else
    return window->m_isFloating;
#endif
}

inline Vector2D floatingOffset(const PHLWINDOW& window) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return window->presentation().floatingOffset();
#else
    return window->m_floatingOffset;
#endif
}

// The window's xdg geometry, empty for an X11 window or one with none set.
inline CBox xdgGeometry(const PHLWINDOW& window) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    if (window->backend().isX11())
        return {};
    return window->backend().geometry().box;
#else
    const auto xdg = window->m_xdgSurface.lock();
    return xdg ? xdg->m_current.geometry : CBox{};
#endif
}

// The window's own fade, the one map and unmap animate.
inline PHLANIMVAR<float>& fadeAlpha(const PHLWINDOW& window) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return window->presentation().alpha(Desktop::View::WINDOW_ALPHA_FADE);
#else
    return window->alpha(Desktop::View::WINDOW_ALPHA_FADE);
#endif
}

// Every alpha the window is drawn with, multiplied.
inline float effectiveAlpha(const PHLWINDOW& window) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return window->presentation().alphaTotal();
#else
    return window->effectiveAlpha();
#endif
}

inline bool opaque(const PHLWINDOW& window) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return window->presentation().opaque();
#else
    return window->opaque();
#endif
}

inline bool mapped(const PHLWINDOW& window) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return window->mapped();
#else
    return window->m_isMapped;
#endif
}

// Mapped, not hidden, with a surface, and not faded all the way out.
inline bool visible(const PHLWINDOW& window) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return !window->isHidden() && window->mapped() && window->wlSurface() && window->wlSurface()->resource() && window->presentation().alphaTotal() != 0.F;
#else
    return window->visible();
#endif
}

// --- popups -----------------------------------------------------------------

// The root of a window's or a layer's popup tree, if it has any.
template <typename T>
const SP<Desktop::View::CPopup>& popupHead(const SP<T>& view) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return view->popupHead();
#else
    return view->m_popupHead;
#endif
}

// Mapped and showing something, as the renderer would draw it.
inline bool shown(const WP<Desktop::View::CPopup>& popup) {
    if (!popup)
        return false;
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return popup->mapped() && popup->resource() && popup->alphaNonZero() && !popup->inert();
#else
    return popup->aliveAndVisible();
#endif
}

// --- layers -----------------------------------------------------------------

// Mapped, with its surface: one that shows.
inline bool visible(const PHLLS& layer) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return layer->mapped();
#else
    return layer->visible();
#endif
}

inline bool mapped(const PHLLS& layer) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return layer->mapped();
#else
    return layer->m_mapped;
#endif
}

// Whether it asks for the keyboard (its keyboard interactivity).
inline bool takesKeyboard(const PHLLS& layer) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return layer->m_keyboardInteractivity != 0;
#else
    return layer->m_interactivity != 0;
#endif
}

// Raised over fullscreen windows by Hyprland.
inline bool aboveFullscreen(const PHLLS& layer) {
#if HYPR_WORLD_HYPRLAND_AT_LEAST(0, 57)
    return static_cast<bool>(layer->m_flags & Desktop::View::LAYER_FLAG_ABOVE_FULLSCREEN);
#else
    return layer->m_aboveFullscreen;
#endif
}

// --- workspaces -------------------------------------------------------------
//
// The app knows workspaces by number: a numbered one by its own, a named
// one by one below 0. 0.56 gives named workspaces such ids itself (from
// -1337 down); 0.57 gives them none, so they are numbered here the same way,
// in the order they are first seen, for as long as the plugin is loaded.

bool isSpecial(const PHLWORKSPACE& workspace);

// The workspace's id for the app, 0 for a special workspace.
int64_t id(const PHLWORKSPACE& workspace);

// The name it shows with.
const std::string& name(const PHLWORKSPACE& workspace);

// The workspace the app knows as `id`, if it exists.
PHLWORKSPACE find(int64_t id);

// Numbered workspace `id`, made on `monitor`. Never a named one.
PHLWORKSPACE createNumbered(int64_t id, const PHLMONITOR& monitor);

// special:<name>, made on `monitor` if it does not exist yet (with no
// monitor, only looked up).
PHLWORKSPACE special(std::string_view name, const PHLMONITOR& monitor);

// What the workspace dispatcher takes for the workspace the app knows as
// `id`, if there is one.
std::optional<std::string> selector(int64_t id);

} // namespace World::Compat
