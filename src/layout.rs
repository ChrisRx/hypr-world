//! Where everything goes, as pure functions of the desktop feed. Meters,
//! y up, the floor at y = 0, the room centered on the origin, the monitor's
//! longer side `frame_size` across, whatever its pixels. What hangs on a
//! frame is placed in the frame's own coordinates (`Frame::pose`).

use std::f32::consts::{FRAC_PI_2, PI};

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
/// Between frames along a wall, bezel to bezel.
const GAP: f32 = 1.0;
/// The bezel's bottom edge above the floor.
pub const SILL: f32 = 0.55;
/// Above a frame's bezel, at least.
const HEADROOM: f32 = 0.9;
const MIN_SPAN: f32 = 6.0;
const MIN_HEIGHT: f32 = 3.2;
pub const EYE_HEIGHT: f32 = 1.62;
/// The camera's vertical field of view.
pub const FOV_Y: f32 = 65.0 * PI / 180.0;

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
    /// The least inside size: x along the front and back walls, z along the
    /// sides, y up to the ceiling. `{ x, y, z }`, each 0 if left out.
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

/// A wall seen from inside the room.
#[derive(Clone, Copy, Debug)]
pub struct Wall {
    /// Into the room.
    pub normal: Vec3,
    /// Left to right along it, as seen from inside.
    pub right: Vec3,
    /// The rotation about y that turns a +Z facing quad to face the room.
    pub yaw: f32,
}

/// Front (z = -hz), right (x = +hx), back (z = +hz), left (x = -hx).
pub const WALLS: [Wall; 4] = [
    Wall {
        normal: Vec3::Z,
        right: Vec3::X,
        yaw: 0.0,
    },
    Wall {
        normal: Vec3::NEG_X,
        right: Vec3::Z,
        yaw: -FRAC_PI_2,
    },
    Wall {
        normal: Vec3::NEG_Z,
        right: Vec3::NEG_X,
        yaw: PI,
    },
    Wall {
        normal: Vec3::X,
        right: Vec3::NEG_Z,
        yaw: FRAC_PI_2,
    },
];

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub workspace: i64,
    pub wall: usize,
    /// The center of its screen, on the frame's face.
    pub center: Vec3,
    /// Along its wall, from the wall's middle.
    pub along: f32,
}

impl Frame {
    /// Where the frame hangs, which its workspace's `Transform` is (scene.rs):
    /// at the center of its screen, turned so that x runs along its wall to
    /// the right, y up, and z out into the room. Its parts, windows and
    /// layers are placed in these coordinates (`Layout::seat`).
    pub fn pose(&self) -> Isometry3d {
        Isometry3d::new(self.center, Quat::from_rotation_y(WALLS[self.wall].yaw))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub half_x: f32,
    pub half_z: f32,
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
    /// In workspace id order.
    pub frames: Vec<Frame>,
}

impl Layout {
    /// The layout for `workspaces` (ascending ids, no repeats) on a monitor
    /// of logical size `monitor`, each frame showing `area` of it.
    pub fn new(workspaces: &[i64], monitor: Vec2, area: Rect, settings: &Settings) -> Self {
        let area = if area.width() > 0.0 && area.height() > 0.0 {
            area
        } else {
            Rect::from_corners(Vec2::ZERO, monitor)
        };

        let k = settings.frame_size / monitor.max_element();
        let frame = area.size() * k;

        // A quarter each, the rest to the front, the back, then the right.
        let count = workspaces.len();
        let mut per_wall = [count / 4; 4];
        for wall in [0, 2, 1].into_iter().take(count % 4) {
            per_wall[wall] += 1;
        }

        let slot = frame.x + 2.0 * BEZEL;
        let span = |n: usize| {
            if n == 0 {
                0.0
            } else {
                n as f32 * slot + (n + 1) as f32 * GAP
            }
        };
        let min = MIN_SPAN.max(slot + 2.0 * GAP);

        let half_x = 0.5
            * span(per_wall[0])
                .max(span(per_wall[2]))
                .max(min)
                .max(settings.room_size.x);
        let half_z = 0.5
            * span(per_wall[1])
                .max(span(per_wall[3]))
                .max(min)
                .max(settings.room_size.z);
        let height = MIN_HEIGHT
            .max(SILL + 2.0 * BEZEL + frame.y + HEADROOM)
            .max(settings.room_size.y);

        let mut ids = workspaces.iter().copied();
        let mut frames = Vec::with_capacity(count);

        for (index, &n) in per_wall.iter().enumerate() {
            let wall = WALLS[index];
            let (plane, _) = halves(index, half_x, half_z);
            let total = n as f32 * slot + (n as f32 - 1.0) * GAP;

            for i in 0..n {
                let along = -total / 2.0 + slot / 2.0 + i as f32 * (slot + GAP);
                let center = -wall.normal * plane
                    + wall.right * along
                    + Vec3::Y * (SILL + BEZEL + frame.y / 2.0)
                    + wall.normal * FRAME_DEPTH;
                let workspace = ids.next().expect("one frame per workspace");
                frames.push(Frame {
                    workspace,
                    wall: index,
                    center,
                    along,
                });
            }
        }

        Self {
            half_x,
            half_z,
            height,
            frame,
            k,
            monitor,
            area,
            frames,
        }
    }

