#pragma once

// Small pieces the plugin's parts share: its notifications, its exception
// guard, work and timers on Hyprland's event loop, finding a function to
// hook, and rounding to whole px.

#include <hyprland/src/debug/log/Logger.hpp>
#include <hyprland/src/managers/eventLoop/EventLoopManager.hpp>
#include <hyprland/src/managers/eventLoop/EventLoopTimer.hpp>
#include <hyprland/src/plugins/PluginAPI.hpp>

#include <cmath>
#include <cstdint>
#include <exception>
#include <functional>
#include <string>
#include <utility>

namespace World {

// The first launch, and the problems: nothing else is announced.
inline void notify(HANDLE handle, const std::string& text, float timeMs, bool problem = false) {
    if (handle)
        HyprlandAPI::addNotification(handle, "[hypr-world] " + text, problem ? CHyprColor{1.0F, 0.45F, 0.3F, 1.0F} : CHyprColor{0.4F, 0.65F, 0.95F, 1.0F},
                                     timeMs);
}

// `body`, with anything it throws logged under `what` and stopped there.
template <typename F>
void guarded(const char* what, F&& body) {
    try {
        body();
    } catch (const std::exception& e) {
        Log::logger->log(Log::ERR, "[hypr-world] {}: {}", what, e.what());
    } catch (...) { Log::logger->log(Log::ERR, "[hypr-world] {}: unknown exception", what); }
}

// `work` from the event loop, once however often it is asked for before it
// runs. `slot` holds it while it waits.
inline void later(uint64_t& slot, std::function<void()> work) {
    if (!g_pEventLoopManager || slot)
        return;
    slot = g_pEventLoopManager->doLater([&slot, work = std::move(work)] {
        slot = 0;
        work();
    });
}

// The work waiting in `slot`, if any, never run.
inline void cancelLater(uint64_t& slot) {
    if (slot && g_pEventLoopManager)
        g_pEventLoopManager->removeDoLater(slot);
    slot = 0;
}

// A timer stopped and off the event loop.
inline void removeTimer(SP<CEventLoopTimer>& timer) {
    if (!timer)
        return;
    timer->cancel();
    if (g_pEventLoopManager)
        g_pEventLoopManager->removeTimer(timer);
    timer.reset();
}

// The one function whose demangled name contains `name`.
inline void* findFunction(HANDLE handle, const std::string& query, const std::string& name) {
    for (const auto& match : HyprlandAPI::findFunctionsByName(handle, query)) {
        if (match.address && match.demangled.find(name) != std::string::npos)
            return match.address;
    }
    return nullptr;
}

inline long px(double value) {
    return std::lround(value);
}

} // namespace World
