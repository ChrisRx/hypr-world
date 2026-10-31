//! The house around the frames, built from the layout: the shell and lamps
//! (house.rs) in Bevy's own lights, and per workspace a bezel and its name,
//! lit while it is the active one, with a dark screen behind its layers and
//! windows. A workspace with a frame is that frame:
//! what hangs on it is its children, in the frame's own coordinates, and
//! moves with it.

use std::collections::HashMap;

use avian3d::prelude::{Collider, RigidBody};
use bevy::{
    asset::{RenderAssetUsages, embedded_asset},
    camera::Exposure,
    ecs::{lifecycle::HookContext, world::DeferredWorld},
    gltf::GltfAssetLabel,
    image::{ImageAddressMode, ImageLoaderSettings, ImageSampler, ImageSamplerDescriptor},
    math::Affine2,
    mesh::{Indices, PrimitiveTopology},
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    world_serialization::{WorldAsset, WorldAssetRoot},
};

use crate::{
    WorldSystems, button,
    config::WorldConfig,
    font8x8::FONT8X8_BASIC,
    glue::{self, Desktop, DesktopChange, Said},
    house::{self, Model, Slab, Surface},
    layout::{self, Layout, SCREEN_LIFT},
    seats::Seats,
    windows::DesktopWindow,
};

/// The desktop's workspaces, and the house, laid out again whenever they,
/// the desktop or the settings change, with the active frame lit.
pub fn plugin(app: &mut App) {
    embedded_asset!(app, "textures/floorboards.png");
    embedded_asset!(app, "models/chandelier.glb");
    embedded_asset!(app, "models/table.glb");
    embedded_asset!(app, "models/plant.glb");
    embedded_asset!(app, "models/pedestal.glb");
    app.init_resource::<WorkspaceIndex>()
        .init_resource::<House>()
        .insert_resource(GlobalAmbientLight {
            color: AMBIENT_COLOR,
            brightness: AMBIENT_LEVEL / AMBIENT_RESPONSE / Exposure::default().exposure(),
            ..default()
        })
        .add_systems(Startup, setup)
        .add_systems(PreUpdate, apply_workspaces.after(WorldSystems::Feed))
        .add_systems(
            Update,
            (
                relayout.run_if(
                    resource_changed::<Desktop>
                        .or_else(resource_changed::<WorldConfig>)
                        .or_else(resource_changed::<Seats>)
                        .or_else(glue::any_changed_or_removed::<DesktopWindow>)
                        .or_else(glue::any_changed_or_removed::<Workspace>),
                ),
                light_frames.run_if(resource_changed::<Desktop>),
            )
                .chain()
                .in_set(WorldSystems::House),
        );
}

/// A workspace on the app's monitor, as the plugin last said it (glue.rs
/// `DesktopChange`). While it has a frame it is that frame: its `Transform`
/// is where the frame hangs (layout.rs `Frame::pose`), and the frame's
/// parts, its windows' quads (windows.rs) and its layers' quads (layers.rs)
/// are its children, placed in the frame's coordinates. Its frame's label
/// shows its name.
#[derive(Component, Reflect, Clone, PartialEq, Debug)]
#[reflect(Component)]
#[require(Transform, Visibility)]
#[component(on_add = index_workspace, on_remove = unindex_workspace)]
pub struct Workspace {
    pub id: i64,
    /// Empty if it has none.
    pub name: String,
}

/// The workspaces' entities by id, kept by `Workspace`'s hooks, as
/// windows.rs `WindowIndex` is.
#[derive(Resource, Default)]
pub struct WorkspaceIndex(HashMap<i64, Entity>);

impl WorkspaceIndex {
    pub fn get(&self, id: i64) -> Option<Entity> {
        self.0.get(&id).copied()
    }

    pub fn ids(&self) -> impl Iterator<Item = i64> + '_ {
        self.0.keys().copied()
    }
}

