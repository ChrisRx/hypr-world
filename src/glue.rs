//! The app's end of the hypr-world glue plugin: a socket into the
//! compositor that hands the app the keyboard and mouse while it is open.
//!
//! A background thread keeps the connection, retrying while the plugin is not
//! loaded, and turns lines into events. Bevy drains them once a frame and
//! writes its own lines through the shared stream.

use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    hash::Hash,
    io::{BufRead, BufReader, IoSliceMut, Write},
    mem::MaybeUninit,
    os::{
        fd::OwnedFd,
        unix::{fs::FileExt, net::UnixStream},
    },
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::{Duration, Instant},
};

use bevy::{
    ecs::system::SystemParam,
    input::{
        ButtonState,
        keyboard::{Key, KeyboardFocusLost, KeyboardInput, NativeKey},
        mouse::{MouseButtonInput, MouseMotion},
    },
    prelude::*,
    window::PrimaryWindow,
};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};

use crate::{
    WorldSystems,
    capture::{Damage, PixelLayout, ShmFrame},
    import,
    launcher::Launcher,
    stats::Stats,
    wake::Wake,
};

/// The plugin's feed, read in first each update, and the hint and the
/// crosshair that say what the keys do. The `Glue` itself comes from
/// main.rs, which starts its threads.
pub fn plugin(app: &mut App) {
    app.init_resource::<Desktop>()
        .init_resource::<WorldSettings>()
        .init_state::<InputMode>()
        .register_type_state::<InputMode>()
        .add_message::<Enter>()
        .add_message::<Leave>()
        .add_message::<Picture>()
        .add_message::<Shared>()
        .add_message::<Surfaces>()
        .add_message::<Unshared>()
        .add_message::<TakeScreenshot>()
        .add_message::<Quit>()
        .add_message::<DesktopChange>()
        .add_systems(Startup, (spawn_hint, spawn_crosshair))
        // Before Bevy reads the input, which the plugin's keys and mouse join.
        .add_systems(PreUpdate, drain.in_set(WorldSystems::Feed));
    for mode in [InputMode::Off, InputMode::Look, InputMode::Type] {
        app.add_systems(OnEnter(mode), show_hint);
    }
    for launcher in [Launcher::Stowed, Launcher::Armed] {
        app.add_systems(OnEnter(launcher), show_hint);
    }
}

/// The line protocol's version, said in each side's hello. The plugin and
/// the app are built from the same tree, so they must match exactly: bump it
/// with any change to the lines either side sends.
const VERSION: u32 = 2;

/// How often the app tells the plugin it is alive; the plugin gives input
/// back to Hyprland after 2 s of silence.
const PING_INTERVAL: Duration = Duration::from_millis(500);
/// A wake the app has left unanswered this long means it is stuck: the
/// pings stop, and the plugin's watchdog gives the input back.
const STUCK: Duration = Duration::from_millis(1500);

/// The app's keys by their evdev code (linux/input-event-codes.h), as the
/// plugin sends them. Physical keys, as `KeyCode` is, so WASD stays where it
/// is on any layout. Any other key is not the app's.
fn key_code(evdev: u32) -> Option<KeyCode> {
    Some(match evdev {
        1 => KeyCode::Escape,
        16 => KeyCode::KeyQ,
        17 => KeyCode::KeyW,
        19 => KeyCode::KeyR,
        29 => KeyCode::ControlLeft,
        30 => KeyCode::KeyA,
        31 => KeyCode::KeyS,
        32 => KeyCode::KeyD,
        42 => KeyCode::ShiftLeft,
        57 => KeyCode::Space,
        _ => return None,
    })
}

/// A mouse button by its evdev code (`BTN_LEFT` and on).
fn mouse_button(evdev: u32) -> MouseButton {
    match evdev {
        0x110 => MouseButton::Left,
        0x111 => MouseButton::Right,
        0x112 => MouseButton::Middle,
        0x113 => MouseButton::Back,
        0x114 => MouseButton::Forward,
        other => MouseButton::Other(other as u16),
    }
}

/// The plugin's keys and mouse, written as Bevy's own input before Bevy
/// reads it (`InputSystems`), so they land in `ButtonInput<KeyCode>`,
/// `ButtonInput<MouseButton>` and `AccumulatedMouseMotion` as a window's
/// would. Its motion is the accelerated pair, as on the desktop.
#[derive(SystemParam)]
struct PluginInput<'w, 's> {
    window: Option<Single<'w, 's, Entity, With<PrimaryWindow>>>,
    keys: MessageWriter<'w, KeyboardInput>,
    buttons: MessageWriter<'w, MouseButtonInput>,
    motion: MessageWriter<'w, MouseMotion>,
    focus_lost: MessageWriter<'w, KeyboardFocusLost>,
}

impl PluginInput<'_, '_> {
    fn window(&self) -> Entity {
        self.window
            .as_deref()
            .copied()
            .unwrap_or(Entity::PLACEHOLDER)
    }

    fn key(&mut self, code: u32, pressed: bool) {
        let Some(key_code) = key_code(code) else {
            return;
        };
        let window = self.window();
        self.keys.write(KeyboardInput {
            key_code,
            logical_key: Key::Unidentified(NativeKey::Unidentified),
            state: state(pressed),
            text: None,
            repeat: false,
            window,
        });
    }

    fn button(&mut self, code: u32, pressed: bool) {
        let window = self.window();
        self.buttons.write(MouseButtonInput {
            button: mouse_button(code),
            state: state(pressed),
            window,
        });
    }

    fn moved(&mut self, delta: Vec2) {
        self.motion.write(MouseMotion { delta });
    }

    /// The plugin took the keyboard back: every key it held is up.
    fn lost(&mut self) {
        self.focus_lost.write(KeyboardFocusLost);
    }
}

fn state(pressed: bool) -> ButtonState {
    if pressed {
        ButtonState::Pressed
    } else {
        ButtonState::Released
    }
}

/// Who has the keyboard and mouse, as the plugin last said.
#[derive(States, Reflect, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum InputMode {
    /// Hyprland: the app gets nothing from the plugin.
    #[default]
    Off,
    /// The app: motion, keys, buttons and scroll all arrive here.
    Look,
    /// As Look for the mouse, but the keyboard goes to one window.
    Type,
}

/// The desktop the app shows, as the plugin's feed describes it: the facts
/// there is one of. Its windows, layers and workspaces are entities
/// (windows.rs, layers.rs, scene.rs), told by `DesktopChange`. Empty while the plugin is not connected.
#[derive(Resource, Reflect, Default)]
#[reflect(Resource)]
pub struct Desktop {
    pub monitor: Option<Monitor>,
    /// The workspace underneath the app.
    pub active: Option<i64>,
    /// The app is in view.
    pub shown: bool,
}

impl Desktop {
    /// The world is shown: in view, or always without the plugin's feed (no
    /// monitor).
    pub fn in_view(&self) -> bool {
        self.shown || self.monitor.is_none()
    }
}

/// What the plugin said of the desktop's windows, layers and workspaces, in
/// the order it said it, for the modules whose entities they are
/// (windows.rs, layers.rs, scene.rs). One message for all of them, so that a
/// reset keeps its place among what each reads.
#[derive(Message, Clone, Debug)]
pub enum DesktopChange {
    /// A window came, or changed.
    Window(u64, Window2d),
    Closed(u64),
    /// A layer came, or changed. Its pixels come in a `Picture`.
    Layer(u64, Layer2d),
    LayerGone(u64),
    /// A workspace on the app's monitor came, or was renamed: its id and
    /// its name, which can be empty.
    Workspace(i64, String),
    WorkspaceGone(i64),
    /// The plugin connected or went away: what it said before is gone, and a
    /// connect brings it all again.
    Reset,
}

/// What became of one thing over a batch of `DesktopChange`s, against the
/// entities there are (`DesktopChange::fold`).
#[derive(Debug, PartialEq)]
pub enum Said<V> {
    /// It has no entity yet, and is this.
    Came(V),
    /// Its entity is still there, and it is this now.
    Still(Entity, V),
    Gone(Entity),
}

