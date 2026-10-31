//! The house around the frames, as plain geometry: each room's floor and
//! ceiling, slabs its shape, its walls with an opening onto the panorama
//! wherever a stretch of wall has room left and the doorway onto the
//! stairs, trim, the ceiling lamps and their lights; the half flight of
//! stairs between the rooms, and the sign over the way down. Every piece is
//! a box turned about y to its wall, and the ramp under the stairs tilted
//! down them too. The furniture is models (src/models): the chandelier
//! upstairs, the side table and the plant on it, and the pedestal
//! downstairs with the button (button.rs), placed here, each with a box
//! that the player bumps into. And a frame's bezel, and where its label
//! goes, in the frame's own coordinates (layout.rs `Frame::pose`).

use bevy::math::{Quat, Vec2, Vec3};

use crate::layout::{
    BEZEL, DOOR_WIDTH, DROP, FRAME_DEPTH, LANDING, Layout, Room, STEPS, Side, TREAD, WALL_THICKNESS,
};

/// The floor's and the ceiling's.
const SLAB: f32 = 0.3;

// Openings, cut into any stretch of wall a frame or the doorway does not
// use.
const OPENING_SILL: f32 = 0.8;
/// And never closer than 0.8 to the ceiling.
const OPENING_TOP: f32 = 3.4;
const OPENING_MAX: f32 = 3.0;
const OPENING_MIN: f32 = 1.2;
/// Plain wall kept either side of an opening.
const OPENING_MARGIN: f32 = 0.6;
/// And around every frame.
const FRAME_CLEAR: f32 = 0.3;
/// And either side of the doorway, for its casing.
const DOOR_CLEAR: f32 = 0.15;

/// An opening's sill board, as thick as this.
const SILL: f32 = 0.05;
/// An opening's frame bars.
const TRIM: f32 = 0.07;
const MULLION: f32 = 0.05;
const BASEBOARD: f32 = 0.12;
const CROWN: f32 = 0.10;
/// How far trim stands off the wall.
const TRIM_PROJECTION: f32 = 0.03;

/// One font8x8 pixel of a label, at most, and the gap above the bezel.
const GLYPH_PIXEL: f32 = 0.045;
const LABEL_GAP: f32 = 0.14;
/// Labels stand this far off the wall.
const LABEL_LIFT: f32 = 0.006;

/// The sign over the way down: its lines, one font8x8 pixel of them, the
/// gap between the lines, the board's margin round them and its thickness,
/// and its gap above the doorway. Two lines, to be no wider than the
/// doorway's casing.
const SIGN: [&str; 2] = ["Other", "Workspaces"];
const SIGN_PIXEL: f32 = 0.018;
const SIGN_LEADING: f32 = 0.04;
const SIGN_MARGIN: f32 = 0.06;
const SIGN_BOARD: f32 = 0.04;
const SIGN_GAP: f32 = 0.1;

/// A lamp in the middle of each ceiling (the chandelier upstairs), and a
/// ring of them halfway out to the walls, one per this many meters round, at most this many.
const LIGHT_SPACING: f32 = 5.0;
const RING_LAMPS: f32 = 8.0;
/// The lamp over the stairs, dimmer.
const STAIRS_LAMP: f32 = 0.35;

/// The middle of the chandelier's hub hangs this far below the ceiling, but
/// no lower than this over the floor, and low enough for its top candles
/// to clear the ceiling. The model hangs from a rod, its origin this far
/// over its hub.
const CHANDELIER_DROP: f32 = 1.6;
const CHANDELIER_LOWEST: f32 = 2.5;
const CHANDELIER_TOP: f32 = 0.7;
const CHANDELIER_HUB: f32 = 0.25;

/// The pedestal downstairs: how high its top is, and how wide.
const PEDESTAL_HEIGHT: f32 = 1.0;
const PEDESTAL_TOP: f32 = 0.5;