fn index_workspace(mut world: DeferredWorld, HookContext { entity, .. }: HookContext) {
    let Some(id) = world.get::<Workspace>(entity).map(|w| w.id) else {
        return;
    };
    world.resource_mut::<WorkspaceIndex>().0.insert(id, entity);
}

fn unindex_workspace(mut world: DeferredWorld, HookContext { entity, .. }: HookContext) {
    let Some(id) = world.get::<Workspace>(entity).map(|w| w.id) else {
        return;
    };
    world.resource_mut::<WorkspaceIndex>().0.remove(&id);
}

/// Takes in what the plugin said of its workspaces, as windows.rs
/// `apply_windows` does its windows.
fn apply_workspaces(
    mut commands: Commands,
    mut changes: MessageReader<DesktopChange>,
    index: Res<WorkspaceIndex>,
    mut workspaces: Query<&mut Workspace>,
    children: Query<&Children>,
    windows: Query<(), With<DesktopWindow>>,
) {
    let said = DesktopChange::fold(changes.read(), &index.0, |change| match change {
        DesktopChange::Workspace(id, name) => Some((*id, Some(name))),
        DesktopChange::WorkspaceGone(id) => Some((*id, None)),
        _ => None,
    });
    for (id, said) in said {
        match said {
            Said::Came(name) => {
                commands.spawn(Workspace {
                    id,
                    name: name.clone(),
                });
            }
            Said::Still(entity, name) => {
                if let Ok(mut workspace) = workspaces.get_mut(entity) {
                    workspace.set_if_neq(Workspace {
                        id,
                        name: name.clone(),
                    });
                }
            }
            Said::Gone(entity) => {
                // Its frame's parts and its layers' quads go with it, but not
                // its windows: by now each is on another workspace or closed,
                // which is for windows.rs to see to, maybe in this batch.
                let on_it: Vec<Entity> = children
                    .get(entity)
                    .map(|children| {
                        children
                            .iter()
                            .filter(|&child| windows.contains(child))
                            .collect()
                    })
                    .unwrap_or_default();
                commands.entity(entity).detach_children(&on_it).despawn();
            }
        }
    }
}

/// The accent, a pale green, for the active workspace's bezel and name.
const ACCENT: Vec3 = Vec3::new(0.631, 0.945, 0.694);

// The lighting, in Bevy's lights at its default exposure (EV100 9.7), which
// the camera keeps. The house had a shader of its own, which added
// w·9/(9 + d²)·N·L per lamp to an sRGB light level of about 0.43 and lit the
// sRGB albedo with it. Bevy's point light gives the linear albedo
// lm/(4π²)·exposure·N·L/d², and its ambient 0.452·brightness·exposure (the
// diffuse EnvBRDF at roughness 1). Fitted over the floor and walls of a
// six-lamp room, the old lighting comes to about 0.25 + 6.5·w·Σ N·L/d²
// times the linear albedo, within 15%: the floor a little darker, the walls
// a little brighter.

/// Linear, as a share of the albedo.
const AMBIENT_LEVEL: f32 = 0.25;
/// The old ambient was a little blue.
const AMBIENT_COLOR: Color = Color::linear_rgb(0.96, 0.96, 1.08);
/// Bevy's diffuse ambient per cd/m², before the exposure.
pub const AMBIENT_RESPONSE: f32 = 0.452;
/// Linear, as a share of the albedo, at 1 m per unit of a lamp's
/// `house::Light::intensity`.
const LAMP_LEVEL: f32 = 6.5;
/// The old lamps' warm white, as it came out against the ambient.
const LAMP_COLOR: Color = Color::linear_rgb(1.0, 0.935, 0.82);
/// No specular to speak of, as before.
const ROUGHNESS: f32 = 0.9;
/// Inverse square is far brighter than the old falloff within a meter of a
/// lamp, which only the ceiling is: it takes this share of the light, and
/// shows `CEILING_LEVEL` of its albedo, linear, where the lamps do not reach.
const CEILING_SHARE: f32 = 0.3;
const CEILING_LEVEL: f32 = 0.15;

