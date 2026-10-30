#include "Input.hpp"

#include "Deliver.hpp"
#include "Server.hpp"
#include "Util.hpp"

#include <hyprland/src/desktop/Workspace.hpp>
#include <hyprland/src/desktop/view/LayerSurface.hpp>
#include <hyprland/src/desktop/view/WLSurface.hpp>
#include <hyprland/src/desktop/view/Window.hpp>
#include <hyprland/src/helpers/time/Time.hpp>
#include <hyprland/src/managers/SeatManager.hpp>
#include <hyprland/src/protocols/core/Compositor.hpp>
#include <hyprland/src/devices/IKeyboard.hpp>
#include <hyprland/src/devices/IPointer.hpp>
#include <hyprland/src/event/EventBus.hpp>
#include <hyprland/src/managers/KeybindManager.hpp>
#include <hyprland/src/managers/SessionLockManager.hpp>
#include <hyprland/src/managers/input/InputManager.hpp>
#include <hyprland/src/plugins/HookSystem.hpp>
#include <hyprland/src/pointer/PointerManager.hpp>
#include <hyprland/src/render/Renderer.hpp>

#include <linux/input-event-codes.h>

#include <chrono>
#include <format>
#include <optional>
#include <stdexcept>
#include <string>
#include <unordered_map>
#include <unordered_set>
#include <vector>

