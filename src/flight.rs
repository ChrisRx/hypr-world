//! Into the world and out of it: the camera's flight between the 2D desktop
//! and the house. Coming in, the view starts where the active
//! workspace's frame fills the screen pixel for pixel, the way the 2D
//! workspace does, and backs away into the room while the world fades in
//! over the desktop. Going out, it flies back into the frame that is active
//! then and fades away, and the plugin hands the screen back to 2D. The fade
//! is the app window's own opacity, which the plugin sets as the app asks.
//! Where the app is, out, in or on the way, is a state (`WorldState`), and a
//! flight is an eased curve, timed.

use std::{
    f32::consts::{PI, TAU},
    time::Duration,
};

use avian3d::prelude::LinearVelocity;
use bevy::prelude::*;

use crate::{
    FlyCamera, WorldSystems, camera,
    glue::{self, Desktop, Enter, Leave},
    layout::{EYE_HEIGHT, Layout},
    player::{EYES, Player},
    scene::House,
    stats::Redraw,
};

pub fn plugin(app: &mut App) {
    app.init_state::<WorldState>()
        .register_type_state::<WorldState>()
        .add_computed_state::<Flying>()
        .init_resource::<Flight>();
    // From out of the world or in it, a flight in starts at the frame.
    for exited in [WorldState::Out, WorldState::In] {
        app.add_systems(
            OnTransition {
                exited,
                entered: WorldState::FlyingIn,
            },
            take_off,
        );
    }
    app.add_systems(
        OnTransition {
            exited: WorldState::FlyingOut,
            entered: WorldState::FlyingIn,
        },
        turn_back,
    )
    .add_systems(
        OnTransition {
            exited: WorldState::In,
            entered: WorldState::FlyingOut,
        },
        leave_the_world,
    )
    .add_systems(OnEnter(WorldState::FlyingOut), head_out)
    .add_systems(OnEnter(WorldState::Out), hide)
    .add_systems(
        Update,
        (
            // Moved on before the plugin is heard, so that what it asks
            // for has the last word over where a flight that ends goes.
            (advance.run_if(in_state(Flying)), steer)
                .chain()
                .in_set(WorldSystems::Flight),
            ride.run_if(in_state(Flying)).in_set(WorldSystems::Camera),
        ),
    );
}

/// A whole flight's length, in seconds, either way.
const DURATION: f32 = 0.55;
/// A longer frame counts as this long, so a hitch does not skip the flight.
const MAX_STEP: Duration = Duration::from_millis(100);
/// Opacity changes smaller than this are not worth telling the plugin.
const OPACITY_STEP: f32 = 1.0 / 255.0;

/// Where the app is: under the 2D desktop, on the flight in or out, or in
/// the world. A run without the plugin never flies anywhere, so it starts in.
#[derive(States, Reflect, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum WorldState {
    /// Hidden, and the plugin shows the 2D desktop.
    Out,
    FlyingIn,
    #[default]
    In,
    FlyingOut,
}

/// On either flight: the camera and the body are the flight's, not the
/// player's.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Flying;

impl ComputedStates for Flying {
    type SourceStates = WorldState;

    fn compute(state: WorldState) -> Option<Self> {
        matches!(state, WorldState::FlyingIn | WorldState::FlyingOut).then_some(Flying)
    }
}

/// Where the camera is, and which way it looks.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pose {
    pub eye: Vec3,
    pub yaw: f32,
    pub pitch: f32,
}

impl Pose {
    /// At `eye`, looking at `target`.
    pub fn looking(eye: Vec3, target: Vec3) -> Self {
        let d = (target - eye).normalize();
        Self {
            eye,
            yaw: f32::atan2(-d.x, -d.z),
            pitch: d.y.asin(),
        }
    }
}

