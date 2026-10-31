//! The red button downstairs, on the pedestal in the middle of the room
//! (house.rs `raise_pedestal`), under a glass cover hinged at its back:
//! the pedestal's model (models/pedestal.blend) has the button and the
//! cover as objects of their own, `Button` and `Cover`, which move. A
//! click on the cover lifts it, a click on the button then presses it and
//! it springs back up, blowing a party blower and throwing confetti all
//! over the room (confetti.rs), and a click on the lifted
//! cover lowers it again. What the crosshair is on is found with the
//! windows (windows.rs `aim`), so a click on the pedestal is not one on a
//! frame behind it.

mod confetti;

use bevy::{
    camera::primitives::Aabb, ecs::system::SystemParam, math::Affine3A, prelude::*,
    transform::helper::TransformHelper, world_serialization::WorldInstanceReady,
};

use crate::{
    WorldSystems, flight::WorldState, glue::InputMode, house::Pedestal, launcher::Launcher,
    stats::Redraw, windows,
};

/// How far the cover lifts, in radians, and how long it takes to.
const OPEN: f32 = 1.9;
const SWING: f32 = 0.35;
/// How far the button goes down, how long it takes, how long it stays down,
/// and how long it takes to come back up.
const TRAVEL: f32 = 0.03;
const DOWN: f32 = 0.05;
const HELD: f32 = 0.15;
const UP: f32 = 0.2;
/// The farthest the eye can be from the pedestal to click on it.
const REACH: f32 = 2.5;

pub fn plugin(app: &mut App) {
    app.init_resource::<AimedPart>()
        .add_message::<Pressed>()
        .add_systems(Startup, confetti::prepare)
        .add_systems(
            Update,
            (
                press.run_if(
                    in_state(WorldState::In)
                        .and_then(not(in_state(InputMode::Off)))
                        .and_then(in_state(Launcher::Stowed)),
                ),
                animate,
                confetti::celebrate,
                confetti::flutter,
            )
                .chain()
                .after(windows::aim)
                .in_set(WorldSystems::Aim),
        );
}

/// The button was pressed.
#[derive(Message, Default)]
struct Pressed;

/// The pedestal's top, and the room it is in.
#[derive(Component)]
struct Stand(Pedestal);

/// What on the pedestal a click goes to.
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Part {
    Cover,
    Button,
}

/// The part the crosshair is on, within reach, if nothing nearer is in the
/// way.
#[derive(Resource, Default, PartialEq, Debug)]
pub struct AimedPart(pub Option<Part>);

/// The cover, turned about its hinge: how it is turned shut, whether it is
/// lifting or lowering, the angle it was at when it started to, and when
/// that was, by `Time<Real>`.
#[derive(Component, Default)]
struct Lid {
    shut: Quat,
    open: bool,
    from: f32,
    since: f32,
}

impl Lid {
    /// How far lifted at `now`.
    fn angle(&self, now: f32) -> f32 {
        let target = if self.open { OPEN } else { 0.0 };
        let k = ((now - self.since) / SWING).clamp(0.0, 1.0);
        self.from + (target - self.from) * k * k * (3.0 - 2.0 * k)
    }
}

/// The button: where it rests, and when it was last pressed while it is
/// still going down or coming back up.
#[derive(Component)]
struct Plunger {
    rest: Vec3,
    since: Option<f32>,
}

/// How far down the button is `after` seconds of a press.
fn depth(after: f32) -> f32 {
    if after < DOWN {
        TRAVEL * after / DOWN
    } else if after < DOWN + HELD {
        TRAVEL
    } else {
        TRAVEL * (1.0 - (after - DOWN - HELD) / UP).max(0.0)
    }
}

/// Makes the pedestal model being spawned on `entity` the button's: the
/// confetti's room, and its button and cover rigged once they are in.
pub fn rig(entity: &mut EntityCommands, pedestal: Pedestal) {
    entity.insert(Stand(pedestal)).observe(rigged);
}

