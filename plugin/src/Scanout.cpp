#include "Scanout.hpp"

#include "App.hpp"
#include "Util.hpp"

#include <hyprland/src/desktop/Workspace.hpp>
#include <hyprland/src/desktop/state/FadingOutState.hpp>
#include <hyprland/src/desktop/state/FocusState.hpp>
#include <hyprland/src/desktop/state/WindowState.hpp>
#include <hyprland/src/desktop/view/LayerSurface.hpp>
#include <hyprland/src/desktop/view/Window.hpp>
#include <hyprland/src/errorOverlay/Overlay.hpp>
#include <hyprland/src/managers/SessionLockManager.hpp>
#include <hyprland/src/managers/fullscreen/FullscreenController.hpp>
#include <hyprland/src/notification/NotificationOverlay.hpp>
#include <hyprland/src/output/Monitor.hpp>
#include <hyprland/src/plugins/HookSystem.hpp>
#include <hyprland/src/protocols/LayerShell.hpp>
#include <hyprland/src/protocols/core/DataDevice.hpp>

#include <format>

namespace World::Scanout {

namespace {

using RecheckFn = void (*)(Monitor::CMonitor* self);

CFunctionHook* g_hook     = nullptr;
RecheckFn      g_original = nullptr;

// Whether the app alone covers `monitor`: what isSolitaryBlocked asks of a
// regular fullscreen window, asked of the app on its special workspace.
bool appAlone(Monitor::CMonitor* monitor) {
    const auto app = App::window();
    if (!app)
        return false;

    // In view on this monitor, fully: not flying in or out, the special
    // workspace not sliding or fading.
    const auto workspace = app->m_workspace;
    if (!workspace || monitor->m_activeSpecialWorkspace != workspace || workspace->m_alpha->value() != 1.F ||
        workspace->m_renderOffset->value() != Vector2D{})
        return false;
    if (Fullscreen::controller()->getFullscreenModes(app).internal != Fullscreen::FSMODE_FULLSCREEN)
        return false;
    if (app->effectiveAlpha() != 1.F || !app->opaque())
        return false;
    if (app->size(Desktop::View::IGeometric::GEOMETRIC_CURRENT) != monitor->m_size ||
        app->position(Desktop::View::IGeometric::GEOMETRIC_CURRENT) != monitor->m_position || app->positionAnimation()->isBeingAnimated() ||
        app->sizeAnimation()->isBeingAnimated())
        return false;
    // One surface, as the display takes one buffer.
    if (!app->getSolitaryResource())
        return false;

    // Nothing drawn over it.
    if (Notification::overlay()->hasAny() || (ErrorOverlay::overlay()->active() && Desktop::focusState()->monitor().get() == monitor) ||
        g_pSessionLockManager->isSessionLocked() || PROTO::data->dndActive())
        return false;
    if (!monitor->m_layerSurfaceLayers[ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY].empty())
        return false;
    for (const auto& top : monitor->m_layerSurfaceLayers[ZWLR_LAYER_SHELL_V1_LAYER_TOP]) {
        // The bar fades out under the app (App.cpp `crossBars`).
        if (top && top->alpha()[Desktop::View::LS_ALPHA_FADE]->value() != 0.F)
            return false;
    }
    for (const auto& fadeout : Desktop::fadingOutState()->fadeouts()) {
        if (fadeout && fadeout->monitor().get() == monitor)
            return false;
    }
    for (const auto& window : Desktop::windowState()->windows()) {
        if (window != app && window->m_isMapped && window->visible() && window->m_workspace == workspace)
            return false;
    }
    return true;
}

void hkRecheckSolitary(Monitor::CMonitor* self) {
    if (g_original)
        g_original(self);
    try {
        // Hyprland found a regular fullscreen window of its own.
        if (!self->m_solitaryClient.expired())
            return;
        if (appAlone(self))
            self->m_solitaryClient = App::window();
    } catch (...) {}
}

} // namespace

void init(HANDLE handle) {
    try {
        void* target = findFunction(handle, "recheckSolitary", "CMonitor::recheckSolitary(");
        if (!target) {
            Log::logger->log(Log::WARN, "[hypr-world] CMonitor::recheckSolitary not found: the app stays composited");
            return;
        }
        g_hook = HyprlandAPI::createFunctionHook(handle, target, reinterpret_cast<void*>(&hkRecheckSolitary));
        if (!g_hook || !g_hook->hook() || !g_hook->m_original) {
            // Not x86_64, or the function did not take the hook.
            Log::logger->log(Log::WARN, "[hypr-world] cannot hook CMonitor::recheckSolitary: the app stays composited");
            if (g_hook)
                HyprlandAPI::removeFunctionHook(handle, g_hook);
            g_hook = nullptr;
            return;
        }
        g_original = reinterpret_cast<RecheckFn>(g_hook->m_original);
    } catch (...) {
        g_hook     = nullptr;
        g_original = nullptr;
    }
}

void shutdown() {
    if (g_hook)
        g_hook->unhook();
    g_hook     = nullptr;
    g_original = nullptr;
}

std::string describe() {
    const auto app     = App::window();
    const auto monitor = App::monitor();
    const bool chosen  = app && monitor && monitor->m_solitaryClient.lock() == app;
    return std::format("scanout hook {}, app {}solitary", g_original ? "in" : "out", chosen ? "" : "not ");
}

} // namespace World::Scanout