/// A lamp's light in lumens, for its `house::Light::intensity`.
fn lumens(intensity: f32) -> f32 {
    LAMP_LEVEL * intensity * 4.0 * std::f32::consts::PI.powi(2) / Exposure::default().exposure()
}

/// A surface of `albedo` (sRGB), lit by `share` of the light, and adding
/// `glow` of it (linear) whatever the light.
fn surface(albedo: Vec3, share: f32, glow: f32) -> StandardMaterial {
    let linear = Color::srgb(albedo.x, albedo.y, albedo.z).to_linear();
    let times = |k: f32| LinearRgba::rgb(linear.red * k, linear.green * k, linear.blue * k);
    StandardMaterial {
        base_color: times(share).into(),
        emissive: times(glow),
        perceptual_roughness: ROUGHNESS,
        ..default()
    }
}

/// A surface the old shader showed `glow` of whatever the light, mixing
/// albedo·L with the albedo in sRGB. Bevy adds emissive to the lit color,
/// linear: the two fitted at the old light levels in shade (0.43) and under
/// a lamp (0.8).
pub(crate) fn glowing(albedo: Vec3, glow: f32) -> StandardMaterial {
    let level = |light: f32| (glow + (1.0 - glow) * light).powf(2.2);
    let (shade, lamp) = (0.43_f32, 0.8_f32);
    let share = (level(lamp) - level(shade)) / (lamp.powf(2.2) - shade.powf(2.2));
    surface(albedo, share, level(shade) - share * shade.powf(2.2))
}

/// The layout the house is built for, and what its frames were built on.
/// The seats are fixed, and the workspaces downstairs are latched while the
/// world is in view, so frames never move under you; out of view the house
/// follows the desktop.
#[derive(Resource, Default)]
pub struct House {
    pub layout: Option<Layout>,
    /// Each frame's workspace and the name its label shows. A seat whose
    /// workspace does not exist, or a workspace that went while the world
    /// was in view, leaves its frame vacant, labeled with the id.
    frames: Vec<(Option<Entity>, String)>,
}

/// The shell: floor, walls, ceiling, trim, lamps and the panes in the
/// openings, the children of one entity, replaced as a whole when the
/// layout changes.
#[derive(Component)]
#[require(Transform, Visibility)]
pub struct Shell;

/// A frame's bezel, screen or name, a child of its workspace, built again
/// with the shell.
#[derive(Component)]
pub struct FramePart;

/// The screen of a frame with no workspace: a seat whose workspace does not
/// exist yet, or one that went while the world is in view. A click on it
/// opens the launcher, and what that opens goes to its workspace (windows.rs
/// `open_here`).
#[derive(Component)]
pub struct Vacant {
    pub workspace: i64,
}

/// What a new layout replaces: the shell's root, its children with it, and
/// every frame's parts.
type Built = Or<(With<Shell>, With<FramePart>)>;

/// Floor, walls, ceiling and the panes in the openings: what the player
/// collides with.
#[derive(Component)]
pub struct Solid;

/// A frame's bezel or name: its material while its workspace is not the
/// active one, and while it is.
#[derive(Component)]
pub struct Lit {
    workspace: i64,
    idle: Handle<StandardMaterial>,
    lit: Handle<StandardMaterial>,
}

#[derive(Resource)]
pub struct Look {
    cube: Handle<Mesh>,
    quad: Handle<Mesh>,
    /// Behind the layers and the windows, where no layer covers it. Unlit.
    screen: Handle<StandardMaterial>,
    /// The floor's boards, a tile of them (`BOARDS`).
    boards: Handle<Image>,
    /// The furniture, by `house::Model`, as `MODELS` lists them.
    models: [Handle<WorldAsset>; 4],
}

