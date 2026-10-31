//! Where everything goes, as pure functions of the desktop feed. Meters,
//! y up, the upper floor at y = 0 and centered on the origin, the monitor's
//! longer side `frame_size` across, whatever its pixels. What hangs on a
//! frame is placed in the frame's own coordinates (`Frame::pose`).
//!
//! The house is two rooms, each a regular polygon, joined by a half flight
//! of stairs. Upstairs is a seat for each workspace from 1 to the seat
//! count, whether it exists or not, one to a side, and one side more for
//! the doorway onto the stairs and a window beside it. Downstairs, turned to
//! face it at the bottom of the stairs, is every other workspace, the same
//! way. Seat 1 is just right of the doorway seen from inside, and the
//! numbers go on round to the right, so that standing in the middle facing
//! away from the stairs they read left to right.

use std::f32::consts::{PI, TAU};

use bevy::math::{Isometry3d, Quat, Rect, Vec2, Vec3};
use serde::Deserialize;

/// The bezel's width around a frame's screen.
pub const BEZEL: f32 = 0.12;
/// How far a frame's face stands off the wall.
pub const FRAME_DEPTH: f32 = 0.06;
/// The screen (wallpaper) in front of the frame's face.
pub const SCREEN_LIFT: f32 = 0.0015;
/// The layers under the windows (the wallpaper) in front of the screen,
/// background then bottom.
const LAYER_LIFT: f32 = 0.001;
/// Windows in front of the screen, and each rank in front of the one below.
const WINDOW_LIFT: f32 = 0.005;
const RANK_LIFT: f32 = 0.008;
/// A window's surfaces past its own (its subsurfaces and popups, windows.rs
/// `SurfaceQuad`), each in front of the one below: 64 of them still stay
/// under the next rank.
pub const SURFACE_LIFT: f32 = RANK_LIFT / 64.0;
/// Either side of a frame, bezel to corner.
const GAP: f32 = 1.0;
/// The bezel's bottom edge above the floor.
pub const SILL: f32 = 0.55;
/// Above a frame's bezel, at least.
const HEADROOM: f32 = 0.9;
/// The least across a room, side to side.
const MIN_SPAN: f32 = 6.0;
const MIN_HEIGHT: f32 = 3.2;
pub const EYE_HEIGHT: f32 = 1.62;
/// The camera's vertical field of view.
pub const FOV_Y: f32 = 65.0 * PI / 180.0;
pub const WALL_THICKNESS: f32 = 0.25;
/// A room has at least this many sides, the stairs' one among them.
const MIN_SIDES: usize = 4;
/// The doorway onto the stairs: its width, its top above the upper floor,
/// and the plain wall between it and the corner on its right.
pub const DOOR_WIDTH: f32 = 1.6;
const DOOR_TOP: f32 = 2.5;
const DOOR_EDGE: f32 = 0.6;
/// The wall left beside the doorway for a window (house.rs `openings`): the
/// widest, its margins, and the doorway's casing.
const BESIDE_DOOR: f32 = 4.5;
/// The half flight: how far down it goes, in how many steps, each this
/// deep, with a landing at either end past the walls.
pub const DROP: f32 = 1.4;
pub const STEPS: usize = 10;
pub const TREAD: f32 = 0.3;
pub const LANDING: f32 = 0.8;

/// The house's dimensions, from the app's settings (`world` in
/// hl.plugin.hypr_world.config).
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The monitor's longer side across a frame, in meters, so that a
    /// monitor of more logical px has smaller windows in a frame of the
    /// same size, as on its screen. A frame that shows less than the whole
    /// monitor is that much smaller.
    pub frame_size: f32,
    /// The least inside size: the larger of x and z across each room, side
    /// to side, and y up to the ceiling. `{ x, y, z }`, each 0 if left out.
    #[serde(deserialize_with = "xyz")]
    pub room_size: Vec3,
}

/// A `Vec3` as a table of `x`, `y` and `z`, each 0 if left out.
fn xyz<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec3, D::Error> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Xyz {
        x: f32,
        y: f32,
        z: f32,
    }
    let Xyz { x, y, z } = Xyz::deserialize(deserializer)?;
    Ok(Vec3::new(x, y, z))
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            frame_size: 5.04,
            room_size: Vec3::ZERO,
        }
    }
}

