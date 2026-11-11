//! The shadow realm: once in a hundred flights in, the world is not the
//! house but the ghost of it. The house is gone, felt as little as seen,
//! and its frames and their windows draw back from you and drift in a green
//! fel murk, with nothing around them but billows of fel light, each soon a
//! still of itself, slowly warping. Out there the desktop is out of reach.
//! It has a sound of its own (drone.rs). It lasts until the world is left,
//! all the way out, and the next flight in rolls again.
//!
//! `HYPR_WORLD_SHADOW` set sends every flight in there, and the app's start
//! too, for working on it with `cargo world`.

mod drone;

use std::f32::consts::{PI, TAU};

use avian3d::prelude::{Collider, ColliderDisabled, RigidBody};
use bevy::{
    asset::RenderAssetUsages,
    camera::{RenderTarget, ScalingMode},
    core_pipeline::tonemapping::{DebandDither, Tonemapping},
    light::Skybox,
    prelude::*,
    render::{
        render_resource::{Extent3d, TextureDimension, TextureFormat},
        view::Msaa,
    },
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};

use crate::{
    FlyCamera, WorldSystems,
    audio::{Play, Playing, Sound},
    flight::WorldState,
    glue::Desktop,
    layout::SCREEN_LIFT,
    materials::{WarpMaterial, WarpParams},
    scene::{self, House, Lit, Shell, Solid, Workspace},
    stats::Redraw,
    wake::Wake,
};

pub fn plugin(app: &mut App) {
    let forced = Forced(std::env::var_os("HYPR_WORLD_SHADOW").is_some());
    app.insert_resource(Shadow(forced.0))
        .insert_resource(forced)
        .init_resource::<Kept>()
        .init_resource::<Drones>();
    // Each flight in from the frame, the way flight.rs `take_off` starts one.
    for exited in [WorldState::Out, WorldState::In] {
        app.add_systems(
            OnTransition {
                exited,
                entered: WorldState::FlyingIn,
            },
            roll,
        );
    }
    app.add_systems(OnEnter(WorldState::Out), leave)
        .add_systems(
            Update,
            (
                (thaw, dress).run_if(resource_changed::<Shadow>),
                (
                    haunt,
                    drift,
                    gather,
                    billow,
                    freeze,
                    develop.run_if(in_state(WorldState::In)),
                    veil,
                    warp,
                    hum,
                    fade_in,
                )
                    .run_if(|shadow: Res<Shadow>| shadow.0),
            )
                .chain()
                .after(scene::relayout)
                .in_set(WorldSystems::House),
        );
}

/// One flight in of this many goes there.
const ODDS: u32 = 100;

/// The fel green: the murk, which glows and is also all there is behind
/// it, how thick it is, so the frames across the room are only just there
/// in it, and the light from high above, the ambient's tint and the
/// billows', with the ambient's and the light's levels, as a share of the
/// albedo (scene.rs has the house's).
const FEL_MURK: Color = Color::srgb(0.035, 0.16, 0.045);
const FEL_DENSITY: f32 = 0.13;
const FEL: Color = Color::linear_rgb(0.35, 1.0, 0.12);
const FEL_AMBIENT: Color = Color::linear_rgb(0.5, 1.1, 0.45);
const FEL_AMBIENT_LEVEL: f32 = 0.1;
const FEL_LIGHT_LEVEL: f32 = 0.4;
/// The billows of fel light about each frame, a glowing green fog: how
/// many, how bright each at its heart (added to what is behind it, so where
/// they overlap the fog is thicker), how far out around the frame they
/// gather, as a share of its size, how far behind it, in meters, their
/// size, and how far they roll about where they are. Faint, large and many,
/// so they run together. Behind the frame, never in front of it, so they
/// glow around it and not over it.
const BILLOWS: usize = 28;
const BILLOW_LEVEL: f32 = 0.12;
const BILLOW_AROUND: (f32, f32) = (0.2, 1.3);
const BILLOW_BEHIND: (f32, f32) = (1.0, 3.5);
const BILLOW_SIZE: (f32, f32) = (3.5, 8.0);
const BILLOW_ROLL: Vec3 = Vec3::new(0.8, 0.6, 0.4);
/// The billows' texture, a few of them so they differ, and how many px
/// across.
const PUFFS: u32 = 4;
const PUFF_PX: u32 = 128;