/// The side table: how high, wide and deep.
const TABLE: Vec3 = Vec3::new(0.9, 0.55, 0.6);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Surface {
    Floor,
    Wall,
    Ceiling,
    Trim,
    Lamp,
    /// The chandelier's rod and the canopy it hangs from.
    Brass,
    /// The landings and the sign's board.
    Wood,
    /// A step: walked on by way of the ramp under the stairs, not itself.
    Step,
}

/// A box: its center, its size along its own axes, and how it is turned.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Block {
    pub center: Vec3,
    pub size: Vec3,
    pub rotation: Quat,
    pub surface: Surface,
}

impl Block {
    /// Floor, walls, ceiling and the landings: what the player collides
    /// with.
    pub fn solid(&self) -> bool {
        matches!(
            self.surface,
            Surface::Floor | Surface::Wall | Surface::Ceiling | Surface::Wood
        )
    }
}

/// A slab the shape of a room: its outline at its bottom, in order round,
/// and how thick it is. Always solid.
#[derive(Clone, PartialEq, Debug)]
pub struct Slab {
    pub outline: Vec<Vec3>,
    pub thickness: f32,
    pub surface: Surface,
}

/// A ceiling lamp's light, a point light without shadows. Its intensity is
/// relative, which scene.rs turns into lumens (`lumens`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Light {
    pub position: Vec3,
    pub intensity: f32,
}

/// A piece of furniture, a model of src/models (scene.rs `Look`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Model {
    Chandelier,
    Table,
    Plant,
    Pedestal,
}

/// A model where it goes: its origin, and how it is turned, its front (+Z)
/// to the room.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Placed {
    pub model: Model,
    pub at: Vec3,
    pub turn: Quat,
}

/// The top of the pedestal downstairs, where the button goes, and the
/// rotation about y that turns its front, +Z, to the stairs. And the room
/// it stands in, for the confetti: the middle of its floor, how far its
/// walls are, at the least, and how high its ceiling.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Pedestal {
    pub top: Vec3,
    pub yaw: f32,
    pub floor: Vec3,
    pub walls: f32,
    pub height: f32,
}

impl Pedestal {
    /// Half the width of its top.
    pub const HALF: f32 = PEDESTAL_TOP / 2.0;
}

/// Words on the wall: their middle, their size, and the rotation about y
/// that turns them, facing +Z, to face the room.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Sign {
    pub center: Vec3,
    pub size: Vec2,
    pub yaw: f32,
    pub text: &'static str,
}

#[derive(Default, Debug)]
pub struct Shell {
    pub blocks: Vec<Block>,
    pub slabs: Vec<Slab>,
    /// Collision only: the panes in the openings, since every opening is
    /// taller than the player and without them the room has doors into the
    /// void, the ramp under the stairs, and the furniture's boxes.
    pub glass: Vec<Block>,
    pub lights: Vec<Light>,
    pub signs: Vec<Sign>,
    pub models: Vec<Placed>,
    pub pedestal: Option<Pedestal>,
}

/// One wall's own coordinates: `a` runs along it, left to right seen from
/// inside, `y` up from its room's floor, `d` into the wall from its inside
/// face (negative stands out into the room).
struct WallFrame<'a> {
    side: &'a Side,
    /// Half its length along `a` on its outside face, where it meets the
    /// next wall's.
    extent: f32,
}

impl<'a> WallFrame<'a> {
    fn new(room: &Room, side: &'a Side) -> Self {
        Self {
            side,
            extent: side.half + WALL_THICKNESS * room.bevel(),
        }
    }

    fn at(&self, a: f32, y: f32, d: f32) -> Vec3 {
        let wall = &self.side.wall;
        self.side.middle + wall.right * a + Vec3::Y * y - wall.normal * d
    }

    fn block(&self, surface: Surface, a: (f32, f32), y: (f32, f32), d: (f32, f32)) -> Block {
        let mid = |(p, q): (f32, f32)| (p + q) / 2.0;
        let len = |(p, q): (f32, f32)| (q - p).abs();
        Block {
            center: self.at(mid(a), mid(y), mid(d)),
            size: Vec3::new(len(a), len(y), len(d)),
            rotation: Quat::from_rotation_y(self.side.wall.yaw),
            surface,
        }
    }
}

