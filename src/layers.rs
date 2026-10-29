//! The monitor's layers in every frame: the wallpaper under each frame's
//! windows and the bar over them, as on the desktop, a quad of each a child
//! of every frame. The plugin draws a layer again whenever it drew
//! something new, at most once a second and only while the app sees it, and
//! sends the pixels; every frame shows the same picture.

use std::collections::HashMap;

use bevy::{
    asset::RenderAssetUsages,
    camera::primitives::Aabb,
    ecs::{lifecycle::HookContext, world::DeferredWorld},
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
};

use crate::{
    WorldSystems,
    glue::{self, DesktopChange, Glue, Layer2d, Level, Picture, Said},
    materials::Rebind,
    scene::{House, WorkspaceIndex},
    stats::Stats,
    windows::{self, DesktopWindow, QuadMesh, Sight, SightSystems},
};

/// The desktop's layers, their pictures, and a quad of each on every frame.
pub fn plugin(app: &mut App) {
    app.init_resource::<LayerIndex>()
        .add_systems(PreUpdate, apply_layers.after(WorldSystems::Feed))
        .add_systems(
            Update,
            (
                (apply_pictures, blend_for_level),
                sync_quads.run_if(
                    glue::any_changed_or_removed::<DesktopLayer>
                        .or_else(any_match_filter::<Added<LayerPicture>>)
                        .or_else(glue::any_changed_or_removed::<DesktopWindow>)
                        .or_else(resource_changed::<House>),
                ),
            )
                .chain()
                .in_set(WorldSystems::Walls),
        )
        .add_systems(PostUpdate, announce_seen.in_set(SightSystems));
}

/// A layer on the monitor, as the plugin last said it (glue.rs
/// `DesktopChange`): the wallpaper, the bar. Its picture is its
/// `LayerPicture` once the first comes, and its quads, one a frame, are its
/// `LayerQuads`.
#[derive(Component, Reflect, Clone, PartialEq, Debug)]
#[reflect(Component)]
#[component(on_add = index_layer, on_remove = unindex_layer)]
pub struct DesktopLayer {
    pub address: u64,
    pub level: Level,
    /// Its current (animated) box, monitor-local logical px.
    pub rect: Rect,
    /// What its client calls it, `noctalia-bar-default` and the like.
    pub namespace: String,
}

impl DesktopLayer {
    fn new(address: u64, layer: &Layer2d) -> Self {
        let Layer2d {
            level,
            rect,
            namespace,
        } = layer;
        Self {
            address,
            level: *level,
            rect: *rect,
            namespace: namespace.clone(),
        }
    }
}

/// The layers' entities by address, kept by `DesktopLayer`'s hooks, as
/// windows.rs `WindowIndex` is.
#[derive(Resource, Default)]
pub struct LayerIndex(HashMap<u64, Entity>);

fn index_layer(mut world: DeferredWorld, HookContext { entity, .. }: HookContext) {
    let Some(address) = world.get::<DesktopLayer>(entity).map(|l| l.address) else {
        return;
    };
    world.resource_mut::<LayerIndex>().0.insert(address, entity);
}

fn unindex_layer(mut world: DeferredWorld, HookContext { entity, .. }: HookContext) {
    let Some(address) = world.get::<DesktopLayer>(entity).map(|l| l.address) else {
        return;
    };
    world.resource_mut::<LayerIndex>().0.remove(&address);
}

/// A layer's picture, which its quad on every frame shows.
#[derive(Component)]
pub struct LayerPicture {
    image: Handle<Image>,
    material: Handle<StandardMaterial>,
    size: UVec2,
}

/// A layer's quad on the frame of `workspace`, and its child.
#[derive(Component)]
#[relationship(relationship_target = LayerQuads)]
pub struct LayerQuad {
    #[relationship]
    pub layer: Entity,
    pub workspace: i64,
}

