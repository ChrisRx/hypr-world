//! The house around the frames, as plain geometry:
//! floor and ceiling slabs, walls with an opening onto the panorama wherever
//! a stretch of wall has room left, trim, the ceiling lamps and their lights.
//! Every wall faces along x or z, so every solid piece is an axis-aligned
//! box in room coordinates. And a frame's bezel, and where its label goes,
//! in the frame's own coordinates (layout.rs `Frame::pose`).

use bevy::math::{Vec2, Vec3};

use crate::layout::{BEZEL, FRAME_DEPTH, Layout, WALLS};

const WALL_THICKNESS: f32 = 0.25;
/// The floor's and the ceiling's.
const SLAB: f32 = 0.3;

// Openings, cut into any stretch of wall a frame does not use.
const OPENING_SILL: f32 = 0.8;
/// And never closer than 0.8 to the ceiling.
const OPENING_TOP: f32 = 3.4;
const OPENING_MAX: f32 = 3.0;
const OPENING_MIN: f32 = 1.2;
/// Plain wall kept either side of an opening.
const OPENING_MARGIN: f32 = 0.6;
/// And around every frame.
const FRAME_CLEAR: f32 = 0.3;

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

/// One ceiling lamp per this many metres of room, at most four a side.
const LIGHT_SPACING: f32 = 5.0;
const LIGHTS_PER_SIDE: f32 = 4.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Surface {
    Floor,
    Wall,
    Ceiling,
    Trim,
    Lamp,
    /// The side table in a corner, and the potted plant on it.
    Wood,
    Pot,
    Leaves,
}

/// An axis-aligned box.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Block {
    pub min: Vec3,
    pub max: Vec3,
    pub surface: Surface,
}

impl Block {
    pub fn centre(&self) -> Vec3 {
        (self.min + self.max) / 2.0
    }

    pub fn size(&self) -> Vec3 {
        self.max - self.min
    }

    /// Floor, walls, ceiling and the table: what the player collides with.
    /// The plant stands on the table, inside its reach.
    pub fn solid(&self) -> bool {
        matches!(
            self.surface,
            Surface::Floor | Surface::Wall | Surface::Ceiling | Surface::Wood
        )
    }
}

/// A ceiling lamp's light, a point light without shadows. Its intensity is
/// relative, which scene.rs turns into lumens (`lumens`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Light {
    pub position: Vec3,
    pub intensity: f32,
}

#[derive(Default, Debug)]
pub struct Shell {
    pub blocks: Vec<Block>,
    /// The panes in the openings, collision only: every opening is
    /// taller than the player, so without them the room has doors into the
    /// void.
    pub glass: Vec<Block>,
    pub lights: Vec<Light>,
}

/// One wall's own coordinates: `a` runs along it, left to right seen from
/// inside, `y` up from the floor, `d` into the wall from its inside face
/// (negative stands out into the room).
struct WallFrame {
    normal: Vec3,
    right: Vec3,
    /// The inside face's distance from the room's centre.
    plane: f32,
    /// Half its length along `a`, corners included.
    extent: f32,
    /// Half its inside length, corner to corner.
    inside: f32,
}

impl WallFrame {
    fn new(layout: &Layout, wall: usize) -> Self {
        let w = WALLS[wall];
        let (plane, inside) = layout.halves(wall);
        // Front and back run the full width, corners included; the side
        // walls fit between them, so no two slabs overlap.
        let corners = if wall.is_multiple_of(2) {
            WALL_THICKNESS
        } else {
            0.0
        };
        Self {
            normal: w.normal,
            right: w.right,
            plane,
            extent: inside + corners,
            inside,
        }
    }

    fn at(&self, a: f32, y: f32, d: f32) -> Vec3 {
        self.right * a + Vec3::Y * y - self.normal * (self.plane + d)
    }

    fn block(&self, surface: Surface, a: (f32, f32), y: (f32, f32), d: (f32, f32)) -> Block {
        let p = self.at(a.0, y.0, d.0);
        let q = self.at(a.1, y.1, d.1);
        Block {
            min: p.min(q),
            max: p.max(q),
            surface,
        }
    }
}

/// The openings of `wall`, as spans along it: one per stretch of free wall
/// that is long enough, centred in it.
fn openings(layout: &Layout, wall: usize, inside: f32) -> Vec<(f32, f32)> {
    let mut used: Vec<(f32, f32)> = layout
        .frames
        .iter()
        .filter(|f| f.wall == wall)
        .map(|f| {
            let half = layout.frame.x / 2.0 + BEZEL + FRAME_CLEAR;
            (f.along - half, f.along + half)
        })
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

    let mut cursor = -inside;
    for (a0, a1) in used {
        fit(cursor, a0);
        cursor = cursor.max(a1);
    }
    fit(cursor, inside);
    out
}

