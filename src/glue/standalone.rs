//! The desktop without the plugin (`hypr-world --standalone`), from
//! Hyprland's own IPC socket: its monitors, workspaces and windows, polled
//! and told as the plugin's feed tells them. There are no layers (the bar's
//! pixels come only from the plugin), no shared buffers and no input: the
//! windows come in through capture alone, and the app takes the mouse
//! through Wayland (controls.rs). With no plugin to say which windows drew
//! something, every window is said to have, each poll, and the capture
//! thread copies those it wants at their pace.

use std::{
    collections::{BTreeMap, HashMap},
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::mpsc::Sender,
    thread,
    time::Duration,
};

use bevy::prelude::*;
use serde_json::Value;

use super::{DesktopChange, Event, Kind, Monitor, Window2d, rect};
use crate::{capture::Damage, wake::Wake};

/// How often Hyprland is asked, and so the most a second any window is
/// copied.
const POLL: Duration = Duration::from_millis(33);

/// $XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock
fn socket_path() -> Option<PathBuf> {
    Some(super::instance_dir()?.join(".socket.sock"))
}

/// One request (`j/clients` and the like), its JSON reply.
pub(super) fn ask(path: &Path, request: &str) -> Option<Value> {
    let mut stream = UnixStream::connect(path).ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).ok()?;
    serde_json::from_slice(&reply).ok()
}

fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key)?.as_f64()
}

/// `[x, y]` as Hyprland writes a position or a size.
fn pair(value: &Value, key: &str) -> Option<Vec2> {
    let pair = value.get(key)?.as_array()?;
    Some(Vec2::new(
        pair.first()?.as_f64()? as f32,
        pair.get(1)?.as_f64()? as f32,
    ))
}

/// What the plugin's feed would say of the desktop: the focused monitor,
/// its active workspace, and its workspaces and windows, by id and address.
#[derive(Default, PartialEq)]
struct Seen {
    monitor: Option<Monitor>,
    active: Option<i64>,
    workspaces: BTreeMap<i64, String>,
    windows: HashMap<u64, Window2d>,
}

fn look(path: &Path) -> Option<Seen> {
    let monitors = ask(path, "j/monitors")?;
    let monitor = monitors
        .as_array()?
        .iter()
        .find(|m| m.get("focused").and_then(Value::as_bool) == Some(true))?;
    let id = monitor.get("id")?.as_i64()?;
    let scale = number(monitor, "scale")? as f32;
    let mut size = Vec2::new(
        number(monitor, "width")? as f32,
        number(monitor, "height")? as f32,
    ) / scale;
    // Turned a quarter either way, its width is its height.
    if number(monitor, "transform").is_some_and(|t| t as i64 % 2 == 1) {
        size = Vec2::new(size.y, size.x);
    }
    let origin = Vec2::new(number(monitor, "x")? as f32, number(monitor, "y")? as f32);
    // Left, top, right, bottom.
    let reserved: Vec<f64> = monitor
        .get("reserved")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();
    let edge = |i: usize| reserved.get(i).copied().unwrap_or(0.0);
    let work = rect(
        edge(0),
        edge(1),
        f64::from(size.x) - edge(0) - edge(2),
        f64::from(size.y) - edge(1) - edge(3),
    );

    let workspaces = ask(path, "j/workspaces")?
        .as_array()?
        .iter()
        .filter(|w| w.get("monitorID").and_then(Value::as_i64) == Some(id))
        .filter_map(|w| {
            let id = w.get("id")?.as_i64()?;
            // Special workspaces have ids below 0, and are not the house's.
            let name = w.get("name")?.as_str()?.to_string();
            (id > 0).then_some((id, name))
        })
        .collect();

    let own = i64::from(std::process::id());
    let windows = ask(path, "j/clients")?
        .as_array()?
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let workspace = c.get("workspace")?.get("id")?.as_i64()?;
            let shown = c.get("mapped")?.as_bool()? && !c.get("hidden")?.as_bool()?;
            let mine = c.get("pid")?.as_i64()? == own;
            if !shown || mine || workspace <= 0 || c.get("monitor")?.as_i64()? != id {
                return None;
            }
            let at = pair(c, "at")? - origin;
            let size = pair(c, "size")?;
            let kind = if c.get("fullscreen")?.as_i64().unwrap_or(0) > 0 {
                Kind::Fullscreen
            } else if c.get("floating")?.as_bool()? {
                Kind::Floating
            } else {
                Kind::Tiled
            };
            let address = super::address(c.get("address")?.as_str()?)?;
            Some((
                address,
                Window2d {
                    workspace,
                    rect: Rect::from_corners(at, at + size),
                    // The place in Hyprland's window list, as the plugin's.
                    stack: i as u32 + 1,
                    kind,
                },
            ))
        })
        .collect();

    Some(Seen {
        monitor: Some(Monitor { size, scale, work }),
        active: monitor.get("activeWorkspace")?.get("id")?.as_i64(),
        workspaces,
        windows,
    })
}