/// A layer's quads, which go when it does.
#[derive(Component)]
#[relationship_target(relationship = LayerQuad, linked_spawn)]
pub struct LayerQuads(Vec<Entity>);

/// Takes in what the plugin said of its layers, as windows.rs
/// `apply_windows` does its windows. A layer that goes takes its picture and
/// its quads with it.
fn apply_layers(
    mut commands: Commands,
    mut changes: MessageReader<DesktopChange>,
    index: Res<LayerIndex>,
    mut layers: Query<&mut DesktopLayer>,
) {
    let said = DesktopChange::fold(changes.read(), &index.0, |change| match change {
        DesktopChange::Layer(address, layer) => Some((*address, Some(layer))),
        DesktopChange::LayerGone(address) => Some((*address, None)),
        _ => None,
    });
    for (address, said) in said {
        match said {
            Said::Came(layer) => {
                commands.spawn(DesktopLayer::new(address, layer));
            }
            Said::Still(entity, layer) => {
                if let Ok(mut current) = layers.get_mut(entity) {
                    current.set_if_neq(DesktopLayer::new(address, layer));
                }
            }
            Said::Gone(entity) => {
                commands.entity(entity).despawn();
            }
        }
    }
}

/// Puts the pictures the plugin sent on their layers.
pub fn apply_pictures(
    mut commands: Commands,
    // Drained, not read: the one taker, and it keeps the pixels.
    mut pictures: ResMut<Messages<Picture>>,
    mut layers: Query<(Entity, &DesktopLayer, Option<&mut LayerPicture>)>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut rebind: ResMut<Rebind>,
    mut stats: ResMut<Stats>,
) {
    // Each is the whole layer: of a layer's pictures, only the last counts.
    let mut last = HashMap::new();
    for picture in pictures.drain() {
        stats.picture();
        last.insert(picture.layer, picture);
    }
    if last.is_empty() {
        return;
    }

    for (entity, layer, shown) in &mut layers {
        let Some(Picture { size, pixels, .. }) = last.remove(&layer.address) else {
            continue;
        };
        // The GPU's alone: replaced whole each time and never read back, so
        // Bevy drops the CPU copy once it is uploaded.
        let image = Image::new(
            Extent3d {
                width: size.x,
                height: size.y,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            pixels,
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::RENDER_WORLD,
        );

        let Some(mut shown) = shown else {
            eprintln!(
                "hypr-world: layer {} is in, {}x{}",
                layer.namespace, size.x, size.y
            );
            let image = images.add(image);
            let material = materials.add(StandardMaterial {
                base_color_texture: Some(image.clone()),
                unlit: true,
                alpha_mode: alpha_mode(layer.level),
                ..default()
            });
            commands.entity(entity).insert(LayerPicture {
                image,
                material,
                size,
            });
            continue;
        };

        let Some(current) = images.get_mut(&shown.image) else {
            continue;
        };
        if size == shown.size {
            // Written into the texture the material already samples.
            current.into_inner().data = image.data;
            continue;
        }
        *current.into_inner() = image;
        shown.size = size;

        // A new size is a new GPU texture, which the material binds once it
        // is in (materials.rs `Rebind`).
        rebind.after_resize(&shown.image, &shown.material);
    }
}

/// Keeps a layer's picture blended for its level, should the layer move to
/// another.
fn blend_for_level(
    layers: Query<(&DesktopLayer, &LayerPicture), Changed<DesktopLayer>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (layer, shown) in &layers {
        let blend = alpha_mode(layer.level);
        if materials
            .get(&shown.material)
            .is_some_and(|m| m.alpha_mode != blend)
            && let Some(material) = materials.get_mut(&shown.material)
        {
            material.into_inner().alpha_mode = blend;
        }
    }
}

/// The wallpaper is drawn solid, so that the windows and the bar in front
/// of it are never sorted behind it; the bottom level keeps its holes; the
/// top level is blended over the windows, as Hyprland blends it.
fn alpha_mode(level: Level) -> AlphaMode {
    match level {
        Level::Background => AlphaMode::Opaque,
        Level::Bottom => AlphaMode::Mask(0.5),
        Level::Top => AlphaMode::Premultiplied,
    }
}

/// Keeps a quad of each layer with a picture on every frame, where the
/// layer is in 2D: under the windows, or over them and under a fullscreen
/// one. A frame that goes takes its quads with it.
pub fn sync_quads(
    mut commands: Commands,
    house: Res<House>,
    workspaces: Res<WorkspaceIndex>,
    mesh: Res<QuadMesh>,
    windows: Query<&DesktopWindow>,
    layers: Query<(Entity, &DesktopLayer, &LayerPicture)>,
    mut quads: Query<(Entity, &LayerQuad, &mut Transform)>,
) {
    let layout = house.layout.as_ref();

    // The quads there are, by layer and frame. Those on no frame go.
    let mut seated = HashMap::new();
    for (entity, quad, _) in &quads {
        if layout.is_some_and(|l| l.frame_of(quad.workspace).is_some()) {
            seated.insert((quad.layer, quad.workspace), entity);
        } else {
            commands.entity(entity).despawn();
        }
    }
    let Some(layout) = layout else { return };

    for (entity, layer, shown) in &layers {
        for frame in &layout.frames {
            // Bare, its workspace gone while the app is in view.
            let Some(on) = workspaces.get(frame.workspace) else {
                continue;
            };
            let (centre, size) = match layer.level {
                Level::Background => layout.under(layer.rect, false),
                Level::Bottom => layout.under(layer.rect, true),
                Level::Top => layout.seat(
                    layer.rect,
                    windows::top_rank(windows.iter(), frame.workspace),
                ),
            };
            let transform = Transform::from_translation(centre).with_scale(size.extend(1.0));

            if let Some(&quad) = seated.get(&(entity, frame.workspace))
                && let Ok((.., mut seat)) = quads.get_mut(quad)
            {
                seat.set_if_neq(transform);
                continue;
            }
            commands.spawn((
                LayerQuad {
                    layer: entity,
                    workspace: frame.workspace,
                },
                ChildOf(on),
                Mesh3d(mesh.0.clone()),
                MeshMaterial3d(shown.material.clone()),
                transform,
            ));
        }
    }
}

/// Tells the plugin which layers the app sees, in any frame on screen,
/// whenever that set changes. It draws and sends only those again (and any
/// the app has never had), so a bar in no frame in view costs nothing.
pub fn announce_seen(
    glue: Res<Glue>,
    sight: Sight,
    layers: Query<&DesktopLayer>,
    quads: Query<(&LayerQuad, &GlobalTransform, Option<&Aabb>)>,
    mut announced: Local<Option<Vec<u64>>>,
) {
    if !glue.connected {
        *announced = None;
        return;
    }

    let mut seen: Vec<u64> = quads
        .iter()
        .filter(|&(_, transform, aabb)| sight.sees(transform, aabb))
        .filter_map(|(quad, ..)| layers.get(quad.layer).ok())
        .map(|layer| layer.address)
        .collect();
    seen.sort_unstable();
    seen.dedup();

    if announced.as_ref() != Some(&seen) {
        let mut line = String::from("layers");
        for address in &seen {
            line.push_str(&format!(" {address:#x}"));
        }
        glue.send(&line);
        *announced = Some(seen);
    }
}

#[cfg(test)]
mod tests {
    use std::{f32::consts::PI, io::Read, os::unix::net::UnixStream};

    use bevy::{asset::AssetPlugin, camera::CameraPlugin, mesh::MeshPlugin};

    use super::*;
    use crate::{
        FlyCamera,
        config::WorldConfig,
        glue::{Desktop, Monitor},
        windows::{Aimed, SightSystems, configure_sight},
    };

    fn bar(address: u64) -> DesktopLayer {
        DesktopLayer {
            address,
            level: Level::Top,
            rect: Rect::new(0.0, 0.0, 2.0, 1.0),
            namespace: "bar".to_string(),
        }
    }

    /// The lines the plugin got since the last call.
    fn received(plugin: &mut UnixStream) -> Vec<String> {
        let mut bytes = Vec::new();
        // Nothing more to read ends it, as an error.
        let _ = plugin.read_to_end(&mut bytes);
        String::from_utf8(bytes)
            .expect("text")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// A layer is found by its address while it is there, and its quads go
    /// with it.
    #[test]
    fn a_layer_that_goes_takes_its_quads() {
        let mut world = World::new();
        world.init_resource::<LayerIndex>();
        let layer = world.spawn(bar(0xa)).id();
        let quads = [1, 2].map(|workspace| world.spawn(LayerQuad { layer, workspace }).id());
        assert_eq!(world.resource::<LayerIndex>().0.get(&0xa), Some(&layer));

        world.despawn(layer);
        assert!(
            world.resource::<LayerIndex>().0.is_empty(),
            "no longer found"
        );
        assert!(
            quads.iter().all(|&quad| world.get_entity(quad).is_err()),
            "its quads went with it"
        );
    }

    /// With Bevy's own frusta and bounds, headless: a layer on two frames
    /// ahead of the camera, another behind it, and the camera turned round.
    #[test]
    fn the_plugin_hears_which_layers_are_seen() {
        let (world_end, mut plugin) = UnixStream::pair().expect("a socket pair");
        plugin.set_nonblocking(true).expect("non-blocking");

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            AssetPlugin::default(),
            MeshPlugin,
            CameraPlugin,
        ))
        .insert_resource(Glue::connected_to(world_end))
        .init_resource::<Desktop>()
        .init_resource::<Aimed>()
        .init_resource::<WorldConfig>()
        .init_resource::<LayerIndex>();
        configure_sight(&mut app);
        app.add_systems(PostUpdate, announce_seen.in_set(SightSystems));

        let camera = app
            .world_mut()
            .spawn((
                Camera3d::default(),
                FlyCamera {
                    yaw: 0.0,
                    pitch: 0.0,
                },
                Transform::default(),
            ))
            .id();
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Rectangle::new(1.0, 1.0));
        let (a, b) = (
            app.world_mut().spawn(bar(0xa)).id(),
            app.world_mut().spawn(bar(0xb)).id(),
        );
        for (layer, workspace, at) in [
            (a, 1, Vec3::new(-1.0, 0.0, -3.0)),
            (a, 2, Vec3::new(1.0, 0.0, -3.0)),
            (b, 1, Vec3::new(0.0, 0.0, 3.0)),
        ] {
            app.world_mut().spawn((
                LayerQuad { layer, workspace },
                Mesh3d(mesh.clone()),
                Transform::from_translation(at),
            ));
        }

        // Twice: the first update works out the quads' bounds.
        app.update();
        app.update();
        assert_eq!(
            received(&mut plugin).last().map(String::as_str),
            Some("layers 0xa"),
            "the layer ahead, once"
        );

        app.update();
        assert!(
            received(&mut plugin).is_empty(),
            "said again only when it changes"
        );

        app.world_mut()
            .get_mut::<Transform>(camera)
            .expect("the camera")
            .rotation = Quat::from_rotation_y(PI);
        app.update();
        assert_eq!(
            received(&mut plugin),
            ["layers 0xb"],
            "turned round: told in the same update"
        );

        // The plugin's feed says the app is out of view: nothing is seen.
        app.world_mut().resource_mut::<Desktop>().monitor = Some(Monitor {
            size: Vec2::new(1440.0, 960.0),
            scale: 2.0,
            work: Rect::new(0.0, 40.0, 1440.0, 960.0),
        });
        app.update();
        assert_eq!(received(&mut plugin), ["layers"], "out of view, none");
    }
}