/// The openings of side `side` of room `room`, as spans along it: one per
/// stretch of free wall that is long enough, centered in it.
fn openings(layout: &Layout, room: usize, side: usize) -> Vec<(f32, f32)> {
    let s = &layout.rooms[room].sides[side];
    let half = layout.frame.x / 2.0 + BEZEL + FRAME_CLEAR;
    let mut used: Vec<(f32, f32)> = layout
        .frames
        .iter()
        .filter(|f| f.room == room && f.side == side)
        .map(|_| (-half, half))
        .chain(s.door.map(|door| {
            let half = door.width / 2.0 + DOOR_CLEAR;
            (door.along - half, door.along + half)
        }))
        .collect();
    used.sort_by(|x, y| x.0.total_cmp(&y.0));

    let mut out = Vec::new();
    let mut fit = |s0: f32, s1: f32| {
        let width = OPENING_MAX.min(s1 - s0 - 2.0 * OPENING_MARGIN);
        if width >= OPENING_MIN {
            let mid = (s0 + s1) / 2.0;
            out.push((mid - width / 2.0, mid + width / 2.0));
        }
    };

    let mut cursor = -s.half;
    for (a0, a1) in used {
        fit(cursor, a0);
        cursor = cursor.max(a1);
    }
    fit(cursor, s.half);
    out
}

/// Both rooms, each with its floor, ceiling, walls with their openings,
/// doorway and trim, and lamps; the stairs between them and the sign over
/// the way down; a side table with a plant in a corner upstairs, and the
/// pedestal downstairs.
pub fn shell(layout: &Layout) -> Shell {
    let mut shell = Shell::default();
    for index in 0..layout.rooms.len() {
        build_room(&mut shell, layout, index);
    }
    build_stairs(&mut shell, layout);
    hang_sign(&mut shell, layout);
    furnish(&mut shell, &layout.rooms[0]);
    raise_pedestal(&mut shell, &layout.rooms[1], layout.height);
    shell
}

fn build_room(shell: &mut Shell, layout: &Layout, index: usize) {
    let room = &layout.rooms[index];
    let h = layout.height;

    // Out to the walls' outside faces, where they meet.
    let out = (room.apothem(0) + WALL_THICKNESS) / room.apothem(0);
    let outline = |y: f32| -> Vec<Vec3> {
        room.corners
            .iter()
            .map(|&c| room.floor + (c - room.floor) * out + Vec3::Y * y)
            .collect()
    };
    shell.slabs.push(Slab {
        outline: outline(-SLAB),
        thickness: SLAB,
        surface: Surface::Floor,
    });
    shell.slabs.push(Slab {
        outline: outline(h),
        thickness: SLAB,
        surface: Surface::Ceiling,
    });

    for side in 0..room.sides.len() {
        build_wall(shell, layout, index, side);
    }

    // The middle, and a ring halfway out once the room is large enough.
    let ring = room.apothem(0) / 2.0;
    let around = (std::f32::consts::TAU * ring / LIGHT_SPACING)
        .round()
        .min(RING_LAMPS) as usize;
    let around = if around < 3 { 0 } else { around };
    // The more lamps, the dimmer each, so a big hall is not lit brighter
    // than a small room.
    let intensity = 1.1 / ((around + 1) as f32).sqrt();
    let spots = std::iter::once(Vec3::ZERO).chain((0..around).map(|i| {
        let at = std::f32::consts::TAU * i as f32 / around as f32;
        ring * Vec3::new(at.sin(), 0.0, at.cos())
    }));
    for spot in spots {
        let at = room.floor + spot;
        let position = if index == 0 && spot == Vec3::ZERO {
            hang_chandelier(shell, at, h)
        } else {
            shell.blocks.push(Block {
                center: at + Vec3::Y * (h - 0.02),
                size: Vec3::new(0.9, 0.04, 0.9),
                rotation: Quat::IDENTITY,
                surface: Surface::Lamp,
            });
            at + Vec3::Y * (h - 0.8)
        };
        shell.lights.push(Light {
            position,
            intensity,
        });
    }
}

