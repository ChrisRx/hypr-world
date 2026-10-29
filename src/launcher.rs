//! The rocket launcher: R takes it out and puts it
//! away, and a click while it is out fires a rocket from the eyes. A rocket
//! that hits a window closes it, as Hyprland's own close does (the plugin's
//! `close`, which asks the window's client, so an app with unsaved work can
//! still ask), and blows up; one that hits the house only blows up. While it
//! is out the plugin keeps clicks and the pointer from the windows
//! (`armed`), so a stray click closes nothing and fires instead.
//!
//! The launcher in view is models/launcher.glb and a rocket
//! models/rocket.glb, embedded in the binary and exported from the .blend
//! files beside them (README.md, "Models").

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use avian3d::prelude::{SpatialQuery, SpatialQueryFilter};
use bevy::{
    asset::embedded_asset,
    gltf::GltfAssetLabel,
    prelude::*,
    world_serialization::{WorldAsset, WorldAssetRoot},
};

use crate::{
    FlyCamera, WorldSystems,
    config::WorldConfig,
    flight::WorldState,
    glue::Glue,
    player::Player,
    stats::Redraw,
    windows::{DesktopWindow, WindowQuad, crosshair_on},
};

/// How fast a rocket flies, in meters a second: slow enough to see go.
const SPEED: f32 = 14.0;
/// How long a rocket that hits nothing flies before it is gone.
const RANGE: Duration = Duration::from_secs(4);
/// How long an explosion lasts, and how large it grows, in meters across.
const BLAST: Duration = Duration::from_millis(450);
const BLAST_SIZE: f32 = 0.9;
/// Where the launcher's ends are, from the eyes, as Quake 3 holds it: large
/// and close, pointing nearly straight ahead, so that only its muzzle end
/// shows, coming up from the bottom of the view just right of the middle.
/// Its back end is behind the eyes, and everything but the last few
/// centimeters runs below the bottom of the view (0.32 m out the view
/// reaches 0.2 m down, `FOV_Y`).
const HELD_BACK: Vec3 = Vec3::new(0.30, -0.30, 0.05);
const HELD_MUZZLE: Vec3 = Vec3::new(0.08, -0.13, -0.50);
/// The launcher's model and a rocket's, embedded by `plugin`.
const MODEL: &str = "embedded://hypr_world/models/launcher.glb";
const ROCKET: &str = "embedded://hypr_world/models/rocket.glb";

/// The launcher's place on the camera: from its back end to its muzzle,
/// scaled so that 1 m down the model's -Z spans the two.
fn held() -> Transform {
    Transform::from_translation((HELD_BACK + HELD_MUZZLE) / 2.0)
        .looking_to(HELD_MUZZLE - HELD_BACK, Vec3::Y)
        .with_scale(Vec3::splat(HELD_BACK.distance(HELD_MUZZLE)))
}

pub fn plugin(app: &mut App) {
    embedded_asset!(app, "models/launcher.glb");
    embedded_asset!(app, "models/rocket.glb");
    app.init_state::<Launcher>()
        .register_type_state::<Launcher>()
        .init_resource::<Hits>()
        .add_systems(Startup, setup)
        .add_systems(OnEnter(Launcher::Armed), take_out)
        .add_systems(OnExit(Launcher::Armed), put_away)
        // Out of the world, it goes away with the world.
        .add_systems(OnExit(WorldState::In), stow)
        .add_systems(
            Update,
            (
                (
                    toggle.run_if(in_state(WorldState::In)),
                    fire.run_if(in_state(Launcher::Armed)),
                )
                    .chain()
                    .in_set(WorldSystems::Input),
                // Against the quads where the step left the camera and the
                // house.
                (fly, blow_up).chain().in_set(WorldSystems::Aim),
            ),
        );
}

/// Whether the launcher is out.
#[derive(States, Reflect, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Launcher {
    #[default]
    Stowed,
    Armed,
}

/// A rocket in flight: its velocity, and how much longer it flies.
#[derive(Component)]
pub struct Rocket {
    velocity: Vec3,
    left: Timer,
}