/// The furniture's models, embedded by `plugin`, each exported from the
/// .blend beside it (README.md, Models), in the order of `house::Model`.
const MODELS: [&str; 4] = [
    "embedded://hypr_world/models/chandelier.glb",
    "embedded://hypr_world/models/table.glb",
    "embedded://hypr_world/models/plant.glb",
    "embedded://hypr_world/models/pedestal.glb",
];

pub fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    assets: Res<AssetServer>,
) {
    commands.insert_resource(Look {
        cube: meshes.add(Cuboid::new(1.0, 1.0, 1.0)),
        quad: meshes.add(Rectangle::new(1.0, 1.0)),
        screen: materials.add(StandardMaterial {
            base_color: Color::srgb(0.02, 0.025, 0.035),
            unlit: true,
            ..default()
        }),
        // sRGB, and repeating: the floor tiles it (`relayout`).
        boards: assets
            .load_builder()
            .with_settings(|settings: &mut ImageLoaderSettings| {
                settings.is_srgb = true;
                settings.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
                    address_mode_u: ImageAddressMode::Repeat,
                    address_mode_v: ImageAddressMode::Repeat,
                    ..ImageSamplerDescriptor::linear()
                });
            })
            .load(BOARDS),
        models: MODELS.map(|path| assets.load(GltfAssetLabel::Scene(0).from_asset(path))),
    });
}

/// Rebuilds the house when the desktop, the seats or the settings change
/// enough to move or rename anything, and hangs each workspace with a frame
/// where its frame is: what hangs on it goes along.
#[allow(clippy::too_many_arguments)]
pub fn relayout(
    mut commands: Commands,
    desktop: Res<Desktop>,
    windows: Query<&DesktopWindow>,
    index: Res<WorkspaceIndex>,
    mut workspaces: Query<(&Workspace, &mut Transform)>,
    config: Res<WorldConfig>,
    seats: Res<Seats>,
    mut house: ResMut<House>,
    look: Res<Look>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    built: Query<Entity, Built>,
    mut latched: Local<Option<(u32, Vec<i64>)>>,
) {
    let Some(monitor) = desktop.monitor else {
        if house.layout.is_some() {
            *house = House::default();
            built.iter().for_each(|e| commands.entity(e).despawn());
        }
        return;
    };

    // A seat each upstairs, whether its workspace is about or not, and
    // every other workspace that is about downstairs.
    let count = seats.count(config.workspaces);
    let about = layout::framed(desktop.active, windows.iter().map(|w| w.workspace));
    let now = (
        count,
        layout::overflow(count, about.into_iter().chain(index.ids())),
    );
    let (count, overflow) = if desktop.shown {
        latched.get_or_insert(now).clone()
    } else {
        *latched = None;
        now
    };
    let upstairs: Vec<i64> = (1..=i64::from(count)).collect();

    // A frame shows the whole monitor, bar and all, as the screen does.
    let whole = Rect::from_corners(Vec2::ZERO, monitor.size);
    let next = Layout::new(&upstairs, &overflow, monitor.size, whole, &config.layout);
    let frames: Vec<(Option<Entity>, String)> = next
        .frames
        .iter()
        .map(|frame| {
            let entity = index.get(frame.workspace);
            let workspace = entity.and_then(|e| workspaces.get(e).ok()).map(|(w, _)| w);
            (entity, label(workspace, frame.workspace))
        })
        .collect();
    if house.layout.as_ref() == Some(&next) && house.frames == frames {
        return;
    }

    built.iter().for_each(|e| commands.entity(e).despawn());
    build(
        &mut commands,
        &look,
        &mut meshes,
        &mut materials,
        &mut images,
        &next,
        &frames,
        desktop.active,
    );
    for (frame, &(entity, _)) in next.frames.iter().zip(&frames) {
        if let Some(Ok((_, mut transform))) = entity.map(|e| workspaces.get_mut(e)) {
            transform.set_if_neq(Transform::from_isometry(frame.pose()));
        }
    }
    house.layout = Some(next);
    house.frames = frames;
}