/// A wall seen from inside its room.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Wall {
    /// Into the room.
    pub normal: Vec3,
    /// Left to right along it, as seen from inside.
    pub right: Vec3,
    /// The rotation about y that turns a +Z facing quad to face the room,
    /// and +X to run along it to the right.
    pub yaw: f32,
}

impl Wall {
    /// The wall that runs along `right`, left to right seen from inside.
    fn along(right: Vec3) -> Self {
        let normal = Vec3::new(-right.z, 0.0, right.x);
        Self {
            normal,
            right,
            yaw: f32::atan2(normal.x, normal.z),
        }
    }
}

/// One side of a room: its wall's inside face.
#[derive(Clone, Debug, PartialEq)]
pub struct Side {
    pub wall: Wall,
    /// The middle of its inside face, on the floor.
    pub middle: Vec3,
    /// Half its inside length, corner to corner.
    pub half: f32,
    /// The doorway onto the stairs, on the stairs' side.
    pub door: Option<Door>,
}

/// A doorway: its middle along its side, from the side's middle, and its
/// width and its height above the room's floor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Door {
    pub along: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Room {
    /// The middle of its floor.
    pub floor: Vec3,
    /// Its inside corners, on the floor: side i runs from corner i to the
    /// next, left to right seen from inside. Side 0 is the stairs'.
    pub corners: Vec<Vec3>,
    pub sides: Vec<Side>,
}

impl Room {
    /// The room with corners `local` round its floor's middle (`corners`),
    /// at `floor`, turned half round if `turned`, with the doorway in
    /// side 0 `door_height` tall.
    fn new(local: &[Vec2], floor: Vec3, turned: bool, door_height: f32) -> Self {
        let corners: Vec<Vec3> = local.iter().map(|&c| floor + flat(c, turned)).collect();
        let n = corners.len();
        let sides = (0..n)
            .map(|i| {
                let (a, b) = (corners[i], corners[(i + 1) % n]);
                let half = a.distance(b) / 2.0;
                Side {
                    wall: Wall::along((b - a) / (2.0 * half)),
                    middle: (a + b) / 2.0,
                    half,
                    door: (i == 0).then(|| Door {
                        along: door_along(half),
                        width: DOOR_WIDTH,
                        height: door_height,
                    }),
                }
            })
            .collect();
        Self {
            floor,
            corners,
            sides,
        }
    }

    /// From the middle of the room to `side`.
    pub fn apothem(&self, side: usize) -> f32 {
        let side = &self.sides[side];
        (self.floor - side.middle).dot(side.wall.normal)
    }

    /// From the middle of the room to its corners.
    pub fn radius(&self) -> f32 {
        self.corners
            .iter()
            .map(|c| c.distance(self.floor))
            .fold(0.0, f32::max)
    }

    /// From `side` straight across the room to its far end.
    pub fn depth(&self, side: usize) -> f32 {
        let side = &self.sides[side];
        self.corners
            .iter()
            .map(|c| (*c - side.middle).dot(side.wall.normal))
            .fold(0.0, f32::max)
    }

    /// The tangent of half the turn at each corner: how far a wall's outside
    /// face reaches past its inside corner, per meter of thickness, to meet
    /// the next wall's.
    pub fn bevel(&self) -> f32 {
        (PI / self.corners.len() as f32).tan()
    }

    /// The middle of the doorway onto the stairs, on the inside face, on the
    /// floor.
    pub fn door(&self) -> Vec3 {
        let side = &self.sides[0];
        let along = side.door.map_or(0.0, |d| d.along);
        side.middle + side.wall.right * along
    }
}

/// A point of a room's plan as a point in the world, turned half round if
/// `turned`.
fn flat(point: Vec2, turned: bool) -> Vec3 {
    let point = if turned { -point } else { point };
    Vec3::new(point.x, 0.0, point.y)
}

/// Where the doorway is on a stairs' side `half` long either way: by the
/// corner on its right, so seat 1 is just past it.
fn door_along(half: f32) -> f32 {
    half - DOOR_EDGE - DOOR_WIDTH / 2.0
}

/// The plan of a regular room of `sides` sides, each `length` long, round
/// its middle, x and z: in order left to right seen from inside, side 0's
/// middle on +z. Larger if it would be less than `least` from the middle to
/// a side.
fn corners(sides: usize, length: f32, least: f32) -> Vec<Vec2> {
    let turn = TAU / sides as f32;
    let apothem = length / (2.0 * (turn / 2.0).tan());
    let radius = apothem.max(least) / (turn / 2.0).cos();
    (0..sides)
        .map(|i| {
            let at = (i as f32 - 0.5) * turn;
            radius * Vec2::new(-at.sin(), at.cos())
        })
        .collect()
}

