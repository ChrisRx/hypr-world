//! The app with a nested Hyprland as its backend (`hypr-world --backend`,
//! plugin/nested.sh backend): the plugin runs in the nested one, headless,
//! and feeds the app as usual, but the app's window is in the session. So the
//! app captures from the nested compositor, is always in view, and takes the
//! mouse itself, as standalone. While it holds the mouse, what the window
//! gets goes on into the nested Hyprland through a virtual keyboard and
//! pointer, as if typed and clicked there: the plugin takes it as any input,
//! for its Look and Type, its chords, and the clicks it delivers to the
//! window under the crosshair. The plugin's keys and buttons come back as its
//! lines do (`drain`); the mouse's motion stays the app's own.

use std::{
    collections::HashSet,
    ffi::{CStr, CString},
    io::Write,
    os::fd::{AsFd, OwnedFd},
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender, channel},
    thread,
    time::Instant,
};

use bevy::{
    input::{
        ButtonState,
        keyboard::KeyboardInput,
        mouse::{MouseButtonInput, MouseScrollUnit, MouseWheel},
    },
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow, WindowFocused},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_keyboard::{self, WlKeyboard},
        wl_pointer,
        wl_registry::WlRegistry,
        wl_seat::WlSeat,
    },
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use xkbcommon_dl::{
    XkbCommon, xkb_context_flags, xkb_key_direction, xkb_keymap_compile_flags, xkb_keymap_format,
    xkb_rule_names, xkb_state, xkb_state_component,
};

use super::Glue;
use crate::capture;

pub mod protocol {
    #![allow(non_upper_case_globals, non_camel_case_types, dead_code)]
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/virtual-keyboard-unstable-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/virtual-keyboard-unstable-v1.xml");
}

use protocol::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

/// The nested Hyprland the app runs against: its instance's folder, where
/// the plugin's socket is, and its Wayland socket.
pub struct Backend {
    pub instance: PathBuf,
    pub display: PathBuf,
}

impl Backend {
    /// The instance `signature`, else the one plugin/nested.sh started.
    pub fn find(signature: Option<&str>) -> Result<Self, String> {
        let runtime =
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").ok_or("no XDG_RUNTIME_DIR")?);
        let signature = match signature {
            Some(signature) => signature.to_string(),
            None => {
                let file = runtime.join("hypr-world-nested").join("instance");
                std::fs::read_to_string(&file)
                    .map_err(|e| {
                        format!(
                            "no nested Hyprland ({}: {e}); plugin/nested.sh backend starts one",
                            file.display()
                        )
                    })?
                    .trim()
                    .to_string()
            }
        };
        let instance = runtime.join("hypr").join(&signature);
        // Its pid, then its Wayland socket's name.
        let lock = instance.join("hyprland.lock");
        let socket = std::fs::read_to_string(&lock)
            .map_err(|e| format!("no Hyprland {signature} ({}: {e})", lock.display()))?
            .lines()
            .nth(1)
            .map(str::to_string)
            .ok_or_else(|| format!("{} names no Wayland socket", lock.display()))?;
        Ok(Self {
            instance,
            display: runtime.join(socket),
        })
    }
}

/// What goes into the nested Hyprland: evdev codes, as its devices have them.
enum Inject {
    Key(u32, bool),
    Button(u32, bool),
    /// Vertical or not, in Wayland's axis units, and in wheel steps.
    Axis(bool, f64, i32),
}

/// The thread with the virtual keyboard and pointer.
pub struct Injector(Sender<Inject>);

impl Injector {
    pub fn start(backend: &Backend) -> Self {
        let (tx, rx) = channel();
        let (instance, display) = (backend.instance.clone(), backend.display.clone());
        thread::spawn(move || {
            if let Err(e) = inject(&instance, &display, &rx) {
                eprintln!("hypr-world: no input into the backend: {e}");
            }
        });
        Self(tx)
    }

    fn send(&self, what: Inject) {
        let _ = self.0.send(what);
    }
}

