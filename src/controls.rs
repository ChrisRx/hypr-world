//! The app's own keys: taking the mouse, quitting, and the control pipe's
//! screenshots (glue.rs `control`).

use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, save_to_disk},
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};

use crate::{
    WorldSystems,
    glue::{Glue, InputMode, Quit, TakeScreenshot},
};

pub fn plugin(app: &mut App) {
    app.add_systems(
        Update,
        (grab_mouse, quit_keys, take_screenshots).in_set(WorldSystems::Input),
    );
}

/// Taking the mouse. With the hypr-world plugin, a click into the app asks
/// it for the keyboard and mouse, and Super+Escape gives them back. Without
/// it, the app grabs the pointer through Wayland, and the windows on the
/// walls cannot be used.
fn grab_mouse(
    glue: Res<Glue>,
    mode: Res<State<InputMode>>,
    mut cursor: Single<&mut CursorOptions, With<PrimaryWindow>>,
    mouse: Res<ButtonInput<MouseButton>>,
) {
    if glue.connected {
        if *mode.get() == InputMode::Off && mouse.just_pressed(MouseButton::Left) {
            glue.send("grab");
        }
        return;
    }

    if mouse.just_pressed(MouseButton::Left) {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
}

/// Q or Escape quits, from the plugin or the window alike (both are Bevy's
/// input), and so does `quit` on the control pipe. Quitting closes the glue
/// socket, and the plugin gives the mouse and keyboard back to Hyprland the
/// moment it sees that.
fn quit_keys(
    glue: Res<Glue>,
    keys: Res<ButtonInput<KeyCode>>,
    mut quit: MessageReader<Quit>,
    mut exit: MessageWriter<AppExit>,
) {
    let asked = quit.read().count() > 0;
    if keys.any_just_pressed([KeyCode::Escape, KeyCode::KeyQ]) || asked {
        glue.send("release");
        exit.write(AppExit::Success);
    }
}

/// The screenshots the control pipe asked for, each of the next frame drawn
/// (redraw.rs draws one for it), saved as a PNG at its path.
fn take_screenshots(mut commands: Commands, mut asked: MessageReader<TakeScreenshot>) {
    for TakeScreenshot(path) in asked.read() {
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()));
    }
}
