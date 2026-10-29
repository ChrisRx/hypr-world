//! The player: movement on an avian capsule. Walking by default, with
//! gravity and jumps; flying with `player.flying`. The camera is the body's
//! child, at its eyes, with an eased look and a gentle bob while walking:
//! the body carries it. While a flight in or out
//! is under way (flight.rs) the camera is the flight's.

use std::f32::consts::TAU;

use avian3d::prelude::*;
use bevy::{
    input::mouse::AccumulatedMouseMotion,
    prelude::*,
    window::{CursorGrabMode, CursorOptions, PrimaryWindow},
};

use crate::{
    FlyCamera, WorldSystems, camera,
    config::WorldConfig,
    flight::{Flying, WorldState},
    glue::{Desktop, InputMode},
    layout::EYE_HEIGHT,
    stats::Redraw,
};

/// The body: its controls read before the physics step, and the camera put
/// on it after.
pub fn plugin(app: &mut App) {
    app.insert_resource(Gravity(Vec3::NEG_Y * GRAVITY))
        .add_systems(Startup, setup)
        .add_systems(OnEnter(WorldState::In), take_over)
        .add_systems(
            Update,
            (
                (
                    collision.run_if(resource_changed::<WorldConfig>),
                    control.run_if(in_state(WorldState::In)),
                )
                    .chain()
                    .in_set(WorldSystems::Player),
                // On a flight the camera is the flight's (flight.rs `ride`).
                eyes.run_if(not(in_state(Flying)))
                    .in_set(WorldSystems::Camera),
            ),
        );
}

/// The capsule: 0.6 m wide and 1.8 m tall, its center 0.9 m above the feet.
const RADIUS: f32 = 0.3;
const CYLINDER: f32 = 1.2;
const HALF_HEIGHT: f32 = 0.9;
/// The eyes above the capsule's center, 1.62 m above the feet.
pub const EYES: f32 = EYE_HEIGHT - HALF_HEIGHT;

pub const GRAVITY: f32 = 14.0;
/// Takes the feet about 1.08 m up.
const JUMP: f32 = 5.5;
/// Falling never gets faster than this.
const TERMINAL: f32 = 40.0;
/// How far below the capsule's center the ground still counts as underfoot.
const GROUND_PROBE: f32 = 1.05;
/// Ctrl's speed-up, up and down too.
const SPRINT: f32 = 2.5;
const PITCH_LIMIT: f32 = 1.5;
/// Slower than this, in m/s, the body is standing still.
const REST: f32 = 0.01;
/// Velocity changes smaller than this, in m/s, are the solver's traces and
/// not the keys': left unwritten, since a write wakes the body (`body`).
const TRACE: f32 = 1e-4;
/// A look this close to where it is going, in radians, is there: far below
/// a pixel.
const STILL_ANGLE: f32 = 1e-5;

/// The walk bob: its height at full speed, how fast it comes and goes, its
/// pace per meter walked, and the roll that goes with it.
const BOB: f32 = 0.035;
const BOB_EASE: f32 = 8.0;
const BOB_PACE: f32 = 6.0;
const BOB_ROLL: f32 = 0.008;

/// The player's settings: `player` in the app's settings.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct PlayerSettings {
    pub flying: bool,
    /// m/s.
    pub move_speed: f32,
    /// The time constant movement eases in and out with, in seconds.
    pub move_inertia: f32,
    /// Radians per pointer px.
    pub look_sensitivity: f32,
    /// The time constant the look eases in with, in seconds.
    pub look_inertia: f32,
    /// Off walks through walls.
    pub collision: bool,
    pub walk_bob: bool,
}

impl Default for PlayerSettings {
    fn default() -> Self {
        Self {
            flying: false,
            move_speed: 4.0,
            move_inertia: 0.05,
            look_sensitivity: 0.0025,
            look_inertia: 0.03,
            collision: true,
            walk_bob: true,
        }
    }
}