#[derive(Default)]
struct State {
    keymap: Option<(OwnedFd, u32)>,
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { fd, size, .. } = event {
            state.keymap = Some((fd, size));
        }
    }
}

delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);
delegate_noop!(State: ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);

/// XKB_KEYMAP_FORMAT_TEXT_V1.
const KEYMAP_TEXT: u32 = 1;

/// The virtual keyboard's keymap's state, for the modifiers it holds: Hyprland
/// applies none of its `input:kb_*` settings to a virtual keyboard, which
/// brings its own keymap, so this one is compiled from them (`compiled`), as
/// Hyprland compiles its real keyboards'. Caps as Ctrl and the like then come
/// out as there. Else the seat's keymap, and the plain modifiers.
enum Keymap {
    Xkb {
        xkb: &'static XkbCommon,
        state: *mut xkb_state,
        mods: [u32; 4],
    },
    Plain(u32),
}

impl Keymap {
    /// The modifiers to tell (depressed, latched, locked, layout) after `key`
    /// went down or up, if they changed.
    fn key(&mut self, key: u32, pressed: bool) -> Option<[u32; 4]> {
        match self {
            Keymap::Xkb { xkb, state, mods } => {
                let direction = if pressed {
                    xkb_key_direction::XKB_KEY_DOWN
                } else {
                    xkb_key_direction::XKB_KEY_UP
                };
                // SAFETY: the state is this thread's own, alive until it ends.
                let now = unsafe {
                    // XKB's keycodes are evdev's plus 8.
                    (xkb.xkb_state_update_key)(*state, key + 8, direction);
                    let serialize = |part| (xkb.xkb_state_serialize_mods)(*state, part);
                    [
                        serialize(xkb_state_component::XKB_STATE_MODS_DEPRESSED),
                        serialize(xkb_state_component::XKB_STATE_MODS_LATCHED),
                        serialize(xkb_state_component::XKB_STATE_MODS_LOCKED),
                        (xkb.xkb_state_serialize_layout)(
                            *state,
                            xkb_state_component::XKB_STATE_LAYOUT_EFFECTIVE,
                        ),
                    ]
                };
                (now != std::mem::replace(mods, now)).then_some(now)
            }
            Keymap::Plain(held) => {
                let modifier = modifier(key);
                if modifier == 0 {
                    return None;
                }
                if pressed {
                    *held |= modifier;
                } else {
                    *held &= !modifier;
                }
                Some([*held, 0, 0, 0])
            }
        }
    }
}