impl DesktopChange {
    /// What a batch of changes comes to for one kind of thing: each that
    /// came, was said again or went, once, by the last word on it. `index`
    /// is its entities by the plugin's id for it, and `pick` reads a change
    /// of its kind as an id and what it is now, or `None` if it went. A
    /// reset makes moot what came before it, and is the end of every entity
    /// not said again after it.
    pub fn fold<'a, K: Copy + Eq + Hash, V>(
        changes: impl IntoIterator<Item = &'a DesktopChange>,
        index: &HashMap<K, Entity>,
        pick: impl Fn(&'a DesktopChange) -> Option<(K, Option<V>)>,
    ) -> Vec<(K, Said<V>)> {
        let mut last: HashMap<K, Option<V>> = HashMap::new();
        let mut reset = false;
        for change in changes {
            if let DesktopChange::Reset = change {
                last.clear();
                reset = true;
            } else if let Some((id, now)) = pick(change) {
                last.insert(id, now);
            }
        }

        let mut said = Vec::new();
        if reset {
            let unsaid = index.iter().filter(|(id, _)| !last.contains_key(*id));
            said.extend(unsaid.map(|(&id, &entity)| (id, Said::Gone(entity))));
        }
        said.extend(last.into_iter().filter_map(|(id, now)| {
            let said = match (now, index.get(&id)) {
                (Some(now), None) => Said::Came(now),
                (Some(now), Some(&entity)) => Said::Still(entity, now),
                (None, Some(&entity)) => Said::Gone(entity),
                (None, None) => return None,
            };
            Some((id, said))
        }));
        said
    }
}

/// A run condition: a `T` came, changed or went since it last ran, as the
/// entities `DesktopChange` keeps do.
pub fn any_changed_or_removed<T: Component>(
    changed: Query<(), Changed<T>>,
    mut removed: RemovedComponents<T>,
) -> bool {
    // Read through, as Bevy's `any_component_removed` does, so that a
    // removal counts once. First, so that it always is.
    removed.read().count() > 0 || !changed.is_empty()
}

/// The app's settings as `hl.plugin.hypr_world.config({ world = ... })` gives
/// them, one JSON object (config.rs reads it). Kept across a reconnect until
/// the plugin sends them again.
#[derive(Resource, Default)]
pub struct WorldSettings(pub serde_json::Value);

impl WorldSettings {
    /// One setting at a dotted path (`player.flying`), as the control pipe's
    /// `set` says it: the value as JSON where it is, else as a string.
    fn set(&mut self, key: &str, value: &str) {
        let value = serde_json::from_str(value).unwrap_or_else(|_| value.into());
        let mut at = &mut self.0;
        for part in key.split('.') {
            if !at.is_object() {
                *at = serde_json::Value::Object(Default::default());
            }
            at = at
                .as_object_mut()
                .expect("an object")
                .entry(part)
                .or_insert(serde_json::Value::Null);
        }
        *at = value;
    }
}

#[derive(Reflect, Clone, Copy, PartialEq, Debug)]
pub struct Monitor {
    /// Logical px.
    pub size: Vec2,
    pub scale: f32,
    /// The monitor without the bar, monitor-local.
    pub work: Rect,
}

/// A window as it is in 2D.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Window2d {
    pub workspace: i64,
    /// Its current (animated) box, monitor-local logical px.
    pub rect: Rect,
    /// Its place in Hyprland's stacking order.
    pub stack: u32,
    pub kind: Kind,
}

#[derive(Reflect, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Tiled,
    Floating,
    Fullscreen,
}

/// A layer surface as it is in 2D.
#[derive(Clone, PartialEq, Debug)]
pub struct Layer2d {
    pub level: Level,
    /// Its current (animated) box, monitor-local logical px.
    pub rect: Rect,
    /// What its client calls it, `noctalia-bar-default` and the like.
    pub namespace: String,
}

/// Hyprland's layer-shell levels the plugin sends. The overlay, over
/// everything, is left out.
#[derive(Reflect, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// Under the windows: the wallpaper.
    Background,
    /// Under the windows, over the background.
    Bottom,
    /// Over the windows, and under a fullscreen one: the bar.
    Top,
}

/// A layer as the plugin drew it: RGBA, premultiplied, top row first, the
/// size of its box in the monitor's pixels. layers.rs takes the pixels.
#[derive(Message)]
pub struct Picture {
    pub layer: u64,
    pub size: UVec2,
    pub pixels: Vec<u8>,
}

/// The plugin asked for the flight in (flight.rs). A message, not an event
/// for an observer: the flight starts from the house laid out this update,
/// which comes after the drain.
#[derive(Message)]
pub struct Enter;

/// The plugin asked for the flight out (flight.rs).
#[derive(Message)]
pub struct Leave;

/// The control pipe asked for a screenshot of the next frame, saved as a
/// PNG here (controls.rs).
#[derive(Message)]
pub struct TakeScreenshot(pub PathBuf);

/// The control pipe said `quit` (controls.rs).
#[derive(Message)]
pub struct Quit;

/// A window's own buffer, as its client committed it (plugin `buffer` and
/// `shm`), to show in place of its captured frames: which of the window's
/// surfaces it is (0 its own, else as `Surfaces` names them), and the
/// buffer.
#[derive(Message)]
pub struct Shared {
    pub window: u64,
    pub surface: u32,
    /// Its fourth byte is alpha, not padding.
    pub alpha: bool,
    pub buffer: SharedBuffer,
}

pub enum SharedBuffer {
    /// A dmabuf, sampled where it is, and the part of it the surface shows,
    /// in buffer pixels. The app says `done` with its id once the GPU has
    /// finished with it (import.rs `SharedLog`).
    Dmabuf {
        id: u64,
        crop: URect,
        dmabuf: import::Dmabuf,
    },
    /// Shared memory: the part the surface shows, copied out on the
    /// connection thread, which handed the buffer back there and then, and
    /// how long the copy took. Or a single-pixel buffer's one pixel, which
    /// came in its line, with no copy and nothing to hand back.
    Pixels {
        frame: ShmFrame,
        copy: Option<Duration>,
    },
}

impl SharedBuffer {
    /// The plugin's id for a dmabuf, which goes back with `done`, the GPU
    /// done with it. Shared memory went back as it was copied.
    pub fn dmabuf_id(&self) -> Option<u64> {
        match self {
            SharedBuffer::Dmabuf { id, .. } => Some(*id),
            SharedBuffer::Pixels { .. } => None,
        }
    }

    /// Its size in pixels, the dmabuf's whole and the copy's as cropped, and
    /// what its pixels are.
    pub fn picture(&self) -> (UVec2, import::BufferFormat) {
        match self {
            SharedBuffer::Dmabuf { dmabuf, .. } => {
                (UVec2::new(dmabuf.width, dmabuf.height), dmabuf.format)
            }
            SharedBuffer::Pixels { frame, .. } => (
                UVec2::new(frame.width, frame.height),
                import::BufferFormat::Rgb(frame.layout),
            ),
        }
    }
}

/// A shared window's surfaces past its own (plugin `surfaces`), bottom to
/// top: its subsurfaces, then its popups with theirs, each by the plugin's
/// id for it, with its box in logical px from the window's top left. A
/// popup's may reach past the window's edges. None: the window is one
/// surface again.
#[derive(Message, Debug, PartialEq)]
pub struct Surfaces {
    pub window: u64,
    pub surfaces: Vec<(u32, Rect)>,
}

/// The plugin went back to capture for a window (plugin `unshared`).
#[derive(Message)]
pub struct Unshared(pub u64);

/// What the plugin and the control pipe said this update that another
/// system acts on, written as Bevy's messages, and the input mode, a state.
#[derive(SystemParam)]
struct Told<'w> {
    mode: ResMut<'w, NextState<InputMode>>,
    desktop: MessageWriter<'w, DesktopChange>,
    enter: MessageWriter<'w, Enter>,
    leave: MessageWriter<'w, Leave>,
    pictures: MessageWriter<'w, Picture>,
    shared: MessageWriter<'w, Shared>,
    surfaces: MessageWriter<'w, Surfaces>,
    unshared: MessageWriter<'w, Unshared>,
    screenshots: MessageWriter<'w, TakeScreenshot>,
    quit: MessageWriter<'w, Quit>,
}

enum Event {
    Connected(bool),
    Hello(u32),
    Damaged(Vec<u64>),
    Mode(InputMode),
    Motion(Vec2),
    Key {
        code: u32,
        pressed: bool,
    },
    Button {
        code: u32,
        pressed: bool,
    },
    Grab(Option<u64>),
    Enter,
    Leave,
    Active(i64),
    Monitor(Monitor),
    /// A window, a workspace or a layer gone, as it goes out (`drain`).
    Desktop(DesktopChange),
    /// A layer, with its pixels.
    Layer(Layer2d, Picture),
    Buffer(Shared),
    Surfaces(Surfaces),
    Unshared(u64),
    Shown(bool),
    /// The whole of the settings.
    Settings(serde_json::Value),
    /// One setting, from the control pipe's `set`.
    Setting(String, String),
    /// From the control pipe (`control`), not the plugin.
    Screenshot(PathBuf),
    Quit,
}