/// A pose eases as the camera turns, by yaw the short way round and by
/// pitch, so the horizon stays level. `Isometry3d`'s quaternion would roll
/// it on a turn that also pitches.
impl Ease for Pose {
    fn interpolating_curve_unbounded(start: Self, end: Self) -> impl Curve<Self> {
        let turn = wrap(end.yaw - start.yaw);
        FunctionCurve::new(Interval::EVERYWHERE, move |s| Pose {
            eye: start.eye.lerp(end.eye, s),
            yaw: start.yaw + turn * s,
            pitch: start.pitch + (end.pitch - start.pitch) * s,
        })
    }
}

/// An angle in [-pi, pi).
fn wrap(angle: f32) -> f32 {
    (angle + PI).rem_euclid(TAU) - PI
}

/// The near and far poses for `workspace`'s frame: filling the screen, and
/// standing well back from it.
fn poses(layout: &Layout, workspace: Option<i64>) -> Option<(Pose, Pose)> {
    let frame = layout.frame_of(workspace?)?;
    let (near_eye, near_target) = layout.near_pose(frame, 1.0);
    let (far_eye, far_target) = layout.far_pose(frame);
    Some((
        Pose::looking(near_eye, near_target),
        Pose::looking(far_eye, far_target),
    ))
}

/// The flight: the way the camera goes, timed, and how far into the world
/// it is, which the fade follows. Which way it goes is the state's.
#[derive(Resource)]
pub struct Flight {
    /// Where a flight in lands: back from the frame, or where the player
    /// stood when the flight out began.
    landing: Pose,
    way: EasingCurve<Pose>,
    timer: Timer,
    /// How far into the world the way starts and ends: 0 is the 2D desktop,
    /// 1 the world.
    depth: (f32, f32),
    /// The opacity the plugin was last told.
    sent: Option<f32>,
}

/// In the world, where the app starts.
impl Default for Flight {
    fn default() -> Self {
        Self {
            landing: Pose::default(),
            way: EasingCurve::new(Pose::default(), Pose::default(), EaseFunction::SmoothStep),
            timer: Timer::default(),
            depth: (1.0, 1.0),
            sent: None,
        }
    }
}

impl Flight {
    fn pose(&self) -> Pose {
        self.way.sample_clamped(self.timer.fraction())
    }

    fn depth(&self) -> f32 {
        let (from, to) = self.depth;
        from + (to - from) * self.timer.fraction()
    }

    /// A new way from `from` to `to`, each a pose and how far into the world
    /// it is, eased from rest to rest. It takes as long as a whole flight
    /// takes over the depth it covers, so a flight turned round midway fades
    /// back as it came.
    fn fly(&mut self, (from, from_depth): (Pose, f32), (to, to_depth): (Pose, f32)) {
        self.way = EasingCurve::new(from, to, EaseFunction::SmoothStep);
        self.timer = Timer::from_seconds(DURATION * (to_depth - from_depth).abs(), TimerMode::Once);
        self.depth = (from_depth, to_depth);
    }
}

/// Where the plugin's `enter` and `leave` send the app. Out wins over in,
/// asked for in the same update. The state changes as the next update
/// starts, which this asks for.
pub fn steer(
    mut enter: MessageReader<Enter>,
    mut leave: MessageReader<Leave>,
    house: Res<House>,
    state: Res<State<WorldState>>,
    mut next: ResMut<NextState<WorldState>>,
    // An `enter` that came before there was a house to fly into.
    mut waiting: Local<bool>,
    mut redraw: Redraw,
) {
    let (entering, leaving) = (enter.read().count() > 0, leave.read().count() > 0);
    let to = if leaving {
        WorldState::FlyingOut
    } else if entering || *waiting {
        WorldState::FlyingIn
    } else {
        return;
    };

    *waiting = false;
    match (state.get(), to) {
        // Out already: out again, so the plugin is told `hide` again. `set`
        // enters the state the app is in, `set_if_different` would not.
        (WorldState::Out, WorldState::FlyingOut) => next.set(WorldState::Out),
        // In from the frame, which takes a house.
        (WorldState::Out | WorldState::In, WorldState::FlyingIn) if house.layout.is_none() => {
            *waiting = true;
            return;
        }
        _ => next.set_if_different(to),
    }
    redraw.ask(["flight"]);
}

