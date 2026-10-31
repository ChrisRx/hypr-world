//! What a press of the button sets off: a party blower's sound
//! (partyblower.wav, from Pixabay, under its own license, not MIT: see
//! partyblower-license.txt), and a burst of confetti out of the button,
//! all over the room. The bits tumble as they fly and fall, bounce off the
//! ceiling and the walls, settle on the floor or the pedestal's top, lie
//! there a while and then shrink away. Moved here by hand rather than by
//! the physics, which has no air to slow them.

use std::f32::consts::{FRAC_PI_2, TAU};

use bevy::prelude::*;

use super::{Pressed, Stand};
use crate::{house::Pedestal, scene::glowing, stats::Redraw, wake::WakeAt};

/// The blower: Pixabay's "partyblower" (226659), cut to the blow and made
/// mono WAV, the one format the app's audio reads.
const BLOWER: &[u8] = include_bytes!("partyblower.wav");

/// Bits a press throws, and the most there can be about at once.
const COUNT: usize = 360;
const MOST: usize = 2000;
/// A bit's size.
const BIT: Vec2 = Vec2::new(0.035, 0.05);
/// How far over the pedestal's top they come out.
const MOUTH: f32 = 0.12;
/// How fast they come out, the least and the most.
const SPEED: (f32, f32) = (6.0, 13.0);
/// How far out from straight up, as the share of their speed outward over
/// that upward, the least and the most.
const SPREAD: (f32, f32) = (0.35, 1.25);
/// Their fall, slowed by the air in proportion to their speed: they come
/// to fall at `GRAVITY / DRAG`.
const GRAVITY: f32 = 6.0;
const DRAG: f32 = 2.5;
/// How fast they tumble, in radians a second, at the most.
const SPIN: f32 = 14.0;
/// What a bounce off the ceiling or a wall leaves of their speed into it.
const BOUNCE: f32 = 0.3;
/// How long they lie once down, and how long they take to shrink away.
const LIE: f32 = 30.0;
const SHRINK: f32 = 1.5;
/// The longest step they move in one update, so a slow one does not throw
/// them through a wall.
const LONGEST_STEP: f32 = 0.05;

/// Paper colors, sRGB.
const COLORS: [Vec3; 8] = [
    Vec3::new(0.95, 0.2, 0.25),
    Vec3::new(1.0, 0.75, 0.1),
    Vec3::new(0.2, 0.55, 0.95),
    Vec3::new(0.25, 0.8, 0.35),
    Vec3::new(1.0, 0.45, 0.7),
    Vec3::new(1.0, 0.5, 0.1),
    Vec3::new(0.6, 0.35, 0.9),
    Vec3::new(0.95, 0.95, 0.9),
];

/// The sound, a bit, and the bits' colors, made once.
#[derive(Resource)]
pub(super) struct Party {
    sound: Handle<AudioSource>,
    bit: Handle<Mesh>,
    colors: Vec<Handle<StandardMaterial>>,
}

/// A bit of confetti: how fast it goes and tumbles, and when it came to
/// rest, by `Time<Real>`, once it has.
#[derive(Component)]
pub(super) struct Bit {
    velocity: Vec3,
    spin: Vec3,
    landed: Option<f32>,
}

pub(super) fn prepare(
    mut commands: Commands,
    mut sounds: ResMut<Assets<AudioSource>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let colors = COLORS
        .iter()
        .map(|&color| {
            // Either face, both lit.
            materials.add(StandardMaterial {
                cull_mode: None,
                double_sided: true,
                ..glowing(color, 0.45)
            })
        })
        .collect();
    commands.insert_resource(Party {
        sound: sounds.add(AudioSource {
            bytes: BLOWER.into(),
        }),
        bit: meshes.add(Rectangle::new(1.0, 1.0)),
        colors,
    });
}

/// On a press, the blower, and a burst of bits up and out of the button, on
/// the shell, which they go with.
pub(super) fn celebrate(
    mut commands: Commands,
    mut pressed: MessageReader<Pressed>,
    party: Res<Party>,
    stand: Query<(&Stand, &ChildOf)>,
    bits: Query<(), With<Bit>>,
) {
    if pressed.read().count() == 0 {
        return;
    }
    commands.spawn((AudioPlayer(party.sound.clone()), PlaybackSettings::DESPAWN));
    let Ok((Stand(pedestal), shell)) = stand.single() else {
        return;
    };
    let mut rng = fastrand::Rng::new();
    let mut between = |(least, most): (f32, f32)| least + rng.f32() * (most - least);
    let mouth = pedestal.top + Vec3::Y * MOUTH;
    for _ in 0..COUNT.min(MOST.saturating_sub(bits.iter().count())) {
        let around = between((0.0, TAU));
        let out = between(SPREAD);
        let direction = Vec3::new(around.sin() * out, 1.0, around.cos() * out).normalize();
        let spin = Vec3::new(
            between((-1.0, 1.0)),
            between((-1.0, 1.0)),
            between((-1.0, 1.0)),
        )
        .normalize_or(Vec3::X)
            * between((0.3, 1.0))
            * SPIN;
        let turned = Quat::from_euler(
            EulerRot::YXZ,
            between((0.0, TAU)),
            between((0.0, TAU)),
            between((0.0, TAU)),
        );
        let colors = party.colors.len();
        let color = party.colors[(between((0.0, colors as f32)) as usize).min(colors - 1)].clone();
        commands.spawn((
            ChildOf(shell.parent()),
            Bit {
                velocity: direction * between(SPEED),
                spin,
                landed: None,
            },
            Mesh3d(party.bit.clone()),
            MeshMaterial3d(color),
            Transform::from_translation(mouth)
                .with_rotation(turned)
                .with_scale(BIT.extend(1.0)),
        ));
    }
}