/// The player's body. What it needs to be one comes with it (`require`):
/// spawning a `Player` where it stands is spawning the body. Avian puts it
/// to sleep once it has stood still for half a second, and anything that
/// moves it wakes it. Awake on the floor it never quite stands still: the
/// floor's contact gives a little, by an amount that depends on the step's
/// length, and the app's steps vary, so the body moves by micrometers every
/// step, and the camera on it with it, a frame drawn each time (redraw.rs).
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
#[require(
    // The camera is its child, and a child's place and visibility come down
    // from its parent's: Bevy warns if the parent has none (B0004).
    Transform,
    Visibility,
    RigidBody = RigidBody::Dynamic,
    Collider = Collider::capsule(RADIUS, CYLINDER),
    // Upright whatever it bumps into, and sliding along walls rather than
    // sticking to them.
    LockedAxes = LockedAxes::ROTATION_LOCKED,
    Friction = Friction::ZERO.with_combine_rule(CoefficientCombine::Min),
    Restitution = Restitution::ZERO.with_combine_rule(CoefficientCombine::Min),
    // Off until `control` turns it on, for walking.
    GravityScale = GravityScale(0.0),
    SweptCcd
)]
pub struct Player {
    /// What the keys ask for, eased in over `move_inertia`.
    velocity: Vec3,
    /// Where the pointer has put the look, yaw and pitch: the camera's look
    /// eases toward it over `look_inertia`.
    aim: Vec2,
    /// The walk bob's height and phase.
    bob: f32,
    phase: f32,
    grounded: bool,
}

/// The body, standing in the middle of the room, and the camera at its eyes:
/// its child, so the body carries it.
pub fn setup(mut commands: Commands) {
    commands.spawn((
        Player::default(),
        Transform::from_xyz(0.0, HALF_HEIGHT, 0.0),
        children![(camera::camera(), Transform::from_xyz(0.0, EYES, 0.0))],
    ));
}

/// Walls or none for the body, by `player.collision` (config.rs), whenever the
/// settings change.
pub fn collision(
    mut commands: Commands,
    config: Res<WorldConfig>,
    body: Option<Single<Entity, With<Player>>>,
) {
    let Some(body) = body else { return };
    if config.player.collision {
        commands.entity(*body).remove::<ColliderDisabled>();
    } else {
        commands.entity(*body).insert(ColliderDisabled);
    }
}

/// In the world after a flight: the look goes on from where the flight left
/// it, and nothing the keys asked for before it carries over.
fn take_over(camera: Single<&FlyCamera>, mut player: Single<&mut Player>) {
    player.aim = Vec2::new(camera.yaw, camera.pitch);
    player.velocity = Vec3::ZERO;
}