/// A rocket's explosion, growing and fading until its time is up.
#[derive(Component)]
pub struct Explosion {
    timer: Timer,
    material: Handle<StandardMaterial>,
}

/// The launcher in view, a child of the camera. Its parts' place and
/// visibility come down from it, and Bevy warns if it has none (B0004).
#[derive(Component)]
#[require(Transform, Visibility)]
pub struct Held;

/// A rocket's model and the mesh every explosion shares, and the
/// launcher's model.
#[derive(Resource)]
pub struct Ammo {
    /// Along -Z, the way it flies, its origin at its middle.
    rocket: Handle<WorldAsset>,
    blast: Handle<Mesh>,
    model: Handle<WorldAsset>,
}

pub fn setup(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>, assets: Res<AssetServer>) {
    commands.insert_resource(Ammo {
        rocket: assets.load(GltfAssetLabel::Scene(0).from_asset(ROCKET)),
        blast: meshes.add(Sphere::new(0.5)),
        model: assets.load(GltfAssetLabel::Scene(0).from_asset(MODEL)),
    });
}

/// R takes the launcher out and puts it away, while the app has the input.
fn toggle(
    keys: Res<ButtonInput<KeyCode>>,
    state: Res<State<Launcher>>,
    mut next: ResMut<NextState<Launcher>>,
) {
    if !keys.just_pressed(KeyCode::KeyR) {
        return;
    }
    next.set(match state.get() {
        Launcher::Stowed => Launcher::Armed,
        Launcher::Armed => Launcher::Stowed,
    });
}

fn stow(mut next: ResMut<NextState<Launcher>>) {
    next.set_if_different(Launcher::Stowed);
}

/// The plugin told, and the launcher in view, with a small light of its own
/// that reaches no wall.
fn take_out(
    mut commands: Commands,
    glue: Res<Glue>,
    ammo: Res<Ammo>,
    camera: Single<Entity, With<FlyCamera>>,
) {
    glue.send("armed 1");
    let held = commands
        .spawn((
            Held,
            held(),
            WorldAssetRoot(ammo.model.clone()),
            ChildOf(*camera),
        ))
        .id();
    commands.spawn((
        PointLight {
            intensity: 40_000.0,
            range: 0.6,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::from_xyz(0.1, 0.25, 0.1),
        ChildOf(held),
    ));
}

fn put_away(mut commands: Commands, glue: Res<Glue>, held: Query<Entity, With<Held>>) {
    glue.send("armed 0");
    for held in &held {
        commands.entity(held).despawn();
    }
}

/// A click fires a rocket from just ahead of the eyes, the way they look.
fn fire(
    mut commands: Commands,
    mouse: Res<ButtonInput<MouseButton>>,
    ammo: Res<Ammo>,
    camera: Single<&GlobalTransform, With<FlyCamera>>,
) {
    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }
    let forward = camera.forward();
    commands.spawn((
        Rocket {
            velocity: forward * SPEED,
            left: Timer::new(RANGE, TimerMode::Once),
        },
        WorldAssetRoot(ammo.rocket.clone()),
        Transform::from_translation(camera.translation() + forward * 0.5)
            .looking_to(forward, Vec3::Y),
    ));
}

/// What a rocket's step ran into first: a window, by its address, or the
/// house, and where.
enum Hit {
    Window(u64, Vec3),
    House(Vec3),
}