    pub fn frame_of(&self, workspace: i64) -> Option<&Frame> {
        self.frames.iter().find(|f| f.workspace == workspace)
    }

    /// Across the room from `wall` to the one facing it.
    pub fn depth(&self, wall: usize) -> f32 {
        2.0 * self.halves(wall).0
    }

    /// Half the room across from `wall` and half along it (`halves`).
    pub fn halves(&self, wall: usize) -> (f32, f32) {
        halves(wall, self.half_x, self.half_z)
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
        let wall = WALLS[frame.wall];
        let tan = (FOV_Y / 2.0).tan() / zoom;
        let d = self.monitor / 2.0 - self.area.center();
        let target = frame.center + wall.right * d.x * self.k - Vec3::Y * d.y * self.k;
        let back =
            (self.monitor.y * self.k / (2.0 * tan) + WINDOW_LIFT).min(self.depth(frame.wall) - 0.5);
        (target + wall.normal * back, target)
    }

    /// Standing well back from `frame`, at eye height, looking at it: three
    /// quarters of the way across the room's shorter side, and never closer
    /// than the near pose. Measured along the frame's own wall's depth
    /// instead, from an end wall of a long room that is its whole length and
    /// puts you near the far end; this way every frame is looked at from the
    /// same distance.
    pub fn far_pose(&self, frame: &Frame) -> (Vec3, Vec3) {
        let wall = WALLS[frame.wall];
        let across = (2.0 * self.half_x).min(2.0 * self.half_z);
        let (near, _) = self.near_pose(frame, 1.0);
        let back = (near - frame.center)
            .length()
            .max((0.75 * across).min(across - 1.0));
        let mut eye = frame.center + wall.normal * back;
        eye.y = EYE_HEIGHT;
        (eye, frame.center)
    }
}

/// Half a room `half_x` by `half_z` across from `wall`, its middle to the
/// wall's plane, and half along the wall: the front and back (even) walls
/// face along z.
fn halves(wall: usize, half_x: f32, half_z: f32) -> (f32, f32) {
    if wall.is_multiple_of(2) {
        (half_z, half_x)
    } else {
        (half_x, half_z)
    }
}

/// The workspaces that get a frame: the active one, even if empty, and every
/// one with a window, in id order.
pub fn framed(active: Option<i64>, with_windows: impl IntoIterator<Item = i64>) -> Vec<i64> {
    let mut ids: Vec<i64> = with_windows.into_iter().chain(active).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The id of the spare frame, an empty one after the rest whose workspace
/// does not exist yet: one past the highest numbered of `taken` (named
/// workspaces have negative ids), 1 with none. Resting the crosshair on it
/// makes the workspace.
pub fn spare(taken: impl IntoIterator<Item = i64>) -> i64 {
    taken.into_iter().filter(|&id| id > 0).max().unwrap_or(0) + 1
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

    /// 1504x1003 with a 40 px bar on top, five workspaces, 5.264 m across.
    #[test]
    fn five_workspaces_with_a_bar() {
        let work = Rect::new(0.0, 40.0, 1504.0, 1003.0);
        let l = Layout::new(
            &[1, 2, 3, 4, 5],
            Vec2::new(1504.0, 1003.0),
            work,
            &Settings {
                frame_size: 5.264,
                ..Settings::default()
            },
        );

        assert!(close(l.frame.x, 5.264) && close(l.frame.y, 3.3705));
        assert!(close(l.half_x, 7.004) && close(l.half_z, 3.752) && close(l.height, 5.0605));

        let walls: Vec<usize> = l.frames.iter().map(|f| f.wall).collect();
        assert_eq!(walls, [0, 0, 1, 2, 3]);

        let y = 2.35525;
        assert!(close3(l.frames[0].center, Vec3::new(-3.252, y, -3.692)));
        assert!(close3(l.frames[1].center, Vec3::new(3.252, y, -3.692)));
        assert!(close3(l.frames[2].center, Vec3::new(6.944, y, 0.0)));
        assert!(close3(l.frames[3].center, Vec3::new(0.0, y, 3.692)));
        assert!(close3(l.frames[4].center, Vec3::new(-6.944, y, 0.0)));

        // The right wall's frame: 2.76 m back, aimed 0.07 m above its center
        // (the bar shifts the work area down).
        let right = &l.frames[2];
        let (eye, target) = l.near_pose(right, 1.0);
        assert!(close3(target, right.center + Vec3::Y * 0.07));
        assert!(close((eye - target).length(), 2.760));

        // Three quarters of the room's width back, 5.628 m, not three
        // quarters of its length (x = -3.562), which is past the middle.
        let (eye, _) = l.far_pose(right);
        assert!(close3(eye, Vec3::new(1.316, 1.62, 0.0)));

        // A frame on a long wall is looked at from the same distance.
        let front = &l.frames[0];
        let (eye, _) = l.far_pose(front);
        assert!(close((eye - front.center).with_y(0.0).length(), 5.628));
    }

    /// 1920x1080 with a 30 px bar, four workspaces, one per wall, 6.72 m
    /// across.
    #[test]
    fn four_workspaces_one_per_wall() {
        let work = Rect::new(0.0, 30.0, 1920.0, 1080.0);
        let l = Layout::new(
            &[1, 2, 3, 4],
            Vec2::new(1920.0, 1080.0),
            work,
            &Settings {
                frame_size: 6.72,
                ..Settings::default()
            },
        );

        assert!(close(l.frame.x, 6.72) && close(l.frame.y, 3.675));
        assert!(
            close(2.0 * l.half_x, 8.96) && close(2.0 * l.half_z, 8.96) && close(l.height, 5.365)
        );

        let front = &l.frames[0];
        let (eye, target) = l.near_pose(front, 1.0);
        assert!(close(target.y, 2.56) && close(eye.y, 2.56));
        assert!(close(eye.z - front.center.z, 2.97));

        let (eye, _) = l.far_pose(front);
        assert!(close((eye - front.center).with_y(0.0).length(), 6.72));
        assert!(close(eye.y, 1.62));
    }

    /// A frame's own coordinates: x along its wall to the right, as seen
    /// from inside, y up, and z out into the room, on every wall.
    #[test]
    fn a_frame_faces_the_room() {
        let monitor = Vec2::new(1920.0, 1080.0);
        let l = Layout::new(
            &[1, 2, 3, 4],
            monitor,
            Rect::from_corners(Vec2::ZERO, monitor),
            &Settings::default(),
        );
        for frame in &l.frames {
            let (wall, pose) = (WALLS[frame.wall], frame.pose());
            let to = |local: Vec3| Vec3::from(pose.transform_point(local));
            assert!(close3(to(Vec3::ZERO), frame.center), "wall {}", frame.wall);
            assert!(
                close3(to(Vec3::X), frame.center + wall.right),
                "wall {}",
                frame.wall
            );
            assert!(
                close3(to(Vec3::Y), frame.center + Vec3::Y),
                "wall {}",
                frame.wall
            );
            assert!(
                close3(to(Vec3::Z), frame.center + wall.normal),
                "wall {}",
                frame.wall
            );
        }
    }

    #[test]
    fn a_window_sits_where_it_is_in_2d() {
        let work = Rect::new(0.0, 0.0, 1000.0, 600.0);
        let l = Layout::new(&[1], Vec2::new(1000.0, 600.0), work, &Settings::default());

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
        let l = Layout::new(&[1], monitor, area, &Settings::default());
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
        assert_eq!(spare([1, 2, 4]), 5);
        assert_eq!(spare([-1337, 3]), 4, "named workspaces do not count");
        assert_eq!(spare([]), 1);
    }
}
