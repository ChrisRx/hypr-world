//! hypr-world: every workspace on the walls of a house, its windows
//! captured live and hung where they are in 2D. It runs as an ordinary
//! Wayland client in its own process. Window contents come from Hyprland's
//! hyprland-toplevel-export-v1 protocol, and the hypr-world plugin feeds it
//! the desktop and hands it the keyboard and mouse.
//!
//!   hypr-world
//!
//! Click to take the mouse and keyboard, then WASD to move, Space/Shift for
//! up/down, mouse to look. Q or Escape quits. Super+Escape gives the mouse
//! and keyboard back without quitting.
//!
//!   hypr-world --standalone
//!
//! runs with no plugin at all, for working on the app (`cargo world`): the
//! desktop comes from Hyprland's IPC, the windows through capture alone, and
//! the app opens fullscreen and takes the mouse itself. There are no layers,
//! and the windows on the walls cannot be used.
//!
//!   hypr-world --backend [instance]
//!
//! runs against a nested Hyprland with the plugin loaded in it, its instance
//! signature, else the one plugin/nested.sh started (`plugin/nested.sh
//! backend`, `cargo nested`): the plugin feeds the app from there, the app
//! opens fullscreen here and captures from there, and while it has the mouse,
//! its keys and clicks go into the nested Hyprland, where the windows on the
//! walls are (glue/backend.rs).
//!
//!   hypr-world --probe [window]
//!
//! captures a few frames of one window (the focused one, the first whose
//! class or title contains `window`, or the one at that address) without
//! opening the scene, and prints their size and timing, to check the capture
//! side on its own.

mod camera;
mod capture;
mod config;
mod controls;
mod flight;
mod font8x8;
mod glue;
mod house;
mod import;
mod launcher;
mod layers;
mod layout;
mod materials;
mod player;
mod redraw;
mod scene;
mod sky;
mod sound;
mod stats;
mod wake;
mod windows;

use std::{process::Command, time::Duration};

use avian3d::prelude::{PhysicsPlugins, PhysicsSystems};
use bevy::{
    asset::{AssetMetaCheck, UnapprovedPathMode},
    input::InputSystems,
    prelude::*,
    render::diagnostic::RenderDiagnosticsPlugin,
    window::{MonitorSelection, WindowMode},
};

// The camera, which nearly every module looks for.
pub use camera::FlyCamera;

/// The app's update, in order. Each module's plugin
/// puts its systems in one, and chains only those that need another's writes
/// in the same update. The step is avian's physics, between `Player` and
/// `Camera`.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorldSystems {
    /// The plugin's lines, read into the app's resources, and its keys and
    /// mouse into Bevy's input (glue.rs). In `PreUpdate`, before
    /// `InputSystems`, so they count in this update.
    Feed,
    /// The house laid out for the desktop, and its sky (scene.rs, sky.rs).
    House,
    /// The windows' and layers' quads on the walls (windows.rs, layers.rs).
    Walls,
    /// The windows' new frames into their textures (windows.rs).
    Capture,
    /// The app's own keys and settings (controls.rs, camera.rs).
    Input,
    /// The flight in and out (flight.rs).
    Flight,
    /// The player's controls, for the step (player.rs).
    Player,
    /// The camera on the body where the step left it (player.rs).
    Camera,
    /// What the crosshair is on, the dwell, and the hint (windows.rs, glue.rs).
    Aim,
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--probe") {
        args.remove(0);
        probe(args.first().map(String::as_str));
        return;
    }
    let standalone = args.first().is_some_and(|a| a == "--standalone");
    let backend = if args.first().is_some_and(|a| a == "--backend") {
        match glue::Backend::find(args.get(1).map(String::as_str)) {
            Ok(backend) => Some(backend),
            Err(e) => {
                eprintln!("hypr-world: {e}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };
    let fullscreen = standalone || backend.is_some();

    // The threads, which share the wake. The glue thread hands the plugin's
    // damage straight to the capture thread, which starts the copies itself.
    let wake = wake::Wake::default();
    let captures = capture::Capture::start(
        wake.clone(),
        backend.as_ref().map(|backend| backend.display.clone()),
    );
    let glue = if let Some(backend) = &backend {
        glue::Glue::backend(wake.clone(), captures.damage(), backend)
    } else if standalone {
        glue::Glue::standalone(wake.clone(), captures.damage())
    } else {
        glue::Glue::start(wake.clone(), captures.damage())
    };

    App::new()
        .add_plugins(
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "hypr-world".into(),
                        // The Wayland app id, for window rules and `hyprctl clients`.
                        name: Some("hypr-world".into()),
                        // Standalone, Hyprland places it as any window: fullscreen,
                        // it leaves the tiled windows it shows where they are.
                        mode: if fullscreen {
                            WindowMode::BorderlessFullscreen(MonitorSelection::Current)
                        } else {
                            WindowMode::Windowed
                        },
                        ..default()
                    }),
                    ..default()
                })
                // The panorama is wherever the settings say, outside any asset
                // folder: a load may read there when it says so (sky.rs). No
                // file of the app's has a `.meta` beside it.
                .set(AssetPlugin {
                    unapproved_path_mode: UnapprovedPathMode::Deny,
                    meta_check: AssetMetaCheck::Never,
                    ..default()
                }),
        )
        // A physics step every rendered frame: smooth at any refresh rate,
        // with nothing to interpolate.
        .add_plugins(PhysicsPlugins::new(Update))
        // GPU time per render pass, for the stats.
        .add_plugins(RenderDiagnosticsPlugin)
        // A frame after a pause covers at most a 30th of a second, so moving
        // again after one does not jump.
        .insert_resource(Time::<Virtual>::from_max_delta(Duration::from_secs_f32(
            1.0 / 30.0,
        )))
        .insert_resource(wake)
        .insert_resource(windows::Captures(captures))
        .insert_resource(glue)
        .configure_sets(PreUpdate, WorldSystems::Feed.before(InputSystems))
        .configure_sets(
            Update,
            (
                WorldSystems::House,
                WorldSystems::Walls,
                WorldSystems::Capture,
                WorldSystems::Input,
                WorldSystems::Flight,
                WorldSystems::Player,
            )
                .chain()
                .before(PhysicsSystems::First),
        )
        .configure_sets(
            Update,
            (WorldSystems::Camera, WorldSystems::Aim)
                .chain()
                .after(PhysicsSystems::Last),
        )
        // The world, a module each.
        .add_plugins((
            wake::plugin,
            glue::plugin,
            config::plugin,
            camera::plugin,
            controls::plugin,
            scene::plugin,
            sky::plugin,
            windows::plugin,
            layers::plugin,
            flight::plugin,
            player::plugin,
            launcher::plugin,
            sound::plugin,
        ))
        // Drawing: the house's material, the windows' frames on the GPU,
        // drawing only on change, and the stats.
        .add_plugins((
            materials::plugin,
            import::plugin,
            redraw::plugin,
            stats::plugin,
        ))
        .add_plugins(remote)
        .run();
}