/// Each rocket a step further, swept from where it was to where it goes, so
/// none passes through a window between two updates: into the nearest of the
/// windows' quads and the house's colliders (not the player's body) along
/// the way. A hit closes the window it is on and blows up there.
#[allow(clippy::too_many_arguments)]
fn fly(
    mut commands: Commands,
    time: Res<Time>,
    glue: Res<Glue>,
    config: Res<WorldConfig>,
    mut hits: ResMut<Hits>,
    ammo: Res<Ammo>,
    house: SpatialQuery,
    body: Option<Single<Entity, With<Player>>>,
    quads: Query<(&DesktopWindow, &WindowQuad, &GlobalTransform)>,
    mut rockets: Query<(Entity, &mut Rocket, &mut Transform)>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut redraw: Redraw,
) {
    if rockets.is_empty() {
        return;
    }
    let filter = SpatialQueryFilter::from_excluded_entities(body.map(|body| *body));
    for (entity, mut rocket, mut transform) in &mut rockets {
        let from = transform.translation;
        let step = rocket.velocity * time.delta_secs();
        transform.translation += step;
        rocket.left.tick(time.delta());

        if let Some(hit) = Dir3::new(step)
            .ok()
            .and_then(|way| first_hit(from, way, step.length(), &quads, &house, &filter))
        {
            let at = match hit {
                Hit::Window(address, at) => {
                    if glue.connected {
                        let within = Duration::from_secs_f32(config.force_close_within);
                        let verb = hits.verb(address, within);
                        glue.send(&format!("{verb} {address:#x}"));
                    }
                    at
                }
                Hit::House(at) => at,
            };
            commands.entity(entity).despawn();
            explode(&mut commands, &ammo, &mut materials, at);
        } else if rocket.left.is_finished() {
            commands.entity(entity).despawn();
        }
    }
    redraw.ask(["rocket"]);
}

/// When each window was last hit, by the wall clock, as the dwell keeps its
/// time (windows.rs `Resting`): `Time` stands still while nothing updates
/// the app, and a window may wait between shots.
#[derive(Resource, Default)]
pub struct Hits(HashMap<u64, Instant>);

impl Hits {
    /// What to tell the plugin of a hit on the window at `address` now: a
    /// close, as Hyprland's own asks its client, or, a second hit within
    /// `within` of the first that did not close it, its app told to end
    /// (`terminate`, SIGTERM). A window that is still there after a close is one its client
    /// kept open, often to ask about unsaved work.
    fn verb(&mut self, address: u64, within: Duration) -> &'static str {
        let now = Instant::now();
        let again = self
            .0
            .insert(address, now)
            .is_some_and(|then| now - then <= within);
        if again && !within.is_zero() {
            "terminate"
        } else {
            "close"
        }
    }
}

/// The first thing along `way` from `from`, no further than `length`.
fn first_hit(
    from: Vec3,
    way: Dir3,
    length: f32,
    quads: &Query<(&DesktopWindow, &WindowQuad, &GlobalTransform)>,
    house: &SpatialQuery,
    filter: &SpatialQueryFilter,
) -> Option<Hit> {
    let ray = Ray3d::new(from, way);
    let window = quads
        .iter()
        .filter_map(|(window, _, quad)| {
            let (t, u, v) = crosshair_on(ray, quad)?;
            ((0.0..=length).contains(&t) && (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v))
                .then_some((t, window.address))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0));
    let wall = house
        .cast_ray(from, way, length, true, filter)
        .map(|hit| hit.distance);

    match (window, wall) {
        (Some((t, address)), wall) if wall.is_none_or(|w| t <= w) => {
            Some(Hit::Window(address, ray.get_point(t)))
        }
        (_, Some(w)) => Some(Hit::House(ray.get_point(w))),
        _ => None,
    }
}

/// An explosion at `at`: a bright ball, with a material of its own to fade,
/// and a light that goes with it.
fn explode(
    commands: &mut Commands,
    ammo: &Ammo,
    materials: &mut Assets<StandardMaterial>,
    at: Vec3,
) {
    let material = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 0.7, 0.25, 1.0),
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        ..default()
    });
    commands.spawn((
        Explosion {
            timer: Timer::new(BLAST, TimerMode::Once),
            material: material.clone(),
        },
        Mesh3d(ammo.blast.clone()),
        MeshMaterial3d(material),
        Transform::from_translation(at).with_scale(Vec3::splat(0.05)),
        children![PointLight {
            intensity: 200_000.0,
            range: 6.0,
            shadow_maps_enabled: false,
            ..default()
        }],
    ));
}