/// The frames: how far back they draw from where they hang, in
/// meters, and how small they get.
const GHOST_BACK: f32 = 3.0;
const GHOST_SCALE: f32 = 0.75;
/// The frames adrift: how far they wander, in meters along their
/// width, up and out, and in radians about their upright and their face;
/// the periods of each, in seconds; and how long drawing back and the
/// drift take to set in, so a flight in starts where the frame is and the
/// frames recede as it lands.
const DRIFT: Vec3 = Vec3::new(0.9, 0.7, 1.2);
const DRIFT_TURN: Vec2 = Vec2::new(0.18, 0.08);
const DRIFT_PERIODS: [f32; 5] = [13.0, 9.0, 11.0, 17.0, 15.0];
const DRIFT_SETTLE: f32 = 4.0;

/// The stills: how long after it comes they are taken, in the
/// world. Their cameras draw from the start, and need the time: one looking
/// straight on (orthographic) takes pipelines of its own, which Bevy
/// compiles in the background, drawing nothing with them until they are
/// ready. By then the flight has landed and the windows' frames are in.
/// Their size, of the monitor's, and how long the warp takes to set in,
/// from the still as it was.
const STILL_AT: f32 = 2.0;
const STILL_SCALE: f32 = 0.5;
const WARP_SETTLE: f32 = 3.0;

/// The sound's loudness, and how long it takes to come up.
const HUM_VOLUME: f32 = 0.5;
const HUM_FADE: f32 = 3.0;

/// Whether the world is the shadow realm, not the house.
#[derive(Resource, Default, PartialEq)]
pub struct Shadow(pub bool);

/// Whether the desktop is there to touch: the crosshair finds windows, and
/// rockets hit them, everywhere but in the realm, where they are stills.
pub fn desktop_in_reach(shadow: Res<Shadow>) -> bool {
    !shadow.0
}

/// `HYPR_WORLD_SHADOW`: always there.
#[derive(Resource, Default)]
struct Forced(bool);

/// The house's look while the realm stands in for it, put back after.
#[derive(Resource, Default)]
struct Kept {
    clear: Option<ClearColor>,
    ambient: Option<GlobalAmbientLight>,
    /// The panorama's brightness, which goes dark.
    sky: Option<f32>,
    /// When the realm came, by `Time`'s clock.
    since: f32,
}

/// What the realm hides: the house's shell and the workspaces' frames.
type Hidden = Or<(With<Shell>, With<Workspace>)>;

/// What `thaw` takes off the workspaces.
type Thawing = Or<(With<Frozen>, With<Billowing>)>;

/// A workspace with its billows about it.
#[derive(Component)]
struct Billowing;

/// A billow of fel light, on the void, about `anchor`'s frame: where in
/// the frame's coordinates, how big, and its own phase and turn, in
/// radians a second.
#[derive(Component)]
struct Billow {
    anchor: Entity,
    at: Vec3,
    size: f32,
    phase: f32,
    spin: f32,
}

/// The billows' quad and looks, made the first time they are needed.
struct Puffs {
    quad: Handle<Mesh>,
    looks: Vec<Handle<StandardMaterial>>,
}

/// The realm's sound, made the first time it is there, off the main
/// thread, and kept.
#[derive(Resource, Default)]
struct Drones(Option<Drone>);

enum Drone {
    Making(Task<Vec<u8>>),
    Made(Handle<Sound>),
}

/// The realm's sound playing, on the void, and when it started.
#[derive(Component)]
struct Hum {
    since: f32,
}

/// A workspace whose still is taken, or being.
#[derive(Component)]
struct Frozen;

/// A frozen workspace's live part, hidden behind its still.
#[derive(Component)]
struct Veiled;

/// A still, or the camera taking it.
#[derive(Component)]
struct Still;

/// The camera taking a still into `image`, of a frame's screen `size`.
#[derive(Component)]
struct Taking {
    image: Handle<Image>,
    size: Vec2,
}

/// A still up, warping since then.
#[derive(Component)]
struct Warping {
    since: f32,
}

/// What a still hides: all on its frame but the bezel, the name, the glow
/// and itself.
type Live = (Without<Lit>, Without<Still>, Without<Veiled>);