/// A frame's label: its workspace's name, or the id when it has none, or
/// is gone.
fn label(workspace: Option<&Workspace>, id: i64) -> String {
    workspace
        .map(|w| w.name.as_str())
        .filter(|name| !name.is_empty())
        .map_or_else(|| id.to_string(), str::to_owned)
}

/// The shell, under its root, and each frame's parts, on its workspace.
#[allow(clippy::too_many_arguments)]
fn build(
    commands: &mut Commands,
    look: &Look,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    images: &mut Assets<Image>,
    layout: &Layout,
    frames: &[(Option<Entity>, String)],
    active: Option<i64>,
) {
    let shell = house::shell(layout);

    let floor = materials.add(StandardMaterial {
        base_color_texture: Some(look.boards.clone()),
        // A slab's u runs along x and v along z, in meters (`slab_mesh`).
        uv_transform: Affine2::from_scale(Vec2::ONE / BOARDS_TILE),
        perceptual_roughness: ROUGHNESS,
        ..default()
    });
    let mut material = |material: StandardMaterial| materials.add(material);
    let plain = |albedo: Vec3| surface(albedo, 1.0, 0.0);

    let wall = material(plain(Vec3::new(0.80, 0.76, 0.70)));
    let ceiling_albedo = Vec3::new(0.90, 0.89, 0.86);
    let ceiling = material(surface(
        ceiling_albedo,
        CEILING_SHARE,
        CEILING_LEVEL - CEILING_SHARE * AMBIENT_LEVEL,
    ));
    let trim = material(plain(Vec3::new(0.94, 0.93, 0.91)));
    let lamp = material(StandardMaterial {
        base_color: Color::srgb(1.0, 0.97, 0.9),
        unlit: true,
        ..default()
    });
    let brass = material(glowing(Vec3::new(0.82, 0.64, 0.3), 0.3));
    let wood = material(plain(Vec3::new(0.36, 0.22, 0.12)));
    let bezel_idle = material(plain(Vec3::new(0.07, 0.075, 0.085)));
    let bezel_lit = material(glowing(ACCENT * 0.9, 0.6));

    let root = commands.spawn(Shell).id();
    for block in &shell.blocks {
        let surface = match block.surface {
            Surface::Floor => &floor,
            Surface::Wall => &wall,
            Surface::Ceiling => &ceiling,
            Surface::Trim => &trim,
            Surface::Lamp => &lamp,
            Surface::Brass => &brass,
            Surface::Wood | Surface::Step => &wood,
        };
        let mut entity = commands.spawn((
            ChildOf(root),
            Mesh3d(look.cube.clone()),
            MeshMaterial3d(surface.clone()),
            Transform::from_translation(block.center)
                .with_rotation(block.rotation)
                .with_scale(block.size),
        ));
        if block.solid() {
            // A unit box, scaled with the block like its mesh.
            entity.insert((Solid, RigidBody::Static, Collider::cuboid(1.0, 1.0, 1.0)));
        }
    }

    for slab in &shell.slabs {
        let surface = if slab.surface == Surface::Floor {
            &floor
        } else {
            &ceiling
        };
        let points: Vec<Vec3> = slab
            .outline
            .iter()
            .flat_map(|&p| [p, p + Vec3::Y * slab.thickness])
            .collect();
        let mut entity = commands.spawn((
            ChildOf(root),
            Mesh3d(meshes.add(slab_mesh(slab))),
            MeshMaterial3d(surface.clone()),
            Transform::default(),
        ));
        if let Some(collider) = Collider::convex_hull(&points) {
            entity.insert((Solid, RigidBody::Static, collider));
        }
    }

    // No shadows: the light goes through the walls to their outsides, which
    // nobody sees, and under the table, as it always did.
    let radius = layout.rooms.iter().map(|r| r.radius()).fold(0.0, f32::max);
    let range = 2.0 * Vec2::new(radius, layout.height).length();
    for light in &shell.lights {
        commands.spawn((
            ChildOf(root),
            PointLight {
                color: LAMP_COLOR,
                intensity: lumens(light.intensity),
                range,
                shadow_maps_enabled: false,
                ..default()
            },
            Transform::from_translation(light.position),
        ));
    }

    // The panes in the openings and the ramp under the stairs: felt, not
    // seen.
    for glass in &shell.glass {
        let size = glass.size;
        commands.spawn((
            ChildOf(root),
            Solid,
            RigidBody::Static,
            Collider::cuboid(size.x, size.y, size.z),
            Transform::from_translation(glass.center).with_rotation(glass.rotation),
        ));
    }

    // The furniture, the pedestal rigged for its button once it is in.
    for placed in &shell.models {
        let mut entity = commands.spawn((
            ChildOf(root),
            WorldAssetRoot(look.models[placed.model as usize].clone()),
            Transform::from_translation(placed.at).with_rotation(placed.turn),
        ));
        if let (Model::Pedestal, Some(pedestal)) = (placed.model, shell.pedestal) {
            button::rig(&mut entity, pedestal);
        }
    }

    // The sign over the way down: pale words on its dark board.
    for sign in &shell.signs {
        let mask = images.add(label_image(sign.text));
        commands.spawn((
            ChildOf(root),
            Mesh3d(look.quad.clone()),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color_texture: Some(mask),
                alpha_mode: AlphaMode::Mask(0.5),
                ..glowing(Vec3::new(0.95, 0.9, 0.78), 0.7)
            })),
            Transform::from_translation(sign.center)
                .with_rotation(Quat::from_rotation_y(sign.yaw))
                .with_scale(sign.size.extend(1.0)),
        ));
    }

    // In the frame's coordinates, the same on every frame.
    let (min, max) = house::bezel(layout);
    let bezel = Transform::from_translation((min + max) / 2.0).with_scale(max - min);
    let screen = Transform::from_xyz(0.0, 0.0, SCREEN_LIFT).with_scale(layout.frame.extend(1.0));

    for (frame, (workspace, name)) in layout.frames.iter().zip(frames) {
        // A frame with no workspace, a seat for one not made yet or one that
        // went while the world is in view, hangs on the shell where its
        // frame is, and goes with it.
        let (parent, owned) = match *workspace {
            Some(workspace) => (workspace, true),
            None => (
                commands
                    .spawn((
                        ChildOf(root),
                        Transform::from_isometry(frame.pose()),
                        Visibility::default(),
                    ))
                    .id(),
                false,
            ),
        };
        let on = active == Some(frame.workspace);

        commands
            .spawn((
                ChildOf(parent),
                Lit {
                    workspace: frame.workspace,
                    idle: bezel_idle.clone(),
                    lit: bezel_lit.clone(),
                },
                Mesh3d(look.cube.clone()),
                MeshMaterial3d(if on {
                    bezel_lit.clone()
                } else {
                    bezel_idle.clone()
                }),
                bezel,
            ))
            .insert_if(FramePart, || owned);

        commands
            .spawn((
                ChildOf(parent),
                Mesh3d(look.quad.clone()),
                MeshMaterial3d(look.screen.clone()),
                screen,
            ))
            .insert_if(FramePart, || owned)
            .insert_if(
                Vacant {
                    workspace: frame.workspace,
                },
                || !owned,
            );

        let text = label_text(name);
        let mask = images.add(label_image(&text));
        let label = |material: StandardMaterial| StandardMaterial {
            base_color_texture: Some(mask.clone()),
            alpha_mode: AlphaMode::Mask(0.5),
            ..material
        };
        let idle = materials.add(label(plain(Vec3::new(0.28, 0.27, 0.26))));
        let lit = materials.add(label(glowing(ACCENT, 0.8)));
        let (center, size) = house::label(layout, text.len());
        commands
            .spawn((
                ChildOf(parent),
                Lit {
                    workspace: frame.workspace,
                    idle: idle.clone(),
                    lit: lit.clone(),
                },
                Mesh3d(look.quad.clone()),
                MeshMaterial3d(if on { lit } else { idle }),
                Transform::from_translation(center).with_scale(size.extend(1.0)),
            ))
            .insert_if(FramePart, || owned);
    }
}