/// Each explosion grown and faded a step, and gone when its time is up.
fn blow_up(
    mut commands: Commands,
    time: Res<Time>,
    mut explosions: Query<(Entity, &mut Explosion, &mut Transform)>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut redraw: Redraw,
) {
    if explosions.is_empty() {
        return;
    }
    for (entity, mut explosion, mut transform) in &mut explosions {
        explosion.timer.tick(time.delta());
        if explosion.timer.is_finished() {
            commands.entity(entity).despawn();
            continue;
        }
        let done = explosion.timer.fraction();
        transform.scale =
            Vec3::splat(BLAST_SIZE * EaseFunction::CubicOut.sample_clamped(done).max(0.05));
        if let Some(material) = materials.get_mut(&explosion.material) {
            material.into_inner().base_color.set_alpha(1.0 - done);
        }
    }
    redraw.ask(["explosion"]);
}

#[cfg(test)]
mod tests {
    use std::{io::Read, os::unix::net::UnixStream};

    use avian3d::prelude::{Collider, PhysicsPlugins, RigidBody};
    use bevy::{
        asset::AssetPlugin, mesh::MeshPlugin, state::app::StatesPlugin, time::TimeUpdateStrategy,
    };

    use bevy::window::RequestRedraw;

    use super::*;
    use crate::{glue::Kind, stats::Stats};

    /// Everything the plugin has been told so far.
    fn told(plugin: &mut UnixStream) -> Vec<String> {
        let mut said = String::new();
        let _ = plugin.read_to_string(&mut said);
        said.lines().map(str::to_string).collect()
    }

    /// A second hit on a window within the setting's time tells its app to
    /// end, a later one closes it again, and a time of 0 only ever gets
    /// closes.
    #[test]
    fn a_window_hit_again_soon_has_its_app_told_to_end() {
        let within = Duration::from_secs(5);
        let mut hits = Hits::default();
        assert_eq!(hits.verb(0xa, within), "close", "the first hit");
        assert_eq!(hits.verb(0xa, within), "terminate", "again at once");
        assert_eq!(hits.verb(0xb, within), "close", "another window's first");

        hits.0.insert(0xa, Instant::now() - Duration::from_secs(6));
        assert_eq!(hits.verb(0xa, within), "close", "again, but too late");

        assert_eq!(hits.verb(0xa, Duration::ZERO), "close", "set to never");
    }