impl Event {
    /// The verb of the plugin's line this came from, for the stats. The
    /// connection itself is not a line.
    fn verb(&self) -> Option<&'static str> {
        Some(match self {
            Event::Connected(_) => return None,
            Event::Hello(_) => "hello",
            Event::Damaged(_) => "damaged",
            Event::Mode(_) => "state",
            Event::Motion(_) => "motion",
            Event::Key { .. } => "key",
            Event::Button { .. } => "button",
            Event::Grab(Some(_)) => "grab",
            Event::Grab(None) => "ungrab",
            Event::Enter => "enter",
            Event::Leave => "leave",
            Event::Active(_) => "workspace",
            Event::Monitor(_) => "monitor",
            Event::Desktop(DesktopChange::Workspace(..)) => "ws",
            Event::Desktop(DesktopChange::WorkspaceGone(_)) => "ws-gone",
            Event::Desktop(DesktopChange::Window(..)) => "window",
            Event::Desktop(DesktopChange::Closed(_)) => "closed",
            Event::Desktop(DesktopChange::Layer(..)) | Event::Layer(..) => "layer",
            Event::Desktop(DesktopChange::LayerGone(_)) => "layer-gone",
            Event::Desktop(DesktopChange::Reset) => return None,
            Event::Buffer(_) => "buffer",
            Event::Surfaces(_) => "surfaces",
            Event::Unshared(_) => "unshared",
            Event::Shown(true) => "shown",
            Event::Shown(false) => "hidden",
            Event::Settings(_) => "settings",
            Event::Setting(..) => "set",
            Event::Screenshot(_) | Event::Quit => return None,
        })
    }
}

type Writer = Arc<Mutex<Option<UnixStream>>>;

#[derive(Resource)]
pub struct Glue {
    events: Mutex<Receiver<Event>>,
    writer: Writer,
    pub connected: bool,
    /// The window a drag holds (the plugin's implicit grab), by address.
    pub grab: Option<u64>,
    /// Counts the connections, for what is said once to each.
    pub connection: u64,
}

/// Lines to the plugin from where `Glue` is not at hand: the render world's
/// `done`s (import.rs).
#[derive(Resource, Clone)]
pub struct PluginSender(Writer);

impl PluginSender {
    pub fn send(&self, line: &str) {
        write_line(&self.0, line);
    }
}

impl Glue {
    /// Starts the connection thread, the heartbeat and the control pipe. The
    /// plugin's damage goes to `damage`, the capture thread. The resource
    /// works either way: without the plugin it simply stays disconnected.
    pub fn start(wake: Wake, damage: Damage) -> Self {
        let (tx, rx) = channel();
        let writer: Writer = Arc::default();

        let (control_tx, control_wake) = (tx.clone(), wake.clone());
        thread::spawn(move || control(&control_tx, &control_wake));
        let (thread_writer, thread_wake) = (writer.clone(), wake.clone());
        thread::spawn(move || run(tx, thread_writer, thread_wake, damage));
        let heartbeat_writer = writer.clone();
        thread::spawn(move || heartbeat(&heartbeat_writer, &wake));

        Self::new(rx, writer)
    }

    /// Connected to `stream`, with no threads: what systems send goes to it,
    /// and nothing comes in.
    #[cfg(test)]
    pub fn connected_to(stream: UnixStream) -> Self {
        let (_, rx) = channel();
        Self {
            connected: true,
            ..Self::new(rx, Arc::new(Mutex::new(Some(stream))))
        }
    }

    /// Fed through the sender it comes with, as the connection thread feeds
    /// it, with no threads, and nothing sent anywhere.
    #[cfg(test)]
    fn fed() -> (Self, Sender<Event>) {
        let (tx, rx) = channel();
        (Self::new(rx, Writer::default()), tx)
    }

    fn new(events: Receiver<Event>, writer: Writer) -> Self {
        Self {
            events: Mutex::new(events),
            writer,
            connected: false,
            grab: None,
            connection: 0,
        }
    }

    /// Sends one line.
    pub fn send(&self, line: &str) {
        write_line(&self.writer, line);
    }

    pub fn sender(&self) -> PluginSender {
        PluginSender(self.writer.clone())
    }
}

/// The control pipe, $XDG_RUNTIME_DIR/hypr-world.ctl: drives the app
/// with no hands, for tests (tools/drive.sh). Its lines are the plugin's input
/// lines (`key`, `motion`, `button`) and `set`, taken as if the plugin had
/// sent them, plus `screenshot <path>`, of the next frame drawn, and `quit`.
/// In $HYPR_WORLD_DIR instead when that is set, so that an app in a nested
/// test session (plugin/nested.sh) has a pipe of its own.
fn control(events: &Sender<Event>, wake: &Wake) {
    let dir = std::env::var_os("HYPR_WORLD_DIR").or_else(|| std::env::var_os("XDG_RUNTIME_DIR"));
    let Some(path) = dir.map(|dir| PathBuf::from(dir).join("hypr-world.ctl")) else {
        return;
    };
    // An app before this one may have left its pipe.
    let _ = std::fs::remove_file(&path);
    if let Err(e) = rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &path,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    ) {
        eprintln!("hypr-world: no control pipe at {}: {e}", path.display());
        return;
    }

    loop {
        // Waits for a writer, and reads until the last one closes it.
        let Ok(pipe) = File::open(&path) else { return };
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            let Some(event) = control_line(&line) else {
                eprintln!("hypr-world: control: no such line {line:?}");
                continue;
            };
            if events.send(event).is_err() {
                return;
            }
            wake.wake();
        }
    }
}

/// One control pipe line. Of the plugin's lines only the input ones, and
/// `set`, a setting for this session until the plugin sends them all
/// again: the rest would make up a desktop.
fn control_line(line: &str) -> Option<Event> {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    match verb {
        "key" | "motion" | "button" => parse(line),
        // set <dotted.key> <value>: the value is the rest of the line.
        "set" => {
            let (key, value) = rest.split_once(' ').unwrap_or((rest, ""));
            (!key.is_empty()).then(|| Event::Setting(key.to_string(), value.to_string()))
        }
        "screenshot" if !rest.is_empty() => Some(Event::Screenshot(PathBuf::from(rest))),
        "quit" => Some(Event::Quit),
        _ => None,
    }
}

/// $XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/hypr-world.sock
fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let instance = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    Some(
        PathBuf::from(runtime)
            .join("hypr")
            .join(instance)
            .join("hypr-world.sock"),
    )
}

fn run(events: Sender<Event>, writer: Writer, wake: Wake, damage: Damage) {
    let Some(path) = socket_path() else {
        eprintln!("hypr-world: not running under Hyprland, no glue");
        return;
    };

    // Every event wakes the app, which may be sleeping between frames.
    let send = |event: Event| {
        let sent = events.send(event).is_ok();
        wake.wake();
        sent
    };

    let mut announced = false;

    loop {
        if let Ok(stream) = UnixStream::connect(&path)
            && let Ok(reader) = stream.try_clone()
        {
            // A plugin that stops reading must not stall the frame that writes.
            let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));

            if let Ok(mut slot) = writer.lock() {
                *slot = Some(stream);
                if let Some(stream) = slot.as_mut() {
                    let _ = writeln!(stream, "hello {VERSION}");
                }
            }

            eprintln!("hypr-world: connected to the hypr-world plugin");
            announced = false;
            if !send(Event::Connected(true)) {
                return;
            }

            let mut lines = Lines::new(reader);
            while let Some(line) = lines.next() {
                // The lines that carry a file.
                let event = if line.starts_with("layer ") {
                    layer(&line, lines.files.pop_front())
                } else if line.starts_with("buffer ") {
                    let shared = buffer(&line, lines.files.pop_front());
                    // One the app cannot take goes straight back.
                    if shared.is_none()
                        && let Some(id) = line.split(' ').nth(3)
                    {
                        write_line(&writer, &format!("done {id}"));
                    }
                    shared
                } else if line.starts_with("shm ") {
                    // Copied out, or not to be: either way its client may
                    // have it back now.
                    let (id, shared) = shm(&line, lines.files.pop_front());
                    if let Some(id) = id {
                        write_line(&writer, &format!("done {id}"));
                    }
                    shared
                } else {
                    parse(&line)
                };
                // Nothing works against a plugin of another version, and
                // retrying would only meet it again.
                if let Some(Event::Hello(version)) = event
                    && version != VERSION
                {
                    eprintln!(
                        "hypr-world: the plugin speaks protocol {version}, this build {VERSION}; \
                         rebuild and reload both from the same tree"
                    );
                    std::process::exit(1);
                }
                match event {
                    // Straight to the capture thread: the app need not wake
                    // to pass it on.
                    Some(Event::Damaged(windows)) => damage.report(&windows),
                    Some(event) => {
                        if !send(event) {
                            return;
                        }
                    }
                    None => {}
                }
            }

            if let Ok(mut slot) = writer.lock() {
                *slot = None;
            }
            eprintln!("hypr-world: lost the hypr-world plugin");
            if !send(Event::Connected(false)) {
                return;
            }
        } else if !announced {
            eprintln!("hypr-world: hypr-world plugin not loaded, retrying");
            announced = true;
        }

        thread::sleep(Duration::from_secs(1));
    }
}