/// A slab's mesh: its top, its bottom and its edges, each face flat,
/// facing out, its uv in meters: x and z on the top and the bottom, along
/// and up on the edges.
fn slab_mesh(slab: &Slab) -> Mesh {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    // A flat convex face, its corners in order either way round, as a fan
    // wound to face `normal`.
    let mut face = |corners: &[Vec3], normal: Vec3, uv: &dyn Fn(Vec3) -> Vec2| {
        let base = positions.len() as u32;
        for &corner in corners {
            positions.push(corner.to_array());
            normals.push(normal.to_array());
            uvs.push(uv(corner).to_array());
        }
        for i in 1..corners.len() as u32 - 1 {
            let (a, b, c) = (corners[0], corners[i as usize], corners[i as usize + 1]);
            let (j, k) = if (b - a).cross(c - a).dot(normal) >= 0.0 {
                (i, i + 1)
            } else {
                (i + 1, i)
            };
            indices.extend([base, base + j, base + k]);
        }
    };

    let up = Vec3::Y * slab.thickness;
    let bottom = &slab.outline;
    let top: Vec<Vec3> = bottom.iter().map(|&p| p + up).collect();
    let plan = |p: Vec3| Vec2::new(p.x, p.z);
    face(&top, Vec3::Y, &plan);
    face(bottom, Vec3::NEG_Y, &plan);

    let middle = bottom.iter().sum::<Vec3>() / bottom.len() as f32;
    for (i, &a) in bottom.iter().enumerate() {
        let b = bottom[(i + 1) % bottom.len()];
        let along = (b - a).normalize();
        let out = ((a + b) / 2.0 - middle).with_y(0.0).normalize();
        face(&[a, b, b + up, a + up], out, &|p| {
            Vec2::new((p - a).dot(along), p.y)
        });
    }

    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
    .with_inserted_indices(Indices::U32(indices))
}