/// The chandelier in the middle of the room upstairs, in place of its flat
/// lamp, over `floor` under a ceiling `h` high: a canopy on the ceiling, a
/// rod down from it, and the model on the rod. Returns where its light
/// goes: in the middle of its hub, so the arms, side on to it, and the hub,
/// facing away, are not lit bright from so close.
fn hang_chandelier(shell: &mut Shell, floor: Vec3, h: f32) -> Vec3 {
    let hub = (h - CHANDELIER_DROP)
        .max(CHANDELIER_LOWEST)
        .min(h - CHANDELIER_TOP);
    let mount = hub + CHANDELIER_HUB;
    let canopy = 0.06;
    let mut brass = |y0: f32, y1: f32, width: f32| {
        shell.blocks.push(Block {
            center: floor + Vec3::Y * (y0 + y1) / 2.0,
            size: Vec3::new(width, y1 - y0, width),
            rotation: Quat::IDENTITY,
            surface: Surface::Brass,
        });
    };
    brass(h - canopy, h, 0.3);
    brass(mount, h - canopy, 0.035);
    shell.models.push(Placed {
        model: Model::Chandelier,
        at: floor + Vec3::Y * mount,
        turn: Quat::IDENTITY,
    });
    floor + Vec3::Y * hub
}

/// What a wall is cut away for: an opening onto the panorama, its pane and
/// mullions, or the doorway onto the stairs.
#[derive(Clone, Copy, PartialEq)]
enum Hole {
    Opening,
    Door,
}

fn build_wall(shell: &mut Shell, layout: &Layout, room_index: usize, side_index: usize) {
    let room = &layout.rooms[room_index];
    let side = &room.sides[side_index];
    let w = WallFrame::new(room, side);
    let h = layout.height;
    let t = WALL_THICKNESS;
    let sill = OPENING_SILL;
    let top = OPENING_TOP.min(h - 0.8);

    let mut holes: Vec<(f32, f32, f32, f32, Hole)> = if top - sill > 1.0 {
        openings(layout, room_index, side_index)
            .into_iter()
            .map(|(a0, a1)| (a0, a1, sill, top, Hole::Opening))
            .collect()
    } else {
        Vec::new()
    };
    if let Some(door) = side.door {
        let half = door.width / 2.0;
        holes.push((
            door.along - half,
            door.along + half,
            0.0,
            door.height,
            Hole::Door,
        ));
    }
    holes.sort_by(|x, y| x.0.total_cmp(&y.0));

    let slab = |shell: &mut Shell, a0: f32, a1: f32, y0: f32, y1: f32| {
        if a1 - a0 > 1e-4 && y1 - y0 > 1e-4 {
            shell
                .blocks
                .push(w.block(Surface::Wall, (a0, a1), (y0, y1), (0.0, t)));
        }
    };

    let mut cursor = -w.extent;
    for &(a0, a1, y0, y1, hole) in &holes {
        slab(shell, cursor, a0, 0.0, h);
        // The wall under an opening stops under its sill, which is the
        // wall's whole depth: with the wall's top level with the sill's
        // they would flicker, one through the other.
        let under = if hole == Hole::Opening { y0 - SILL } else { y0 };
        slab(shell, a0, a1, 0.0, under);
        slab(shell, a0, a1, y1, h);
        cursor = a1;

        // Casing on the inside face, either side and over the top.
        let mut trim = |a: (f32, f32), y: (f32, f32), d: (f32, f32)| {
            shell.blocks.push(w.block(Surface::Trim, a, y, d));
        };
        trim(
            (a0 - TRIM, a0),
            (y0 - TRIM, y1 + TRIM),
            (-TRIM_PROJECTION, 0.0),
        );
        trim(
            (a1, a1 + TRIM),
            (y0 - TRIM, y1 + TRIM),
            (-TRIM_PROJECTION, 0.0),
        );
        trim((a0, a1), (y1, y1 + TRIM), (-TRIM_PROJECTION, 0.0));
        if hole == Hole::Door {
            continue;
        }

        // An opening's pane, a deeper sill, and a cross of mullions in it.
        shell
            .glass
            .push(w.block(Surface::Wall, (a0, a1), (y0, y1), (0.4 * t, 0.6 * t)));
        trim(
            (a0 - 2.0 * TRIM, a1 + 2.0 * TRIM),
            (y0 - SILL, y0),
            (-0.1, t),
        );
        let mid_a = (a0 + a1) / 2.0;
        let mid_y = y0 + 0.62 * (y1 - y0);
        trim(
            (mid_a - MULLION / 2.0, mid_a + MULLION / 2.0),
            (y0, y1),
            (0.45 * t, 0.55 * t),
        );
        trim(
            (a0, a1),
            (mid_y - MULLION / 2.0, mid_y + MULLION / 2.0),
            (0.45 * t, 0.55 * t),
        );
    }
    slab(shell, cursor, w.extent, 0.0, h);

    // Baseboard, corner to corner but for the doorway, and crown molding.
    let mut runs = vec![(-side.half, side.half)];
    if let Some(door) = side.door {
        let half = door.width / 2.0 + TRIM;
        runs = vec![
            (-side.half, door.along - half),
            (door.along + half, side.half),
        ];
    }
    for (a0, a1) in runs.into_iter().filter(|(a0, a1)| a1 > a0) {
        shell.blocks.push(w.block(
            Surface::Trim,
            (a0, a1),
            (0.0, BASEBOARD),
            (-TRIM_PROJECTION / 2.0, 0.0),
        ));
    }
    shell.blocks.push(w.block(
        Surface::Trim,
        (-side.half, side.half),
        (h - CROWN, h),
        (-TRIM_PROJECTION, 0.0),
    ));
}