/// Moves the bits in the air, and shrinks away the ones that have lain long
/// enough. Another update is asked for while any moves, and a wake for the
/// first that is due to shrink.
pub(super) fn flutter(
    mut commands: Commands,
    time: Res<Time<Real>>,
    stand: Query<&Stand>,
    mut bits: Query<(Entity, &mut Bit, &mut Transform)>,
    mut redraw: Redraw,
    mut wake_at: ResMut<WakeAt>,
) {
    if bits.is_empty() {
        return;
    }
    let Ok(Stand(pedestal)) = stand.single() else {
        return;
    };
    let now = time.elapsed_secs();
    let dt = time.delta_secs().min(LONGEST_STEP);
    let mut rng = fastrand::Rng::new();
    let mut moving = false;
    let mut due: Option<f32> = None;
    for (entity, mut bit, mut transform) in &mut bits {
        if let Some(landed) = bit.landed {
            let shrinking = now - landed - LIE;
            if shrinking < 0.0 {
                due = Some(due.map_or(landed + LIE, |due| due.min(landed + LIE)));
            } else if shrinking >= SHRINK {
                commands.entity(entity).despawn();
            } else {
                transform.scale = (BIT * (1.0 - shrinking / SHRINK)).extend(1.0);
                moving = true;
            }
            continue;
        }
        moving = true;
        let was = transform.translation;
        let (at, velocity, ground) = fly(pedestal, was, bit.velocity, dt);
        bit.velocity = velocity;
        match ground {
            Some(ground) => {
                // Flat, a hair over what it lies on, and over the others.
                transform.translation = at.with_y(ground + 0.001 + rng.f32() * 0.003);
                transform.rotation =
                    Quat::from_rotation_y(rng.f32() * TAU) * Quat::from_rotation_x(-FRAC_PI_2);
                bit.landed = Some(now);
            }
            None => {
                transform.translation = at;
                transform.rotation =
                    (Quat::from_scaled_axis(bit.spin * dt) * transform.rotation).normalize();
            }
        }
    }
    if moving {
        redraw.ask(["confetti"]);
    }
    if let Some(due) = due {
        wake_at.at(due);
    }
}

/// A bit `dt` on from `was` at `velocity`, in the room `pedestal` stands
/// in: where it gets to, how fast it then goes, and the height of what it
/// came down on, if it did.
fn fly(pedestal: &Pedestal, was: Vec3, velocity: Vec3, dt: f32) -> (Vec3, Vec3, Option<f32>) {
    let mut velocity = velocity + (Vec3::NEG_Y * GRAVITY - velocity * DRAG) * dt;
    let mut at = was + velocity * dt;

    let ceiling = pedestal.floor.y + pedestal.height - 0.02;
    if at.y > ceiling {
        at.y = ceiling;
        velocity.y = -velocity.y.abs() * BOUNCE;
    }
    // The walls, as the circle inside them.
    let walls = pedestal.walls - 0.03;
    let out = (at - pedestal.floor).with_y(0.0);
    if out.length() > walls {
        let normal = out.normalize();
        at = (pedestal.floor + normal * walls).with_y(at.y);
        let into = velocity.dot(normal);
        if into > 0.0 {
            velocity -= normal * into * (1.0 + BOUNCE);
        }
    }

    // On the pedestal's top, coming down from over it, or on the floor.
    let over_top = (was - pedestal.floor).with_y(0.0).length() < Pedestal::HALF
        && (at - pedestal.floor).with_y(0.0).length() < Pedestal::HALF
        && was.y >= pedestal.top.y;
    let ground = if over_top {
        pedestal.top.y
    } else {
        pedestal.floor.y
    };
    let landed = (at.y <= ground).then_some(ground);
    (at, velocity, landed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pedestal() -> Pedestal {
        Pedestal {
            top: Vec3::new(0.0, 0.0, 0.0),
            yaw: 0.0,
            floor: Vec3::new(0.0, -1.0, 0.0),
            walls: 4.0,
            height: 3.2,
        }
    }

    /// Thrown out of the button, a bit flies, stays inside the room, and
    /// comes down on its floor.
    #[test]
    fn a_bit_flies_and_lands_in_the_room() {
        let p = pedestal();
        let mut at = p.top + Vec3::Y * MOUTH;
        let mut velocity = Vec3::new(10.0, 8.0, 0.0);
        let mut highest = at.y;
        for step in 0..2000 {
            let (next, v, ground) = fly(&p, at, velocity, 1.0 / 60.0);
            assert!(next.y <= p.floor.y + p.height, "through the ceiling");
            assert!(
                (next - p.floor).with_y(0.0).length() <= p.walls,
                "through a wall"
            );
            highest = highest.max(next.y);
            (at, velocity) = (next, v);
            if let Some(ground) = ground {
                assert_eq!(ground, p.floor.y);
                assert!(step > 60, "down after {step} steps");
                assert!(highest > p.top.y + 1.0, "never rose");
                return;
            }
        }
        panic!("never came down: {at}");
    }

    /// One that comes straight down over the pedestal lands on its top.
    #[test]
    fn a_bit_lands_on_the_pedestal() {
        let p = pedestal();
        let mut at = p.top + Vec3::new(0.1, 0.5, 0.0);
        let mut velocity = Vec3::ZERO;
        for _ in 0..600 {
            let (next, v, ground) = fly(&p, at, velocity, 1.0 / 60.0);
            if let Some(ground) = ground {
                assert_eq!(ground, p.top.y);
                return;
            }
            (at, velocity) = (next, v);
        }
        panic!("never came down: {at}");
    }
}
