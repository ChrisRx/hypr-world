//! The panorama: an equirectangular image all around at
//! infinity, seen through the house's openings. It is the camera's `Skybox`,
//! which Bevy draws after the house and only where nothing else is
//! (bevy_core_pipeline `skybox/skybox.wesl`). The image loads through the
//! asset server, and again when the file changed, which is looked at each
//! time the world comes into view. A task off the main thread turns each load
//! into the cubemap a skybox takes. Without one, the void is a dark blue.

use std::{
    f32::consts::{PI, TAU},
    path::PathBuf,
    time::SystemTime,
};

use bevy::{
    asset::RenderAssetUsages,
    camera::Exposure,
    image::{ImageLoaderSettings, ImageSampler},
    light::Skybox,
    prelude::*,
    render::render_resource::{
        Extent3d, TextureDimension, TextureFormat, TextureViewDescriptor, TextureViewDimension,
    },
    tasks::{AsyncComputeTaskPool, Task, futures::check_ready},
};

use crate::{
    FlyCamera, WorldSystems, config::WorldConfig, glue::Desktop, stats::Redraw, wake::Wake,
};

pub fn plugin(app: &mut App) {
    app.init_resource::<Panorama>()
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (watch, start_cubemap, put_up)
                .chain()
                .in_set(WorldSystems::House),
        );
}

const VOID: Color = Color::srgb(0.012, 0.019, 0.032);

#[derive(Resource, Default)]
pub struct Panorama {
    /// The file asked for, with ~ expanded.
    path: Option<PathBuf>,
    /// Its modification time when it was last read.
    modified: Option<SystemTime>,
    /// The image as loaded, kept on the CPU for its pixels and for reloads.
    source: Option<Handle<Image>>,
    /// The cubemap being made from it.
    making: Option<Task<Image>>,
    /// Said once that the file is not there.
    reported: bool,
}

pub fn setup(mut commands: Commands) {
    commands.insert_resource(ClearColor(VOID));
}

/// Loads the panorama when the setting names another file, and again when
/// the file changed since it was read, which is looked at whenever the
/// settings change and each time the world comes into view. Takes the sky
/// down when the setting goes.
#[allow(clippy::too_many_arguments)]
pub fn watch(
    mut commands: Commands,
    assets: Res<AssetServer>,
    wake: Res<Wake>,
    config: Res<WorldConfig>,
    desktop: Res<Desktop>,
    mut panorama: ResMut<Panorama>,
    mut was_shown: Local<bool>,
    camera: Single<Entity, With<FlyCamera>>,
) {
    let came_into_view = desktop.shown && !*was_shown;
    *was_shown = desktop.shown;
    if !config.is_changed() && !came_into_view {
        return;
    }

    let wanted = config.panorama.clone();
    if wanted != panorama.path {
        // Another file, or none. The sky up now stays until the next is.
        *panorama = Panorama {
            path: wanted,
            ..default()
        };
        if panorama.path.is_none() {
            commands.entity(*camera).remove::<Skybox>();
            return;
        }
    }

    let Some(path) = panorama.path.clone() else {
        return;
    };
    let Ok(modified) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
        if !panorama.reported {
            eprintln!("hypr-world: no panorama at {}", path.display());
            panorama.reported = true;
        }
        return;
    };
    if panorama.modified == Some(modified) {
        return;
    }
    panorama.modified = Some(modified);
    panorama.reported = false;

    if panorama.source.is_some() {
        // With the settings it was first loaded with, which its handle keeps.
        // Only on coming into view, when the flight in keeps the app
        // updating until it is in.
        assets.reload(path);
        return;
    }
    // Outside the asset folder, which only a load that says so may read
    // (main.rs sets `UnapprovedPathMode::Deny`). Kept on the CPU, where the
    // cubemap is made from it, and never uploaded. The guard wakes the app
    // when the load is over: it may be asleep, with nothing else to wake it.
    panorama.source = Some(
        assets
            .load_builder()
            .with_settings(|settings: &mut ImageLoaderSettings| {
                settings.asset_usage = RenderAssetUsages::MAIN_WORLD
            })
            .override_unapproved()
            .with_guard(WakeWhenDone(wake.clone()))
            .load(path),
    );
}

/// Wakes the app when dropped, which a load's guard is at the load's end.
struct WakeWhenDone(Wake);

impl Drop for WakeWhenDone {
    fn drop(&mut self) {
        self.0.wake();
    }
}

