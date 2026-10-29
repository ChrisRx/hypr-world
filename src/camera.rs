//! The camera: the player's eyes, a child of the body, which player.rs
//! turns and carries. Window pixels reach the screen as they are, as in 2D,
//! and the `msaa` setting smooths the house's edges.

use bevy::{
    prelude::*,
    render::view::{DebandDither, Msaa, Tonemapping},
};

use crate::{WorldSystems, config::WorldConfig, layout};

pub fn plugin(app: &mut App) {
    app.add_systems(
        Update,
        antialias
            .run_if(resource_changed::<WorldConfig>)
            .in_set(WorldSystems::Input),
    );
}

/// The camera, and where it looks, which player.rs eases toward.
#[derive(Component, Reflect, PartialEq)]
#[reflect(Component)]
pub struct FlyCamera {
    pub yaw: f32,
    pub pitch: f32,
}

/// The camera, which player.rs spawns on the body, at its eyes.
pub fn camera() -> impl Bundle {
    (
        Camera3d::default(),
        Projection::Perspective(PerspectiveProjection {
            fov: layout::FOV_Y,
            near: 0.05,
            ..default()
        }),
        Tonemapping::None,
        DebandDither::Disabled,
        Msaa::Sample2,
        FlyCamera {
            yaw: 0.0,
            pitch: 0.0,
        },
    )
}

/// The camera's rotation turned `yaw` about y, then `pitch` about x, then
/// rolled `roll` about the view (0 looks along -z).
pub fn rotation(yaw: f32, pitch: f32, roll: f32) -> Quat {
    Quat::from_euler(EulerRot::YXZ, yaw, pitch, roll)
}

/// The `msaa` setting (config.rs) on the camera.
fn antialias(config: Res<WorldConfig>, mut camera: Query<&mut Msaa, With<FlyCamera>>) {
    for mut current in &mut camera {
        current.set_if_neq(config.msaa);
    }
}