/// The events from `before` to `now`, in the order the plugin sends them.
fn changes(before: &Seen, now: &Seen) -> Vec<Event> {
    let mut events = Vec::new();
    if now.monitor != before.monitor
        && let Some(monitor) = now.monitor
    {
        events.push(Event::Monitor(monitor));
    }
    for (&id, name) in &now.workspaces {
        if before.workspaces.get(&id) != Some(name) {
            events.push(Event::Desktop(DesktopChange::Workspace(id, name.clone())));
        }
    }
    for &id in before.workspaces.keys() {
        if !now.workspaces.contains_key(&id) {
            events.push(Event::Desktop(DesktopChange::WorkspaceGone(id)));
        }
    }
    if now.active != before.active
        && let Some(id) = now.active
    {
        events.push(Event::Active(id));
    }
    for (&address, &window) in &now.windows {
        if before.windows.get(&address) != Some(&window) {
            events.push(Event::Desktop(DesktopChange::Window(address, window)));
        }
    }
    for &address in before.windows.keys() {
        if !now.windows.contains_key(&address) {
            events.push(Event::Desktop(DesktopChange::Closed(address)));
        }
    }
    events
}

pub(super) fn run(events: Sender<Event>, wake: Wake, damage: Damage) {
    let Some(path) = socket_path() else {
        eprintln!("hypr-world: not running under Hyprland, no desktop");
        return;
    };
    eprintln!("hypr-world: standalone, the desktop from Hyprland's IPC");

    // Always in view: there is no plugin to hide the app.
    if events.send(Event::Shown(true)).is_err() {
        return;
    }
    wake.wake();

    let mut seen = Seen::default();
    loop {
        if let Some(now) = look(&path) {
            let told = changes(&seen, &now);
            if !told.is_empty() {
                for event in told {
                    if events.send(event).is_err() {
                        return;
                    }
                }
                wake.wake();
            }
            let addresses: Vec<u64> = now.windows.keys().copied().collect();
            damage.report(&addresses);
            seen = now;
        }
        thread::sleep(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(workspace: i64, x: f32) -> Window2d {
        Window2d {
            workspace,
            rect: Rect::new(x, 0.0, x + 100.0, 100.0),
            stack: 1,
            kind: Kind::Tiled,
        }
    }

    fn verbs(events: &[Event]) -> Vec<&'static str> {
        events.iter().filter_map(Event::verb).collect()
    }

    #[test]
    fn says_only_what_changed() {
        let before = Seen {
            monitor: None,
            active: Some(1),
            workspaces: BTreeMap::from([(1, "1".into()), (2, "2".into())]),
            windows: HashMap::from([(0xa, window(1, 0.0)), (0xb, window(2, 0.0))]),
        };
        let now = Seen {
            monitor: Some(Monitor {
                size: Vec2::new(1440.0, 960.0),
                scale: 2.0,
                work: Rect::new(0.0, 40.0, 1440.0, 960.0),
            }),
            active: Some(1),
            workspaces: BTreeMap::from([(1, "1".into()), (3, "3".into())]),
            windows: HashMap::from([(0xa, window(1, 10.0)), (0xc, window(3, 0.0))]),
        };

        let mut said = verbs(&changes(&before, &now));
        said.sort();
        assert_eq!(
            said,
            ["closed", "monitor", "window", "window", "ws", "ws-gone"]
        );
        assert!(changes(&now, &now).is_empty(), "nothing again");
    }
}