/// The pedestal's `Button` and `Cover`, found by name, take the parts that
/// move them, where they rest.
fn rigged(
    ready: On<WorldInstanceReady>,
    mut commands: Commands,
    children: Query<&Children>,
    nodes: Query<(&Name, &Transform)>,
) {
    for entity in children.iter_descendants(ready.entity) {
        let Ok((name, transform)) = nodes.get(entity) else {
            continue;
        };
        match name.as_str() {
            "Button" => {
                commands.entity(entity).insert((
                    Part::Button,
                    Plunger {
                        rest: transform.translation,
                        since: None,
                    },
                ));
            }
            "Cover" => {
                commands.entity(entity).insert((
                    Part::Cover,
                    Lid {
                        shut: transform.rotation,
                        ..default()
                    },
                ));
            }
            _ => {}
        }
    }
}

/// The parts, and their meshes' bounds, for the aim.
#[derive(SystemParam)]
pub struct Parts<'w, 's> {
    parts: Query<'w, 's, (&'static Part, &'static Children)>,
    bounds: Query<'w, 's, &'static Aabb>,
}

impl Parts<'_, '_> {
    /// The nearest part `ray` meets within reach, by its meshes' bounds,
    /// and how far along it.
    pub fn aimed(&self, ray: Ray3d, places: &TransformHelper) -> Option<(f32, Part)> {
        self.parts
            .iter()
            .flat_map(|(&part, children)| children.iter().map(move |mesh| (part, mesh)))
            .filter_map(|(part, mesh)| {
                let aabb = self.bounds.get(mesh).ok()?;
                let place = places.compute_global_transform(mesh).ok()?;
                let bounds = Affine3A::from_scale_rotation_translation(
                    Vec3::from(aabb.half_extents) * 2.0,
                    Quat::IDENTITY,
                    aabb.center.into(),
                );
                let t = meets(ray, place.affine() * bounds)?;
                (t <= REACH).then_some((t, part))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
    }
}

/// How far along `ray` it first meets the unit box that `place` puts in
/// the world, if it does.
fn meets(ray: Ray3d, place: Affine3A) -> Option<f32> {
    let inverse = place.inverse();
    // Along the transformed direction, unnormalized, t is the same as along
    // the ray's.
    let origin = inverse.transform_point3(ray.origin);
    let direction = inverse.transform_vector3(*ray.direction);
    let (mut near, mut far) = (0.0_f32, f32::INFINITY);
    for axis in 0..3 {
        let (o, d) = (origin[axis], direction[axis]);
        if d.abs() < 1e-9 {
            if o.abs() > 0.5 {
                return None;
            }
            continue;
        }
        let (a, b) = ((-0.5 - o) / d, (0.5 - o) / d);
        near = near.max(a.min(b));
        far = far.min(a.max(b));
    }
    (near <= far).then_some(near)
}

/// A click on the cover lifts or lowers it, and one on the button, with the
/// cover lifted, presses it.
fn press(
    time: Res<Time<Real>>,
    mouse: Res<ButtonInput<MouseButton>>,
    aimed: Res<AimedPart>,
    mut lids: Query<&mut Lid>,
    mut plungers: Query<&mut Plunger>,
    mut pressed: MessageWriter<Pressed>,
) {
    let Some(part) = aimed.0 else {
        return;
    };
    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }
    let now = time.elapsed_secs();
    for mut lid in &mut lids {
        if lid.open && part == Part::Button {
            for mut plunger in &mut plungers {
                plunger.since = Some(now);
            }
            pressed.write(Pressed);
        } else {
            *lid = Lid {
                open: !lid.open,
                from: lid.angle(now),
                since: now,
                ..*lid
            };
        }
    }
}

/// The cover and the button where they are at this update, and another one
/// asked for while either moves. Timed from the click rather than by the
/// update's delta, which after the app has slept is long.
fn animate(
    time: Res<Time<Real>>,
    mut lids: Query<(&Lid, &mut Transform), Without<Plunger>>,
    mut plungers: Query<(&mut Plunger, &mut Transform), Without<Lid>>,
    mut redraw: Redraw,
) {
    let now = time.elapsed_secs();
    let mut moving = false;
    for (lid, mut transform) in &mut lids {
        let turned = transform.with_rotation(lid.shut * Quat::from_rotation_x(-lid.angle(now)));
        transform.set_if_neq(turned);
        moving |= now - lid.since < SWING;
    }
    for (mut plunger, mut transform) in &mut plungers {
        let Some(since) = plunger.since else {
            continue;
        };
        let after = now - since;
        let at = transform.with_translation(plunger.rest - Vec3::Y * depth(after));
        transform.set_if_neq(at);
        if after >= DOWN + HELD + UP {
            plunger.since = None;
        } else {
            moving = true;
        }
    }
    if moving {
        redraw.ask(["button"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ray_meets_a_turned_box() {
        let place = Transform::from_xyz(0.0, 1.0, -3.0)
            .with_rotation(Quat::from_rotation_y(0.5))
            .with_scale(Vec3::splat(0.2))
            .compute_affine();
        let ahead = Ray3d::new(Vec3::Y, Dir3::NEG_Z);
        let t = meets(ahead, place).expect("ahead");
        assert!(t > 2.85 && t < 2.9, "{t}");
        assert_eq!(meets(Ray3d::new(Vec3::Y, Dir3::Z), place), None, "behind");
        let past = Ray3d::new(Vec3::new(0.3, 1.0, 0.0), Dir3::NEG_Z);
        assert_eq!(meets(past, place), None, "beside");
    }

    #[test]
    fn the_cover_swings_and_the_button_springs_back() {
        let lid = Lid {
            open: true,
            from: 0.0,
            since: 1.0,
            ..default()
        };
        assert_eq!(lid.angle(1.0), 0.0);
        assert!((lid.angle(1.0 + SWING / 2.0) - OPEN / 2.0).abs() < 1e-5);
        assert_eq!(lid.angle(9.0), OPEN);

        assert_eq!(depth(0.0), 0.0);
        assert_eq!(depth(DOWN + HELD / 2.0), TRAVEL);
        assert!(depth(DOWN + HELD + UP).abs() < 1e-6);
    }

    /// The first click lifts the cover, the next presses the button, and a
    /// click on the lifted cover lowers it.
    #[test]
    fn a_click_lifts_the_cover_and_then_presses_the_button() {
        let mut app = App::new();
        app.init_resource::<Time<Real>>()
            .init_resource::<ButtonInput<MouseButton>>()
            .init_resource::<AimedPart>()
            .add_message::<Pressed>()
            .add_systems(Update, press);
        let lid = app.world_mut().spawn(Lid::default()).id();
        let plunger = app
            .world_mut()
            .spawn(Plunger {
                rest: Vec3::ZERO,
                since: None,
            })
            .id();
        let click = |app: &mut App, part| {
            app.world_mut().resource_mut::<AimedPart>().0 = Some(part);
            let mut mouse = app.world_mut().resource_mut::<ButtonInput<MouseButton>>();
            mouse.release_all();
            mouse.clear();
            mouse.press(MouseButton::Left);
            app.update();
        };

        let pressed = |app: &mut App| {
            app.world_mut()
                .resource_mut::<Messages<Pressed>>()
                .drain()
                .count()
        };

        click(&mut app, Part::Cover);
        assert!(app.world().get::<Lid>(lid).unwrap().open);
        assert!(app.world().get::<Plunger>(plunger).unwrap().since.is_none());
        assert_eq!(pressed(&mut app), 0);

        click(&mut app, Part::Button);
        assert!(app.world().get::<Plunger>(plunger).unwrap().since.is_some());
        assert!(app.world().get::<Lid>(lid).unwrap().open);
        assert_eq!(pressed(&mut app), 1, "the party");

        click(&mut app, Part::Cover);
        assert!(!app.world().get::<Lid>(lid).unwrap().open);
    }
}