/// The half flight between the rooms: from the middle of the upper doorway
/// on the upper room's inside face, `down` across to the lower room's
/// inside face, `length` away and `DROP` below.
#[derive(Clone, Debug, PartialEq)]
pub struct Stairs {
    pub top: Vec3,
    /// Level, the way down.
    pub down: Vec3,
    /// Level, to the right going down.
    pub across: Vec3,
    pub length: f32,
    /// The passage's ceiling, the top of the upper doorway.
    pub ceiling: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub workspace: i64,
    /// Its room, 0 upstairs, and its side there.
    pub room: usize,
    pub side: usize,
    pub wall: Wall,
    /// The center of its screen, on the frame's face.
    pub center: Vec3,
}

impl Frame {
    /// Where the frame hangs, which its workspace's `Transform` is (scene.rs):
    /// at the center of its screen, turned so that x runs along its wall to
    /// the right, y up, and z out into the room. Its parts, windows and
    /// layers are placed in these coordinates (`Layout::seat`).
    pub fn pose(&self) -> Isometry3d {
        Isometry3d::new(self.center, Quat::from_rotation_y(self.wall.yaw))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// Each room's, floor to ceiling.
    pub height: f32,
    /// A frame's screen: the part of the monitor it shows, in meters.
    pub frame: Vec2,
    /// Meters per logical px.
    pub k: f32,
    /// The monitor's logical size.
    pub monitor: Vec2,
    /// The part of the monitor a frame shows, monitor-local. The app shows
    /// all of it, bar and wallpaper too (layers.rs); some tests show only the
    /// work area.
    pub area: Rect,
    /// Upstairs, then downstairs.
    pub rooms: [Room; 2],
    pub stairs: Stairs,
    /// Upstairs in seat order, then downstairs.
    pub frames: Vec<Frame>,
}

impl Layout {
    /// The layout for `seats`, the workspaces upstairs in seat order, and
    /// `overflow`, those downstairs, on a monitor of logical size `monitor`,
    /// each frame showing `area` of it.
    pub fn new(
        seats: &[i64],
        overflow: &[i64],
        monitor: Vec2,
        area: Rect,
        settings: &Settings,
    ) -> Self {
        let area = if area.width() > 0.0 && area.height() > 0.0 {
            area
        } else {
            Rect::from_corners(Vec2::ZERO, monitor)
        };

        let k = settings.frame_size / monitor.max_element();
        let frame = area.size() * k;

        // Every side as long as a frame's, or the doorway's and a window's,
        // so the room stays regular and every frame is as far from its
        // middle.
        let slot = frame.x + 2.0 * BEZEL;
        let length = (slot + 2.0 * GAP).max(DOOR_EDGE + DOOR_WIDTH + BESIDE_DOOR);
        let least = 0.5 * MIN_SPAN.max(settings.room_size.x).max(settings.room_size.z);
        let height = MIN_HEIGHT
            .max(SILL + 2.0 * BEZEL + frame.y + HEADROOM)
            .max(settings.room_size.y);
        let plan = |count: usize| corners((count + 1).max(MIN_SIDES), length, least);

        let upstairs = Room::new(&plan(seats.len()), Vec3::ZERO, false, DOOR_TOP);
        let top = upstairs.door();
        let down = -upstairs.sides[0].wall.normal;
        let length_down = 2.0 * (WALL_THICKNESS + LANDING) + STEPS as f32 * TREAD;

        // Downstairs faces back up the stairs, its own doorway at the bottom:
        // turned half round, its doorway, as far round its middle as the
        // upper one is, comes out the other way.
        let below = plan(overflow.len());
        let half = below[0].distance(below[1]) / 2.0;
        let door =
            (below[0] + below[1]) / 2.0 + (below[1] - below[0]) / (2.0 * half) * door_along(half);
        let bottom = top + down * length_down - Vec3::Y * DROP;
        let downstairs = Room::new(&below, bottom - flat(door, true), true, DOOR_TOP + DROP);

        let stairs = Stairs {
            top,
            down,
            across: down.cross(Vec3::Y),
            length: length_down,
            ceiling: DOOR_TOP,
        };

        let rooms = [upstairs, downstairs];
        let mut frames = Vec::with_capacity(seats.len() + overflow.len());
        for (index, workspaces) in [seats, overflow].into_iter().enumerate() {
            let room = &rooms[index];
            for (i, &workspace) in workspaces.iter().enumerate() {
                let side = &room.sides[i + 1];
                frames.push(Frame {
                    workspace,
                    room: index,
                    side: i + 1,
                    wall: side.wall,
                    center: side.middle
                        + side.wall.normal * FRAME_DEPTH
                        + Vec3::Y * (SILL + BEZEL + frame.y / 2.0),
                });
            }
        }

        Self {
            height,
            frame,
            k,
            monitor,
            area,
            rooms,
            stairs,
            frames,
        }
    }