/// The half flight: a landing past the upper wall, the steps, a landing
/// before the lower wall, walls either side and a ceiling over them level
/// with the top of the upper doorway, and a lamp. The steps are not walked
/// on themselves: a ramp under them, through every step's nose, is.
fn build_stairs(shell: &mut Shell, layout: &Layout) {
    let stairs = &layout.stairs;
    let t = WALL_THICKNESS;
    let w = DOOR_WIDTH / 2.0;
    let turn = Quat::from_rotation_y(f32::atan2(stairs.down.x, stairs.down.z));
    // `s` down the stairs from the upper inside face, `c` across them, `y`
    // up.
    let at =
        |s: f32, c: f32, y: f32| stairs.top + stairs.down * s + stairs.across * c + Vec3::Y * y;
    let block = |surface: Surface, s: (f32, f32), c: (f32, f32), y: (f32, f32)| Block {
        center: at((s.0 + s.1) / 2.0, (c.0 + c.1) / 2.0, (y.0 + y.1) / 2.0),
        size: Vec3::new((c.1 - c.0).abs(), (y.1 - y.0).abs(), (s.1 - s.0).abs()),
        rotation: turn,
        surface,
    };

    let start = t + LANDING;
    let run = STEPS as f32 * TREAD;
    let rise = DROP / STEPS as f32;
    let (bottom, end) = (-DROP - SLAB, stairs.length - t);

    shell
        .blocks
        .push(block(Surface::Wood, (t, start), (-w, w), (bottom, 0.0)));
    for step in 0..STEPS {
        let s0 = start + step as f32 * TREAD;
        shell.blocks.push(block(
            Surface::Step,
            (s0, s0 + TREAD),
            (-w, w),
            (bottom, -(step as f32 + 1.0) * rise),
        ));
    }
    shell.blocks.push(block(
        Surface::Wood,
        (start + run, end),
        (-w, w),
        (bottom, -DROP),
    ));

    for c in [(-w - t, -w), (w, w + t)] {
        shell
            .blocks
            .push(block(Surface::Wall, (t, end), c, (bottom, stairs.ceiling)));
    }
    shell.blocks.push(block(
        Surface::Ceiling,
        (t, end),
        (-w - t, w + t),
        (stairs.ceiling, stairs.ceiling + SLAB),
    ));

    // Its top through the steps' noses, from the top landing's edge to the
    // bottom one's.
    let thick = 0.1;
    let slope = f32::atan2(DROP, run);
    let tilt = turn * Quat::from_rotation_x(slope);
    shell.glass.push(Block {
        center: at(start + run / 2.0, 0.0, -DROP / 2.0) - tilt * Vec3::Y * (thick / 2.0),
        size: Vec3::new(DOOR_WIDTH, thick, run.hypot(DROP)),
        rotation: tilt,
        surface: Surface::Step,
    });

    let lamp = at(stairs.length / 2.0, 0.0, stairs.ceiling);
    shell.blocks.push(Block {
        center: lamp - Vec3::Y * 0.02,
        size: Vec3::new(0.5, 0.04, 0.5),
        rotation: turn,
        surface: Surface::Lamp,
    });
    shell.lights.push(Light {
        position: lamp - Vec3::Y * 0.4,
        intensity: STAIRS_LAMP,
    });
}