/// The keymap the nested Hyprland would give a real keyboard, from its
/// `input:kb_*` settings, as text in a file to hand the virtual keyboard,
/// and its state. None without libxkbcommon, or with settings it cannot
/// compile.
fn compiled(instance: &Path) -> Option<(Keymap, OwnedFd, u32)> {
    let xkb = xkbcommon_dl::xkbcommon_option()?;
    let socket = instance.join(".socket.sock");
    // Unset, which Hyprland says as `[[EMPTY]]`: libxkbcommon's default for
    // that part.
    let setting = |name: &str| {
        let reply = super::standalone::ask(&socket, &format!("j/getoption input:{name}"))?;
        let value = reply.get("str")?.as_str()?;
        if value.is_empty() || value == "[[EMPTY]]" {
            return None;
        }
        CString::new(value).ok()
    };
    let parts = [
        "kb_rules",
        "kb_model",
        "kb_layout",
        "kb_variant",
        "kb_options",
    ]
    .map(setting);
    let ptr = |part: &Option<CString>| part.as_ref().map_or(std::ptr::null(), |p| p.as_ptr());
    let names = xkb_rule_names {
        rules: ptr(&parts[0]),
        model: ptr(&parts[1]),
        layout: ptr(&parts[2]),
        variant: ptr(&parts[3]),
        options: ptr(&parts[4]),
    };

    // SAFETY: libxkbcommon's own calls, each on what the one before made.
    let (text, state) = unsafe {
        let context = (xkb.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
        if context.is_null() {
            return None;
        }
        let keymap = (xkb.xkb_keymap_new_from_names)(
            context,
            &names,
            xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
        );
        (xkb.xkb_context_unref)(context);
        if keymap.is_null() {
            return None;
        }
        // Malloc'd, and left: once a run, and there is no free to hand.
        let text =
            (xkb.xkb_keymap_get_as_string)(keymap, xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1);
        // The state keeps its own reference to the keymap.
        let state = (xkb.xkb_state_new)(keymap);
        (xkb.xkb_keymap_unref)(keymap);
        if text.is_null() || state.is_null() {
            return None;
        }
        (CStr::from_ptr(text).to_bytes_with_nul().to_vec(), state)
    };

    let file =
        rustix::fs::memfd_create("hypr-world-keymap", rustix::fs::MemfdFlags::CLOEXEC).ok()?;
    std::fs::File::from(file.try_clone().ok()?)
        .write_all(&text)
        .ok()?;
    let keymap = Keymap::Xkb {
        xkb,
        state,
        mods: [0; 4],
    };
    Some((keymap, file, u32::try_from(text.len()).ok()?))
}

/// The XKB modifier a key holds down, as a mask of the real modifiers:
/// Shift, Control, Mod1 (Alt), Mod4 (Super), Mod5 (AltGr).
fn modifier(evdev: u32) -> u32 {
    match evdev {
        42 | 54 => 1,
        29 | 97 => 1 << 2,
        56 => 1 << 3,
        125 | 126 => 1 << 6,
        100 => 1 << 7,
        _ => 0,
    }
}

fn inject(instance: &Path, display: &Path, asked: &Receiver<Inject>) -> Result<(), String> {
    let conn = capture::connect(Some(display))?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("wayland registry: {e}"))?;
    let qh = queue.handle();
    let seat: WlSeat = globals
        .bind(&qh, 1..=7, ())
        .map_err(|e| format!("wl_seat: {e}"))?;
    let keyboards: ZwpVirtualKeyboardManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("zwp_virtual_keyboard_manager_v1: {e}"))?;
    let pointers: ZwlrVirtualPointerManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| format!("zwlr_virtual_pointer_manager_v1: {e}"))?;

    let mut state = State::default();
    let (mut keys, file, size) = match compiled(instance) {
        Some(compiled) => compiled,
        None => {
            // The seat's own keymap, as its keyboard has it now.
            eprintln!("hypr-world: no keymap of the backend's settings, taking its keyboard's");
            let keyboard = seat.get_keyboard(&qh, ());
            while state.keymap.is_none() {
                queue
                    .blocking_dispatch(&mut state)
                    .map_err(|e| format!("wayland: {e}"))?;
            }
            if keyboard.version() >= 3 {
                keyboard.release();
            }
            let (file, size) = state.keymap.take().expect("a keymap");
            (Keymap::Plain(0), file, size)
        }
    };

    let virtual_keyboard = keyboards.create_virtual_keyboard(&seat, &qh, ());
    virtual_keyboard.keymap(KEYMAP_TEXT, file.as_fd(), size);
    let pointer = pointers.create_virtual_pointer(Some(&seat), &qh, ());
    conn.flush().map_err(|e| format!("wayland: {e}"))?;

    let started = Instant::now();
    for what in asked {
        let time = started.elapsed().as_millis() as u32;
        match what {
            Inject::Key(key, pressed) => {
                virtual_keyboard.key(time, key, u32::from(pressed));
                if let Some([depressed, latched, locked, layout]) = keys.key(key, pressed) {
                    virtual_keyboard.modifiers(depressed, latched, locked, layout);
                }
            }
            Inject::Button(button, pressed) => {
                let state = if pressed {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                };
                pointer.button(time, button, state);
                pointer.frame();
            }
            Inject::Axis(vertical, value, steps) => {
                let axis = if vertical {
                    wl_pointer::Axis::VerticalScroll
                } else {
                    wl_pointer::Axis::HorizontalScroll
                };
                pointer.axis_source(wl_pointer::AxisSource::Wheel);
                if pointer.version() >= 2 && steps != 0 {
                    pointer.axis_discrete(time, axis, value, steps);
                } else {
                    pointer.axis(time, axis, value);
                }
                pointer.frame();
            }
        }
        // Nothing comes back that matters, but the socket is read so that
        // it never fills.
        if let Some(guard) = conn.prepare_read() {
            let _ = guard.read();
        }
        let _ = queue.dispatch_pending(&mut state);
        conn.flush().map_err(|e| format!("wayland: {e}"))?;
    }
    Ok(())
}