    pub fn frame_of(&self, workspace: i64) -> Option<&Frame> {
        self.frames.iter().find(|f| f.workspace == workspace)
    }

    /// Where a window with monitor-local box `rect` hangs on its frame, in
    /// the frame's coordinates (`Frame::pose`): its center and size. Rank 0
    /// is tiled, then floating windows in stacking order, then the top
    /// layers (the bar, layers.rs), then a fullscreen window, each a little
    /// further out.
    pub fn seat(&self, rect: Rect, rank: u32) -> (Vec3, Vec2) {
        self.place(rect, WINDOW_LIFT + RANK_LIFT * rank as f32)
    }

    /// Where a layer under the windows (the wallpaper) goes on its frame,
    /// as `seat`: between the screen and the windows, the bottom level over
    /// the background.
    pub fn under(&self, rect: Rect, bottom: bool) -> (Vec3, Vec2) {
        self.place(
            rect,
            SCREEN_LIFT + LAYER_LIFT * if bottom { 2.0 } else { 1.0 },
        )
    }

    fn place(&self, rect: Rect, lift: f32) -> (Vec3, Vec2) {
        // From the middle of the area the frame shows, in meters: 2D's y
        // runs down, the frame's up.
        let c = (rect.center() - self.area.center()) * self.k;
        (Vec3::new(c.x, -c.y, lift), rect.size() * self.k)
    }

    /// The pose from which `frame` fills the screen pixel for pixel, as the
    /// 2D workspace does: (eye, the point it looks at). A frame that shows
    /// the whole monitor is looked at in its center.
    pub fn near_pose(&self, frame: &Frame, zoom: f32) -> (Vec3, Vec3) {
        let tan = (FOV_Y / 2.0).tan() / zoom;
        let d = self.monitor / 2.0 - self.area.center();
        let target = frame.center + frame.wall.right * d.x * self.k - Vec3::Y * d.y * self.k;
        let depth = self.rooms[frame.room].depth(frame.side);
        let back = (self.monitor.y * self.k / (2.0 * tan) + WINDOW_LIFT).min(depth - 0.5);
        (target + frame.wall.normal * back, target)
    }