namespace World::Input {

namespace {

using Clock = std::chrono::steady_clock;

HANDLE g_handle = nullptr;

// The app pings at least this often while it holds input, so silence this
// long means it hung.
constexpr auto kWatchdogTimeout = std::chrono::seconds(2);
constexpr auto kWatchdogTick    = std::chrono::milliseconds(500);

EState            g_state = EState::Off;
Clock::time_point g_lastApp;

bool intercepting() {
    return g_state != EState::Off;
}

// --- function hooks -------------------------------------------------------

// CInputManager::onMouseMoved carries the relative motion, accelerated and
// not, before anything reaches a client or moves the cursor. Not
// CPointerManager::move, which by then has already sent relative motion to
// the client under the cursor and has no unaccelerated delta.
// SMotionEvent holds a smart pointer, so both sides pass it by invisible
// reference; declaring it by value on both keeps them matched.
using MotionFn  = void (*)(void* self, IPointer::SMotionEvent event);
using SetSurfFn = void (*)(void* self, SP<Desktop::View::CWLSurface> surface, const Vector2D& hotspot);
using SetBufFn  = void (*)(void* self, SP<Aquamarine::IBuffer> buffer, const Vector2D& hotspot, const float& scale);

CFunctionHook* g_motionHook     = nullptr;
MotionFn       g_motionOriginal = nullptr;
CFunctionHook* g_surfHook       = nullptr;
SetSurfFn      g_surfOriginal   = nullptr;
CFunctionHook* g_bufHook        = nullptr;
SetBufFn       g_bufOriginal    = nullptr;

// Every cursor image change funnels through the pointer manager's two
// setters, whoever asks: the renderer, the cursor-shape protocol, a client.
// Gating them is what keeps the cursor hidden; IHyprRenderer's own hidden
// flag is recomputed every 500 ms.
bool g_cursorHidden = false;

void hkOnMouseMoved(void* self, IPointer::SMotionEvent event) {
    if (intercepting()) {
        try {
            Server::send(std::format("motion {} {} {} {}", event.delta.x, event.delta.y, event.unaccel.x, event.unaccel.y));
        } catch (...) {}
        return; // swallowed: the cursor stays put, nothing reaches a client
    }

    if (g_motionOriginal)
        g_motionOriginal(self, event);
}

void hkSetCursorSurface(void* self, SP<Desktop::View::CWLSurface> surface, const Vector2D& hotspot) {
    if (g_cursorHidden)
        return;
    if (g_surfOriginal)
        g_surfOriginal(self, surface, hotspot);
}

void hkSetCursorBuffer(void* self, SP<Aquamarine::IBuffer> buffer, const Vector2D& hotspot, const float& scale) {
    if (g_cursorHidden)
        return;
    if (g_bufOriginal)
        g_bufOriginal(self, buffer, hotspot, scale);
}

template <typename Fn>
CFunctionHook* hook(HANDLE handle, const std::string& query, const std::string& name, void* detour, Fn* original) {
    void* target = findFunction(handle, query, name);
    if (!target)
        throw std::runtime_error("[hypr-world] " + name + " not found");

    CFunctionHook* installed = HyprlandAPI::createFunctionHook(handle, target, detour);
    if (!installed || !installed->hook() || !installed->m_original)
        throw std::runtime_error("[hypr-world] cannot hook " + name + " (does another plugin hook it?)");

    *original = reinterpret_cast<Fn>(installed->m_original);
    return installed;
}

void unhookAll() {
    for (CFunctionHook** h : {&g_motionHook, &g_surfHook, &g_bufHook}) {
        if (*h)
            (*h)->unhook();
        *h = nullptr;
    }
    g_motionOriginal = nullptr;
    g_surfOriginal   = nullptr;
    g_bufOriginal    = nullptr;
}

// --- cursor and pointer focus ---------------------------------------------

void hideCursor() {
    g_cursorHidden = true;
    Pointer::mgr()->resetCursorImage();
}

void showCursor() {
    g_cursorHidden = false;
    if (g_pHyprRenderer)
        g_pHyprRenderer->setCursorFromName("default", true);
}

// After Off, Hyprland's pointer focus is whatever it was when the app took
// over, under a cursor that has not moved since. simulateMouseMovement puts
// it back in line with the cursor, and the client under it re-applies its
// own cursor image on enter. It re-enters the input listeners, so it never
// runs from inside one: always from the event loop.
uint64_t g_resync = 0;

void scheduleResync() {
    later(g_resync, [] {
        if (g_pInputManager && !intercepting())
            g_pInputManager->simulateMouseMovement();
    });
}

// --- listeners --------------------------------------------------------------

struct SListeners {
    CHyprSignalListener key, button, axis, move, lock;
};
SListeners g_listeners;

// Presses taken from Hyprland, by keycode or button, and whether the app was
// told about them. A release is taken exactly when its press was, whatever
// the state is by then: a release that reaches a client without its press,
// or a press without its release, leaves keys repeating or binds firing.
std::unordered_map<uint32_t, bool> g_takenKeys;
std::unordered_map<uint32_t, bool> g_takenButtons;

// Every key physically down, whoever got it. Super is read from here as
// well as from the modifier mask, so the escape chord and the Super
// passthrough do not depend on how the keymap names it.
std::unordered_set<uint32_t> g_heldKeys;

uint32_t modifiers() {
    return g_pInputManager ? g_pInputManager->getModsFromAllKBs() : 0;
}

bool isSuperKey(uint32_t keycode) {
    return keycode == KEY_LEFTMETA || keycode == KEY_RIGHTMETA;
}

bool superHeld(uint32_t mods) {
    return (mods & HL_MODIFIER_META) || g_heldKeys.contains(KEY_LEFTMETA) || g_heldKeys.contains(KEY_RIGHTMETA);
}

// Ctrl+Alt+F1..F12 always reach Hyprland, so a session can always be left.
bool isVtSwitch(uint32_t keycode, uint32_t mods) {
    const bool fkey = (keycode >= KEY_F1 && keycode <= KEY_F10) || keycode == KEY_F11 || keycode == KEY_F12;
    return fkey && (mods & HL_MODIFIER_CTRL) && (mods & HL_MODIFIER_ALT);
}

// --- aim and delivery -------------------------------------------------------

// Where the app's crosshair is, as it last said: a point on a window's
// captured image of `buffer` pixels. Unclamped during a drag.
struct SAim {
    uint64_t address = 0;
    double   u = 0, v = 0;
    Vector2D buffer;
};
std::optional<SAim> g_aim;

// The app's rocket launcher is out (`armed`): clicks fire it, and no
// window gets them, nor the pointer.
bool g_armed = false;

// A drag: the surface a press went to keeps every motion and button until
// the last delivered button is up, as an implicit grab does in 2D.
struct SGrab {
    uint64_t                 address = 0;
    PHLWINDOWREF             window;
    WP<CWLSurfaceResource>   surface;
    Vector2D                 offset;
    double                   scale = 1.0;
    std::unordered_set<uint32_t> buttons;
};
std::optional<SGrab> g_grab;

uint32_t nowMs() {
    return static_cast<uint32_t>(Time::millis(Time::steadyNow()));
}

// The surface under the crosshair, if it is on a window's image or on one
// of its popups.
struct SAimed {
    PHLWINDOW         window;
    Deliver::STarget  target;
    Vector2D          local; // in the target surface
};

std::optional<SAimed> aimed() {
    if (!g_aim)
        return std::nullopt;

    const auto window = Deliver::findWindow(g_aim->address);
    if (!window)
        return std::nullopt;

    // Past the window's image only a popup that reaches past its edges can
    // be under it: the app aims at those too when it shows the window's own
    // surfaces.
    const bool     inside      = g_aim->u >= 0 && g_aim->u <= 1 && g_aim->v >= 0 && g_aim->v <= 1;
    const Vector2D windowLocal = Deliver::windowLocal(window, g_aim->u, g_aim->v, g_aim->buffer);
    const auto     target      = inside ? Deliver::resolve(window, windowLocal) : Deliver::popupAt(window, windowLocal);
    if (!target)
        return std::nullopt;

    return SAimed{window, *target, target->local(windowLocal)};
}

// What clicks, scroll and hover go to: what is aimed at, unless the rocket
// launcher is out.
std::optional<SAimed> pointed() {
    return g_armed ? std::nullopt : aimed();
}

// The point in the grabbed surface the crosshair is over now, if the app is
// still aiming at the grabbed window.
std::optional<Vector2D> grabbedLocal() {
    if (!g_grab || !g_aim || g_aim->address != g_grab->address)
        return std::nullopt;

    const auto window = g_grab->window.lock();
    if (!window || !g_grab->surface.lock())
        return std::nullopt;

    return (Deliver::windowLocal(window, g_aim->u, g_aim->v, g_aim->buffer) - g_grab->offset) * g_grab->scale;
}

bool grabbedHasFocus() {
    const auto surface = g_grab ? g_grab->surface.lock() : nullptr;
    return surface && g_pSeatManager && g_pSeatManager->m_state.pointerFocus.lock() == surface;
}

// The seat's pointer on `surface` at `local`. setPointerFocus does nothing
// for the surface that already has it, so the motion always follows.
void pointAt(const SP<CWLSurfaceResource>& surface, const Vector2D& local, uint32_t timeMs) {
    if (!g_pSeatManager || !surface)
        return;
    g_pSeatManager->setPointerFocus(surface, local);
    g_pSeatManager->sendPointerMotion(timeMs, local);
    g_pSeatManager->sendPointerFrame();
}

void pointAtNothing() {
    if (g_pSeatManager)
        g_pSeatManager->setPointerFocus(nullptr, {});
}

// Hover: every aim update moves the pointer there, so the window highlights
// what is under the crosshair as it would under the cursor.
void deliverAim() {
    if (!intercepting())
        return;

    if (g_grab) {
        if (const auto local = grabbedLocal(); local && grabbedHasFocus()) {
            g_pSeatManager->sendPointerMotion(nowMs(), *local);
            g_pSeatManager->sendPointerFrame();
        }
        return;
    }

    if (const auto hit = pointed())
        pointAt(hit->target.surface, hit->local, nowMs());
    else
        pointAtNothing();
}

void endGrab() {
    if (!g_grab)
        return;
    g_grab.reset();
    Server::send("ungrab");
}

// --- typing -----------------------------------------------------------------

// The window being typed into, and the surface that had the keyboard before
// (normally the app's own window), which gets it back afterwards. Focus
// moves at the seat only: Hyprland's own window focus switches the monitor
// to the window's workspace, which would take the app's window out of view.
PHLWINDOWREF           g_typing;
WP<CWLSurfaceResource> g_keyboardBefore;
// Its workspace, for the window to type into next when it closes.
PHLWORKSPACEREF g_typingWorkspace;
// The windows typed into, the latest last. Typing moves the seat's keyboard
// only, so Hyprland's own focus history never sees them.
std::vector<PHLWINDOWREF> g_typedInto;

SP<CWLSurfaceResource> typingSurface() {
    const auto window = g_typing.lock();
    if (!window || !Desktop::View::validMapped(window) || !window->wlSurface())
        return nullptr;
    return window->wlSurface()->resource();
}

// The keyboard back where it was before typing started.
void restoreKeyboard() {
    if (g_pSeatManager)
        g_pSeatManager->setKeyboardFocus(g_keyboardBefore.lock());
    g_typing.reset();
    g_keyboardBefore.reset();
    g_typingWorkspace.reset();
}

// The window to type into once the one being typed into closed, as
// Hyprland would focus one in 2D: the one on its workspace typed into last,
// else that workspace's focus candidate.
PHLWINDOW nextToType() {
    const auto workspace = g_typingWorkspace.lock();
    if (!workspace)
        return nullptr;
    const auto onIt = [&](const PHLWINDOW& window) { return window && Desktop::View::validMapped(window) && window->m_workspace == workspace; };

    std::erase_if(g_typedInto, [](const PHLWINDOWREF& ref) { return !ref.lock(); });
    for (auto it = g_typedInto.rbegin(); it != g_typedInto.rend(); ++it) {
        if (const auto window = it->lock(); onIt(window))
            return window;
    }
    const auto window = workspace->getFocusCandidate();
    return onIt(window) ? window : nullptr;
}

// A chord the plugin consumed never reaches Hyprland's keybind manager, so
// to it Super was pressed and released alone, and a bind on Super by itself
// (a launcher) would fire on the release. Shadowing the binds on keys still
// held is what Hyprland itself does after a bind fires.
void consumeChord() {
    if (g_pKeybindManager)
        g_pKeybindManager->shadowKeybinds();
}

// A layer surface that takes the keyboard, a launcher like rofi, has the
// seat's keyboard focus: what the user types is meant for it.
bool keyboardOnLayer() {
    if (!g_pSeatManager)
        return false;
    const auto focus = g_pSeatManager->m_state.keyboardFocus.lock();
    if (!focus)
        return false;
    const auto surface = Desktop::View::CWLSurface::fromResource(focus);
    const auto layer   = surface ? Desktop::View::CLayerSurface::fromView(surface->view()) : nullptr;
    return layer && layer->m_mapped && layer->m_interactivity != 0;
}

void onKey(IKeyboard::SKeyEvent event, Event::SCallbackInfo& info) {
    const bool pressed = event.state == WL_KEYBOARD_KEY_STATE_PRESSED;

    // Tracked before anything else, so it is right whatever happens below.
    // Super's own state is read before this key changes it, like the mask.
    const bool superWasHeld = superHeld(modifiers());
    if (pressed)
        g_heldKeys.insert(event.keycode);
    else
        g_heldKeys.erase(event.keycode);

    if (!pressed) {
        const auto taken = g_takenKeys.find(event.keycode);
        if (taken == g_takenKeys.end())
            return;

        const bool told = taken->second;
        g_takenKeys.erase(taken);
        info.cancelled = true;

        if (told)
            Server::send(std::format("key {} 0 {}", event.keycode, modifiers()));
        return;
    }

    if (!intercepting())
        return;

    const uint32_t mods = modifiers();

    // The escape hatch, checked before anything else.
    if (event.keycode == KEY_ESC && superWasHeld) {
        g_takenKeys[event.keycode] = false;
        info.cancelled             = true;
        consumeChord();
        leave("escape");
        return;
    }

    if (isVtSwitch(event.keycode, mods))
        return;

    // Super+Left Alt: between looking and typing.
    if (event.keycode == KEY_LEFTALT && superWasHeld) {
        g_takenKeys[event.keycode] = false;
        info.cancelled             = true;
        consumeChord();
        if (g_state == EState::Type)
            leaveType("chord");
        else
            enterType("chord");
        return;
    }

    // Super and anything pressed with it stay Hyprland's: every bind keeps
    // working while the app has input (Super+Q closes the app, Super+L
    // toggles it). The app moves with plain keys only.
    if (isSuperKey(event.keycode) || superWasHeld)
        return;

    // While a launcher opened over the app has the keyboard, plain keys go
    // to it, Escape included, and the mouse stays the app's. Once it closes
    // Hyprland moves the keyboard back, and the app has it again. Before
    // Type, which would take the keyboard back to its window.
    if (keyboardOnLayer())
        return;

    if (g_state == EState::Type) {
        // To the window, through Hyprland as any key. Hyprland can move the
        // keyboard in the meantime (a layer surface taking it, a refocus), so
        // it is put back first; with the window gone, typing stops and the
        // key goes to the app instead.
        if (const auto surface = typingSurface()) {
            if (g_pSeatManager && g_pSeatManager->m_state.keyboardFocus.lock() != surface)
                g_pSeatManager->setKeyboardFocus(surface);
            return;
        }
        // Closed (Ctrl+D in a terminal): typing goes on in the window its
        // workspace focuses next, and this key with it, as in 2D.
        if (const auto next = nextToType()) {
            typeInto(next, "window gone");
            return;
        }
        leaveType("window gone");
    }

    g_takenKeys[event.keycode] = true;
    info.cancelled             = true;
    Server::send(std::format("key {} 1 {}", event.keycode, mods));
}

// Buttons go to the window under the crosshair; with nothing there, to the
// app only. g_takenButtons records which, so the release goes the same way.
void onButton(IPointer::SButtonEvent event, Event::SCallbackInfo& info) {
    const bool pressed = event.state == WL_POINTER_BUTTON_STATE_PRESSED;

    if (!pressed) {
        const auto taken = g_takenButtons.find(event.button);
        if (taken == g_takenButtons.end())
            return;

        const bool delivered = taken->second;
        g_takenButtons.erase(taken);
        info.cancelled = true;

        if (delivered && g_grab && g_grab->buttons.erase(event.button)) {
            // Only to the surface that got the press, if it still has the
            // pointer; a leave in between already released it there.
            if (grabbedHasFocus()) {
                g_pSeatManager->sendPointerButton(event.timeMs, event.button, WL_POINTER_BUTTON_STATE_RELEASED);
                g_pSeatManager->sendPointerFrame();
            }
            if (g_grab->buttons.empty()) {
                endGrab();
                deliverAim(); // hovering again, wherever the crosshair is now
            }
        }

        Server::send(std::format("button {} 0 {}", event.button, delivered ? 1 : 0));
        return;
    }

    if (!intercepting())
        return;

    bool delivered = false;

    if (g_grab) {
        // Another button during a drag: the same surface.
        if (grabbedHasFocus()) {
            g_pSeatManager->sendPointerButton(event.timeMs, event.button, WL_POINTER_BUTTON_STATE_PRESSED);
            g_pSeatManager->sendPointerFrame();
            g_grab->buttons.insert(event.button);
            delivered = true;
        }
    } else if (const auto hit = pointed()) {
        pointAt(hit->target.surface, hit->local, event.timeMs);
        g_pSeatManager->sendPointerButton(event.timeMs, event.button, WL_POINTER_BUTTON_STATE_PRESSED);
        g_pSeatManager->sendPointerFrame();

        g_grab = SGrab{
            .address = g_aim->address,
            .window  = hit->window,
            .surface = hit->target.surface,
            .offset  = hit->target.offset,
            .scale   = hit->target.scale,
            .buttons = {event.button},
        };
        Server::send(std::format("grab 0x{:x}", g_aim->address));
        delivered = true;

        // A left click on a window types into it as well, until
        // Super+Left Alt, as a click focuses a window in 2D.
        if (event.button == BTN_LEFT && g_typing.lock() != hit->window)
            typeInto(hit->window, "click");
    }

    g_takenButtons[event.button] = delivered;
    info.cancelled               = true;
    Server::send(std::format("button {} 1 {}", event.button, delivered ? 1 : 0));
}

// Scroll goes where a click would; with nothing there, to the app.
void onAxis(IPointer::SAxisEvent event, Event::SCallbackInfo& info) {
    if (!intercepting())
        return;

    info.cancelled = true;

    bool delivered = false;

    if (g_grab) {
        delivered = grabbedHasFocus();
    } else if (const auto hit = pointed()) {
        pointAt(hit->target.surface, hit->local, event.timeMs);
        delivered = true;
    }

    if (!delivered) {
        Server::send(std::format("axis {} {} {}", event.axis == WL_POINTER_AXIS_VERTICAL_SCROLL ? 'v' : 'h', event.delta, event.deltaDiscrete));
        return;
    }

    // As Hyprland's own wheel path, without the per-window scroll factor:
    // one discrete step per notch, the v120 value as the device gave it.
    const int32_t discrete = event.deltaDiscrete == 0 ? 0 : (event.deltaDiscrete > 0 ? 1 : -1);
    g_pSeatManager->sendPointerAxis(event.timeMs, event.axis, event.delta, discrete, event.deltaDiscrete, event.source, event.relativeDirection);
    g_pSeatManager->sendPointerFrame();
}

// Absolute cursor moves also come from refocusing after a window closes or a
// workspace changes; canceled, they cannot move focus under the frozen
// cursor while the app has it.
void onMove(Vector2D, Event::SCallbackInfo& info) {
    if (intercepting())
        info.cancelled = true;
}

// --- watchdog ---------------------------------------------------------------

SP<CEventLoopTimer> g_watchdog;

void onWatchdog(SP<CEventLoopTimer> self, void*) {
    guarded("watchdog", [] {
        if (intercepting() && Clock::now() - g_lastApp > kWatchdogTimeout)
            leave("watchdog");
    });
    self->updateTimeout(kWatchdogTick);
}

} // namespace

void init(HANDLE handle) {
    g_handle = handle;

    g_motionHook = hook(handle, "onMouseMoved", "CInputManager::onMouseMoved(", reinterpret_cast<void*>(&hkOnMouseMoved), &g_motionOriginal);
    g_surfHook   = hook(handle, "setCursorSurface", "CPointerManager::setCursorSurface(", reinterpret_cast<void*>(&hkSetCursorSurface), &g_surfOriginal);
    g_bufHook    = hook(handle, "setCursorBuffer", "CPointerManager::setCursorBuffer(", reinterpret_cast<void*>(&hkSetCursorBuffer), &g_bufOriginal);

    g_watchdog = makeShared<CEventLoopTimer>(kWatchdogTick, onWatchdog, nullptr);
    g_pEventLoopManager->addTimer(g_watchdog);
}

void startListeners() {
    auto& input = Event::bus()->m_events.input;

    g_listeners.key = input.keyboard.key.listen([](IKeyboard::SKeyEvent event, Event::SCallbackInfo& info) { guarded("key", [&] { onKey(event, info); }); });
    g_listeners.button =
        input.mouse.button.listen([](IPointer::SButtonEvent event, Event::SCallbackInfo& info) { guarded("button", [&] { onButton(event, info); }); });
    g_listeners.axis = input.mouse.axis.listen([](IPointer::SAxisEvent event, Event::SCallbackInfo& info) { guarded("axis", [&] { onAxis(event, info); }); });
    g_listeners.move = input.mouse.move.listen([](Vector2D position, Event::SCallbackInfo& info) { guarded("move", [&] { onMove(position, info); }); });
    g_listeners.lock = g_pSessionLockManager->m_events.lock.listen([] { guarded("lock", [] { leave("lock"); }); });
}

void shutdown() {
    const bool wasIntercepting = intercepting();
    const bool wasTyping       = g_state == EState::Type;

    g_state = EState::Off;
    if (wasTyping)
        restoreKeyboard();
    g_takenKeys.clear();
    g_takenButtons.clear();
    g_heldKeys.clear();
    g_typedInto.clear();
    g_aim.reset();
    g_grab.reset();
    g_listeners = {};

    if (wasIntercepting)
        pointAtNothing();

    cancelLater(g_resync);
    removeTimer(g_watchdog);

    if (wasIntercepting) {
        showCursor();
        // Synchronously: a doLater from here would run after the unload.
        if (g_pInputManager)
            g_pInputManager->simulateMouseMovement();
    }
    g_cursorHidden = false;

    unhookAll();
}

EState state() {
    return g_state;
}

void enterLook(const char* why) {
    if (!Server::connected() || intercepting())
        return;

    g_state   = EState::Look;
    g_lastApp = Clock::now();
    hideCursor();
    Server::send(std::format("state look {}", why));
    deliverAim();
}

void leave(const char* why) {
    if (g_state == EState::Off)
        return;

    const bool wasTyping = g_state == EState::Type;

    // Off first: what follows can re-enter the listeners.
    g_state = EState::Off;

    if (wasTyping)
        restoreKeyboard();

    // The pointer leaves the app's windows. A leave releases any button a
    // drag still holds, client-side; the physical release stays swallowed.
    endGrab();
    pointAtNothing();

    showCursor();
    scheduleResync();
    Server::send(std::format("state off {}", why));
}

void enterType(const char* why) {
    if (g_state != EState::Look)
        return;

    const auto hit = aimed();
    if (!hit)
        return;

    typeInto(hit->window, why);
}

void typeInto(const PHLWINDOW& window, const char* why) {
    if (!intercepting() || !window || !Desktop::View::validMapped(window))
        return;

    const auto surface = window->wlSurface() ? window->wlSurface()->resource() : nullptr;
    if (!surface || !g_pSeatManager)
        return;

    // From Type, the keyboard still goes back where it was before typing
    // started, not to the window typed into last.
    if (g_state == EState::Look)
        g_keyboardBefore = g_pSeatManager->m_state.keyboardFocus;
    std::erase_if(g_typedInto, [&](const PHLWINDOWREF& ref) { return !ref.lock() || ref.lock() == window; });
    g_typedInto.emplace_back(window);
    g_typing          = window;
    g_typingWorkspace = window->m_workspace;
    g_state           = EState::Type;

    // Keys the app held (a movement key) are released to it as they come
    // up: their presses were taken, so their releases are too.
    g_pSeatManager->setKeyboardFocus(surface);

    Server::send(std::format("state type 0x{:x} {}", reinterpret_cast<uint64_t>(window.get()), why));
}

void leaveType(const char* why) {
    if (g_state != EState::Type)
        return;

    g_state = EState::Look;
    restoreKeyboard();
    Server::send(std::format("state look {}", why));
}

void appSeen() {
    g_lastApp = Clock::now();
}

void aim(uint64_t address, double u, double v, const Vector2D& buffer) {
    g_aim = SAim{address, u, v, buffer};
    deliverAim();
}

void aimNone() {
    g_aim.reset();
    deliverAim();
}

void setArmed(bool armed) {
    g_armed = armed;
    if (!armed || !intercepting())
        return;
    // What a drag held goes, and what was hovered is left.
    endGrab();
    pointAtNothing();
}

void appConnected(bool connected) {
    // The app starts with its launcher put away.
    g_armed = false;
    if (!connected)
        leave("app");
}

} // namespace World::Input