/// "Other Workspaces", in two lines, on a board centered over the upper
/// doorway, no wider than its casing, so clear of the window beside it.
fn hang_sign(shell: &mut Shell, layout: &Layout) {
    let side = &layout.rooms[0].sides[0];
    let Some(door) = side.door else {
        return;
    };
    let w = WallFrame::new(&layout.rooms[0], side);

    let line = 8.0 * SIGN_PIXEL;
    let widest = SIGN.iter().map(|text| text.len()).max().unwrap_or(0);
    let text = Vec2::new(
        line * widest as f32,
        line * SIGN.len() as f32 + SIGN_LEADING * (SIGN.len() - 1) as f32,
    );
    let board = text + Vec2::splat(2.0 * SIGN_MARGIN);
    let along = door.along;
    let y = door.height + SIGN_GAP + board.y / 2.0;

    shell.blocks.push(w.block(
        Surface::Wood,
        (along - board.x / 2.0, along + board.x / 2.0),
        (y - board.y / 2.0, y + board.y / 2.0),
        (-SIGN_BOARD, 0.0),
    ));
    let top = y + text.y / 2.0;
    for (i, words) in SIGN.into_iter().enumerate() {
        let middle = top - line / 2.0 - i as f32 * (line + SIGN_LEADING);
        shell.signs.push(Sign {
            center: w.at(along, middle, -SIGN_BOARD - LABEL_LIFT),
            size: Vec2::new(line * words.len() as f32, line),
            yaw: side.wall.yaw,
            text: words,
        });
    }
}

/// The pedestal in the middle of the room downstairs, its front to the
/// stairs, for the button (button.rs).
fn raise_pedestal(shell: &mut Shell, room: &Room, height: f32) {
    let yaw = room.sides[0].wall.yaw + std::f32::consts::PI;
    let turn = Quat::from_rotation_y(yaw);
    shell.models.push(Placed {
        model: Model::Pedestal,
        at: room.floor,
        turn,
    });
    shell.glass.push(Block {
        center: room.floor + Vec3::Y * PEDESTAL_HEIGHT / 2.0,
        size: Vec3::new(PEDESTAL_TOP, PEDESTAL_HEIGHT, PEDESTAL_TOP),
        rotation: turn,
        surface: Surface::Wood,
    });
    let walls = (0..room.sides.len())
        .map(|side| room.apothem(side))
        .fold(f32::INFINITY, f32::min);
    shell.pedestal = Some(Pedestal {
        top: room.floor + Vec3::Y * PEDESTAL_HEIGHT,
        yaw,
        floor: room.floor,
        walls,
        height,
    });
}

/// A side table in the corner left of the stairs' side, by its window, out
/// of the way of the middle, where the player starts, and of the frames,
/// which hang higher, and a potted plant on it.
fn furnish(shell: &mut Shell, room: &Room) {
    let corner = room.corners[0];
    let at = corner + (room.floor - corner).normalize() * 0.9;
    let turn = Quat::from_rotation_y(room.sides[0].wall.yaw);
    shell.models.push(Placed {
        model: Model::Table,
        at,
        turn,
    });
    shell.models.push(Placed {
        model: Model::Plant,
        at: at + Vec3::Y * TABLE.y,
        turn,
    });
    // The plant stands on the table, inside its box.
    shell.glass.push(Block {
        center: at + Vec3::Y * TABLE.y / 2.0,
        size: TABLE,
        rotation: turn,
        surface: Surface::Wood,
    });
}