/// Look and movement, before the physics step. One source for
/// both: the plugin's keys and mouse join the window's as Bevy's input.
#[allow(clippy::too_many_arguments)]
pub fn control(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    motion: Res<AccumulatedMouseMotion>,
    mode: Res<State<InputMode>>,
    config: Res<WorldConfig>,
    spatial: SpatialQuery,
    cursor: Option<Single<&CursorOptions, With<PrimaryWindow>>>,
    camera: Single<&mut FlyCamera>,
    body: Single<(
        Entity,
        &Transform,
        &mut Player,
        &mut LinearVelocity,
        &mut GravityScale,
    )>,
) {
    let settings = &config.player;
    let mut look = camera.into_inner();
    let (entity, transform, player, mut velocity, mut gravity) = body.into_inner();
    let player = player.into_inner();
    let dt = time.delta_secs();

    // The plugin's motion while it has the mouse, the window's while it has
    // grabbed the pointer: both come in as Bevy's own (glue.rs `PluginInput`).
    let looking = *mode.get() != InputMode::Off
        || cursor.is_some_and(|c| c.grab_mode == CursorGrabMode::Locked);
    let delta = if looking { motion.delta } else { Vec2::ZERO };

    // The pointer moves where the look is going, and the look follows over
    // a few hundredths of a second rather than at once.
    player.aim.x -= delta.x * settings.look_sensitivity;
    player.aim.y =
        (player.aim.y - delta.y * settings.look_sensitivity).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    let mut now = Vec2::new(look.yaw, look.pitch);
    nudge(&mut now, player.aim, settings.look_inertia, dt);
    // Settled exactly, so the camera stops changing.
    if now.abs_diff_eq(player.aim, STILL_ANGLE) {
        now = player.aim;
    }
    look.set_if_neq(FlyCamera {
        yaw: now.x,
        pitch: now.y,
    });

    let held = |key: KeyCode| keys.pressed(key);
    let axis = |plus: bool, minus: bool| f32::from(u8::from(plus)) - f32::from(u8::from(minus));

    // Right and forward, never faster on the diagonal, and pitch never tilts
    // it: looking down does not walk you into the floor.
    let mut walk = Vec2::new(
        axis(held(KeyCode::KeyD), held(KeyCode::KeyA)),
        axis(held(KeyCode::KeyW), held(KeyCode::KeyS)),
    );
    if walk.length() > 1.0 {
        walk = walk.normalize();
    }
    let rise = if settings.flying {
        axis(held(KeyCode::Space), held(KeyCode::ShiftLeft))
    } else {
        0.0
    };
    let sprint = held(KeyCode::ControlLeft);
    let speed = settings.move_speed * if sprint { SPRINT } else { 1.0 };

    let (forward, right) = basis(look.yaw);
    let target = (forward * walk.y + right * walk.x + Vec3::Y * rise) * speed;

    nudge(&mut player.velocity, target, settings.move_inertia, dt);
    if walk == Vec2::ZERO && rise == 0.0 && player.velocity.abs().element_sum() < 0.01 {
        player.velocity = Vec3::ZERO;
    }

    let filter = SpatialQueryFilter::from_excluded_entities([entity]);
    player.grounded = spatial
        .cast_ray(
            transform.translation,
            Dir3::NEG_Y,
            GROUND_PROBE,
            true,
            &filter,
        )
        .is_some();

    gravity.set_if_neq(GravityScale(if settings.flying { 0.0 } else { 1.0 }));

    let wanted = if settings.flying {
        player.velocity
    } else {
        // The horizontal from the keys, the vertical from gravity, and a jump
        // only from the ground.
        let jump = keys.just_pressed(KeyCode::Space);
        let fall = if jump && player.grounded {
            JUMP
        } else {
            velocity.y.max(-TERMINAL)
        };
        Vec3::new(player.velocity.x, fall, player.velocity.z)
    };
    // Only a change the keys make: a body asleep stays asleep until they
    // move it.
    if velocity.distance(wanted) > TRACE {
        velocity.0 = wanted;
    }
}

/// The camera on the body, after the physics step: at its eyes, looking
/// where the look has got to, and bobbing while it walks. Its transform is
/// its place on the body, which carries it.
pub fn eyes(
    time: Res<Time>,
    config: Res<WorldConfig>,
    desktop: Res<Desktop>,
    body: Single<(&LinearVelocity, &mut Player)>,
    camera: Single<(&mut Transform, &FlyCamera)>,
    mut redraw: Redraw,
) {
    let settings = &config.player;
    let (velocity, player) = body.into_inner();
    let (mut eyes, look) = camera.into_inner();
    let player = player.into_inner();
    let dt = time.delta_secs();

    let walking = !settings.flying && settings.walk_bob && player.grounded;
    // Slower than this is standing still, so the bob settles to nothing and
    // not to a trace of what the physics left over.
    let speed = Some(velocity.with_y(0.0).length())
        .filter(|&speed| speed >= REST)
        .unwrap_or(0.0);
    let target = if walking {
        BOB * (speed / settings.move_speed.max(0.01)).min(1.0)
    } else {
        0.0
    };
    player.bob.smooth_nudge(&target, BOB_EASE, dt);
    // Settled exactly, so the camera stops changing.
    if target == 0.0 && player.bob < 1e-4 {
        player.bob = 0.0;
    }
    player.phase = (player.phase + speed * dt * BOB_PACE) % (2.0 * TAU);

    let lift = player.bob * player.phase.sin();
    let roll = player.bob / BOB * BOB_ROLL * (player.phase / 2.0).sin();

    // Only a change marks it: every move is a frame drawn (redraw.rs). The
    // look and the bob settle exactly, so standing still this stays put.
    let rotation = camera::rotation(look.yaw, look.pitch, -roll);
    eyes.set_if_neq(Transform::from_xyz(0.0, EYES + lift, 0.0).with_rotation(rotation));

    // The app only updates when something happens:
    // keep the updates coming while the player moves, turns, bobs or falls,
    // in view. Out of view nobody sees it, and a body left falling (a house
    // rebuilt out from under it) would keep the app busy for nothing.
    let in_view = desktop.in_view();
    let reasons = [
        (
            "moving",
            player.velocity != Vec3::ZERO || velocity.length() > REST,
        ),
        ("turning", player.aim != Vec2::new(look.yaw, look.pitch)),
        ("bobbing", player.bob > 0.0),
        ("falling", !settings.flying && !player.grounded),
    ];
    if in_view && reasons.iter().any(|&(_, why)| why) {
        redraw.ask(
            reasons
                .iter()
                .filter(|&&(_, why)| why)
                .map(|&(name, _)| name),
        );
    }
}