/// The font only has ASCII: anything else is a '?' per byte.
fn label_text(name: &str) -> String {
    name.bytes()
        .map(|b| if b < 128 { b as char } else { '?' })
        .collect()
}

/// `text` in font8x8, white where a glyph is lit and clear elsewhere, one
/// texel per font pixel.
fn label_image(text: &str) -> Image {
    let width = 8 * text.len().max(1);
    let mut pixels = vec![0u8; width * 8 * 4];

    for (i, byte) in text.bytes().enumerate() {
        for (row, bits) in FONT8X8_BASIC[byte as usize & 0x7f].iter().enumerate() {
            for col in 0..8 {
                if bits & (1 << col) != 0 {
                    let at = (row * width + i * 8 + col) * 4;
                    pixels[at..at + 4].fill(255);
                }
            }
        }
    }

    let mut image = Image::new(
        Extent3d {
            width: width as u32,
            height: 8,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        pixels,
        TextureFormat::Rgba8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    );
    image.sampler = ImageSampler::nearest();
    image
}

/// The floor's tile, `BOARDS_TILE`, embedded by `plugin`: oak boards along
/// x, 18 cm wide and 1.4 m long, every row staggered, each row wrapping round
/// so that the tile repeats. 200 texels a meter, so a seam is 8 mm.
const BOARDS: &str = "embedded://hypr_world/textures/floorboards.png";
const BOARD_WIDTH: f32 = 0.18;
const BOARD_LENGTH: f32 = 1.4;
/// The tile's size in meters, x by z, three boards by 28 rows.
const BOARDS_TILE: Vec2 = Vec2::new(3.0 * BOARD_LENGTH, 28.0 * BOARD_WIDTH);

/// Lights the active workspace's bezel and name.
pub fn light_frames(
    desktop: Res<Desktop>,
    mut parts: Query<(&Lit, &mut MeshMaterial3d<StandardMaterial>)>,
) {
    for (lit, mut material) in &mut parts {
        let want = if desktop.active == Some(lit.workspace) {
            &lit.lit
        } else {
            &lit.idle
        };
        material.set_if_neq(MeshMaterial3d(want.clone()));
    }
}

#[cfg(test)]
mod tests {
    use bevy::ecs::system::RunSystemOnce;

    use super::*;

    /// The workspaces the plugin said, as entities: their labels, and their
    /// entities by id.
    fn workspaces(app: &mut App) -> Vec<(i64, String, Entity)> {
        let mut said =
            app.world_mut()
                .run_system_once(
                    |workspaces: Query<&Workspace>, index: Res<WorkspaceIndex>| {
                        let ids = index.0.iter().map(|(&id, &entity)| {
                            (id, label(workspaces.get(entity).ok(), id), entity)
                        });
                        ids.collect::<Vec<_>>()
                    },
                )
                .expect("the system runs");
        said.sort_unstable();
        said
    }

    /// What the plugin says of its workspaces comes to an entity each, said
    /// again or renamed in place, labeled by its name or else its id, and
    /// gone after a reset unless said again.
    #[test]
    fn the_plugins_workspaces_are_entities() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<DesktopChange>()
            .init_resource::<WorkspaceIndex>()
            .add_systems(PreUpdate, apply_workspaces);
        let tell = |app: &mut App, changes: Vec<DesktopChange>| {
            for change in changes {
                app.world_mut().write_message(change);
            }
            app.update();
        };

        tell(
            &mut app,
            vec![
                DesktopChange::Workspace(1, "web".to_string()),
                DesktopChange::Workspace(2, String::new()),
                DesktopChange::Workspace(3, "chat".to_string()),
                DesktopChange::WorkspaceGone(3),
            ],
        );
        let said = workspaces(&mut app);
        let labels: Vec<(i64, &str)> = said
            .iter()
            .map(|(id, label, _)| (*id, label.as_str()))
            .collect();
        assert_eq!(
            labels,
            [(1, "web"), (2, "2")],
            "a label is the name, or else the id"
        );
        let two = said[1].2;

        tell(
            &mut app,
            vec![DesktopChange::Workspace(2, "mail".to_string())],
        );
        assert_eq!(
            workspaces(&mut app)[1],
            (2, "mail".to_string(), two),
            "renamed in place"
        );

        tell(
            &mut app,
            vec![
                DesktopChange::Reset,
                DesktopChange::Workspace(2, "mail".to_string()),
            ],
        );
        assert_eq!(
            workspaces(&mut app),
            [(2, "mail".to_string(), two)],
            "after a reset, only what was said again"
        );
    }

    /// A workspace that goes takes its frame with it, but not a window still
    /// hanging on it: one that moved to another workspace in the same batch
    /// is let go, for windows.rs to hang on its new frame.
    #[test]
    fn a_workspace_that_goes_leaves_its_windows() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<DesktopChange>()
            .init_resource::<WorkspaceIndex>()
            .init_resource::<crate::windows::WindowIndex>()
            .add_systems(PreUpdate, apply_workspaces);

        let workspace = app
            .world_mut()
            .spawn(Workspace {
                id: 3,
                name: String::new(),
            })
            .id();
        let part = app.world_mut().spawn((FramePart, ChildOf(workspace))).id();
        let moved = DesktopWindow {
            address: 0xa,
            workspace: 2,
            rect: Rect::new(0.0, 0.0, 10.0, 10.0),
            stack: 0,
            kind: glue::Kind::Tiled,
        };
        let window = app.world_mut().spawn((moved, ChildOf(workspace))).id();

        app.world_mut()
            .write_message(DesktopChange::WorkspaceGone(3));
        app.update();
        assert!(
            app.world().get_entity(part).is_err(),
            "its frame went with it"
        );
        let window = app.world().get_entity(window).expect("the window stays");
        assert!(!window.contains::<ChildOf>(), "let go");
    }
}