/// Into the world, from the active workspace's frame as the house is laid
/// out now.
fn take_off(desktop: Res<Desktop>, house: Res<House>, mut flight: ResMut<Flight>) {
    // With no frame for the active workspace there is nothing to fly
    // from: the middle of the room, facing the front wall, and a fade.
    let (near, far) = house
        .layout
        .as_ref()
        .and_then(|l| poses(l, desktop.active))
        .unwrap_or_else(|| {
            let middle = Pose::looking(Vec3::Y * EYE_HEIGHT, Vec3::new(0.0, EYE_HEIGHT, -1.0));
            (middle, middle)
        });
    flight.fly((near, 0.0), (far, 1.0));
    flight.landing = far;
    flight.sent = None;
}

/// Out of the world from where the player stands, which is where the flight
/// goes back to if it turns round.
fn leave_the_world(camera: Single<(&GlobalTransform, &FlyCamera)>, mut flight: ResMut<Flight>) {
    flight.landing = seen_from(*camera);
}

/// Out, from wherever the camera is, into the frame active now (the dwell
/// may have changed it).
fn head_out(
    desktop: Res<Desktop>,
    house: Res<House>,
    camera: Single<(&GlobalTransform, &FlyCamera)>,
    mut flight: ResMut<Flight>,
) {
    let current = seen_from(*camera);
    let near = house
        .layout
        .as_ref()
        .and_then(|l| poses(l, desktop.active))
        .map_or(current, |(near, _)| near);
    let depth = flight.depth();
    flight.fly((current, depth), (near, 0.0));
}

/// Turned round on the way out: back to where the flight in lands.
fn turn_back(mut flight: ResMut<Flight>) {
    let (now, depth, landing) = (flight.pose(), flight.depth(), flight.landing);
    flight.fly((now, depth), (landing, 1.0));
}

/// Where the camera was last drawn: it is the body's child, and nothing has
/// moved either since.
fn seen_from((place, look): (&GlobalTransform, &FlyCamera)) -> Pose {
    Pose {
        eye: place.translation(),
        yaw: look.yaw,
        pitch: look.pitch,
    }
}

/// All the way out: the plugin hands the screen back to 2D.
fn hide(glue: Res<glue::Glue>) {
    glue.send("hide");
}

/// Moves the flight on: the camera along it, the player's body with it, and
/// the app window's opacity over the desktop. At its end the app is in, or
/// out.
#[allow(clippy::too_many_arguments)]
pub fn advance(
    time: Res<Time>,
    glue: Res<glue::Glue>,
    state: Res<State<WorldState>>,
    mut next: ResMut<NextState<WorldState>>,
    mut flight: ResMut<Flight>,
    mut camera: Single<&mut FlyCamera>,
    body: Single<(&mut Transform, &mut LinearVelocity), With<Player>>,
    mut redraw: Redraw,
) {
    // The app only draws when something changes: a flight is nothing but.
    redraw.ask(["flight"]);

    flight.timer.tick(time.delta().min(MAX_STEP));

    // The body rides along with zero velocity, so the player takes over
    // standing where the flight ends.
    let pose = flight.pose();
    camera.yaw = pose.yaw;
    camera.pitch = pose.pitch;
    let (mut transform, mut velocity) = body.into_inner();
    transform.translation = pose.eye - Vec3::Y * EYES;
    velocity.0 = Vec3::ZERO;

    // In by the time it is halfway in, out from halfway out.
    let opacity = EaseFunction::SmoothStep.sample_clamped((2.0 * flight.depth()).min(1.0));
    let settled = opacity == 0.0 || opacity == 1.0;
    if flight
        .sent
        .is_none_or(|sent| sent != opacity && ((sent - opacity).abs() >= OPACITY_STEP || settled))
    {
        glue.send(&format!("alpha {opacity:.4}"));
        flight.sent = Some(opacity);
    }

    if flight.timer.is_finished() {
        next.set(if *state.get() == WorldState::FlyingIn {
            WorldState::In
        } else {
            WorldState::Out
        });
    }
}