/// Level forward and right for a camera turned `yaw` about y (0 looks
/// along -z).
fn basis(yaw: f32) -> (Vec3, Vec3) {
    let (sin, cos) = yaw.sin_cos();
    (Vec3::new(-sin, 0.0, -cos), Vec3::new(cos, 0.0, -sin))
}

/// `value` eased toward `target` with the time constant `tau`: Bevy's
/// `smooth_nudge` at the rate 1 / tau. No time constant is no easing, and
/// its own branch: 1 / 0 is an infinite rate, and with no time passed,
/// infinity times 0 is NaN.
fn nudge<T: StableInterpolate>(value: &mut T, target: T, tau: f32, dt: f32) {
    if tau > 0.0 {
        value.smooth_nudge(&target, 1.0 / tau, dt);
    } else {
        *value = target;
    }
}

/// The player as the app runs it, headless, on a floor: its plugin, avian
/// stepping between the controls and the camera, and what its systems read.
/// The body and the camera come in the first update, in the world, and the
/// flight waits for an `Enter`. For the tests here and in flight.rs.
#[cfg(test)]
pub fn on_a_floor() -> App {
    use std::os::unix::net::UnixStream;

    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        TransformPlugin,
        bevy::asset::AssetPlugin::default(),
        bevy::state::app::StatesPlugin,
        PhysicsPlugins::new(Update),
        plugin,
        crate::flight::plugin,
    ))
    .configure_sets(
        Update,
        (
            (WorldSystems::Flight, WorldSystems::Player)
                .chain()
                .before(PhysicsSystems::First),
            WorldSystems::Camera.after(PhysicsSystems::Last),
        ),
    )
    // glue.rs's, which this leaves out.
    .init_state::<InputMode>()
    .init_resource::<WorldConfig>()
    .init_resource::<crate::scene::House>()
    .init_resource::<Desktop>()
    .init_resource::<crate::stats::Stats>()
    .init_resource::<ButtonInput<KeyCode>>()
    .init_resource::<AccumulatedMouseMotion>()
    .insert_resource(crate::glue::Glue::connected_to(
        UnixStream::pair().expect("a socket pair").0,
    ))
    .add_message::<bevy::window::RequestRedraw>()
    .add_message::<crate::glue::Enter>()
    .add_message::<crate::glue::Leave>();
    // What App::run does first: avian sets some of its resources up here.
    app.finish();
    app.cleanup();

    let shell = app.world_mut().spawn(crate::scene::Shell).id();
    let floor = Transform::from_xyz(0.0, -0.15, 0.0).with_scale(Vec3::new(20.0, 0.3, 20.0));
    app.world_mut().spawn((
        ChildOf(shell),
        RigidBody::Static,
        Collider::cuboid(1.0, 1.0, 1.0),
        floor,
    ));
    app
}

#[cfg(test)]
mod tests {
    use bevy::window::RequestRedraw;

    use super::*;
    use crate::stats::Stats;

    #[test]
    fn the_basis_matches_the_camera() {
        for yaw in [0.0, 0.7, -2.1, 3.0] {
            let rotation = Quat::from_euler(EulerRot::YXZ, yaw, 0.4, 0.0);
            let (forward, right) = basis(yaw);
            let camera_forward = (rotation * Vec3::NEG_Z).with_y(0.0).normalize();
            let camera_right = (rotation * Vec3::X).with_y(0.0).normalize();
            assert!(forward.distance(camera_forward) < 1e-5 && right.distance(camera_right) < 1e-5);
        }
    }