/// Floor, ceiling, the four walls with their openings and trim, the lamps,
/// and a side table with a plant in a corner.
pub fn shell(layout: &Layout) -> Shell {
    let mut shell = Shell::default();
    let (hx, hz, h) = (
        layout.half_x + WALL_THICKNESS,
        layout.half_z + WALL_THICKNESS,
        layout.height,
    );

    shell.blocks.push(Block {
        min: Vec3::new(-hx, -SLAB, -hz),
        max: Vec3::new(hx, 0.0, hz),
        surface: Surface::Floor,
    });
    shell.blocks.push(Block {
        min: Vec3::new(-hx, h, -hz),
        max: Vec3::new(hx, h + SLAB, hz),
        surface: Surface::Ceiling,
    });

    for wall in 0..4 {
        build_wall(&mut shell, layout, wall);
    }

    let count = |half: f32| {
        (2.0 * half / LIGHT_SPACING)
            .round()
            .clamp(1.0, LIGHTS_PER_SIDE)
    };
    let (nx, nz) = (count(layout.half_x), count(layout.half_z));
    // The more lamps, the dimmer each, so a big hall is not lit brighter
    // than a small room.
    let intensity = 1.1 / (nx * nz).sqrt();

    for i in 0..nx as usize {
        for j in 0..nz as usize {
            let x = -layout.half_x + (i as f32 + 0.5) * (2.0 * layout.half_x / nx);
            let z = -layout.half_z + (j as f32 + 0.5) * (2.0 * layout.half_z / nz);
            shell.blocks.push(Block {
                min: Vec3::new(x - 0.45, h - 0.04, z - 0.45),
                max: Vec3::new(x + 0.45, h, z + 0.45),
                surface: Surface::Lamp,
            });
            shell.lights.push(Light {
                position: Vec3::new(x, h - 0.8, z),
                intensity,
            });
        }
    }

    furnish(&mut shell, layout);
    shell
}

/// A side table in the corner where the left and back walls meet, out of
/// the way of the middle, where the player starts, and of the frames, which
/// hang higher: a top on four legs, and a potted plant on it, all boxes like
/// the rest of the house.
fn furnish(shell: &mut Shell, layout: &Layout) {
    // The table's middle, clear of both walls.
    let at = Vec3::new(-layout.half_x + 0.7, 0.0, -layout.half_z + 0.6);
    let mut add = |surface: Surface, centre: Vec3, size: Vec3| {
        shell.blocks.push(Block {
            min: at + centre - size / 2.0,
            max: at + centre + size / 2.0,
            surface,
        });
    };

    let (half_w, half_d, height) = (0.45, 0.3, 0.55);
    add(
        Surface::Wood,
        Vec3::Y * (height - 0.03),
        Vec3::new(2.0 * half_w, 0.06, 2.0 * half_d),
    );
    for (x, z) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let leg = Vec3::new(
            x * (half_w - 0.05),
            (height - 0.06) / 2.0,
            z * (half_d - 0.05),
        );
        add(Surface::Wood, leg, Vec3::new(0.05, height - 0.06, 0.05));
    }

    add(
        Surface::Pot,
        Vec3::Y * (height + 0.11),
        Vec3::new(0.22, 0.22, 0.22),
    );
    add(
        Surface::Leaves,
        Vec3::Y * (height + 0.38),
        Vec3::new(0.42, 0.32, 0.42),
    );
    add(
        Surface::Leaves,
        Vec3::Y * (height + 0.62),
        Vec3::new(0.26, 0.18, 0.26),
    );
}

fn build_wall(shell: &mut Shell, layout: &Layout, wall: usize) {
    let w = WallFrame::new(layout, wall);
    let h = layout.height;
    let t = WALL_THICKNESS;
    let sill = OPENING_SILL;
    let top = OPENING_TOP.min(h - 0.8);

    let openings = if top - sill > 1.0 {
        openings(layout, wall, w.inside)
    } else {
        Vec::new()
    };

    let slab = |shell: &mut Shell, a0: f32, a1: f32, y0: f32, y1: f32| {
        if a1 - a0 > 1e-4 && y1 - y0 > 1e-4 {
            shell
                .blocks
                .push(w.block(Surface::Wall, (a0, a1), (y0, y1), (0.0, t)));
        }
    };

    let mut cursor = -w.extent;
    for &(a0, a1) in &openings {
        slab(shell, cursor, a0, 0.0, h);
        slab(shell, a0, a1, 0.0, sill);
        slab(shell, a0, a1, top, h);
        cursor = a1;

        shell
            .glass
            .push(w.block(Surface::Wall, (a0, a1), (sill, top), (0.4 * t, 0.6 * t)));

        // Casing on the inside face, a deeper sill, and a cross of mullions
        // in the opening itself.
        let mut trim = |a: (f32, f32), y: (f32, f32), d: (f32, f32)| {
            shell.blocks.push(w.block(Surface::Trim, a, y, d));
        };
        trim(
            (a0 - TRIM, a0),
            (sill - TRIM, top + TRIM),
            (-TRIM_PROJECTION, 0.0),
        );
        trim(
            (a1, a1 + TRIM),
            (sill - TRIM, top + TRIM),
            (-TRIM_PROJECTION, 0.0),
        );
        trim((a0, a1), (top, top + TRIM), (-TRIM_PROJECTION, 0.0));
        trim(
            (a0 - 2.0 * TRIM, a1 + 2.0 * TRIM),
            (sill - 0.05, sill),
            (-0.1, 0.5 * t),
        );

        let mid_a = (a0 + a1) / 2.0;
        let mid_y = sill + 0.62 * (top - sill);
        trim(
            (mid_a - MULLION / 2.0, mid_a + MULLION / 2.0),
            (sill, top),
            (0.45 * t, 0.55 * t),
        );
        trim(
            (a0, a1),
            (mid_y - MULLION / 2.0, mid_y + MULLION / 2.0),
            (0.45 * t, 0.55 * t),
        );
    }
    slab(shell, cursor, w.extent, 0.0, h);

    // Baseboard and crown moulding, inside corner to inside corner.
    let inside = (-w.inside, w.inside);
    shell.blocks.push(w.block(
        Surface::Trim,
        inside,
        (0.0, BASEBOARD),
        (-TRIM_PROJECTION / 2.0, 0.0),
    ));
    shell.blocks.push(w.block(
        Surface::Trim,
        inside,
        (h - CROWN, h),
        (-TRIM_PROJECTION, 0.0),
    ));
}