/// Starts making a cubemap from a panorama that finished loading, first or
/// again, on a task off the main thread: a sample for every texel of six
/// faces. An image it cannot read is a warning, which Bevy's error handler
/// logs with the system's name (`match_severity`, the default).
pub fn start_cubemap(
    mut events: MessageReader<AssetEvent<Image>>,
    images: Res<Assets<Image>>,
    mut panorama: ResMut<Panorama>,
) -> Result {
    let source = panorama.source.as_ref().map(Handle::id);
    // Read through either way, so that no event counts twice.
    let loaded = events
        .read()
        .filter(|event| source.is_some_and(|id| event.is_loaded_with_dependencies(id)))
        .count()
        > 0;
    let Some(source) = source.filter(|_| loaded) else {
        return Ok(());
    };

    let equirect = images
        .get(source)
        .and_then(Equirect::of)
        .ok_or_else(|| BevyError::warning("the panorama is not an image the app can read"))?;
    panorama.making = Some(AsyncComputeTaskPool::get().spawn(async move { equirect.cubemap() }));
    Ok(())
}

/// Puts up a cubemap that is ready, as the camera's skybox. Until then, asks
/// for another update to look again: a task cannot wake the app.
pub fn put_up(
    mut commands: Commands,
    mut panorama: ResMut<Panorama>,
    mut images: ResMut<Assets<Image>>,
    mut redraw: Redraw,
    camera: Single<(Entity, Option<&Exposure>), With<FlyCamera>>,
) {
    let Some(task) = &mut panorama.making else {
        return;
    };
    let Some(cubemap) = check_ready(task) else {
        redraw.ask(["sky"]);
        return;
    };
    panorama.making = None;

    // A skybox's samples come out times its brightness and the camera's
    // exposure (bevy_core_pipeline `skybox/mod.rs:79`). This one undoes the
    // exposure, so the image shows as it is, unlit.
    let (camera, exposure) = *camera;
    let exposure = exposure.map_or_else(|| Exposure::default().exposure(), Exposure::exposure);
    commands.entity(camera).insert(Skybox {
        image: Some(images.add(cubemap)),
        brightness: 1.0 / exposure,
        ..default()
    });
}

/// A panorama's pixels: 8-bit sRGB RGBA, the top row first.
struct Equirect {
    pixels: Vec<u8>,
    width: usize,
    height: usize,
}

impl Equirect {
    fn of(image: &Image) -> Option<Self> {
        let converted;
        let image = if image.texture_descriptor.format == TextureFormat::Rgba8UnormSrgb {
            image
        } else {
            converted = image.convert(TextureFormat::Rgba8UnormSrgb)?;
            &converted
        };
        Some(Self {
            pixels: image.data.clone()?,
            width: image.width() as usize,
            height: image.height() as usize,
        })
    }

    /// The six faces a skybox takes, as a cube array. A face's texels are
    /// widest at its centre, 2 / size radians, so a face width / π across
    /// keeps all of the panorama's detail (360° / width a pixel): 652 for
    /// one 2048 wide, 10 MB.
    fn cubemap(&self) -> Image {
        let size = ((self.width as f32 / PI).ceil() as usize).max(1);
        // From -1 to 1 across a face, at a texel's centre.
        let across = |i: usize| 2.0 * (i as f32 + 0.5) / size as f32 - 1.0;

        let mut data = Vec::with_capacity(size * size * 6 * 4);
        for face in 0..6 {
            for row in 0..size {
                for column in 0..size {
                    data.extend_from_slice(&self.seen_along(face_direction(
                        face,
                        across(column),
                        across(row),
                    )));
                }
            }
        }

        let mut image = Image::new(
            Extent3d {
                width: size as u32,
                height: size as u32,
                depth_or_array_layers: 6,
            },
            TextureDimension::D2,
            data,
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::RENDER_WORLD,
        );
        image.texture_view_descriptor = Some(TextureViewDescriptor {
            dimension: Some(TextureViewDimension::Cube),
            ..default()
        });
        image.sampler = ImageSampler::linear();
        image
    }

    /// The colour seen along `cube`, a direction in the cube's frame. Bevy
    /// looks a skybox up with z turned round, cube maps being left-handed
    /// (`skybox.wesl`), so in the world that is (x, y, -z). The mapping puts
    /// the image's centre straight ahead at -z, +x three quarters of the way across, the seam
    /// behind at +z, the top row overhead.
    fn seen_along(&self, cube: Vec3) -> [u8; 4] {
        let dir = cube * Vec3::new(1.0, 1.0, -1.0);
        let longitude = dir.x.atan2(-dir.z);
        let latitude = dir.y.atan2(dir.x.hypot(dir.z));
        self.sample((0.5 + longitude / TAU).rem_euclid(1.0), 0.5 - latitude / PI)
    }