/// The plugin's lines, and the files some of them carry (SCM_RIGHTS). A
/// file comes with the first byte of its line, so it is here, in order, by
/// the time its line is whole.
struct Lines {
    stream: UnixStream,
    pending: Vec<u8>,
    files: VecDeque<OwnedFd>,
}

impl Lines {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            pending: Vec::new(),
            files: VecDeque::new(),
        }
    }

    /// The next line, without its newline; None once the plugin is gone.
    fn next(&mut self) -> Option<String> {
        loop {
            if let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.pending.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Some(String::from_utf8_lossy(&line).into_owned());
            }

            let mut chunk = [0u8; 4096];
            let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let received = match recvmsg(
                &self.stream,
                &mut [IoSliceMut::new(&mut chunk)],
                &mut control,
                RecvFlags::CMSG_CLOEXEC,
            ) {
                Ok(received) => received,
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return None,
            };
            for message in control.drain() {
                if let RecvAncillaryMessage::ScmRights(files) = message {
                    self.files.extend(files);
                }
            }
            if received.bytes == 0 {
                return None;
            }
            self.pending.extend_from_slice(&chunk[..received.bytes]);
        }
    }
}

/// layer <addr> <level> <x> <y> <w> <h> <pw> <ph> <namespace>, and the file
/// that came with it: pw x ph pixels, RGBA. A file too short for them is
/// not this line's, and the line is dropped.
fn layer(line: &str, file: Option<OwnedFd>) -> Option<Event> {
    let tokens: Vec<&str> = line.splitn(10, ' ').collect();
    let number = |i: usize| tokens.get(i).and_then(|t| t.parse::<f64>().ok());

    let address = address(tokens.get(1)?)?;
    let level = match *tokens.get(2)? {
        "0" => Level::Background,
        "1" => Level::Bottom,
        "2" => Level::Top,
        _ => return None,
    };
    let rect = rect(number(3)?, number(4)?, number(5)?, number(6)?);
    let size = UVec2::new(tokens.get(7)?.parse().ok()?, tokens.get(8)?.parse().ok()?);
    let namespace = tokens.get(9).copied().unwrap_or_default().to_string();

    let file = File::from(file?);
    let len = size.x as usize * size.y as usize * 4;
    if file.metadata().ok()?.len() < len as u64 {
        return None;
    }
    let mut pixels = vec![0; len];
    file.read_exact_at(&mut pixels, 0).ok()?;

    Some(Event::Layer(
        Layer2d {
            level,
            rect,
            namespace,
        },
        Picture {
            layer: address,
            size,
            pixels,
        },
    ))
}