/// A frame's bezel, round its screen from the wall's face out to the
/// frame's, as a box in the frame's coordinates (all frames' are the same).
pub fn bezel(layout: &Layout) -> (Vec3, Vec3) {
    let half = (layout.frame / 2.0 + BEZEL).extend(0.0);
    (-half - Vec3::Z * FRAME_DEPTH, half)
}

/// Where a frame's label of `len` characters goes, in the frame's
/// coordinates: centered over the bezel, a hair off the wall. Its center and
/// size; it faces the room like the frame. Glyphs are at most 4.5 cm a
/// pixel, smaller when the name would be wider than the bezel.
pub fn label(layout: &Layout, len: usize) -> (Vec3, Vec2) {
    let len = len.max(1) as f32;
    let pixel = GLYPH_PIXEL.min((layout.frame.x + 2.0 * BEZEL) / (8.0 * len));
    let bottom = layout.frame.y / 2.0 + BEZEL + LABEL_GAP;
    let center = Vec3::new(0.0, bottom + 4.0 * pixel, LABEL_LIFT - FRAME_DEPTH);
    (center, Vec2::new(8.0 * pixel * len, 8.0 * pixel))
}

#[cfg(test)]
mod tests {
    use bevy::math::Rect;

    use super::*;
    use crate::layout::Settings;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 2e-3
    }

    /// A worked example: 1504x1003, four seats, 5.264 m across, nothing
    /// downstairs.
    fn example() -> Layout {
        let monitor = Vec2::new(1504.0, 1003.0);
        Layout::new(
            &[1, 2, 3, 4],
            &[],
            monitor,
            Rect::from_corners(Vec2::ZERO, monitor),
            &Settings {
                frame_size: 5.264,
                ..Settings::default()
            },
        )
    }

    /// A frame fills its side, the stairs' side has a window left of the
    /// doorway, and the empty room downstairs a window on every side but the
    /// stairs'.
    #[test]
    fn openings_only_where_a_wall_has_room() {
        let l = example();
        for side in 1..=4 {
            assert!(openings(&l, 0, side).is_empty(), "side {side}");
        }

        // 7.504 m of side, the doorway's casing from 1.402 to 3.302 along it.
        let stairs = openings(&l, 0, 0);
        assert_eq!(stairs.len(), 1);
        let (a0, a1) = stairs[0];
        assert!(close(a1 - a0, OPENING_MAX));
        assert!(a1 < l.rooms[0].sides[0].door.expect("a doorway").along - 0.8);

        assert!(openings(&l, 1, 0).len() == 1);
        assert!((1..4).all(|side| openings(&l, 1, side).len() == 1));
    }

    #[test]
    fn lamps_light_both_rooms_and_the_stairs() {
        let l = example();
        let s = shell(&l);
        // A lamp in each room's middle, a ring of three round it in the
        // five-sided room upstairs, none in the small room downstairs, and
        // the stairs' own.
        assert_eq!(s.lights.len(), 6);
        // The middle one upstairs is the chandelier's, 1.6 under the
        // ceiling.
        let h = l.height - 0.8;
        assert!(close(s.lights[0].position.y, l.height - 1.6));
        assert!(close(s.lights[0].intensity, 0.55));
        assert!(close(s.lights[1].position.y, h));
        assert!(close(s.lights[4].position.y, -DROP + h));
        assert!(close(s.lights[4].intensity, 1.1));

        // A pane upstairs, four downstairs, the ramp, and the table's and
        // the pedestal's boxes.
        assert_eq!(s.glass.len(), 8);
        let models: Vec<Model> = s.models.iter().map(|placed| placed.model).collect();
        assert_eq!(
            models,
            [
                Model::Chandelier,
                Model::Table,
                Model::Plant,
                Model::Pedestal
            ]
        );
        assert_eq!(s.slabs.len(), 4);
    }

    /// The steps go down from the upper floor to the lower, the ramp's top
    /// through every step's nose, and the sign hangs over the doorway.
    #[test]
    fn the_stairs_go_down_a_half_flight() {
        let l = example();
        let s = shell(&l);
        let steps: Vec<&Block> = s
            .blocks
            .iter()
            .filter(|b| b.surface == Surface::Step)
            .collect();
        assert_eq!(steps.len(), STEPS);
        let tops: Vec<f32> = steps.iter().map(|b| b.center.y + b.size.y / 2.0).collect();
        assert!(close(tops[0], -DROP / STEPS as f32) && close(tops[STEPS - 1], -DROP));

        let ramp = s
            .glass
            .iter()
            .find(|b| b.surface == Surface::Step)
            .expect("a ramp");
        let up = ramp.rotation * Vec3::Y;
        let top_face = ramp.center + up * ramp.size.y / 2.0;
        for (i, step) in steps.iter().enumerate() {
            // The nose: its tread's edge on the way down.
            let nose = step.center + Vec3::Y * step.size.y / 2.0 + l.stairs.down * TREAD / 2.0;
            let above = (nose - top_face).dot(up);
            assert!(above.abs() < 1e-3, "step {i}'s nose {above} off the ramp");
        }

        let words: Vec<&str> = s.signs.iter().map(|sign| sign.text).collect();
        assert_eq!(words, ["Other", "Workspaces"]);
        let side = &l.rooms[0].sides[0];
        let door = side.door.expect("a doorway");
        assert!(s.signs[0].center.y > s.signs[1].center.y);
        assert!(s.signs[1].center.y - s.signs[1].size.y / 2.0 > door.height);
        assert!(s.signs[0].center.y + s.signs[0].size.y / 2.0 < l.height - CROWN);
        for sign in &s.signs {
            assert!(close(sign.yaw, side.wall.yaw));
            // Centered over the doorway, and the board within its casing.
            let along = (sign.center - side.middle).dot(side.wall.right);
            assert!(close(along, door.along), "{along} {}", door.along);
            assert!(sign.size.x / 2.0 + SIGN_MARGIN < door.width / 2.0 + DOOR_CLEAR);
        }
    }

    /// The pedestal stands in the middle of the room downstairs, its front
    /// to the stairs.
    #[test]
    fn the_pedestal_faces_the_stairs() {
        let l = example();
        let pedestal = shell(&l).pedestal.expect("a pedestal");
        let room = &l.rooms[1];
        assert!((pedestal.top - room.floor - Vec3::Y * PEDESTAL_HEIGHT).length() < 1e-3);
        let front = Quat::from_rotation_y(pedestal.yaw) * Vec3::Z;
        let to_stairs = (room.sides[0].middle - room.floor).with_y(0.0).normalize();
        assert!(front.dot(to_stairs) > 0.999, "{front} {to_stairs}");
    }

    #[test]
    fn a_label_sits_over_its_bezel() {
        let l = example();
        let (center, size) = label(&l, 1);
        let (bezel_min, bezel_max) = bezel(&l);

        assert!(close(size.x, 8.0 * GLYPH_PIXEL) && close(size.y, 8.0 * GLYPH_PIXEL));
        assert!(close(center.x, 0.0));
        assert!(close(center.y - size.y / 2.0, bezel_max.y + LABEL_GAP));
        // A hair off the wall's face, into the room.
        assert!(close(center.z, bezel_min.z + LABEL_LIFT));
    }

    /// The bezel stands round the screen, from the wall to the frame's face.
    #[test]
    fn a_bezel_frames_the_screen() {
        let l = example();
        let (min, max) = bezel(&l);
        assert!(
            close(max.x - min.x, l.frame.x + 2.0 * BEZEL)
                && close(max.y - min.y, l.frame.y + 2.0 * BEZEL)
        );
        assert!(
            close(min.x + max.x, 0.0) && close(min.y + max.y, 0.0),
            "centered on the screen"
        );
        assert!(
            close(min.z, -FRAME_DEPTH) && close(max.z, 0.0),
            "from the wall to the frame's face"
        );
    }
}