/// What the realm puts up, and takes down again.
type Haunting = Or<(With<Void>, With<Still>)>;

/// The realm's own things, the children of one entity.
#[derive(Component)]
#[require(Transform, Visibility)]
struct Void;

/// A flight in: once in `ODDS`, to the shadow realm.
fn roll(forced: Res<Forced>, mut shadow: ResMut<Shadow>) {
    shadow.set_if_neq(Shadow(forced.0 || fastrand::u32(..ODDS) == 0));
}

/// All the way out: the house again, for the next flight in to roll.
fn leave(mut shadow: ResMut<Shadow>) {
    shadow.set_if_neq(Shadow(false));
}

/// Into the realm or out of it: the void put up, or taken down and the
/// house's look put back.
#[allow(clippy::too_many_arguments)]
fn dress(
    mut commands: Commands,
    shadow: Res<Shadow>,
    mut kept: ResMut<Kept>,
    time: Res<Time>,
    voids: Query<Entity, Haunting>,
    camera: Single<Entity, With<FlyCamera>>,
    mut clear: ResMut<ClearColor>,
    mut ambient: ResMut<GlobalAmbientLight>,
    mut hidden: Query<&mut Visibility, Hidden>,
    house: Res<House>,
    mut workspaces: Query<(Entity, &Workspace, &mut Transform)>,
    solids: Query<Entity, (With<Solid>, With<ColliderDisabled>)>,
    mut sky: Query<&mut Skybox, With<FlyCamera>>,
    mut redraw: Redraw,
) {
    voids.iter().for_each(|e| commands.entity(e).despawn());
    commands.entity(*camera).remove::<DistanceFog>();
    let Kept {
        clear: was_clear,
        ambient: was_ambient,
        sky: was_sky,
        ..
    } = std::mem::take(&mut *kept);
    if let Some(was) = was_clear {
        *clear = was;
    }
    if let Some(was) = was_ambient {
        *ambient = was;
    }
    if let (Some(was), Ok(mut sky)) = (was_sky, sky.single_mut()) {
        sky.brightness = was;
    }
    for mut visibility in &mut hidden {
        visibility.set_if_neq(Visibility::Inherited);
    }
    // Back where they hang, after a drift.
    for (_, workspace, mut transform) in &mut workspaces {
        if let Some(home) = home(&house, workspace) {
            transform.set_if_neq(home);
        }
    }
    for solid in &solids {
        commands.entity(solid).remove::<ColliderDisabled>();
    }
    redraw.ask(["shadow"]);

    if !shadow.0 {
        return;
    }
    info!("into the shadow realm");

    kept.clear = Some(clear.clone());
    kept.ambient = Some(ambient.clone());
    kept.since = time.elapsed_secs();
    *clear = ClearColor(FEL_MURK);
    *ambient = GlobalAmbientLight {
        color: FEL_AMBIENT,
        brightness: FEL_AMBIENT_LEVEL
            / scene::AMBIENT_RESPONSE
            / bevy::camera::Exposure::default().exposure(),
        ..ambient.clone()
    };
    commands.entity(*camera).insert(DistanceFog {
        color: FEL_MURK,
        falloff: FogFalloff::Exponential {
            density: FEL_DENSITY,
        },
        ..default()
    });

    let void = commands.spawn(Void).id();
    // From high up and to one side, as from nothing.
    commands.spawn((
        ChildOf(void),
        DirectionalLight {
            color: FEL,
            illuminance: FEL_LIGHT_LEVEL * PI / bevy::camera::Exposure::default().exposure(),
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::default().looking_to(Vec3::new(0.3, -1.0, 0.45), Vec3::Y),
    ));
    // Something to stand on, though nothing is there to see, as far as
    // anyone walks, its top where the house's floor was.
    commands.spawn((
        ChildOf(void),
        RigidBody::Static,
        Collider::cuboid(1.0, 1.0, 1.0),
        Transform::from_xyz(0.0, -0.5, 0.0).with_scale(Vec3::new(2000.0, 1.0, 2000.0)),
    ));
}

/// Keeps the house out of the realm: hidden, and not in the way, though it
/// is built again (scene.rs `relayout`) or a panorama comes in.
fn haunt(
    mut commands: Commands,
    mut kept: ResMut<Kept>,
    mut shells: Query<&mut Visibility, With<Shell>>,
    solids: Query<Entity, (With<Solid>, Without<ColliderDisabled>)>,
    mut sky: Query<&mut Skybox, With<FlyCamera>>,
) {
    for mut visibility in &mut shells {
        visibility.set_if_neq(Visibility::Hidden);
    }
    for solid in &solids {
        commands.entity(solid).insert(ColliderDisabled);
    }
    if let Ok(mut sky) = sky.single_mut()
        && sky.brightness != 0.0
    {
        kept.sky = Some(sky.brightness);
        sky.brightness = 0.0;
    }
}

/// Where `workspace`'s frame hangs in the house as laid out, if it has one.
fn home(house: &House, workspace: &Workspace) -> Option<Transform> {
    let frame = house.layout.as_ref()?.frame_of(workspace.id)?;
    Some(Transform::from_isometry(frame.pose()))
}

/// The frames adrift about where they hang, each on its own slow
/// swell, setting in gently. The crosshair follows them where they are
/// (windows.rs `aim`).
fn drift(
    time: Res<Time>,
    kept: Res<Kept>,
    house: Res<House>,
    mut workspaces: Query<(&Workspace, &mut Transform)>,
    mut redraw: Redraw,
) {
    let t = time.elapsed_secs() - kept.since;
    let settled = EaseFunction::SmoothStep.sample_clamped(t / DRIFT_SETTLE);
    let swell = |i: usize, phase: f32| (t * TAU / DRIFT_PERIODS[i] + phase).sin() * settled;
    for (workspace, mut transform) in &mut workspaces {
        let Some(home) = home(&house, workspace) else {
            continue;
        };
        // Out of step with the others: its phase from its id.
        let phase = workspace.id as f32 * 2.399;
        let along = Vec3::new(
            DRIFT.x * swell(0, phase),
            DRIFT.y * swell(1, phase * 1.7),
            DRIFT.z * swell(2, phase * 2.3) - GHOST_BACK * settled,
        );
        let turn = Quat::from_euler(
            EulerRot::YXZ,
            DRIFT_TURN.x * swell(3, phase * 0.6),
            0.0,
            DRIFT_TURN.y * swell(4, phase * 1.3),
        );
        transform.set_if_neq(Transform {
            translation: home.transform_point(along),
            rotation: home.rotation * turn,
            scale: Vec3::splat(1.0 + (GHOST_SCALE - 1.0) * settled),
        });
    }
    redraw.ask(["shadow"]);
}

/// Each workspace with a frame: a camera taking a still of its screen
/// and all on it, looking at its face from just in front, until the still
/// goes up (`develop`). It is the workspace's child, so it sees the frame as it
/// is, adrift or not.
fn freeze(
    mut commands: Commands,
    desktop: Res<Desktop>,
    house: Res<House>,
    workspaces: Query<(Entity, &Workspace), Without<Frozen>>,
    mut images: ResMut<Assets<Image>>,
) {
    let (Some(layout), Some(monitor)) = (&house.layout, desktop.monitor) else {
        return;
    };
    let px = (monitor.size * monitor.scale * STILL_SCALE)
        .max(Vec2::ONE)
        .as_uvec2();
    for (entity, workspace) in &workspaces {
        if layout.frame_of(workspace.id).is_none() {
            continue;
        }
        let mut image = Image::new_target_texture(px.x, px.y, TextureFormat::Bgra8UnormSrgb, None);
        image.asset_usage = RenderAssetUsages::RENDER_WORLD;
        let image = images.add(image);
        commands.entity(entity).insert(Frozen);
        commands.spawn((
            ChildOf(entity),
            Still,
            Taking {
                image: image.clone(),
                size: layout.frame,
            },
            Camera3d::default(),
            Camera {
                order: -1,
                clear_color: ClearColorConfig::Custom(Color::BLACK),
                ..default()
            },
            RenderTarget::Image(image.into()),
            // The screen exactly, and nothing more than half a meter in
            // front of the frame or behind it.
            Projection::Orthographic(OrthographicProjection {
                near: 0.1,
                far: 1.0,
                scaling_mode: ScalingMode::Fixed {
                    width: layout.frame.x,
                    height: layout.frame.y,
                },
                ..OrthographicProjection::default_3d()
            }),
            // As the player's camera draws it (camera.rs).
            Tonemapping::None,
            DebandDither::Disabled,
            Msaa::Off,
            Transform::from_xyz(0.0, 0.0, 0.5),
        ));
    }
}

/// The stills taken, in the world and `STILL_AT` into the realm: the
/// cameras go, their last drawing kept, and the stills go up over the
/// screens.
fn develop(
    mut commands: Commands,
    time: Res<Time>,
    kept: Res<Kept>,
    taking: Query<(Entity, &Taking, &ChildOf)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut warps: ResMut<Assets<WarpMaterial>>,
) {
    if time.elapsed_secs() - kept.since < STILL_AT {
        return;
    }
    for (camera, taken, parent) in &taking {
        commands.entity(camera).despawn();
        commands.spawn((
            ChildOf(parent.parent()),
            Still,
            Warping {
                since: time.elapsed_secs(),
            },
            Mesh3d(meshes.add(Rectangle::new(1.0, 1.0))),
            MeshMaterial3d(warps.add(WarpMaterial {
                params: WarpParams::default(),
                still: taken.image.clone(),
            })),
            Transform::from_xyz(0.0, 0.0, SCREEN_LIFT).with_scale(taken.size.extend(1.0)),
        ));
    }
}

/// Behind a still, the live screen, windows and layers hidden, and any
/// that come later: only the bezel and the name stay, with the glow.
fn veil(
    mut commands: Commands,
    frozen: Query<&Children, With<Frozen>>,
    stills: Query<(), With<Warping>>,
    mut live: Query<(Entity, &mut Visibility), Live>,
) {
    for children in &frozen {
        if !children.iter().any(|child| stills.contains(child)) {
            continue;
        }
        for child in children.iter() {
            if let Ok((entity, mut visibility)) = live.get_mut(child) {
                *visibility = Visibility::Hidden;
                commands.entity(entity).insert(Veiled);
            }
        }
    }
}

/// The stills warping, seen from where the eye is, in the murk.
fn warp(
    time: Res<Time>,
    eye: Single<&GlobalTransform, With<FlyCamera>>,
    stills: Query<(&Warping, &MeshMaterial3d<WarpMaterial>)>,
    mut warps: ResMut<Assets<WarpMaterial>>,
) {
    let now = time.elapsed_secs();
    let (murk, fel) = (FEL_MURK.to_linear(), FEL.to_linear());
    for (warping, material) in &stills {
        let Some(mut warp) = warps.get_mut(&material.0) else {
            continue;
        };
        let settled = EaseFunction::SmoothStep.sample_clamped((now - warping.since) / WARP_SETTLE);
        warp.params = WarpParams {
            murk: Vec4::new(murk.red, murk.green, murk.blue, FEL_DENSITY),
            fel: Vec4::new(fel.red, fel.green, fel.blue, 1.0),
            eye: eye.translation().extend(now),
            warp: Vec4::new(settled, 0.0, 0.0, 0.0),
        };
    }
}

/// Out of the realm: the live screens back, and the stills to take again
/// next time.
fn thaw(
    mut commands: Commands,
    mut veiled: Query<(Entity, &mut Visibility), With<Veiled>>,
    frozen: Query<Entity, Thawing>,
) {
    for (entity, mut visibility) in &mut veiled {
        *visibility = Visibility::Inherited;
        commands.entity(entity).remove::<Veiled>();
    }
    for entity in &frozen {
        commands.entity(entity).remove::<(Frozen, Billowing)>();
    }
}

/// Billows of fel light about each workspace with a frame, mostly
/// around its edges and all behind it.
#[allow(clippy::too_many_arguments)]
fn gather(
    mut commands: Commands,
    house: Res<House>,
    void: Query<Entity, With<Void>>,
    workspaces: Query<(Entity, &Workspace), Without<Billowing>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut puffs: Local<Option<Puffs>>,
) {
    let (Some(layout), Ok(void)) = (&house.layout, void.single()) else {
        return;
    };
    for (entity, workspace) in &workspaces {
        if layout.frame_of(workspace.id).is_none() {
            continue;
        }
        let puffs = puffs.get_or_insert_with(|| Puffs {
            quad: meshes.add(Rectangle::new(1.0, 1.0)),
            looks: (0..PUFFS)
                .map(|seed| {
                    let fel = FEL.to_linear() * BILLOW_LEVEL;
                    materials.add(StandardMaterial {
                        base_color: fel.with_alpha(1.0).into(),
                        base_color_texture: Some(images.add(puff(seed))),
                        alpha_mode: AlphaMode::Add,
                        unlit: true,
                        // Turned toward the eye whichever way round.
                        cull_mode: None,
                        ..default()
                    })
                })
                .collect(),
        });
        commands.entity(entity).insert(Billowing);
        let half = layout.frame / 2.0;
        let between = |(low, high): (f32, f32)| low + fastrand::f32() * (high - low);
        for i in 0..BILLOWS {
            let around = (i as f32 + fastrand::f32()) * TAU / BILLOWS as f32;
            let out = between(BILLOW_AROUND) * 2.0;
            commands.spawn((
                ChildOf(void),
                Billow {
                    anchor: entity,
                    at: Vec3::new(
                        half.x * out * around.cos(),
                        half.y * out * around.sin(),
                        -between(BILLOW_BEHIND),
                    ),
                    size: between(BILLOW_SIZE),
                    phase: fastrand::f32() * TAU,
                    spin: (fastrand::f32() - 0.5) * 0.3,
                },
                Mesh3d(puffs.quad.clone()),
                MeshMaterial3d(puffs.looks[i % puffs.looks.len()].clone()),
                Transform::default(),
            ));
        }
    }
}

/// The billows rolling about their frames as they drift, swelling and
/// shrinking and turning slowly, each facing the eye.
fn billow(
    time: Res<Time>,
    eye: Single<&GlobalTransform, With<FlyCamera>>,
    anchors: Query<&GlobalTransform, With<Workspace>>,
    mut billows: Query<(&Billow, &mut Transform, &mut Visibility)>,
) {
    let t = time.elapsed_secs();
    let eye = eye.translation();
    for (billow, mut transform, mut visibility) in &mut billows {
        let Ok(anchor) = anchors.get(billow.anchor) else {
            visibility.set_if_neq(Visibility::Hidden);
            continue;
        };
        visibility.set_if_neq(Visibility::Inherited);
        let p = billow.phase;
        let wave = |period: f32, phase: f32| (t * TAU / period + phase).sin();
        let roll = BILLOW_ROLL * Vec3::new(wave(11.0, p), wave(14.0, p * 1.3), wave(17.0, p * 0.7));
        let at = anchor.transform_point(billow.at + roll);
        let swell = 1.0 + 0.2 * wave(9.0, p * 1.9);
        let (scale, ..) = anchor.to_scale_rotation_translation();
        *transform = Transform::from_translation(at)
            .looking_at(eye, Vec3::Y)
            .with_scale(Vec3::splat(billow.size * swell * scale.x));
        transform.rotate_local_z(p + billow.spin * t);
    }
}

/// A soft round puff of light, white, its alpha falling gently away from
/// the middle to nothing at its edge, and broken up by noise into wisps.
fn puff(seed: u32) -> Image {
    let mut rng = fastrand::Rng::with_seed(u64::from(seed) + 0xfe1);
    // Value noise on a lattice that wraps, three octaves.
    const CELLS: usize = 8;
    let lattice: Vec<f32> = (0..CELLS * CELLS * 7).map(|_| rng.f32()).collect();
    let noise = |x: f32, y: f32, octave: usize| {
        let n = CELLS << octave;
        let (x, y) = (x * n as f32, y * n as f32);
        let (ix, iy) = (x.floor() as usize, y.floor() as usize);
        let (fx, fy) = (x.fract(), y.fract());
        let at = |i: usize, j: usize| lattice[((j % n) * n + i % n) % lattice.len()];
        let ease = |f: f32| f * f * (3.0 - 2.0 * f);
        let top = at(ix, iy) + (at(ix + 1, iy) - at(ix, iy)) * ease(fx);
        let bottom = at(ix, iy + 1) + (at(ix + 1, iy + 1) - at(ix, iy + 1)) * ease(fx);
        top + (bottom - top) * ease(fy)
    };
    let mut data = Vec::with_capacity((PUFF_PX * PUFF_PX * 4) as usize);
    for y in 0..PUFF_PX {
        for x in 0..PUFF_PX {
            let (u, v) = (
                (x as f32 + 0.5) / PUFF_PX as f32,
                (y as f32 + 0.5) / PUFF_PX as f32,
            );
            let r = Vec2::new(u - 0.5, v - 0.5).length() * 2.0;
            let falloff = EaseFunction::SmoothStep.sample_clamped(1.0 - r);
            let cloud = 0.5 * noise(u, v, 0) + 0.3 * noise(u, v, 1) + 0.2 * noise(u, v, 2);
            let alpha = (falloff * (0.25 + 0.75 * cloud)).clamp(0.0, 1.0);
            data.extend_from_slice(&[255, 255, 255, (alpha * 255.0) as u8]);
        }
    }
    Image::new(
        Extent3d {
            width: PUFF_PX,
            height: PUFF_PX,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

/// The realm's sound, looping on the void: made first if it has not been,
/// which wakes the app when it is done.
#[allow(clippy::too_many_arguments)]
fn hum(
    mut commands: Commands,
    time: Res<Time>,
    wake: Res<Wake>,
    mut drones: ResMut<Drones>,
    mut sources: ResMut<Assets<Sound>>,
    void: Query<Entity, With<Void>>,
    hums: Query<(), With<Hum>>,
) {
    let Ok(void) = void.single() else {
        return;
    };
    if !hums.is_empty() {
        return;
    }
    let drone = drones.0.get_or_insert_with(|| {
        let wake = wake.clone();
        Drone::Making(AsyncComputeTaskPool::get().spawn(async move {
            let wav = drone::wav();
            wake.wake();
            wav
        }))
    });
    if let Drone::Making(task) = drone {
        let Some(wav) = check_ready(task) else {
            return;
        };
        *drone = Drone::Made(sources.add(Sound { bytes: wav.into() }));
    }
    let Drone::Made(sound) = drone else {
        return;
    };
    commands.spawn((
        ChildOf(void),
        Hum {
            since: time.elapsed_secs(),
        },
        Play::looped(sound.clone(), 0.0),
    ));
}

/// The sound coming up from nothing.
fn fade_in(time: Res<Time>, hums: Query<(&Hum, &Playing)>, mut redraw: Redraw) {
    for (hum, playing) in &hums {
        let up = ((time.elapsed_secs() - hum.since) / HUM_FADE).min(1.0);
        let volume = HUM_VOLUME * EaseFunction::SmoothStep.sample_clamped(up);
        if playing.volume() != volume {
            playing.set_volume(volume);
            // Updates to come up in, standing still or not.
            redraw.ask(["shadow"]);
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy::{mesh::MeshPlugin, state::app::StatesPlugin};

    use super::*;
    use crate::stats::Stats;

    /// A house of a shell with a wall in it and a workspace, the camera, and
    /// every flight in going to the realm.
    fn haunted() -> (App, Entity, Entity, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            StatesPlugin,
            AssetPlugin::default(),
            MeshPlugin,
        ))
        .init_asset::<StandardMaterial>()
        .init_state::<WorldState>()
        .init_resource::<Stats>()
        .add_message::<bevy::window::RequestRedraw>()
        .init_resource::<scene::WorkspaceIndex>()
        .init_resource::<House>()
        .init_resource::<crate::wake::Wake>()
        .init_asset::<Sound>()
        .init_asset::<Image>()
        .init_asset::<WarpMaterial>()
        .init_resource::<Desktop>()
        .init_resource::<ClearColor>()
        .init_resource::<GlobalAmbientLight>()
        .add_plugins(plugin)
        .insert_resource(Forced(true))
        .insert_resource(Shadow(false));
        let world = app.world_mut();
        let camera = world
            .spawn(FlyCamera {
                yaw: 0.0,
                pitch: 0.0,
            })
            .id();
        let shell = world.spawn(Shell).id();
        let wall = world
            .spawn((ChildOf(shell), Solid, Collider::cuboid(1.0, 1.0, 1.0)))
            .id();
        let workspace = world
            .spawn(Workspace {
                id: 1,
                name: String::new(),
            })
            .id();
        app.update();
        (app, camera, shell, wall, workspace)
    }

    fn go(app: &mut App, to: WorldState) {
        app.world_mut()
            .resource_mut::<NextState<WorldState>>()
            .set(to);
        app.update();
        app.update();
    }

    fn voids(app: &mut App) -> usize {
        app.world_mut()
            .query_filtered::<(), With<Void>>()
            .iter(app.world())
            .count()
    }

    /// In: the house hidden and out of the way and the murk closing in, but
    /// the frames still there. All the way out: the house as it was, and
    /// nothing of the realm left.
    #[test]
    fn the_realm_comes_and_goes_with_the_flights() {
        let (mut app, camera, shell, wall, workspace) = haunted();
        let was = app.world().resource::<ClearColor>().0;
        go(&mut app, WorldState::Out);
        go(&mut app, WorldState::FlyingIn);

        let world = app.world();
        assert!(world.resource::<Shadow>().0);
        assert_eq!(world.get::<Visibility>(shell), Some(&Visibility::Hidden));
        assert_eq!(
            world.get::<Visibility>(workspace),
            Some(&Visibility::Inherited),
            "the frames stay"
        );
        assert!(
            world.get::<ColliderDisabled>(wall).is_some(),
            "not in the way"
        );
        assert!(world.get::<DistanceFog>(camera).is_some());
        assert_eq!(world.resource::<ClearColor>().0, FEL_MURK);
        assert_eq!(voids(&mut app), 1);

        go(&mut app, WorldState::Out);
        let world = app.world();
        assert!(!world.resource::<Shadow>().0);
        assert_eq!(world.get::<Visibility>(shell), Some(&Visibility::Inherited));
        assert_eq!(
            world.get::<Visibility>(workspace),
            Some(&Visibility::Inherited)
        );
        assert!(world.get::<ColliderDisabled>(wall).is_none());
        assert!(world.get::<DistanceFog>(camera).is_none());
        assert_eq!(world.resource::<ClearColor>().0, was);
        assert_eq!(voids(&mut app), 0);
    }

    /// Once in: billows about each frame, and a still taken of
    /// it, put up over its screen, and what was live on it hidden behind,
    /// until the realm goes.
    #[test]
    fn the_ghosts_frames_turn_to_stills() {
        use std::time::Duration;

        use bevy::time::TimeUpdateStrategy;

        use crate::{glue::Monitor, layout::Layout};

        let (mut app, _, _, _, workspace) = haunted();
        let size = Vec2::new(1000.0, 600.0);
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            100,
        )));
        app.world_mut().resource_mut::<House>().layout = Some(Layout::new(
            &[1],
            &[],
            size,
            Rect::from_corners(Vec2::ZERO, size),
            &Default::default(),
        ));
        app.world_mut().resource_mut::<Desktop>().monitor = Some(Monitor {
            size,
            scale: 1.0,
            work: Rect::from_corners(Vec2::ZERO, size),
        });
        let window = app
            .world_mut()
            .spawn((ChildOf(workspace), Visibility::default()))
            .id();
        go(&mut app, WorldState::Out);
        go(&mut app, WorldState::FlyingIn);
        go(&mut app, WorldState::In);

        let mut taken = false;
        for _ in 0..40 {
            app.update();
            taken |= app
                .world_mut()
                .query::<&Taking>()
                .iter(app.world())
                .next()
                .is_some();
        }
        assert!(taken, "a camera took it");
        let world = app.world_mut();
        assert_eq!(world.query::<&Taking>().iter(world).count(), 0, "and went");
        assert_eq!(world.query::<&Warping>().iter(world).count(), 1, "one up");
        assert_eq!(world.get::<Visibility>(window), Some(&Visibility::Hidden));
        assert_eq!(
            world.query::<&Billow>().iter(world).count(),
            BILLOWS,
            "billows about it"
        );

        go(&mut app, WorldState::Out);
        let world = app.world_mut();
        assert_eq!(world.query::<&Still>().iter(world).count(), 0);
        assert_eq!(world.query::<&Billow>().iter(world).count(), 0);
        assert!(world.get::<Frozen>(workspace).is_none());
        assert!(world.get::<Billowing>(workspace).is_none());
        assert_eq!(
            world.get::<Visibility>(window),
            Some(&Visibility::Inherited)
        );
    }
}