/// A frame's bezel, round its screen from the wall's face out to the
/// frame's, as a box in the frame's coordinates (all frames' are the same).
pub fn bezel(layout: &Layout) -> (Vec3, Vec3) {
    let half = (layout.frame / 2.0 + BEZEL).extend(0.0);
    (-half - Vec3::Z * FRAME_DEPTH, half)
}

/// Where a frame's label of `len` characters goes, in the frame's
/// coordinates: centred over the bezel, a hair off the wall. Its centre and
/// size; it faces the room like the frame. Glyphs are at most 4.5 cm a
/// pixel, smaller when the name would be wider than the bezel.
pub fn label(layout: &Layout, len: usize) -> (Vec3, Vec2) {
    let len = len.max(1) as f32;
    let pixel = GLYPH_PIXEL.min((layout.frame.x + 2.0 * BEZEL) / (8.0 * len));
    let bottom = layout.frame.y / 2.0 + BEZEL + LABEL_GAP;
    let centre = Vec3::new(0.0, bottom + 4.0 * pixel, LABEL_LIFT - FRAME_DEPTH);
    (centre, Vec2::new(8.0 * pixel * len, 8.0 * pixel))
}

#[cfg(test)]
mod tests {
    use bevy::math::Rect;

    use super::*;
    use crate::layout::Settings;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 2e-3
    }

    /// A worked example: 1504x1003, a 40 px bar, five workspaces.
    fn example() -> Layout {
        let work = Rect::new(0.0, 40.0, 1504.0, 1003.0);
        Layout::new(
            &[1, 2, 3, 4, 5],
            Vec2::new(1504.0, 1003.0),
            work,
            &Settings::default(),
        )
    }

    #[test]
    fn openings_only_where_a_wall_has_room() {
        let l = example();
        let spans = |wall: usize| openings(&l, wall, WallFrame::new(&l, wall).inside);

        // Two frames fill the front wall, one each the sides.
        assert!(spans(0).is_empty() && spans(1).is_empty() && spans(3).is_empty());

        // The back wall's one frame leaves room either side of it. Its
        // right runs along -x, so a span (a0, a1) is x from -a1 to -a0.
        let back = spans(2);
        assert_eq!(back.len(), 2);
        assert!(close(back[0].0, -6.404) && close(back[0].1, -3.652));
        assert!(close(back[1].0, 3.652) && close(back[1].1, 6.404));
    }

    #[test]
    fn lamps_spread_over_the_ceiling() {
        let s = shell(&example());
        assert_eq!(s.lights.len(), 6);
        assert!(
            s.lights
                .iter()
                .all(|l| close(l.intensity, 0.449) && close(l.position.y, 4.2605))
        );

        let mut xs: Vec<f32> = s.lights.iter().map(|l| l.position.x).collect();
        xs.dedup();
        assert!(xs.iter().any(|&x| close(x, -4.669)) && xs.iter().any(|&x| close(x, 4.669)));
        assert!(s.lights.iter().all(|l| close(l.position.z.abs(), 1.876)));

        // Two panes, for the back wall's two openings.
        assert_eq!(s.glass.len(), 2);
    }

    #[test]
    fn a_label_sits_over_its_bezel() {
        let l = example();
        let (centre, size) = label(&l, 1);
        let (bezel_min, bezel_max) = bezel(&l);

        assert!(close(size.x, 8.0 * GLYPH_PIXEL) && close(size.y, 8.0 * GLYPH_PIXEL));
        assert!(close(centre.x, 0.0));
        assert!(close(centre.y - size.y / 2.0, bezel_max.y + LABEL_GAP));
        // A hair off the wall's face, into the room.
        assert!(close(centre.z, bezel_min.z + LABEL_LIFT));
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
            "centred on the screen"
        );
        assert!(
            close(min.z, -FRAME_DEPTH) && close(max.z, 0.0),
            "from the wall to the frame's face"
        );
    }
}