    #[test]
    fn easing() {
        // A time constant's worth covers 1 - 1/e of the way.
        let mut value = 0.0;
        nudge(&mut value, 1.0, 0.05, 0.05);
        assert!((value - (1.0 - (-1.0f32).exp())).abs() < 1e-6, "{value}");

        // None is there at once, even in an update with no time in it.
        for dt in [0.016, 0.0] {
            let mut value = Vec2::ZERO;
            nudge(&mut value, Vec2::ONE, 0.0, dt);
            assert_eq!(value, Vec2::ONE, "dt {dt}");
        }
    }

    /// The capsule against colliders made the way the house makes them
    /// (scaled unit boxes, children of the shell's root), headless: it lands
    /// on the floor and stops at a wall it walks into.
    #[test]
    fn the_capsule_stands_and_stops() {
        use std::time::Duration;

        use bevy::time::TimeUpdateStrategy;

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            bevy::asset::AssetPlugin::default(),
            PhysicsPlugins::new(Update),
        ))
        .insert_resource(Gravity(Vec3::NEG_Y * GRAVITY))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
            1.0 / 60.0,
        )));
        // What App::run does first: avian sets some of its resources up here.
        app.finish();
        app.cleanup();

        let shell = app.world_mut().spawn(crate::scene::Shell).id();
        let block = |center: Vec3, size: Vec3| {
            let at = Transform::from_translation(center).with_scale(size);
            (
                ChildOf(shell),
                RigidBody::Static,
                Collider::cuboid(1.0, 1.0, 1.0),
                at,
            )
        };
        app.world_mut().spawn(block(
            Vec3::new(0.0, -0.15, 0.0),
            Vec3::new(20.0, 0.3, 20.0),
        ));
        // Its inside face at x = 3.
        app.world_mut().spawn(block(
            Vec3::new(3.125, 2.0, 0.0),
            Vec3::new(0.25, 4.0, 20.0),
        ));

        let player = app
            .world_mut()
            .spawn((Player::default(), Transform::from_xyz(0.0, 2.0, 0.0)))
            .id();
        app.world_mut().entity_mut(player).insert(GravityScale(1.0));

        // Walking at 4 m/s towards the wall for four seconds.
        for _ in 0..240 {
            let mut velocity = app
                .world_mut()
                .get_mut::<LinearVelocity>(player)
                .expect("a body");
            velocity.x = 4.0;
            app.update();
        }

        let at = app
            .world()
            .get::<Transform>(player)
            .expect("a body")
            .translation;
        assert!(
            (at.y - HALF_HEIGHT).abs() < 0.05,
            "standing on the floor, center at {at}"
        );
        assert!(
            at.x < 3.0 - RADIUS + 0.02 && at.x > 2.5,
            "stopped at the wall, center at {at}"
        );
    }

    /// How many redraws were asked for in the last update.
    #[derive(Resource, Default)]
    struct Asked(usize);

    fn count(mut redraws: MessageReader<RequestRedraw>, mut asked: ResMut<Asked>) {
        asked.0 = redraws.read().count();
    }

    /// Either of these would keep the app updating for nothing: a bob that
    /// never settles on a trace of horizontal velocity the physics left, or
    /// a body falling out of view.
    #[test]
    fn standing_still_or_out_of_view_asks_for_no_updates() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<RequestRedraw>()
            .init_resource::<WorldConfig>()
            .init_resource::<Desktop>()
            .init_resource::<Stats>()
            .init_resource::<Asked>()
            // The first update of an app has no time in it, and no bob.
            .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_secs_f32(1.0 / 60.0),
            ))
            .add_systems(Update, eyes)
            .add_systems(Last, count);
        let body = app
            .world_mut()
            .spawn((
                Player {
                    grounded: true,
                    ..default()
                },
                LinearVelocity(Vec3::new(1e-6, 0.0, 0.0)),
                Transform::default(),
            ))
            .id();
        app.world_mut().spawn((
            FlyCamera {
                yaw: 0.0,
                pitch: 0.0,
            },
            Transform::default(),
        ));
        let asked = |app: &App| app.world().resource::<Asked>().0;

        for _ in 0..3 {
            app.update();
        }
        assert_eq!(
            asked(&app),
            0,
            "standing, with a trace of velocity: nothing to update for"
        );

        app.world_mut()
            .get_mut::<Player>(body)
            .expect("the body")
            .grounded = false;
        app.update();
        assert!(asked(&app) > 0, "falling in view keeps the updates coming");

        app.world_mut().resource_mut::<Desktop>().monitor = Some(crate::glue::Monitor {
            size: Vec2::new(1440.0, 960.0),
            scale: 2.0,
            work: Rect::new(0.0, 40.0, 1440.0, 960.0),
        });
        app.update();
        assert_eq!(asked(&app), 0, "falling out of view does not");
    }

    /// Updates in which the camera moved, its `GlobalTransform` changed:
    /// each one a frame drawn (redraw.rs).
    #[derive(Resource, Default)]
    struct Moves(usize);

    fn count_moves(
        camera: Single<Ref<GlobalTransform>, With<FlyCamera>>,
        mut moves: ResMut<Moves>,
    ) {
        if camera.is_changed() {
            moves.0 += 1;
        }
    }

    /// The camera on the body stays put while the body stands still, after
    /// a landing and after a walk, with steps of any length: the body
    /// asleep, nothing moves it. Awake, the floor would move it by
    /// micrometers, a different amount for each length of step.
    #[test]
    fn standing_still_the_camera_stays_put() {
        use std::time::Duration;

        use bevy::{time::TimeUpdateStrategy, transform::TransformSystems};

        let mut app = on_a_floor();
        app.init_resource::<Moves>()
            .add_systems(PostUpdate, count_moves.after(TransformSystems::Propagate));
        app.update();
        let mut bodies = app
            .world_mut()
            .query_filtered::<&mut Transform, With<Player>>();
        bodies
            .single_mut(app.world_mut())
            .expect("the body")
            .translation
            .y += 0.5;

        // Steps of 1 to 33 ms, as the app's come; the moves in `updates` of them.
        let steps = [16, 33, 4, 25, 1, 9, 30, 12].map(Duration::from_millis);
        let run = |app: &mut App, updates: usize| {
            app.world_mut().resource_mut::<Moves>().0 = 0;
            for &step in steps.iter().cycle().take(updates) {
                app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
                app.update();
            }
            app.world().resource::<Moves>().0
        };

        assert!(run(&mut app, 100) > 0, "the landing");
        assert_eq!(run(&mut app, 200), 0, "standing after the landing");

        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::KeyW);
        assert!(
            run(&mut app, 30) > 0,
            "the keys wake the body, and the camera goes with it"
        );
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(KeyCode::KeyW);
        run(&mut app, 100);
        assert_eq!(run(&mut app, 200), 0, "standing after the walk");
    }

    /// A child with a place or a visibility has a parent with one, or Bevy
    /// warns (B0004): the camera on the body.
    #[test]
    fn the_body_can_carry_the_camera() {
        use bevy::ecs::system::RunSystemOnce;

        let mut world = World::new();
        world.run_system_once(setup).expect("setup ran");
        let mut children =
            world.query::<(&ChildOf, Has<GlobalTransform>, Has<InheritedVisibility>)>();
        let mut parents = world.query::<(Has<GlobalTransform>, Has<InheritedVisibility>)>();
        for (child_of, place, visibility) in children.iter(&world) {
            let (parent_place, parent_visibility) =
                parents.get(&world, child_of.parent()).expect("its parent");
            assert!(parent_place || !place, "a place without its parent's");
            assert!(
                parent_visibility || !visibility,
                "a visibility without its parent's"
            );
        }
        assert_eq!(children.iter(&world).count(), 1, "the camera, on the body");
    }

    #[test]
    fn the_eyes_and_the_jump_are_at_their_heights() {
        assert!((HALF_HEIGHT + EYES - 1.62).abs() < 1e-6);
        assert!((CYLINDER / 2.0 + RADIUS - HALF_HEIGHT).abs() < 1e-6);
        // Apex of a jump: v^2 / 2g, about 1.08 m.
        assert!((JUMP * JUMP / (2.0 * GRAVITY) - 1.08).abs() < 0.01);
    }
}