/// The evdev code of a key (linux/input-event-codes.h), as the nested
/// Hyprland's keyboard has it.
fn evdev(key: KeyCode) -> Option<u32> {
    use KeyCode::*;
    Some(match key {
        Escape => 1,
        Digit1 => 2,
        Digit2 => 3,
        Digit3 => 4,
        Digit4 => 5,
        Digit5 => 6,
        Digit6 => 7,
        Digit7 => 8,
        Digit8 => 9,
        Digit9 => 10,
        Digit0 => 11,
        Minus => 12,
        Equal => 13,
        Backspace => 14,
        Tab => 15,
        KeyQ => 16,
        KeyW => 17,
        KeyE => 18,
        KeyR => 19,
        KeyT => 20,
        KeyY => 21,
        KeyU => 22,
        KeyI => 23,
        KeyO => 24,
        KeyP => 25,
        BracketLeft => 26,
        BracketRight => 27,
        Enter => 28,
        ControlLeft => 29,
        KeyA => 30,
        KeyS => 31,
        KeyD => 32,
        KeyF => 33,
        KeyG => 34,
        KeyH => 35,
        KeyJ => 36,
        KeyK => 37,
        KeyL => 38,
        Semicolon => 39,
        Quote => 40,
        Backquote => 41,
        ShiftLeft => 42,
        Backslash => 43,
        KeyZ => 44,
        KeyX => 45,
        KeyC => 46,
        KeyV => 47,
        KeyB => 48,
        KeyN => 49,
        KeyM => 50,
        Comma => 51,
        Period => 52,
        Slash => 53,
        ShiftRight => 54,
        NumpadMultiply => 55,
        AltLeft => 56,
        Space => 57,
        CapsLock => 58,
        F1 => 59,
        F2 => 60,
        F3 => 61,
        F4 => 62,
        F5 => 63,
        F6 => 64,
        F7 => 65,
        F8 => 66,
        F9 => 67,
        F10 => 68,
        NumLock => 69,
        ScrollLock => 70,
        Numpad7 => 71,
        Numpad8 => 72,
        Numpad9 => 73,
        NumpadSubtract => 74,
        Numpad4 => 75,
        Numpad5 => 76,
        Numpad6 => 77,
        NumpadAdd => 78,
        Numpad1 => 79,
        Numpad2 => 80,
        Numpad3 => 81,
        Numpad0 => 82,
        NumpadDecimal => 83,
        IntlBackslash => 86,
        F11 => 87,
        F12 => 88,
        NumpadEnter => 96,
        ControlRight => 97,
        NumpadDivide => 98,
        PrintScreen => 99,
        AltRight => 100,
        Home => 102,
        ArrowUp => 103,
        PageUp => 104,
        ArrowLeft => 105,
        ArrowRight => 106,
        End => 107,
        ArrowDown => 108,
        PageDown => 109,
        Insert => 110,
        Delete => 111,
        Pause => 119,
        SuperLeft => 125,
        SuperRight => 126,
        ContextMenu => 127,
        _ => return None,
    })
}

/// A mouse button's evdev code (`BTN_LEFT` and on).
fn button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => 0x112,
        MouseButton::Back => 0x113,
        MouseButton::Forward => 0x114,
        MouseButton::Other(code) => u32::from(code),
    }
}

/// Wayland's axis units for one wheel step, as libinput gives them.
const STEP: f64 = 15.0;