    /// Bilinear, wrapping round the sky and clamped pole to pole.
    fn sample(&self, u: f32, v: f32) -> [u8; 4] {
        let x = u * self.width as f32 - 0.5;
        let y = (v * self.height as f32 - 0.5).clamp(0.0, (self.height - 1) as f32);
        let (left, top) = (x.floor(), y.floor());
        let (right_share, lower_share) = (x - left, y - top);

        let column = |x: f32| (x as isize).rem_euclid(self.width as isize) as usize;
        let (c0, c1) = (column(left), column(left + 1.0));
        let (r0, r1) = (top as usize, (top as usize + 1).min(self.height - 1));
        let texel =
            |column: usize, row: usize| &self.pixels[(row * self.width + column) * 4..][..4];
        let (upper_left, upper_right) = (texel(c0, r0), texel(c1, r0));
        let (lower_left, lower_right) = (texel(c0, r1), texel(c1, r1));

        std::array::from_fn(|i| {
            let mix =
                |a: u8, b: u8, share: f32| f32::from(a) + (f32::from(b) - f32::from(a)) * share;
            let upper = mix(upper_left[i], upper_right[i], right_share);
            let lower = mix(lower_left[i], lower_right[i], right_share);
            (upper + (lower - upper) * lower_share).round() as u8
        })
    }
}

/// The direction through a texel of a cube face, `a` and `b` from -1 to 1
/// across and down it, in the cube's own frame. The faces in the GPU's
/// order: +x, -x, +y, -y, +z, -z.
fn face_direction(face: usize, a: f32, b: f32) -> Vec3 {
    match face {
        0 => Vec3::new(1.0, -b, -a),
        1 => Vec3::new(-1.0, -b, a),
        2 => Vec3::new(a, 1.0, b),
        3 => Vec3::new(a, -1.0, -b),
        4 => Vec3::new(a, -b, 1.0),
        _ => Vec3::new(-a, -b, -1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panorama 256 by 128 whose red is its column and green its row.
    fn gradient() -> Equirect {
        let (width, height) = (256, 128);
        let mut pixels = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            for column in 0..width {
                pixels.extend_from_slice(&[column as u8, row as u8, 0, 255]);
            }
        }
        Equirect {
            pixels,
            width,
            height,
        }
    }

    fn texel(cubemap: &Image, face: usize, column: usize, row: usize) -> [u8; 4] {
        let size = cubemap.width() as usize;
        let at = ((face * size + row) * size + column) * 4;
        cubemap.data.as_ref().expect("pixels")[at..at + 4]
            .try_into()
            .expect("four bytes")
    }

    #[test]
    fn a_cubemap_is_six_square_faces_as_fine_as_the_panorama() {
        let cubemap = gradient().cubemap();
        assert_eq!(
            (cubemap.width(), cubemap.height()),
            (82, 82),
            "256 / π, rounded up"
        );
        assert_eq!(cubemap.texture_descriptor.size.depth_or_array_layers, 6);
        assert_eq!(
            cubemap
                .texture_view_descriptor
                .as_ref()
                .and_then(|v| v.dimension),
            Some(TextureViewDimension::Cube)
        );
    }

    /// Where the old sphere showed each part of the image, with Bevy's z
    /// turned round: ahead on the +z face.
    #[test]
    fn the_skybox_shows_the_panorama_where_the_sphere_did() {
        let cubemap = gradient().cubemap();
        let (mid, step) = (cubemap.width() as usize / 2, cubemap.width() as usize / 4);
        let centre = |face| texel(&cubemap, face, mid, mid);

        let [red, green, ..] = centre(4);
        assert!(
            red.abs_diff(128) <= 2 && green.abs_diff(64) <= 2,
            "ahead, the centre: {red} {green}"
        );
        assert!(
            centre(0)[0].abs_diff(192) <= 2,
            "+x, three quarters across: {:?}",
            centre(0)
        );
        assert!(
            centre(1)[0].abs_diff(64) <= 2,
            "-x, a quarter across: {:?}",
            centre(1)
        );
        assert!(centre(2)[1] <= 1, "overhead, the top row: {:?}", centre(2));
        assert!(
            centre(3)[1] >= 126,
            "underfoot, the bottom row: {:?}",
            centre(3)
        );

        // Not mirrored: right of ahead is further across, and above it
        // nearer the top.
        assert!(
            texel(&cubemap, 4, mid + step, mid)[0] > red,
            "right of ahead"
        );
        assert!(
            texel(&cubemap, 4, mid, mid - step)[1] < green,
            "above ahead"
        );
    }
}