/// The camera exactly on the flight, wherever the step left the body:
/// `advance` puts the body on the flight before the step, and gravity or a
/// wall may move it in the step. The body never turns (its rotation is
/// locked), so the flight's place less the body's is the camera's place on
/// it.
fn ride(
    flight: Res<Flight>,
    body: Single<&Transform, (With<Player>, Without<FlyCamera>)>,
    mut camera: Single<&mut Transform, With<FlyCamera>>,
) {
    let pose = flight.pose();
    let rotation = camera::rotation(pose.yaw, pose.pitch, 0.0);
    camera.set_if_neq(
        Transform::from_translation(pose.eye - body.translation).with_rotation(rotation),
    );
}

#[cfg(test)]
mod tests {
    use std::{io::Read, os::unix::net::UnixStream};

    use bevy::time::TimeUpdateStrategy;

    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn same(a: Pose, b: Pose) -> bool {
        a.eye.distance(b.eye) < 1e-4 && close(wrap(a.yaw - b.yaw), 0.0) && close(a.pitch, b.pitch)
    }

    #[test]
    fn turns_the_short_way_round() {
        assert!(close(wrap(3.0 * PI / 2.0), -PI / 2.0));
        let from = Pose {
            eye: Vec3::ZERO,
            yaw: 3.0,
            pitch: 0.0,
        };
        let to = Pose {
            eye: Vec3::X,
            yaw: -3.0,
            pitch: 0.0,
        };
        // Across the seam at pi, not the long way through 0.
        let half = EasingCurve::new(from, to, EaseFunction::Linear).sample_clamped(0.5);
        assert!(close(wrap(half.yaw), wrap(PI)));
    }