    /// The launcher's own systems, as its plugin wires them, in the world,
    /// with the physics the rockets sweep against, and a camera at the eyes
    /// looking down -Z.
    fn world() -> (App, UnixStream) {
        let (world_end, mut plugin) = UnixStream::pair().expect("a socket pair");
        plugin
            .set_nonblocking(true)
            .expect("a socket that does not wait");
        let glue = Glue::connected_to(world_end);

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            StatesPlugin,
            TransformPlugin,
            AssetPlugin::default(),
            MeshPlugin,
            PhysicsPlugins::new(Update),
        ))
        .init_asset::<StandardMaterial>()
        .init_asset::<WorldAsset>()
        .init_state::<WorldState>()
        .insert_resource(glue)
        .init_resource::<WorldConfig>()
        .init_resource::<Stats>()
        .init_resource::<ButtonInput<KeyCode>>()
        .init_resource::<ButtonInput<MouseButton>>()
        .add_message::<RequestRedraw>()
        .init_resource::<crate::windows::WindowIndex>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            16,
        )))
        .configure_sets(Update, (WorldSystems::Input, WorldSystems::Aim).chain())
        .add_plugins(super::plugin);
        app.finish();
        app.cleanup();
        app.world_mut().spawn((
            FlyCamera {
                yaw: 0.0,
                pitch: 0.0,
            },
            Transform::from_xyz(0.0, 1.5, 0.0),
        ));
        app.update();
        let _ = told(&mut plugin);
        (app, plugin)
    }

    fn press_r(app: &mut App) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyR);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(KeyCode::KeyR);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        // The state applies as the next update starts.
        app.update();
    }

    fn held(app: &mut App) -> usize {
        app.world_mut()
            .query_filtered::<(), With<Held>>()
            .iter(app.world())
            .count()
    }

    #[test]
    fn r_takes_the_launcher_out_and_puts_it_away() {
        let (mut app, mut plugin) = world();

        press_r(&mut app);
        assert_eq!(
            *app.world().resource::<State<Launcher>>().get(),
            Launcher::Armed
        );
        assert_eq!(
            told(&mut plugin),
            ["armed 1"],
            "the plugin keeps the clicks from here"
        );
        assert_eq!(held(&mut app), 1, "in view");

        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .release(MouseButton::Left);
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .clear();
        assert_eq!(
            app.world_mut().query::<&Rocket>().iter(app.world()).count(),
            1,
            "a click fires"
        );

        press_r(&mut app);
        assert_eq!(
            *app.world().resource::<State<Launcher>>().get(),
            Launcher::Stowed
        );
        assert_eq!(told(&mut plugin), ["armed 0"]);
        assert_eq!(held(&mut app), 0);
    }

    /// A rocket fired at the window 3 m ahead closes it, and one fired at a
    /// wall between the eyes and a window stops at the wall; each blows up,
    /// and its explosion is gone soon after.
    #[test]
    fn a_rocket_closes_the_window_it_hits_and_stops_at_a_wall() {
        let (mut app, mut plugin) = world();
        let window = |app: &mut App, address: u64, at: Vec3| {
            let desktop = DesktopWindow {
                address,
                workspace: 1,
                rect: Rect::default(),
                stack: 0,
                kind: Kind::Tiled,
            };
            let quad = WindowQuad::new(0);
            app.world_mut().spawn((
                desktop,
                quad,
                Transform::from_translation(at).with_scale(Vec3::new(1.0, 1.0, 1.0)),
            ));
        };
        window(&mut app, 0xa, Vec3::new(0.0, 1.5, -3.0));
        window(&mut app, 0xb, Vec3::new(4.0, 1.5, -3.0));
        // A wall across the way to the second window, a meter short of it.
        app.world_mut().spawn((
            RigidBody::Static,
            Collider::cuboid(1.0, 2.0, 0.2),
            Transform::from_xyz(4.0, 1.5, -2.0),
        ));
        app.update();

        let fire_at = |app: &mut App, from: Vec3| {
            let ammo = app.world().resource::<Ammo>();
            let model = ammo.rocket.clone();
            app.world_mut().spawn((
                Rocket {
                    velocity: Vec3::NEG_Z * SPEED,
                    left: Timer::new(RANGE, TimerMode::Once),
                },
                WorldAssetRoot(model),
                Transform::from_translation(from),
            ));
        };
        let rockets = |app: &mut App| app.world_mut().query::<&Rocket>().iter(app.world()).count();
        let explosions = |app: &mut App| {
            app.world_mut()
                .query::<&Explosion>()
                .iter(app.world())
                .count()
        };

        fire_at(&mut app, Vec3::new(0.0, 1.5, -0.5));
        for _ in 0..20 {
            app.update();
        }
        assert_eq!(told(&mut plugin), ["close 0xa"], "the window it hit");
        assert_eq!(rockets(&mut app), 0);
        assert_eq!(explosions(&mut app), 1);
        for _ in 0..40 {
            app.update();
        }
        assert_eq!(explosions(&mut app), 0, "gone once its time is up");

        fire_at(&mut app, Vec3::new(4.0, 1.5, -0.5));
        for _ in 0..20 {
            app.update();
        }
        assert!(told(&mut plugin).is_empty(), "the wall, before the window");
        assert_eq!(rockets(&mut app), 0);
        let at = app
            .world_mut()
            .query_filtered::<&Transform, With<Explosion>>()
            .single(app.world())
            .expect("one explosion")
            .translation;
        assert!(
            (-1.95..=-1.85).contains(&at.z),
            "at the wall's face, z -1.9, not {at}"
        );
    }
}
