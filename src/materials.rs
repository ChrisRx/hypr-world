//! The app's own materials: a window's surface in YUV turned into RGB, and
//! a still of a workspace warping in the shadow realm (shadow.rs). Their
//! shaders are built into the binary, so the app runs from any directory. The
//! house is Bevy's `StandardMaterial` in Bevy's lights (scene.rs).
//! The panorama is Bevy's `Skybox` (sky.rs). And the rebinding of a material
//! to its image when the image is resized.

use std::collections::HashMap;

use bevy::{
    asset::embedded_asset,
    pbr::{Material, MaterialPlugin},
    prelude::*,
    render::render_resource::{AsBindGroup, ShaderType},
    shader::ShaderRef,
};

use crate::{WorldSystems, stats::Redraw};

const YUV_SHADER: &str = "embedded://hypr_world/shaders/yuv.wesl";
const WARP_SHADER: &str = "embedded://hypr_world/shaders/warp.wesl";

pub fn plugin(app: &mut App) {
    embedded_asset!(app, "shaders/yuv.wesl");
    embedded_asset!(app, "shaders/warp.wesl");
    app.add_plugins((
        MaterialPlugin::<YuvMaterial>::default(),
        MaterialPlugin::<WarpMaterial>::default(),
    ))
    .init_resource::<Rebind>()
    // After the frames and pictures are in, so that a resize in this
    // update asks for the next one.
    .add_systems(Update, rebind.after(WorldSystems::Capture));
}

/// Materials to prepare again once their image's resize is in. A material
/// binds its images' texture views when it is prepared, and is prepared
/// again only when it changes itself, so it keeps sampling a resized
/// image's old texture. Marked on the image's `AssetEvent::Modified`,
/// an update after the resize: the render world prepares the new texture
/// before the material that binds it, which it would not order by itself if
/// both changed in one update.
#[derive(Resource, Default)]
pub struct Rebind(HashMap<AssetId<Image>, Handle<StandardMaterial>>);

impl Rebind {
    /// `image`, which `material` samples, was just resized.
    pub fn after_resize(&mut self, image: &Handle<Image>, material: &Handle<StandardMaterial>) {
        self.0.insert(image.id(), material.clone());
    }
}

fn rebind(
    mut events: MessageReader<AssetEvent<Image>>,
    mut rebind: ResMut<Rebind>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut redraw: Redraw,
) {
    for event in events.read() {
        if let AssetEvent::Modified { id } = event
            && let Some(material) = rebind.0.remove(id)
            && let Some(material) = materials.get_mut(&material)
        {
            // Getting it is not enough: only writing through it marks it.
            material.into_inner();
        }
    }
    // A resize waiting for its event keeps the updates coming: the app may
    // have nothing else to wake it, a window that resized and went still.
    if !rebind.0.is_empty() {
        redraw.ask(["rebind"]);
    }
}

/// A window's surface in YUV (NV12, P010), its two planes sampled through
/// images of their own (import.rs `plane_image`) and turned into RGB in its
/// shader (shaders/yuv.wesl) the way Hyprland shows it: BT.601 at limited
/// range, which is what Mesa's EGL import of a YUV dmabuf defaults to, and
/// Hyprland gives it no other. Unlit and opaque: YUV has no alpha.
#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct YuvMaterial {
    #[uniform(0)]
    pub params: YuvParams,
    #[texture(1)]
    #[sampler(2)]
    pub luma: Handle<Image>,
    #[texture(3)]
    #[sampler(4)]
    pub chroma: Handle<Image>,
}

#[derive(Clone, Copy, Debug, PartialEq, ShaderType)]
pub struct YuvParams {
    /// The part of the buffer the surface shows, as the quad's uv goes into
    /// it: scaled by xy, then moved by zw (a `StandardMaterial`'s
    /// `uv_transform`, with no turn).
    pub crop: Vec4,
}

impl Material for YuvMaterial {
    fn fragment_shader() -> ShaderRef {
        YUV_SHADER.into()
    }
}

/// A still of a workspace, warping slowly in the murk (shaders/warp.wesl).
/// Unlit and opaque, and fogged by its own shader, which has no view
/// bindings: the eye and the time come in its params.
#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
pub struct WarpMaterial {
    #[uniform(0)]
    pub params: WarpParams,
    #[texture(1)]
    #[sampler(2)]
    pub still: Handle<Image>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, ShaderType)]
pub struct WarpParams {
    /// The murk's linear color, and its density in w.
    pub murk: Vec4,
    /// The fel green, linear.
    pub fel: Vec4,
    /// The eye, and the time in seconds in w.
    pub eye: Vec4,
    /// How far the warp has set in, from 0 to 1, in x.
    pub warp: Vec4,
}

impl Material for WarpMaterial {
    fn fragment_shader() -> ShaderRef {
        WARP_SHADER.into()
    }
}

/// The compiler cannot check the shader, and a broken one is an invisible
/// window. This compiles it as Bevy does, with wesl against Bevy's own
/// forward_io module, and validates the result with naga.
#[cfg(test)]
mod tests {
    use std::{borrow::Cow, path::PathBuf};

    use wesl::{
        CompileOptions, Feature, Features, Resolver,
        error::ResolveError,
        syntax::{ModulePath, PathOrigin},
    };