    /// Standing in the middle of the frame's room, at eye height, looking at
    /// it, or no nearer than the near pose: every frame is looked at from
    /// the same distance.
    pub fn far_pose(&self, frame: &Frame) -> (Vec3, Vec3) {
        let room = &self.rooms[frame.room];
        let (near, _) = self.near_pose(frame, 1.0);
        let back = (near - frame.center)
            .length()
            .max(room.apothem(frame.side) - FRAME_DEPTH);
        let mut eye = frame.center + frame.wall.normal * back;
        eye.y = room.floor.y + EYE_HEIGHT;
        (eye, frame.center)
    }
}

/// The workspaces that are about: the active one, even if empty, and every
/// one with a window, in id order.
pub fn framed(active: Option<i64>, with_windows: impl IntoIterator<Item = i64>) -> Vec<i64> {
    let mut ids: Vec<i64> = with_windows.into_iter().chain(active).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The workspaces downstairs: those of `ids` with no seat of the `seats`,
/// named ones (below 0) first, then by number.
pub fn overflow(seats: u32, ids: impl IntoIterator<Item = i64>) -> Vec<i64> {
    let mut ids: Vec<i64> = ids
        .into_iter()
        .filter(|&id| !(1..=i64::from(seats)).contains(&id))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 2e-3
    }

    fn close3(a: Vec3, b: Vec3) -> bool {
        close(a.x, b.x) && close(a.y, b.y) && close(a.z, b.z)
    }

    /// 1920x1080, 6.72 m across, eight seats.
    fn eight() -> Layout {
        let monitor = Vec2::new(1920.0, 1080.0);
        Layout::new(
            &[1, 2, 3, 4, 5, 6, 7, 8],
            &[],
            monitor,
            Rect::from_corners(Vec2::ZERO, monitor),
            &Settings {
                frame_size: 6.72,
                ..Settings::default()
            },
        )
    }

    /// Eight seats make nine sides, every one as long, the stairs' behind
    /// the middle, and seat 1 on its right seen from inside, the numbers
    /// going round to the right: every frame as far from the middle and
    /// facing it.
    #[test]
    fn eight_seats_and_the_stairs_go_round_a_room() {
        let l = eight();
        let up = &l.rooms[0];
        assert_eq!(up.sides.len(), 9);
        // 6.96 m of frame and 2 m of gap.
        assert!(up.sides.iter().all(|s| close(s.half, 4.48)));
        assert!(
            close3(up.sides[0].wall.normal, Vec3::NEG_Z),
            "the stairs behind"
        );
        assert!(close(up.apothem(0), 12.309));

        let sides: Vec<usize> = l.frames.iter().map(|f| f.side).collect();
        assert_eq!(sides, [1, 2, 3, 4, 5, 6, 7, 8]);
        for frame in &l.frames {
            let flat = frame.center.with_y(0.0);
            assert!(close(flat.length(), 12.309 - FRAME_DEPTH));
            assert!(
                close3(frame.wall.normal, -flat.normalize()),
                "facing the middle"
            );
        }
        // Seat 1 on the left of the stairs seen from the middle facing away
        // from them, and seats 4 and 5 either side of straight ahead.
        assert!(l.frames[0].center.x < 0.0 && l.frames[0].center.z > 0.0);
        assert!(close(l.frames[3].center.x, -l.frames[4].center.x));
        assert!(l.frames[3].center.z < 0.0 && l.frames[3].center.x < 0.0);

        // The doorway by the corner on seat 1's side.
        let door = up.sides[0].door.expect("a doorway");
        assert!(close(door.along, 4.48 - 0.6 - 0.8) && close(door.height, 2.5));
        assert!(up.door().x < 0.0);
    }

    /// Filling the screen: as far back as the field of view takes. From
    /// afar: the middle of the room, at eye height.
    #[test]
    fn a_frame_is_looked_at_from_the_middle() {
        let l = eight();
        let front = &l.frames[3];
        let (eye, target) = l.near_pose(front, 1.0);
        assert!(close3(target, front.center));
        assert!(close((eye - target).length(), 2.972));

        for frame in &l.frames {
            let (eye, target) = l.far_pose(frame);
            assert!(close3(eye, Vec3::Y * EYE_HEIGHT), "{eye}");
            assert!(close3(target, frame.center));
        }
    }

    /// Few seats still make four sides, and the room grows to `room_size`.
    #[test]
    fn a_room_has_four_sides_at_least() {
        let monitor = Vec2::new(1000.0, 600.0);
        let whole = Rect::from_corners(Vec2::ZERO, monitor);
        let l = Layout::new(&[1], &[], monitor, whole, &Settings::default());
        assert_eq!(l.rooms[0].sides.len(), 4);
        assert_eq!(l.rooms[1].sides.len(), 4, "an empty room downstairs");

        let l = Layout::new(
            &[1, 2, 3, 4],
            &[],
            monitor,
            whole,
            &Settings {
                room_size: Vec3::new(30.0, 6.0, 0.0),
                ..Settings::default()
            },
        );
        assert_eq!(l.rooms[0].sides.len(), 5);
        assert!((0..5).all(|side| close(l.rooms[0].apothem(side), 15.0)));
        assert!(close(l.height, 6.0));
    }

    /// The rest of the workspaces are downstairs, the bottom of the stairs a
    /// half flight down and across from the top, where the lower room's own
    /// doorway is, that room wholly past the upper one's stairs side.
    #[test]
    fn the_rest_are_down_the_stairs() {
        let monitor = Vec2::new(1920.0, 1080.0);
        let l = Layout::new(
            &[1, 2, 3],
            &[-99, 11, 12, 13, 14],
            monitor,
            Rect::from_corners(Vec2::ZERO, monitor),
            &Settings::default(),
        );
        let (up, down) = (&l.rooms[0], &l.rooms[1]);
        assert_eq!(down.sides.len(), 6);
        assert!(close(down.floor.y, -DROP));

        let stairs = &l.stairs;
        assert!(close3(stairs.top, up.door()));
        assert!(close3(
            stairs.top + stairs.down * stairs.length - Vec3::Y * DROP,
            down.door()
        ));
        assert!(
            close3(down.sides[0].wall.normal, stairs.down),
            "its stairs side at the bottom of the stairs"
        );
        assert!(close(stairs.length, 2.0 * (WALL_THICKNESS + LANDING) + 3.0));
        assert!(close3(stairs.across, Vec3::NEG_X));

        // Past the upper stairs side's outside face.
        let past = |p: Vec3| (p - up.sides[0].middle).dot(stairs.down);
        assert!(down.corners.iter().all(|&c| past(c) >= WALL_THICKNESS));

        let workspaces: Vec<(i64, usize, usize)> = l
            .frames
            .iter()
            .map(|f| (f.workspace, f.room, f.side))
            .collect();
        assert_eq!(
            workspaces,
            [
                (1, 0, 1),
                (2, 0, 2),
                (3, 0, 3),
                (-99, 1, 1),
                (11, 1, 2),
                (12, 1, 3),
                (13, 1, 4),
                (14, 1, 5)
            ]
        );
        let below = l.frame_of(11).expect("a frame");
        let (eye, _) = l.far_pose(below);
        assert!(close3(eye, down.floor + Vec3::Y * EYE_HEIGHT));
    }

    /// A frame's own coordinates: x along its wall to the right, as seen
    /// from inside, y up, and z out into the room, on every side.
    #[test]
    fn a_frame_faces_the_room() {
        let monitor = Vec2::new(1920.0, 1080.0);
        let l = Layout::new(
            &[1, 2, 3, 4, 5],
            &[6, 7],
            monitor,
            Rect::from_corners(Vec2::ZERO, monitor),
            &Settings::default(),
        );
        for frame in &l.frames {
            let (wall, pose) = (frame.wall, frame.pose());
            let to = |local: Vec3| Vec3::from(pose.transform_point(local));
            let name = frame.workspace;
            assert!(close3(to(Vec3::ZERO), frame.center), "{name}");
            assert!(close3(to(Vec3::X), frame.center + wall.right), "{name}");
            assert!(close3(to(Vec3::Y), frame.center + Vec3::Y), "{name}");
            assert!(close3(to(Vec3::Z), frame.center + wall.normal), "{name}");
        }
    }

    #[test]
    fn a_window_sits_where_it_is_in_2d() {
        let work = Rect::new(0.0, 0.0, 1000.0, 600.0);
        let l = Layout::new(
            &[1],
            &[],
            Vec2::new(1000.0, 600.0),
            work,
            &Settings::default(),
        );

        // The whole work area: centered on the frame, the frame's size.
        let (center, size) = l.seat(work, 0);
        assert!(close3(center, Vec3::Z * 0.005));
        assert!(close(size.x, l.frame.x) && close(size.y, l.frame.y));

        // The top-left quarter, floating and one rank out.
        let (center, _) = l.seat(Rect::new(0.0, 0.0, 500.0, 300.0), 1);
        assert!(close3(
            center,
            Vec3::new(-l.frame.x / 4.0, l.frame.y / 4.0, 0.013)
        ));
    }

    /// The app's frames show the whole monitor: the wallpaper under the
    /// windows, the bar along the top over them.
    #[test]
    fn a_frame_shows_the_whole_monitor() {
        let monitor = Vec2::new(1440.0, 960.0);
        let area = Rect::from_corners(Vec2::ZERO, monitor);
        let l = Layout::new(&[1], &[], monitor, area, &Settings::default());
        let frame = &l.frames[0];

        let (center, size) = l.under(area, false);
        assert!(close(size.x, l.frame.x) && close(size.y, l.frame.y));
        assert!(center.z > SCREEN_LIFT && center.z < WINDOW_LIFT);

        let (center, size) = l.seat(Rect::new(0.0, 0.0, 1440.0, 54.0), 1);
        assert!(close(center.y, (480.0 - 27.0) * l.k) && close(size.y, 54.0 * l.k));

        let (_, target) = l.near_pose(frame, 1.0);
        assert!(close3(target, frame.center));
    }

    #[test]
    fn framed_workspaces() {
        assert_eq!(framed(Some(4), [2, 1, 2]), [1, 2, 4]);
        assert_eq!(framed(None, []), Vec::<i64>::new());
        assert_eq!(overflow(10, [3, 12, -1337, 11, 12, 10]), [-1337, 11, 12]);
        assert_eq!(overflow(0, [2, 1]), [1, 2]);
    }
}