/// What the window got, while the app holds the mouse, into the nested
/// Hyprland and out of Bevy's input, before Bevy reads it. The plugin's
/// keys and buttons, written last update by `drain`, are told apart by
/// their window: theirs is `Entity::PLACEHOLDER`, and Bevy has read them
/// already. Only a release whose press went in goes in, and when the app
/// lets go, whatever is still held is let go there too.
#[allow(clippy::too_many_arguments)]
pub(super) fn forward(
    glue: Res<Glue>,
    mut cursor: Single<&mut CursorOptions, With<PrimaryWindow>>,
    mut keys: ResMut<Messages<KeyboardInput>>,
    mut buttons: ResMut<Messages<MouseButtonInput>>,
    mut wheels: ResMut<Messages<MouseWheel>>,
    mut focus: MessageReader<WindowFocused>,
    mut held: Local<(HashSet<u32>, HashSet<u32>)>,
    mut holding: Local<bool>,
) {
    let Some(injector) = glue.inject.as_ref() else {
        return;
    };
    let (held_keys, held_buttons) = &mut *held;

    // Away from the window, the session takes the pointer back: so does
    // the nested Hyprland's input.
    if focus.read().any(|f| !f.focused) && cursor.grab_mode == CursorGrabMode::Locked {
        glue.send("release");
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }

    let locked = glue.connected && cursor.grab_mode == CursorGrabMode::Locked;
    if !locked {
        if std::mem::take(&mut *holding) {
            for key in held_keys.drain() {
                injector.send(Inject::Key(key, false));
            }
            for button in held_buttons.drain() {
                injector.send(Inject::Button(button, false));
            }
        }
        return;
    }
    // Just taken: what came before, the click that took it among it, is not
    // the nested Hyprland's.
    if !std::mem::replace(&mut *holding, true) {
        keys.drain().for_each(drop);
        buttons.drain().for_each(drop);
        wheels.drain().for_each(drop);
        return;
    }

    for key in keys.drain() {
        let Some(code) = evdev(key.key_code) else {
            continue;
        };
        if key.window == Entity::PLACEHOLDER || key.repeat {
            continue;
        }
        let pressed = key.state == ButtonState::Pressed;
        if pressed && held_keys.insert(code) || !pressed && held_keys.remove(&code) {
            injector.send(Inject::Key(code, pressed));
        }
    }
    for press in buttons.drain() {
        if press.window == Entity::PLACEHOLDER {
            continue;
        }
        let code = button_code(press.button);
        let pressed = press.state == ButtonState::Pressed;
        if pressed && held_buttons.insert(code) || !pressed && held_buttons.remove(&code) {
            injector.send(Inject::Button(code, pressed));
        }
    }
    for wheel in wheels.drain() {
        if wheel.window == Entity::PLACEHOLDER {
            continue;
        }
        // Bevy's up is Wayland's negative.
        let (x, y) = match wheel.unit {
            MouseScrollUnit::Line => (f64::from(wheel.x) * STEP, f64::from(wheel.y) * STEP),
            MouseScrollUnit::Pixel => (f64::from(wheel.x), f64::from(wheel.y)),
        };
        let steps = |v: f32| match wheel.unit {
            MouseScrollUnit::Line => -(v.round() as i32),
            MouseScrollUnit::Pixel => 0,
        };
        if y != 0.0 {
            injector.send(Inject::Axis(true, -y, steps(wheel.y)));
        }
        if x != 0.0 {
            injector.send(Inject::Axis(false, -x, steps(wheel.x)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_the_plugins() {
        // The app's own keys, as the plugin sends them back (glue.rs
        // `key_code`), go in as the same codes.
        for code in [1, 16, 17, 19, 29, 30, 31, 32, 42, 57] {
            let key = super::super::key_code(code).expect("an app key");
            assert_eq!(evdev(key), Some(code), "{key:?}");
        }
    }

    #[test]
    fn modifiers_are_xkbs() {
        assert_eq!(
            modifier(42) | modifier(29) | modifier(56) | modifier(125),
            1 | 4 | 8 | 64
        );
        assert_eq!(modifier(30), 0);
    }
}