    /// bevy_pbr 0.20's sources: among the crates in $HYPR_WORLD_CRATES,
    /// where the Nix build has them vendored (package.nix), or else in the
    /// cargo registry.
    fn bevy_pbr() -> PathBuf {
        const CRATE: &str = "bevy_pbr-0.20.0";
        if let Some(crates) = std::env::var_os("HYPR_WORLD_CRATES") {
            let vendored = PathBuf::from(crates).join(CRATE);
            assert!(vendored.exists(), "{CRATE} in {}", vendored.display());
            return vendored;
        }
        let cargo = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cargo")
            });
        std::fs::read_dir(cargo.join("registry/src"))
            .expect("the cargo registry")
            .flatten()
            .map(|e| e.path().join(CRATE))
            .find(|p| p.exists())
            .expect("bevy_pbr 0.20.0 in the cargo registry")
    }

    struct Sources {
        forward_io: String,
    }

    impl Resolver for Sources {
        fn resolve_source<'a>(&'a self, path: &ModulePath) -> Result<Cow<'a, str>, ResolveError> {
            let not_found =
                || ResolveError::ModuleNotFound(path.clone(), "not in the test".to_string());
            let path = self.canonical_path(path);
            let PathOrigin::Package(package) = &path.origin else {
                return Err(not_found());
            };
            let components: Vec<&str> = path.components.iter().map(String::as_str).collect();

            match (package.as_str(), components.as_slice()) {
                ("constants", []) => Ok("const MATERIAL_BIND_GROUP = 3;\n".into()),
                ("bevy_pbr", ["render", "forward_io"]) => Ok(Cow::Borrowed(&self.forward_io)),
                ("hypr_world", ["shaders", "yuv"]) => Ok(include_str!("shaders/yuv.wesl").into()),
                ("hypr_world", ["shaders", "warp"]) => Ok(include_str!("shaders/warp.wesl").into()),
                _ => Err(not_found()),
            }
        }

        // As Bevy's resolver: a package imported from another one comes as
        // `importer/package`.
        fn canonical_path(&self, path: &ModulePath) -> ModulePath {
            match &path.origin {
                PathOrigin::Package(package) if package.contains('/') => ModulePath {
                    origin: PathOrigin::Package(
                        package.rsplit('/').next().unwrap_or_default().to_string(),
                    ),
                    components: path.components.clone(),
                },
                _ => path.clone(),
            }
        }
    }

    fn check(name: &str) {
        let forward_io = std::fs::read_to_string(bevy_pbr().join("src/render/forward_io.wesl"))
            .expect("forward_io.wesl");
        let sources = Sources { forward_io };

        // What Bevy turns on for a mesh with positions, normals and UVs.
        let mut features = Features {
            default: Feature::Disable,
            flags: Default::default(),
        };
        for flag in ["VERTEX_POSITIONS", "VERTEX_NORMALS", "VERTEX_UVS_A"] {
            features.flags.insert(flag.to_string(), Feature::Enable);
        }
        let options = CompileOptions {
            condcomp: true,
            visibility: false,
            features,
            ..Default::default()
        };

        let main = ModulePath::new(
            PathOrigin::Package("hypr_world".to_string()),
            vec!["shaders".to_string(), name.to_string()],
        );
        let wgsl = wesl::compile(&main, &options, &sources)
            .unwrap_or_else(|e| panic!("{name}.wesl: {e}"))
            .to_string();

        let module = naga::front::wgsl::parse_str(&wgsl)
            .unwrap_or_else(|e| panic!("{name}.wesl: {}\n{wgsl}", e.emit_to_string(&wgsl)));
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("{name}.wesl: {e:?}\n{wgsl}"));
    }

    #[test]
    fn yuv_shader_compiles() {
        check("yuv");
    }

    #[test]
    fn warp_shader_compiles() {
        check("warp");
    }

    /// A resized image's material is marked an update after the image, on
    /// its `AssetEvent::Modified`, and not in the same one.
    #[test]
    fn a_resized_images_material_is_marked_an_update_later() {
        use bevy::{
            asset::{AssetPlugin, RenderAssetUsages},
            prelude::*,
            render::render_resource::{Extent3d, TextureDimension, TextureFormat},
        };

        use super::{Rebind, rebind};

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<Image>()
            .init_asset::<StandardMaterial>()
            .init_resource::<Rebind>()
            .init_resource::<crate::stats::Stats>()
            .add_message::<bevy::window::RequestRedraw>()
            .add_systems(Update, rebind);

        let image = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(Image::default());
        let material = app
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .add(StandardMaterial {
                base_color_texture: Some(image.clone()),
                ..default()
            });
        app.update();
        app.update();

        let marked = |app: &App| {
            let events = app
                .world()
                .resource::<Messages<AssetEvent<StandardMaterial>>>();
            events
                .iter_current_update_messages()
                .any(|event| event.is_modified(&material))
        };

        // Resized, between updates, as apply_frames does within one.
        let resized = Image::new_fill(
            Extent3d {
                width: 4,
                height: 2,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            &[0, 0, 0, 255],
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::RENDER_WORLD,
        );
        *app.world_mut()
            .resource_mut::<Assets<Image>>()
            .get_mut(&image)
            .expect("the image")
            .into_inner() = resized;
        app.world_mut()
            .resource_mut::<Rebind>()
            .after_resize(&image, &material);

        app.update();
        assert!(!marked(&app), "not in the update the image changed");
        app.update();
        assert!(marked(&app), "an update later");
    }
}