/// With `--features remote`, the running world over JSON-RPC on
/// 127.0.0.1:15702 (Bevy's remote protocol): the reflected components and
/// resources, read and written live.
#[cfg(feature = "remote")]
fn remote(app: &mut App) {
    app.add_plugins((
        bevy::remote::RemotePlugin::default(),
        bevy::remote::http::RemoteHttpPlugin::default(),
    ));
}

#[cfg(not(feature = "remote"))]
fn remote(_: &mut App) {}

/// --probe: a few frames of one window, their sizes and timing.
fn probe(arg: Option<&str>) {
    let (address, label) = match pick_window(arg) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("hypr-world: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("hypr-world: capturing {label} ({address:#x})");

    // No event loop to wake: it waits on the frames itself.
    let capture = capture::Capture::start(wake::Wake::default(), None);
    let handle = capture::export_handle(address);
    capture.request(handle);

    let start = std::time::Instant::now();
    for i in 0..10 {
        let Some((_, frame)) = capture.wait() else {
            std::process::exit(1)
        };
        capture.request(handle);

        // The pixels stay on the GPU, so only the buffer is reported.
        println!(
            "frame {i}: dmabuf {}x{} {:?} slot {} stride {} modifier {:#x}{}, at {:?}",
            frame.width,
            frame.height,
            frame.layout,
            frame.slot.id,
            frame.slot.stride,
            frame.slot.modifier,
            if frame.y_invert { " y-inverted" } else { "" },
            start.elapsed()
        );
    }
}

/// The window to probe, as (address, description). The address is what
/// `hyprctl clients` prints; its low 32 bits are the export handle.
fn pick_window(arg: Option<&str>) -> Result<(u64, String), String> {
    let json = |args: &[&str]| -> Result<serde_json::Value, String> {
        let out = Command::new("hyprctl")
            .args(args)
            .output()
            .map_err(|e| format!("running hyprctl: {e}"))?;
        serde_json::from_slice(&out.stdout).map_err(|e| format!("parsing hyprctl output: {e}"))
    };

    let describe = |w: &serde_json::Value| -> Option<(u64, String)> {
        let address = w["address"].as_str()?;
        let raw = u64::from_str_radix(address.trim_start_matches("0x"), 16).ok()?;
        let label = format!(
            "{} \"{}\"",
            w["class"].as_str().unwrap_or("?"),
            w["title"].as_str().unwrap_or("")
        );
        Some((raw, label))
    };

    let Some(arg) = arg else {
        let active = json(&["activewindow", "-j"])?;
        return describe(&active).ok_or_else(|| "no focused window".into());
    };

    let clients = json(&["clients", "-j"])?;
    let needle = arg.to_lowercase();

    clients
        .as_array()
        .into_iter()
        .flatten()
        .find(|w| {
            let field = |k: &str| w[k].as_str().unwrap_or("").to_lowercase();
            field("address") == needle
                || field("class").contains(&needle)
                || field("title").contains(&needle)
        })
        .and_then(describe)
        .ok_or_else(|| format!("no window matches \"{arg}\""))
}
