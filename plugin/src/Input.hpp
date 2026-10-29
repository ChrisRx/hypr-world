#pragma once

// Input interception while the app is open.
//
// Off passes everything through. Look takes every key, button, scroll and
// mouse motion away from Hyprland and hands it to the app, with the cursor
// frozen and hidden; clicks and scroll go to the window under the crosshair.
// Type is Look with the keyboard going to one window instead of the app.
// Super+Escape always goes back to Off, and so does losing the app.

#include <hyprland/src/plugins/PluginAPI.hpp>

#include <cstdint>

namespace World::Input {

enum class EState {
    Off,
    Look,
    // As Look for the mouse; the keyboard goes to the window that was under
    // the crosshair, which has the seat's keyboard focus.
    Type,
};

// The function hooks and the watchdog. Throws if a hook cannot be installed,
// which usually means another plugin holds it.
void init(HANDLE handle);

// The event bus listeners. Last in PLUGIN_INIT: a throw after listeners
// exist ejects the plugin without PLUGIN_EXIT.
void startListeners();

// Back to Off and everything released, synchronously (PLUGIN_EXIT).
void shutdown();

EState state();

// Enter Look, if the app is connected. `why` goes to the app.
void enterLook(const char* why);

// Back to Off, restoring the cursor and Hyprland's pointer focus.
void leave(const char* why);

// Look to Type, typing into the window under the crosshair (nothing happens
// with no window there), and Type back to Look. Super+Left Alt does both.
void enterType(const char* why);
void leaveType(const char* why);
// From Look or Type, typing into `window`, wherever the crosshair is: a
// window opened from the app (App.cpp `redirectNow`). Nothing happens
// while Off.
void typeInto(const PHLWINDOW& window, const char* why);

// Any message from the app: feeds the watchdog.
void appSeen();

// The app's crosshair is at (u, v) on the captured image (`buffer` pixels)
// of the window at `address`, or on no window. In Look, the pointer follows
// it there; buttons and scroll go where it is.
void aim(uint64_t address, double u, double v, const Vector2D& buffer);
void aimNone();

// armed 0|1: the app's rocket launcher is out. A click then fires it and
// reaches no window: buttons and scroll go to the app alone, any drag ends
// and nothing is hovered, until it is put away.
void setArmed(bool armed);

// The app connected or went away.
void appConnected(bool connected);

} // namespace World::Input