/// buffer <addr> <sid> <id> <w> <h> <fourcc> <modifier> <cx> <cy> <cw>
/// <ch> <planes> (<offset> <stride>)..., and the dmabuf that came with it,
/// which holds every plane (the plugin sends no other).
fn buffer(line: &str, file: Option<OwnedFd>) -> Option<Event> {
    let mut tokens: Vec<&str> = line.split(' ').collect();
    if tokens.len() <= 2 {
        return None;
    }
    let surface = tokens.remove(2).parse().ok()?;
    let (number, hex) = (|i| nth_number(&tokens, i), |i| nth_hex(&tokens, i));

    let window = address(tokens.get(1)?)?;
    let id = tokens.get(2)?.parse().ok()?;
    let (width, height) = (number(3)?, number(4)?);
    let fourcc = u32::try_from(hex(5)?).ok()?;
    let format = import::format_of(fourcc)?;
    let modifier = hex(6)?;
    let (x, y) = (number(7)?, number(8)?);
    let crop = URect::new(
        x,
        y,
        x.checked_add(number(9)?)?,
        y.checked_add(number(10)?)?,
    );
    let count = number(11)? as usize;
    // YUV has its luma and its chroma in planes of their own.
    let least = if matches!(format, import::BufferFormat::Yuv(_)) {
        2
    } else {
        1
    };
    if !(least..=4).contains(&count) || tokens.len() != 12 + 2 * count || crop.is_empty() {
        return None;
    }

    let file = file?;
    let planes = (0..count)
        .map(|i| {
            let (offset, stride) = (number(12 + 2 * i)?, number(13 + 2 * i)?);
            Some(import::DmabufPlane {
                fd: file.try_clone().ok()?,
                offset,
                stride,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let dmabuf = import::Dmabuf {
        width,
        height,
        format,
        modifier,
        planes,
    };
    let alpha = import::has_alpha(fourcc);
    Some(Event::Buffer(Shared {
        window,
        surface,
        alpha,
        buffer: SharedBuffer::Dmabuf { id, crop, dmabuf },
    }))
}

/// shm <addr> <sid> <id> <w> <h> <fourcc> <cx> <cy> <cw> <ch> <offset>
/// <stride>, and the shared-memory pool that came with it: the part of the
/// buffer the surface shows, copied out of the pool, top row first. The
/// plugin's id for the buffer, to say `done` with however the copy went, as
/// its client may have it back once the pixels are out, and the frame if
/// they came. Read, not mapped: a client that shrinks its pool under the
/// app would fault a mapping, where a read only comes up short.
fn shm(line: &str, file: Option<OwnedFd>) -> (Option<u64>, Option<Event>) {
    let tokens: Vec<&str> = line.split(' ').collect();
    let id = tokens.get(3).and_then(|token| token.parse().ok());
    let started = Instant::now();
    let copied = || {
        let (number, hex) = (|i| nth_number(&tokens, i), |i| nth_hex(&tokens, i));

        let window = address(tokens.get(1)?)?;
        let surface = number(2)?;
        let (width, height) = (number(4)?, number(5)?);
        let fourcc = u32::try_from(hex(6)?).ok()?;
        let layout = import::layout_of(fourcc)?;
        let (x, y, w, h) = (number(7)?, number(8)?, number(9)?, number(10)?);
        let offset: u64 = tokens.get(11)?.parse().ok()?;
        let stride = u64::from(number(12)?);
        let fits = x.checked_add(w)? <= width && y.checked_add(h)? <= height;
        if tokens.len() != 13 || w == 0 || h == 0 || !fits || stride < u64::from(width) * 4 {
            return None;
        }

        // The crop's rows whole, from its first row's start to its last
        // pixel, in one read, then each row moved down to where the packed
        // frame has it.
        let start = offset.checked_add(u64::from(y) * stride)?;
        let span = u64::from(h - 1)
            .checked_mul(stride)?
            .checked_add(u64::from(x + w) * 4)?;
        let file = File::from(file?);
        if file.metadata().ok()?.len() < start.checked_add(span)? {
            return None;
        }
        let mut pixels = vec![0; usize::try_from(span).ok()?];
        file.read_exact_at(&mut pixels, start).ok()?;
        let (row, stride) = (w as usize * 4, stride as usize);
        if x > 0 || stride > row {
            for r in 0..h as usize {
                let from = r * stride + x as usize * 4;
                pixels.copy_within(from..from + row, r * row);
            }
        }
        pixels.truncate(h as usize * row);

        let frame = ShmFrame {
            width: w,
            height: h,
            layout,
            pixels,
        };
        let buffer = SharedBuffer::Pixels {
            frame,
            copy: Some(started.elapsed()),
        };
        Some(Event::Buffer(Shared {
            window,
            surface,
            alpha: import::has_alpha(fourcc),
            buffer,
        }))
    };
    (id, copied())
}

/// pixel <addr> <sid> <argb>: a surface whose buffer is a single pixel
/// (wp_single_pixel_buffer_v1), stretched over its box: its colour as
/// ARGB8888 in hex, premultiplied as Wayland's buffers are, which in memory
/// is the one pixel in BGRA order. Nothing is held for it, so no
/// id and no `done`.
fn pixel(tokens: &[&str]) -> Option<Event> {
    let [_, window, surface, argb] = tokens else {
        return None;
    };
    let window = address(window)?;
    let surface = surface.parse().ok()?;
    let argb = u32::from_str_radix(argb, 16).ok()?;
    let frame = ShmFrame {
        width: 1,
        height: 1,
        layout: PixelLayout::Bgra,
        pixels: argb.to_le_bytes().to_vec(),
    };
    let buffer = SharedBuffer::Pixels { frame, copy: None };
    Some(Event::Buffer(Shared {
        window,
        surface,
        alpha: argb >> 24 != 0xff,
        buffer,
    }))
}

/// surfaces <addr> [<sid> <x> <y> <w> <h>]...: all of the line or nothing.
fn surfaces(tokens: &[&str]) -> Option<Event> {
    let window = address(tokens.get(1)?)?;
    let boxes = tokens.get(2..)?;
    if boxes.len() % 5 != 0 {
        return None;
    }
    let surfaces = boxes
        .chunks(5)
        .map(|surface| {
            let number = |i: usize| surface[i].parse::<f64>().ok();
            Some((
                surface[0].parse().ok()?,
                rect(number(1)?, number(2)?, number(3)?, number(4)?),
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Event::Surfaces(Surfaces { window, surfaces }))
}

/// One line from the plugin. Unknown verbs are ignored.
fn parse(line: &str) -> Option<Event> {
    let tokens: Vec<&str> = line.split(' ').filter(|t| !t.is_empty()).collect();
    let number = |i: usize| tokens.get(i).and_then(|t| t.parse::<f64>().ok());

    match *tokens.first()? {
        "state" => Some(Event::Mode(match *tokens.get(1)? {
            "look" => InputMode::Look,
            "type" => InputMode::Type,
            _ => InputMode::Off,
        })),
        // dx dy ux uy, accelerated then raw: the app looks with the
        // accelerated pair, as the pointer moves on the desktop.
        "motion" => Some(Event::Motion(Vec2::new(
            number(1)? as f32,
            number(2)? as f32,
        ))),
        "key" => Some(Event::Key {
            code: tokens.get(1)?.parse().ok()?,
            pressed: *tokens.get(2)? == "1",
        }),
        "button" => Some(Event::Button {
            code: tokens.get(1)?.parse().ok()?,
            pressed: *tokens.get(2)? == "1",
        }),
        "grab" => Some(Event::Grab(Some(address(tokens.get(1)?)?))),
        "ungrab" => Some(Event::Grab(None)),
        "enter" => Some(Event::Enter),
        "leave" => Some(Event::Leave),
        // workspace <id> <addr|->: the window part is for following a single
        // window, which the house has no use for.
        "workspace" => Some(Event::Active(tokens.get(1)?.parse().ok()?)),
        "monitor" => Some(Event::Monitor(Monitor {
            size: Vec2::new(number(1)? as f32, number(2)? as f32),
            scale: number(3)? as f32,
            work: rect(number(4)?, number(5)?, number(6)?, number(7)?),
        })),
        // ws <id> <name>: the name is the rest of the line.
        "ws" => {
            let mut parts = line.splitn(3, ' ');
            let id = parts.nth(1)?.parse().ok()?;
            Some(Event::Desktop(DesktopChange::Workspace(
                id,
                parts.next().unwrap_or_default().to_string(),
            )))
        }
        "ws-gone" => Some(Event::Desktop(DesktopChange::WorkspaceGone(
            tokens.get(1)?.parse().ok()?,
        ))),
        "window" => Some(Event::Desktop(DesktopChange::Window(
            address(tokens.get(1)?)?,
            Window2d {
                workspace: tokens.get(2)?.parse().ok()?,
                rect: rect(number(3)?, number(4)?, number(5)?, number(6)?),
                stack: tokens.get(7)?.parse().ok()?,
                kind: match *tokens.get(8)? {
                    "F" => Kind::Fullscreen,
                    "f" => Kind::Floating,
                    _ => Kind::Tiled,
                },
            },
        ))),
        "closed" => Some(Event::Desktop(DesktopChange::Closed(address(
            tokens.get(1)?,
        )?))),
        "layer-gone" => Some(Event::Desktop(DesktopChange::LayerGone(address(
            tokens.get(1)?,
        )?))),
        "unshared" => Some(Event::Unshared(address(tokens.get(1)?)?)),
        "surfaces" => surfaces(&tokens),
        "pixel" => pixel(&tokens),
        "shown" => Some(Event::Shown(true)),
        "hidden" => Some(Event::Shown(false)),
        "hello" => Some(Event::Hello(tokens.get(1)?.parse().ok()?)),
        "damaged" => Some(Event::Damaged(
            tokens[1..].iter().filter_map(|t| address(t)).collect(),
        )),
        // settings <json>: the rest of the line, all of them.
        "settings" => Some(Event::Settings(
            serde_json::from_str(line.split_once(' ')?.1).ok()?,
        )),
        _ => None,
    }
}

/// The `i`th token as a number, or as 0x and hex.
fn nth_number(tokens: &[&str], i: usize) -> Option<u32> {
    tokens.get(i)?.parse().ok()
}

fn nth_hex(tokens: &[&str], i: usize) -> Option<u64> {
    u64::from_str_radix(tokens.get(i)?.strip_prefix("0x")?, 16).ok()
}

/// A box given as x, y, width, height.
fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect::new(x as f32, y as f32, (x + w) as f32, (y + h) as f32)
}

/// A window address as the plugin writes it, 0x and hex.
fn address(token: &str) -> Option<u64> {
    u64::from_str_radix(token.trim_start_matches("0x"), 16).ok()
}

/// Moves this frame's plugin events into the resources and messages. Runs
/// before anything that reads them. The desktop is only touched (and so only
/// marked changed) when the feed says something of the monitor, the active
/// workspace or the view: windows, layers and workspaces go out as
/// `DesktopChange`s.
fn drain(
    wake: Res<Wake>,
    mut glue: ResMut<Glue>,
    mut desktop: ResMut<Desktop>,
    mut settings: ResMut<WorldSettings>,
    mut stats: ResMut<Stats>,
    mut input: PluginInput,
    mut told: Told,
) {
    // Before reading: anything that comes after this wakes the app again.
    wake.seen();

    let events: Vec<Event> = match glue.events.lock() {
        Ok(rx) => rx.try_iter().collect(),
        Err(_) => return,
    };

    for event in events {
        if let Some(verb) = event.verb() {
            stats.line(verb);
        }
        match event {
            Event::Connected(connected) => {
                glue.connected = connected;
                glue.connection += u64::from(connected);
                if !connected {
                    input.lost();
                    glue.grab = None;
                }
                // The plugin's input starts off either way, and the hint says
                // whether there is a plugin: `set`, which enters Off again
                // from Off, where `set_if_different` would not.
                told.mode.set(InputMode::Off);
                // A (re)connect brings the whole feed afresh.
                *desktop = Desktop::default();
                told.desktop.write(DesktopChange::Reset);
            }
            Event::Mode(mode) => {
                // Here, not on entering Off: Bevy reads the input next, in
                // this update.
                if mode == InputMode::Off {
                    input.lost();
                    glue.grab = None;
                }
                told.mode.set_if_different(mode);
            }
            Event::Grab(window) => glue.grab = window,
            Event::Enter => {
                told.enter.write(Enter);
            }
            Event::Leave => {
                told.leave.write(Leave);
            }
            Event::Buffer(shared) => {
                if let SharedBuffer::Pixels {
                    frame,
                    copy: Some(copy),
                } = &shared.buffer
                {
                    stats.shm_copied(frame.pixels.len(), *copy);
                }
                told.shared.write(shared);
            }
            Event::Surfaces(surfaces) => {
                told.surfaces.write(surfaces);
            }
            Event::Unshared(window) => {
                told.unshared.write(Unshared(window));
            }
            // A mismatched version never gets here: the connection thread
            // exits on it, so a connected plugin speaks ours.
            Event::Hello(_) => {}
            // Handed to the capture thread in `run`, never queued for the app.
            Event::Damaged(_) => {}
            Event::Screenshot(path) => {
                told.screenshots.write(TakeScreenshot(path));
            }
            Event::Quit => {
                told.quit.write(Quit);
            }
            Event::Motion(delta) => input.moved(delta),
            Event::Key { code, pressed } => input.key(code, pressed),
            Event::Button { code, pressed } => input.button(code, pressed),
            Event::Active(id) => desktop.active = Some(id),
            Event::Monitor(monitor) => desktop.monitor = Some(monitor),
            Event::Desktop(change) => {
                told.desktop.write(change);
            }
            Event::Layer(layer, picture) => {
                told.desktop
                    .write(DesktopChange::Layer(picture.layer, layer));
                told.pictures.write(picture);
            }
            Event::Shown(shown) => desktop.shown = shown,
            Event::Settings(value) => settings.0 = value,
            Event::Setting(key, value) => settings.set(&key, &value),
        }
    }
}

/// Writes one line. A failed write drops the connection; the reading thread
/// then notices and reconnects.
fn write_line(writer: &Writer, line: &str) {
    let Ok(mut writer) = writer.lock() else {
        return;
    };
    if let Some(stream) = writer.as_mut()
        && writeln!(stream, "{line}").is_err()
    {
        let _ = stream.shutdown(std::net::Shutdown::Both);
        *writer = None;
    }
}

/// Keeps the plugin's watchdog fed from a thread of its own, so the app
/// never wakes just for that: a ping every half second while connected,
/// unless a wake has gone unanswered. Then the app is stuck, and the
/// plugin's watchdog gives the mouse and keyboard back.
fn heartbeat(writer: &Writer, wake: &Wake) {
    loop {
        thread::sleep(PING_INTERVAL);
        if !wake.stuck(STUCK) {
            write_line(writer, "ping");
        }
    }
}

/// The hint line at the bottom of the view: who has the input, and the keys
/// that matter right now. A tiling setup shows no window titles, so this is
/// where it is said.
#[derive(Component)]
pub struct Hint;

/// With the hint for the mode as it is: the mode's first `OnEnter` runs
/// before `Startup`, when there is no hint yet to show it on.
pub fn spawn_hint(
    mut commands: Commands,
    glue: Res<Glue>,
    mode: Res<State<InputMode>>,
    launcher: Option<Res<State<Launcher>>>,
) {
    commands.spawn((
        Hint,
        Text::new(hint(glue.connected, *mode.get(), armed(launcher))),
        TextFont::from_font_size(20.0),
        TextColor(Color::srgb(0.92, 0.93, 0.95)),
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(16.0),
            bottom: Val::Px(12.0),
            ..default()
        },
    ));
}

/// A dot at the centre of the view: what clicks, scroll and hover go to.
#[derive(Component)]
pub struct Crosshair;

pub fn spawn_crosshair(mut commands: Commands) {
    const SIZE: f32 = 6.0;

    commands.spawn((
        Crosshair,
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            top: Val::Percent(50.0),
            width: Val::Px(SIZE),
            height: Val::Px(SIZE),
            margin: UiRect {
                left: Val::Px(-SIZE / 2.0),
                top: Val::Px(-SIZE / 2.0),
                ..default()
            },
            ..default()
        },
        BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.85)),
    ));
}

/// The hint for the input mode just entered, or the launcher taken out or
/// put away. A (re)connect enters Off again (`drain`), for the hint to say
/// whether there is a plugin.
fn show_hint(
    glue: Res<Glue>,
    mode: Res<State<InputMode>>,
    launcher: Option<Res<State<Launcher>>>,
    mut text: Single<&mut Text, With<Hint>>,
) {
    text.0 = hint(glue.connected, *mode.get(), armed(launcher)).to_string();
}

/// The rocket launcher is out.
fn armed(launcher: Option<Res<State<Launcher>>>) -> bool {
    launcher.is_some_and(|launcher| *launcher.get() == Launcher::Armed)
}

fn hint(connected: bool, mode: InputMode, armed: bool) -> &'static str {
    match (connected, mode) {
        (false, _) => "No hypr-world plugin, so no windows · Q or Esc quits",
        (true, InputMode::Off) => "Click to take the mouse and keyboard · Q or Esc quits",
        (true, _) if armed => {
            "The rocket launcher is out: a click closes the window it hits · R puts it away\n\
             Super+Escape gives the mouse back · Q or Esc quits"
        }
        (true, InputMode::Look) => {
            "Super+Left Alt types into the window under the dot · rest it on another workspace to switch\n\
             R takes out the rocket launcher · Super+L hides the world · Super+Escape gives the mouse back · Q or Esc quits"
        }
        (true, InputMode::Type) => {
            "Typing into the window · Super+Left Alt stops typing · Super+Escape gives the mouse back"
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io::IoSlice, os::fd::AsFd, path::Path};

    use rustix::{
        fs::{MemfdFlags, memfd_create},
        net::{
            AddressFamily, SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags,
            SocketType, sendmsg, socketpair,
        },
    };

    use super::*;

    /// The plugin's keys and mouse land in Bevy's input in the update they
    /// come in, a key not the app's is dropped, and the plugin taking the
    /// keyboard back lets go of every key.
    #[test]
    fn the_plugins_keys_and_mouse_are_bevys_input() {
        use bevy::input::{InputPlugin, InputSystems, mouse::AccumulatedMouseMotion};

        let (glue, events) = Glue::fed();
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, InputPlugin, bevy::state::app::StatesPlugin))
            .init_state::<InputMode>()
            .insert_resource(glue)
            .insert_resource(Wake::default())
            .init_resource::<Desktop>()
            .init_resource::<WorldSettings>()
            .init_resource::<Stats>()
            .add_message::<Enter>()
            .add_message::<Leave>()
            .add_message::<Picture>()
            .add_message::<Shared>()
            .add_message::<Surfaces>()
            .add_message::<Unshared>()
            .add_message::<TakeScreenshot>()
            .add_message::<Quit>()
            .add_message::<DesktopChange>()
            .add_systems(PreUpdate, drain.before(InputSystems));

        for event in [
            Event::Mode(InputMode::Look),
            Event::Key {
                code: 17,
                pressed: true,
            },
            Event::Motion(Vec2::new(3.0, -2.0)),
        ] {
            events.send(event).expect("the app's end");
        }
        app.update();
        assert!(
            app.world()
                .resource::<ButtonInput<KeyCode>>()
                .just_pressed(KeyCode::KeyW),
            "W, this update"
        );
        assert_eq!(
            app.world().resource::<AccumulatedMouseMotion>().delta,
            Vec2::new(3.0, -2.0)
        );

        for event in [
            Event::Key {
                code: 99,
                pressed: true,
            },
            Event::Mode(InputMode::Off),
        ] {
            events.send(event).expect("the app's end");
        }
        app.update();
        let keys = app.world().resource::<ButtonInput<KeyCode>>();
        assert!(
            keys.get_pressed().next().is_none(),
            "the plugin took the keyboard back: none held"
        );
        assert_eq!(
            app.world().resource::<AccumulatedMouseMotion>().delta,
            Vec2::ZERO,
            "no motion since"
        );
    }

    /// What the plugin and the control pipe ask of other systems comes out
    /// of the drain as Bevy's messages, a layer's picture with its pixels.
    #[test]
    fn requests_come_out_as_messages() {
        let (glue, events) = Glue::fed();
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, bevy::state::app::StatesPlugin))
            .insert_resource(glue)
            .insert_resource(Wake::default())
            .init_resource::<Stats>()
            .add_plugins(plugin)
            .add_message::<KeyboardInput>()
            .add_message::<MouseButtonInput>()
            .add_message::<MouseMotion>()
            .add_message::<KeyboardFocusLost>();

        let layer = Layer2d {
            level: Level::Top,
            rect: Rect::new(0.0, 0.0, 2.0, 1.0),
            namespace: "bar".to_string(),
        };
        let picture = Picture {
            layer: 0x1,
            size: UVec2::new(2, 1),
            pixels: vec![255; 8],
        };
        for event in [
            Event::Enter,
            Event::Leave,
            Event::Quit,
            Event::Screenshot(PathBuf::from("/tmp/x.png")),
        ] {
            events.send(event).expect("the app's end");
        }
        events
            .send(Event::Layer(layer, picture))
            .expect("the app's end");
        app.update();

        let world = app.world();
        assert_eq!(world.resource::<Messages<Enter>>().len(), 1);
        assert_eq!(world.resource::<Messages<Leave>>().len(), 1);
        assert_eq!(world.resource::<Messages<Quit>>().len(), 1);
        assert_eq!(world.resource::<Messages<TakeScreenshot>>().len(), 1);
        let pictures = world.resource::<Messages<Picture>>();
        assert_eq!(pictures.len(), 1);
        let changes = world.resource::<Messages<DesktopChange>>();
        assert!(
            changes
                .iter_current_update_messages()
                .any(|c| matches!(c, DesktopChange::Layer(0x1, _))),
            "the layer, for layers.rs"
        );
    }

    /// A batch of changes comes to one outcome a window, by the last word on
    /// it, and a reset is the end of every window not said again after it.
    #[test]
    fn a_batch_of_changes_comes_to_the_last_word_on_each() {
        let window = |workspace| Window2d {
            workspace,
            rect: Rect::new(0.0, 0.0, 1.0, 1.0),
            stack: 1,
            kind: Kind::Tiled,
        };
        // A window's workspace stands for what it is now.
        let windows = |change: &DesktopChange| match change {
            DesktopChange::Window(address, window) => Some((*address, Some(window.workspace))),
            DesktopChange::Closed(address) => Some((*address, None)),
            _ => None,
        };
        let mut world = World::new();
        let (a, b) = (world.spawn_empty().id(), world.spawn_empty().id());
        let index = HashMap::from([(0xa, a), (0xb, b)]);
        let fold = |changes: &[DesktopChange]| {
            let mut said = DesktopChange::fold(changes, &index, windows);
            said.sort_by_key(|&(address, _)| address);
            said
        };

        let changes = [
            DesktopChange::Window(0xa, window(1)),
            DesktopChange::Window(0xa, window(2)),
            DesktopChange::Closed(0xb),
            DesktopChange::Window(0xc, window(1)),
            DesktopChange::Window(0xd, window(1)),
            DesktopChange::Closed(0xd),
        ];
        assert_eq!(
            fold(&changes),
            [
                (0xa, Said::Still(a, 2)),
                (0xb, Said::Gone(b)),
                (0xc, Said::Came(1))
            ],
            "the last word on each, and nothing of one that came and went"
        );

        let changes = [
            DesktopChange::Window(0xb, window(1)),
            DesktopChange::Reset,
            DesktopChange::Window(0xa, window(3)),
        ];
        assert_eq!(
            fold(&changes),
            [(0xa, Said::Still(a, 3)), (0xb, Said::Gone(b))],
            "what came before a reset is moot, and what is not said again after it is gone"
        );
    }

    /// The control pipe takes the plugin's input lines and its own few, and
    /// nothing that would make up a desktop.
    #[test]
    fn the_control_pipe_takes_input_settings_screenshots_and_quit() {
        assert!(matches!(
            control_line("key 17 1"),
            Some(Event::Key {
                code: 17,
                pressed: true
            })
        ));
        assert!(
            matches!(control_line("motion 16 -4 16 -4"), Some(Event::Motion(m)) if m == Vec2::new(16.0, -4.0))
        );
        assert!(matches!(
            control_line("button 272 1"),
            Some(Event::Button {
                code: 272,
                pressed: true
            })
        ));
        assert!(matches!(
            control_line("screenshot /tmp/a b.png"),
            Some(Event::Screenshot(path)) if path == Path::new("/tmp/a b.png")
        ));
        assert!(matches!(control_line("quit"), Some(Event::Quit)));
        assert!(matches!(
            control_line("set player.flying true"),
            Some(Event::Setting(key, value)) if key == "player.flying" && value == "true"
        ));

        for refused in [
            "shown",
            "window 0x1 1 0 0 10 10 0 t",
            "screenshot",
            "jump",
            "set",
            "settings {}",
        ] {
            assert!(control_line(refused).is_none(), "{refused:?}");
        }
    }

    /// The plugin's settings come whole, and the control pipe's `set` goes
    /// into them at its dotted path, as JSON where it is JSON.
    #[test]
    fn settings_come_whole_and_set_goes_in_at_its_path() {
        let Some(Event::Settings(value)) =
            parse(r#"settings {"panorama":"~/a b.jpg","player":{"move_speed":6}}"#)
        else {
            panic!("no settings");
        };
        let mut settings = WorldSettings(value);
        assert!(parse("settings {").is_none(), "not JSON");

        settings.set("player.flying", "true");
        settings.set("room_size.x", "16");
        settings.set("sound", "~/swoosh.wav");
        settings.set("panorama", "");
        assert_eq!(
            settings.0,
            serde_json::json!({
                "panorama": "",
                "player": { "move_speed": 6, "flying": true },
                "room_size": { "x": 16 },
                "sound": "~/swoosh.wav",
            })
        );

        // A table over a value.
        settings.set("sound.x", "1");
        assert_eq!(settings.0["sound"], serde_json::json!({ "x": 1 }));
    }

    /// As the plugin sends them: plain lines, and a layer line with its
    /// pixels in a file on its first byte.
    #[test]
    fn a_layer_line_brings_its_pixels() {
        let (app, plugin) = socketpair(
            AddressFamily::UNIX,
            SocketType::STREAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .unwrap();

        rustix::io::write(&plugin, b"ws 1 one\n").unwrap();

        let pixels = [1, 2, 3, 4, 5, 6, 7, 8];
        let file = memfd_create("layer", MemfdFlags::CLOEXEC).unwrap();
        rustix::io::write(&file, &pixels).unwrap();
        let files = [file.as_fd()];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&files)));
        let line = b"layer 0x2a 2 0 0 1440 54 2 1 noctalia-bar-default\n";
        sendmsg(
            &plugin,
            &[IoSlice::new(line)],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        drop(file);

        rustix::io::write(&plugin, b"layer-gone 0x2a\n").unwrap();
        drop(plugin);

        let mut lines = Lines::new(UnixStream::from(app));
        assert_eq!(lines.next().as_deref(), Some("ws 1 one"));

        let line = lines.next().unwrap();
        let Some(Event::Layer(shown, picture)) = layer(&line, lines.files.pop_front()) else {
            panic!("no layer in {line:?}");
        };
        assert_eq!(shown.level, Level::Top);
        assert_eq!(shown.rect, Rect::new(0.0, 0.0, 1440.0, 54.0));
        assert_eq!(shown.namespace, "noctalia-bar-default");
        assert_eq!((picture.layer, picture.size), (0x2a, UVec2::new(2, 1)));
        assert_eq!(picture.pixels, pixels);

        assert!(matches!(
            parse(&lines.next().unwrap()),
            Some(Event::Desktop(DesktopChange::LayerGone(0x2a)))
        ));
        assert!(lines.next().is_none(), "the plugin hung up");
    }

    /// A layer line whose file went missing, or is too short, is dropped.
    #[test]
    fn a_layer_line_without_its_pixels_is_dropped() {
        let line = "layer 0x2a 0 0 0 1440 960 2880 1920 noctalia-wallpaper";
        assert!(layer(line, None).is_none());

        let short = memfd_create("layer", MemfdFlags::CLOEXEC).unwrap();
        rustix::io::write(&short, &[0; 16]).unwrap();
        assert!(layer(line, Some(short)).is_none());
    }

    /// A window's own buffer comes as its line says, every plane on the one
    /// dmabuf that came with it, and a line that does not add up is not
    /// taken. The line names which of the window's surfaces it is.
    #[test]
    fn a_windows_own_buffer_takes_its_dmabuf() {
        let file = || memfd_create("dmabuf", MemfdFlags::CLOEXEC).unwrap();
        let line = "buffer 0x2a 0 7 640 480 0x34325258 0x20000001046bb04 2 3 600 470 2 0 2560 1310720 1024";
        let Some(Event::Buffer(Shared {
            window,
            surface,
            alpha,
            buffer:
                SharedBuffer::Dmabuf {
                    id,
                    crop,
                    dmabuf: b,
                },
        })) = buffer(line, Some(file()))
        else {
            panic!("not taken")
        };
        assert_eq!(
            (window, surface, id, crop),
            (0x2a, 0, 7, URect::new(2, 3, 602, 473))
        );
        assert!(!alpha, "XRGB8888");
        assert_eq!(
            (b.width, b.height, b.format, b.modifier),
            (
                640,
                480,
                import::BufferFormat::Rgb(crate::capture::PixelLayout::Bgra),
                0x20000001046bb04
            )
        );
        assert_eq!(
            b.planes
                .iter()
                .map(|p| (p.offset, p.stride))
                .collect::<Vec<_>>(),
            [(0, 2560), (1310720, 1024)]
        );
        let inode = |plane: &import::DmabufPlane| rustix::fs::fstat(&plane.fd).unwrap().st_ino;
        assert_eq!(inode(&b.planes[0]), inode(&b.planes[1]), "one dmabuf");

        for broken in [
            // A plane short of what it says.
            "buffer 0x2a 0 7 640 480 0x34325258 0x0 0 0 640 480 2 0 2560",
            // NV12 in one plane, where it has two.
            "buffer 0x2a 0 7 640 480 0x3231564e 0x0 0 0 640 480 1 0 2560",
            // YUYV, which the app does not sample.
            "buffer 0x2a 0 7 640 480 0x56595559 0x0 0 0 640 480 1 0 2560",
            // Nothing of the buffer is the window.
            "buffer 0x2a 0 7 640 480 0x34325258 0x0 0 0 0 480 1 0 2560",
            // No surface id.
            "buffer 0x2a 9 240 160 0x34325241 0x0 0 0 240 160 1 0 960",
        ] {
            assert!(buffer(broken, Some(file())).is_none(), "{broken}");
        }
        assert!(buffer(line, None).is_none(), "its dmabuf went missing");

        // A popup's, ARGB8888.
        let line = "buffer 0x2a 3 9 240 160 0x34325241 0x0 0 0 240 160 1 0 960";
        let Some(Event::Buffer(Shared {
            window,
            surface,
            alpha,
            buffer: SharedBuffer::Dmabuf { id, crop, .. },
        })) = buffer(line, Some(file()))
        else {
            panic!("not taken")
        };
        assert_eq!(
            (window, surface, id, crop),
            (0x2a, 3, 9, URect::new(0, 0, 240, 160))
        );
        assert!(alpha, "ARGB8888");

        // A video subsurface's, NV12: its luma, then its chroma after it in
        // the one dmabuf.
        let line = "buffer 0x2a 4 11 320 240 0x3231564e 0x0 0 0 320 240 2 0 512 122880 512";
        let Some(Event::Buffer(Shared {
            surface,
            alpha,
            buffer: SharedBuffer::Dmabuf { dmabuf, .. },
            ..
        })) = buffer(line, Some(file()))
        else {
            panic!("NV12 not taken")
        };
        assert_eq!(
            (surface, dmabuf.format),
            (4, import::BufferFormat::Yuv(import::Yuv::Nv12))
        );
        assert_eq!(
            dmabuf
                .planes
                .iter()
                .map(|p| (p.offset, p.stride))
                .collect::<Vec<_>>(),
            [(0, 512), (122880, 512)]
        );
        assert!(!alpha, "YUV has no alpha");
    }

    #[test]
    fn a_single_pixel_comes_in_its_line() {
        // Half-transparent red, premultiplied: ARGB 0x80800000.
        let Some(Event::Buffer(Shared {
            window,
            surface,
            alpha,
            buffer: SharedBuffer::Pixels { frame, copy },
        })) = parse("pixel 0x2a 3 80800000")
        else {
            panic!("not taken")
        };
        assert_eq!(
            (window, surface, alpha, copy),
            (0x2a, 3, true, None),
            "nothing copied, nothing to hand back"
        );
        assert_eq!(
            (frame.width, frame.height, frame.layout),
            (1, 1, PixelLayout::Bgra)
        );
        assert_eq!(
            frame.pixels,
            [0x00, 0x00, 0x80, 0x80],
            "blue, green, red, alpha in memory"
        );

        let Some(Event::Buffer(Shared { alpha, .. })) = parse("pixel 0x2a 0 ff102030") else {
            panic!("not taken")
        };
        assert!(!alpha, "opaque");
        for broken in [
            "pixel 0x2a 3",
            "pixel 0x2a 3 80800000 1",
            "pixel 0x2a x 80800000",
            "pixel 0x2a 3 red",
        ] {
            assert!(parse(broken).is_none(), "{broken}");
        }
    }

    /// A surface's buffer in shared memory comes as the part of its pool the
    /// line says, copied out into a packed frame, top row first. A line that
    /// does not add up, or a part past the pool's end, is not taken, and its
    /// id comes back either way, for the plugin to be told `done`.
    #[test]
    fn a_buffer_in_shared_memory_is_copied_out_of_its_pool() {
        // A pool with something before the buffer, and rows of 6 pixels in
        // 32 bytes, each pixel its column, its row, and two markers.
        let (width, height, stride, offset) = (6u32, 4u32, 32u32, 64u32);
        let mut pool = vec![0xee; (offset + height * stride) as usize];
        for y in 0..height {
            for x in 0..width {
                let at = (offset + y * stride + x * 4) as usize;
                pool[at..at + 4].copy_from_slice(&[x as u8, y as u8, 0x5a, 0xff]);
            }
        }
        let file = || {
            let file = memfd_create("pool", MemfdFlags::CLOEXEC).unwrap();
            rustix::io::write(&file, &pool).unwrap();
            file
        };

        // Columns 1 to 4 of rows 1 and 2.
        let line = format!("shm 0x2a 2 11 {width} {height} 0x34325241 1 1 4 2 {offset} {stride}");
        let (id, event) = shm(&line, Some(file()));
        assert_eq!(id, Some(11));
        let Some(Event::Buffer(Shared {
            window,
            surface,
            alpha,
            buffer: SharedBuffer::Pixels { frame, .. },
        })) = event
        else {
            panic!("not taken")
        };
        assert_eq!((window, surface, alpha), (0x2a, 2, true));
        assert_eq!(
            (frame.width, frame.height, frame.layout),
            (4, 2, crate::capture::PixelLayout::Bgra)
        );
        let expected: Vec<u8> = (1..3)
            .flat_map(|y| (1..5).flat_map(move |x| [x, y, 0x5a, 0xff]))
            .collect();
        assert_eq!(frame.pixels, expected, "packed, top row first");

        for broken in [
            // Rows longer than the pool has room for.
            format!("shm 0x2a 2 12 {width} {height} 0x34325241 0 0 6 4 {offset} 64"),
            // Past the buffer's right edge.
            format!("shm 0x2a 2 13 {width} {height} 0x34325241 3 0 4 4 {offset} {stride}"),
            // NV12, which the app does not take.
            format!("shm 0x2a 2 14 {width} {height} 0x3231564e 0 0 6 4 {offset} {stride}"),
            // Terabytes, refused before anything is made room for.
            "shm 0x2a 2 15 1048576 1048576 0x34325241 0 0 1048576 1048576 0 4194304".to_string(),
        ] {
            let (id, event) = shm(&broken, Some(file()));
            assert!(event.is_none(), "{broken}");
            assert!(id.is_some(), "{broken}: handed back all the same");
        }
        assert!(shm(&line, None).1.is_none(), "its pool went missing");
    }

    /// A shared window's surfaces past its own come in their order with
    /// their boxes, none when it is one surface again, and a line that does
    /// not add up is not taken.
    #[test]
    fn a_windows_surfaces_come_in_order() {
        let Some(Event::Surfaces(said)) = parse("surfaces 0x2a 1 40 40 240 140 4 500 340 240 160")
        else {
            panic!("not taken")
        };
        assert_eq!(
            said,
            Surfaces {
                window: 0x2a,
                surfaces: vec![
                    (1, Rect::new(40.0, 40.0, 280.0, 180.0)),
                    (4, Rect::new(500.0, 340.0, 740.0, 500.0))
                ],
            }
        );
        let Some(Event::Surfaces(said)) = parse("surfaces 0x2a") else {
            panic!("not taken")
        };
        assert_eq!(
            said,
            Surfaces {
                window: 0x2a,
                surfaces: vec![]
            },
            "one surface again"
        );
        for broken in [
            "surfaces 0x2a 1 40 40 240",
            "surfaces 0x2a 1 40 40 240 x",
            "surfaces",
        ] {
            assert!(parse(broken).is_none(), "{broken}");
        }
    }
}