    /// The body on a floor, in a house of one frame, workspace 1's, which is
    /// active, 50 ms an update, and the plugin's end of the glue.
    fn flying() -> (App, UnixStream) {
        let mut app = crate::player::on_a_floor();
        let (world_end, plugin_end) = UnixStream::pair().expect("a socket pair");
        plugin_end
            .set_nonblocking(true)
            .expect("a socket that does not wait");
        let work = Rect::new(0.0, 0.0, 1000.0, 600.0);
        app.insert_resource(glue::Glue::connected_to(world_end))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
                50,
            )));
        app.world_mut().resource_mut::<House>().layout = Some(Layout::new(
            &[1],
            Vec2::new(1000.0, 600.0),
            work,
            &Default::default(),
        ));
        app.world_mut().resource_mut::<Desktop>().active = Some(1);
        // On the floor a while first, as the app always is before a flight:
        // avian's very first step does not see a body moved just before it.
        for _ in 0..10 {
            app.update();
        }
        (app, plugin_end)
    }

    fn state(app: &App) -> WorldState {
        *app.world().resource::<State<WorldState>>().get()
    }

    /// Where the flight's way starts and ends.
    fn ends(app: &App) -> (Pose, Pose) {
        let way = &app.world().resource::<Flight>().way;
        (way.sample_clamped(0.0), way.sample_clamped(1.0))
    }

    /// How many `hide`s the plugin has been told since it last looked.
    fn hides(plugin: &mut UnixStream) -> usize {
        let mut said = String::new();
        // Everything there is so far, then WouldBlock.
        let _ = plugin.read_to_string(&mut said);
        said.lines().filter(|&line| line == "hide").count()
    }

    /// In from the frame; out, though asked for in the update the flight in
    /// lands; turned round on the way out, back to where the flight in
    /// lands, and from out of the world, back to where the player stood; all
    /// the way out with `hide`; and `hide` again for a `leave` when out.
    #[test]
    fn the_world_goes_where_the_plugin_asks() {
        let (mut app, mut plugin) = flying();
        let flight = |app: &App| {
            let flight = app.world().resource::<Flight>();
            (flight.pose(), flight.depth())
        };

        app.world_mut().write_message(Enter);
        app.update();
        assert_eq!(state(&app), WorldState::In, "an update before it changes");
        app.update();
        assert_eq!(state(&app), WorldState::FlyingIn);
        let layout = app
            .world()
            .resource::<House>()
            .layout
            .as_ref()
            .expect("a house");
        let (near, far) = poses(layout, Some(1)).expect("a frame");
        assert!(
            same(ends(&app).0, near) && same(ends(&app).1, far),
            "from the frame"
        );

        // Asked to leave in the update the flight in lands.
        let mut landing = app.world_mut().resource_mut::<Flight>();
        let almost = landing.timer.duration() - Duration::from_millis(10);
        landing.timer.set_elapsed(almost);
        app.world_mut().write_message(Leave);
        app.update();
        assert!(
            app.world().resource::<Flight>().timer.is_finished(),
            "landed"
        );
        app.update();
        assert_eq!(state(&app), WorldState::FlyingOut, "and out after all");

        app.update();
        let (_, depth) = flight(&app);
        app.world_mut().write_message(Enter);
        app.update();
        let (stepped, _) = flight(&app);
        app.update();
        assert_eq!(state(&app), WorldState::FlyingIn, "turned round");
        assert!(
            same(ends(&app).0, stepped) && same(ends(&app).1, far),
            "from where it was to where it lands"
        );
        // A step on out, and one back in: as deep as it was.
        assert!(
            close(flight(&app).1, depth),
            "{} for {depth}",
            flight(&app).1
        );

        // In, a meter to one side, then out and turned around.
        while state(&app) != WorldState::In {
            app.update();
        }
        let mut bodies = app
            .world_mut()
            .query_filtered::<&mut Transform, With<Player>>();
        bodies
            .single_mut(app.world_mut())
            .expect("the body")
            .translation
            .x += 1.0;
        app.update();
        let mut cameras = app.world_mut().query::<(&GlobalTransform, &FlyCamera)>();
        let stood = seen_from(cameras.single(app.world()).expect("the camera"));
        assert!(stood.eye.distance(far.eye) > 0.9);
        app.world_mut().write_message(Leave);
        app.update();
        app.update();
        app.world_mut().write_message(Enter);
        app.update();
        app.update();
        assert_eq!(state(&app), WorldState::FlyingIn);
        assert!(same(ends(&app).1, stood), "back to where the player stood");

        app.world_mut().write_message(Leave);
        for _ in 0..30 {
            app.update();
        }
        assert_eq!(state(&app), WorldState::Out);
        assert_eq!(hides(&mut plugin), 1);

        app.world_mut().write_message(Leave);
        app.update();
        app.update();
        assert_eq!(state(&app), WorldState::Out);
        assert_eq!(hides(&mut plugin), 1, "again");
    }

    /// The camera is exactly where the flight is, though it rides on the
    /// body and the body falls a little out of the flight's place every
    /// step, and the player stands where the flight ends.
    #[test]
    fn the_camera_is_the_flights_while_it_flies() {
        let (mut app, _plugin) = flying();
        app.world_mut().write_message(Enter);
        app.update();

        let mut cameras = app
            .world_mut()
            .query_filtered::<&GlobalTransform, With<FlyCamera>>();
        let mut checked = 0;
        loop {
            app.update();
            if state(&app) != WorldState::FlyingIn {
                break;
            }
            let pose = app.world().resource::<Flight>().pose();
            let place = cameras.single(app.world()).expect("the camera");
            let turned = Quat::from_euler(EulerRot::YXZ, pose.yaw, pose.pitch, 0.0);
            assert!(
                place.translation().distance(pose.eye) < 1e-5,
                "at {} for {}",
                place.translation(),
                pose.eye
            );
            assert!(
                place.forward().distance(turned * Vec3::NEG_Z) < 1e-5
                    && place.up().distance(turned * Vec3::Y) < 1e-5,
                "turned {} for {turned}",
                place.rotation()
            );
            checked += 1;
        }
        assert!(checked >= 10, "only {checked} updates of flight");

        let far = app.world().resource::<Flight>().landing;
        let place = cameras.single(app.world()).expect("the camera");
        assert!(
            place.translation().distance(far.eye) < 0.01,
            "at {} after a flight to {}",
            place.translation(),
            far.eye
        );
    }
}
