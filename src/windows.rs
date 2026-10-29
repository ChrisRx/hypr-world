//! The windows on the walls: an entity per window on the desktop, and on a
//! framed workspace its quad, a child of its frame seated from its live 2D
//! box, its texture kept up to date from the capture while it is in view.
//! And what the crosshair does with them: aim, and the dwell that switches
//! workspace.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use bevy::{
    asset::RenderAssetUsages,
    camera::{
        primitives::{Aabb, Frustum},
        visibility::VisibilitySystems,
    },
    ecs::{lifecycle::HookContext, system::SystemParam, world::DeferredWorld},
    math::Affine2,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    transform::helper::TransformHelper,
};

use crate::{
    FlyCamera, WorldSystems,
    capture::{self, DmabufFrame, PixelLayout, ShmFrame},
    config::WorldConfig,
    flight::WorldState,
    glue::{self, Desktop, DesktopChange, InputMode, Kind, Said, Window2d},
    import::{self, BufferFormat},
    layout,
    materials::{YuvMaterial, YuvParams},
    scene::{House, Vacant, WorkspaceIndex},
    stats::Stats,
    wake::WakeAt,
};

/// How long the crosshair rests on a window of another workspace before the
/// app switches to it.
const DWELL: f32 = 0.25;

/// Windows copied at least this often, in seconds between copies, are kept
/// awake (`announce_pace`); the rest Hyprland may suspend.
const AWAKE: f32 = 1.0 / 15.0;

/// The least time between copies of a window, in seconds, by how large it
/// is on screen (`Sight::share`): every frame from half the view up, fifteen
/// a second from a fifth, four from a twelfth, and one below that.
fn copy_interval(share: f32) -> f32 {
    match share {
        share if share >= 0.5 => 0.0,
        share if share >= 0.2 => AWAKE,
        share if share >= 1.0 / 12.0 => 0.25,
        _ => 1.0,
    }
}

/// The capture thread, which copies windows when asked.
#[derive(Resource)]
pub struct Captures(pub capture::Capture);

/// A window's quad on its frame, on the window's entity: its picture and
/// what the capture thread was told of it.
#[derive(Component)]
pub struct WindowQuad {
    handle: u32,
    /// The capture thread's GPU buffers its dmabuf frames came in, by
    /// `DmabufSlot::id`, each sampled through an image and a material of its
    /// own (import.rs).
    slots: HashMap<u64, Slot>,
    /// Those of them the compositor must not write into yet.
    kept: capture::Kept,
    /// The window's own surface's buffers, while the plugin shares them in
    /// place of captures (glue.rs `Shared`). Its other surfaces are quads
    /// of their own, its children (`SurfaceQuad`).
    own: OwnBuffers,
    /// The plugin went back to capture for it: its own buffers stay on it,
    /// and its surfaces' on theirs, until a captured frame comes.
    unsharing: bool,
    /// The buffer size of the frame on it, zero before the first.
    size: UVec2,
    layout: Option<PixelLayout>,
    /// The frame on it is upside down (a y-inverted dmabuf), and its
    /// material samples it so.
    flipped: bool,
    /// Its first frame was asked for.
    asked_first: bool,
    /// What the capture thread was last told: copies at most this often,
    /// or none.
    want: Option<Duration>,
    /// Shared with a surface below its own (glue.rs `Surfaces`), seen
    /// where its own is clear.
    below: bool,
    /// The buffer of its own surface on it has alpha.
    alpha: bool,
}

/// A window on the desktop, as the plugin last said it (glue.rs
/// `DesktopChange`). Its entity is its quad too while its workspace is on a
/// frame: `sync_quads` puts the `WindowQuad`, mesh and material on it then,
/// and makes it a child of the frame, and takes them off when not.
#[derive(Component, Reflect, Clone, Copy, PartialEq, Debug)]
#[reflect(Component)]
#[component(on_add = index_window, on_remove = unindex_window)]
pub struct DesktopWindow {
    pub address: u64,
    pub workspace: i64,
    /// Its current (animated) box, monitor-local logical px.
    pub rect: Rect,
    /// Its place in Hyprland's stacking order.
    pub stack: u32,
    pub kind: Kind,
}

impl DesktopWindow {
    fn new(address: u64, window: &Window2d) -> Self {
        let Window2d {
            workspace,
            rect,
            stack,
            kind,
        } = *window;
        Self {
            address,
            workspace,
            rect,
            stack,
            kind,
        }
    }
}

/// The windows' entities by address, and by export handle for the frames
/// the capture thread hands back. Kept by `DesktopWindow`'s hooks as its
/// entities come and go, never by hand.
#[derive(Resource, Default)]
pub struct WindowIndex {
    by_address: HashMap<u64, Entity>,
    by_handle: HashMap<u32, Entity>,
}

impl WindowIndex {
    pub fn get(&self, address: u64) -> Option<Entity> {
        self.by_address.get(&address).copied()
    }
}

fn index_window(mut world: DeferredWorld, HookContext { entity, .. }: HookContext) {
    let Some(&DesktopWindow { address, .. }) = world.get::<DesktopWindow>(entity) else {
        return;
    };
    let mut index = world.resource_mut::<WindowIndex>();
    index.by_address.insert(address, entity);
    index
        .by_handle
        .insert(capture::export_handle(address), entity);
}

fn unindex_window(mut world: DeferredWorld, HookContext { entity, .. }: HookContext) {
    let Some(&DesktopWindow { address, .. }) = world.get::<DesktopWindow>(entity) else {
        return;
    };
    let mut index = world.resource_mut::<WindowIndex>();
    index.by_address.remove(&address);
    index.by_handle.remove(&capture::export_handle(address));
}

/// Takes in what the plugin said of its windows: a new one becomes an
/// entity, one said again gets its `DesktopWindow` (marked changed only if
/// it is), and a closed one goes, and its capture with its quad
/// (`forget_capture`). In `PreUpdate`, after the drain, so that `Update`
/// finds them.
fn apply_windows(
    mut commands: Commands,
    mut changes: MessageReader<DesktopChange>,
    index: Res<WindowIndex>,
    mut windows: Query<&mut DesktopWindow>,
) {
    let said = DesktopChange::fold(changes.read(), &index.by_address, |change| match change {
        DesktopChange::Window(address, window) => Some((*address, Some(window))),
        DesktopChange::Closed(address) => Some((*address, None)),
        _ => None,
    });
    for (address, said) in said {
        match said {
            Said::Came(window) => {
                commands.spawn(DesktopWindow::new(address, window));
            }
            Said::Still(entity, window) => {
                if let Ok(mut current) = windows.get_mut(entity) {
                    current.set_if_neq(DesktopWindow::new(address, window));
                }
            }
            Said::Gone(entity) => {
                commands.entity(entity).despawn();
            }
        }
    }
}

/// A window's quad gone, off its frame or with the window: the capture
/// thread is done with the window, its copy in flight and its buffers go,
/// and so do their images in the render world (import.rs). An observer of
/// the removal runs while the quad is still there to read.
fn forget_capture(
    gone: On<Remove<WindowQuad>>,
    mut quads: Query<&mut WindowQuad>,
    captures: Res<Captures>,
    mut dmabufs: ResMut<import::DmabufImages>,
    mut log: ResMut<import::SharedLog>,
) {
    let Ok(mut quad) = quads.get_mut(gone.entity) else {
        return;
    };
    captures.0.forget(quad.handle);
    quad.forget_slots(&mut dmabufs);
    quad.own.let_go(&mut log, &mut dmabufs);
}

/// A surface's quad gone, with its window or not: its buffer goes back to
/// its client once the GPU is done with it, and its images go.
fn forget_surface(
    gone: On<Remove<SurfaceQuad>>,
    quads: Query<&SurfaceQuad>,
    mut dmabufs: ResMut<import::DmabufImages>,
    mut log: ResMut<import::SharedLog>,
) {
    if let Ok(quad) = quads.get(gone.entity) {
        quad.own.let_go(&mut log, &mut dmabufs);
    }
}

#[derive(Resource)]
pub struct QuadMesh(pub Handle<Mesh>);

/// The window under the crosshair, by address.
#[derive(Resource, Default)]
pub struct Aimed(pub Option<u64>);

/// The workspace of the vacant frame under the crosshair (scene.rs
/// `Vacant`), when no window is.
#[derive(Resource, Default)]
pub struct AimedFrame(pub Option<i64>);

/// The `aim` line the plugin was last told, so that it is told only changes.
#[derive(Resource, Default)]
pub struct AimLine(String);

impl AimLine {
    fn say(&mut self, glue: &glue::Glue, line: String) {
        if self.0 != line {
            glue.send(&line);
            self.0 = line;
        }
    }
}

pub fn setup(mut commands: Commands, mut meshes: ResMut<Assets<Mesh>>) {
    commands.insert_resource(QuadMesh(meshes.add(Rectangle::new(1.0, 1.0))));
}

/// Keeps a quad on each window of a framed workspace, a child of the frame
/// seated where the window is in 2D, and none on the rest. A window's change
/// can move its workspace's others (`ranks`), so all are seated again, and
/// only those that moved are marked changed.
#[allow(clippy::too_many_arguments)]
pub fn sync_quads(
    mut commands: Commands,
    house: Res<House>,
    workspaces: Res<WorkspaceIndex>,
    mesh: Res<QuadMesh>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut windows: Query<(
        Entity,
        &DesktopWindow,
        Option<&mut Transform>,
        Has<WindowQuad>,
    )>,
    parents: Query<&ChildOf>,
) {
    let layout = house.layout.as_ref();
    let ranks = ranks(windows.iter().map(|(entity, window, ..)| (entity, window)));

    for (entity, window, transform, has_quad) in &mut windows {
        // Its frame: its workspace, while the layout has a frame for it.
        let framed = layout.filter(|l| l.frame_of(window.workspace).is_some());
        let Some((layout, frame)) = framed.zip(workspaces.get(window.workspace)) else {
            // No frame for it (any more): no quad, nor its surfaces' (its
            // children), and nothing to copy.
            if has_quad {
                commands.entity(entity).despawn_children().remove::<(
                    WindowQuad,
                    Mesh3d,
                    MeshMaterial3d<StandardMaterial>,
                    ChildOf,
                )>();
            }
            continue;
        };
        let (centre, size) = layout.seat(window.rect, ranks.get(&entity).copied().unwrap_or(0));
        let seated = Transform::from_translation(centre).with_scale(size.extend(1.0));

        // On another workspace, or new: onto its frame.
        if parents.get(entity).ok().map(ChildOf::parent) != Some(frame) {
            commands.entity(entity).insert(ChildOf(frame));
        }
        if has_quad && let Some(mut transform) = transform {
            transform.set_if_neq(seated);
            continue;
        }

        // A dark placeholder until its first frame.
        let image = images.add(Image::new_fill(
            Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            &[5, 6, 9, 255],
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::RENDER_WORLD,
        ));
        let material = materials.add(StandardMaterial {
            base_color_texture: Some(image.clone()),
            unlit: true,
            ..default()
        });
        commands.entity(entity).insert((
            WindowQuad::new(capture::export_handle(window.address)),
            Mesh3d(mesh.0.clone()),
            MeshMaterial3d(material),
            seated,
        ));
    }
}

/// How far out from its frame each window sits: 0 tiled, 1..n floating in
/// stacking order, n+2 fullscreen, per workspace. The top layers (the bar,
/// layers.rs) are n+1 in between, as in Hyprland.
fn ranks<'a>(
    windows: impl IntoIterator<Item = (Entity, &'a DesktopWindow)>,
) -> HashMap<Entity, u32> {
    let mut floating: HashMap<i64, Vec<(u32, Entity)>> = HashMap::new();
    let mut fullscreen = Vec::new();
    for (entity, window) in windows {
        match window.kind {
            Kind::Floating => floating
                .entry(window.workspace)
                .or_default()
                .push((window.stack, entity)),
            Kind::Fullscreen => fullscreen.push((entity, window.workspace)),
            Kind::Tiled => {}
        }
    }

    let mut ranks = HashMap::new();
    for list in floating.values_mut() {
        list.sort_unstable();
        for (i, &(_, entity)) in list.iter().enumerate() {
            ranks.insert(entity, i as u32 + 1);
        }
    }
    for (entity, workspace) in fullscreen {
        ranks.insert(
            entity,
            floating.get(&workspace).map_or(0, |l| l.len() as u32) + 2,
        );
    }
    ranks
}

/// The rank of the top layers on `workspace`'s frame: over its floating
/// windows, under a fullscreen one.
pub fn top_rank<'a>(windows: impl IntoIterator<Item = &'a DesktopWindow>, workspace: i64) -> u32 {
    let floating = windows
        .into_iter()
        .filter(|w| w.workspace == workspace && w.kind == Kind::Floating)
        .count();
    floating as u32 + 1
}

/// A dmabuf as a quad samples it, a capture slot or the window's own
/// buffer: an image id of its own, which the render world puts the imported
/// buffer under (import.rs), and a material, with the part of the buffer it
/// shows (`uv_transform`).
pub struct Slot {
    image: Handle<Image>,
    material: Handle<StandardMaterial>,
    uv: Affine2,
}

impl Slot {
    /// A slot sampling `dmabuf` through `uv`: its image reserved, the
    /// render world told what to put under it, and a material of its own.
    fn new(
        dmabuf: import::DmabufImage,
        uv: Affine2,
        alpha_mode: AlphaMode,
        images: &mut Assets<Image>,
        materials: &mut Assets<StandardMaterial>,
        dmabufs: &mut import::DmabufImages,
    ) -> Self {
        let image = images.reserve_handle();
        dmabufs.0.insert(image.id(), dmabuf);
        let material = materials.add(StandardMaterial {
            base_color_texture: Some(image.clone()),
            uv_transform: uv,
            alpha_mode,
            unlit: true,
            ..default()
        });
        Self {
            image,
            material,
            uv,
        }
    }

    /// Its material shows the part `uv` of the buffer from here.
    fn set_uv(&mut self, uv: Affine2, materials: &mut Assets<StandardMaterial>) {
        if self.uv != uv
            && let Some(material) = materials.get_mut(&self.material)
        {
            material.into_inner().uv_transform = uv;
            self.uv = uv;
        }
    }
}

impl WindowQuad {
    /// The quad of the window whose export handle is `handle`, before its
    /// first frame.
    pub(crate) fn new(handle: u32) -> Self {
        Self {
            handle,
            slots: HashMap::new(),
            kept: capture::Kept::default(),
            own: OwnBuffers::default(),
            unsharing: false,
            size: UVec2::ZERO,
            layout: None,
            flipped: false,
            asked_first: false,
            want: None,
            below: false,
            alpha: false,
        }
    }

    /// How its own surface's buffers are drawn: opaque, as a captured frame
    /// is, unless a surface below shows through its clear parts. Then cut
    /// out where it is clear rather than blended: a mask stays in the
    /// depth-tested passes, where blending would sort it with its surfaces
    /// above by their centres, which at a slant can put it over them. A
    /// pixel half clear or more is a hole, the rest opaque.
    fn alpha_mode(&self) -> AlphaMode {
        if self.below && self.alpha {
            AlphaMode::Mask(0.5)
        } else {
            AlphaMode::Opaque
        }
    }

    /// Lets go of its slots: Bevy drops their images and materials with
    /// their last handles, in the render world too.
    fn forget_slots(&mut self, dmabufs: &mut import::DmabufImages) {
        for slot in self.slots.values() {
            dmabufs.0.remove(&slot.image.id());
        }
        self.slots.clear();
        self.kept = capture::Kept::default();
    }

    /// Off the window's own buffers.
    fn forget_shared(&mut self, log: &mut import::SharedLog, dmabufs: &mut import::DmabufImages) {
        self.own.forget(log, dmabufs);
        self.unsharing = false;
    }

    /// It shows the window's own buffers, and wants no captures.
    fn shares(&self) -> bool {
        self.own.showing() && !self.unsharing
    }
}

/// A surface's own buffers while the plugin shares them (glue.rs `Shared`):
/// its dmabufs by the file each is (its inode), each sampled through an
/// image and a material of its own like a capture's slots, or in YUV
/// through an image a plane and a `YuvMaterial`, the pixels last copied out
/// of its shared memory, and which is on it now. A client draws into a few
/// in turn.
#[derive(Default)]
pub struct OwnBuffers {
    slots: HashMap<u64, Slot>,
    yuv: HashMap<u64, YuvSlot>,
    on: Option<Showing>,
    /// What its dmabufs are: a new size or format comes in new ones.
    kind: Option<(UVec2, BufferFormat)>,
    pixels: Option<Pixels>,
}

/// A dmabuf in YUV as a quad samples it: an image id of its own for each
/// plane, which the render world puts the plane's view under (import.rs
/// `plane_image`), and the material that turns them into RGB.
struct YuvSlot {
    luma: Handle<Image>,
    chroma: Handle<Image>,
    material: Handle<YuvMaterial>,
    params: YuvParams,
}

/// The material a surface's quad draws its buffer with, of the kind it is.
#[derive(Clone, PartialEq)]
pub enum Look {
    Rgb(Handle<StandardMaterial>),
    Yuv(Handle<YuvMaterial>),
}

/// Which of a surface's own buffers is on its quad.
#[derive(Clone, Copy)]
enum Showing {
    /// A dmabuf: the plugin's id for it, and its key in `slots`.
    Dmabuf { id: u64, key: u64 },
    /// The pixels last copied out of its shared memory.
    Pixels,
}

/// An image the app fills with pixels copied out of a surface's shared
/// memory, of their size and byte order, and the material that samples it.
struct Pixels {
    image: Handle<Image>,
    material: Handle<StandardMaterial>,
    size: UVec2,
    layout: PixelLayout,
}

impl OwnBuffers {
    fn showing(&self) -> bool {
        self.on.is_some()
    }

    /// Puts `buffer` on it (`show_dmabuf`, `show_yuv`, `show_pixels`),
    /// blending by `alpha_mode`, and gives the material to draw it with.
    /// None for a dmabuf whose file cannot be told apart from others, which
    /// never goes on.
    fn put(
        &mut self,
        buffer: glue::SharedBuffer,
        alpha_mode: AlphaMode,
        pictures: &mut Pictures,
    ) -> Option<Look> {
        match buffer {
            glue::SharedBuffer::Dmabuf { id, crop, dmabuf } => {
                let key = dmabuf
                    .planes
                    .first()
                    .and_then(|plane| rustix::fs::fstat(&plane.fd).ok())?
                    .st_ino;
                Some(match dmabuf.format {
                    BufferFormat::Rgb(_) => {
                        Look::Rgb(self.show_dmabuf(key, id, crop, dmabuf, alpha_mode, pictures))
                    }
                    BufferFormat::Yuv(_) => {
                        Look::Yuv(self.show_yuv(key, id, crop, dmabuf, pictures))
                    }
                })
            }
            glue::SharedBuffer::Pixels { frame, .. } => {
                Some(Look::Rgb(self.show_pixels(frame, alpha_mode, pictures)))
            }
        }
    }

    /// Its dmabufs are now of `size` and `format`: those of another go.
    fn of_kind(&mut self, size: UVec2, format: BufferFormat, pictures: &mut Pictures) {
        let kind = (size, format);
        if self.kind != Some(kind) {
            self.forget(&mut pictures.log, &mut pictures.dmabufs);
            self.kind = Some(kind);
        }
    }

    /// Puts the YUV `dmabuf`, the file `key` and the plugin's buffer `id`,
    /// on it, as `show_dmabuf` does an RGB one: an image a plane, the
    /// render world's views of the one texture, and a material that samples
    /// both, made the first time it comes.
    fn show_yuv(
        &mut self,
        key: u64,
        id: u64,
        crop: URect,
        dmabuf: import::Dmabuf,
        pictures: &mut Pictures,
    ) -> Handle<YuvMaterial> {
        let size = UVec2::new(dmabuf.width, dmabuf.height);
        self.of_kind(size, dmabuf.format, pictures);
        let uv = cropped(crop, size);
        let params = YuvParams {
            crop: Vec4::new(
                uv.matrix2.x_axis.x,
                uv.matrix2.y_axis.y,
                uv.translation.x,
                uv.translation.y,
            ),
        };

        let Pictures {
            images,
            yuv_materials,
            dmabufs,
            log,
            ..
        } = pictures;
        let slot = self.yuv.entry(key).or_insert_with(|| {
            let (luma, chroma) = (images.reserve_handle(), images.reserve_handle());
            dmabufs
                .0
                .insert(luma.id(), import::DmabufImage::Shared(Arc::new(dmabuf)));
            dmabufs
                .0
                .insert(chroma.id(), import::DmabufImage::Chroma { luma: luma.id() });
            let material = yuv_materials.add(YuvMaterial {
                params,
                luma: luma.clone(),
                chroma: chroma.clone(),
            });
            YuvSlot {
                luma,
                chroma,
                material,
                params,
            }
        });
        if slot.params != params
            && let Some(material) = yuv_materials.get_mut(&slot.material)
        {
            material.into_inner().params = params;
            slot.params = params;
        }
        let (image, material) = (slot.luma.id(), slot.material.clone());
        self.now_showing(id, key, image, log);
        material
    }

    /// Puts `dmabuf`, the file `key` and the plugin's buffer `id`, on it:
    /// the material of the buffer, made the first time it comes, showing the
    /// part of it the plugin said (`crop`).
    fn show_dmabuf(
        &mut self,
        key: u64,
        id: u64,
        crop: URect,
        dmabuf: import::Dmabuf,
        alpha_mode: AlphaMode,
        pictures: &mut Pictures,
    ) -> Handle<StandardMaterial> {
        let size = UVec2::new(dmabuf.width, dmabuf.height);
        self.of_kind(size, dmabuf.format, pictures);
        let uv = cropped(crop, size);

        let Pictures {
            images,
            materials,
            dmabufs,
            log,
            ..
        } = pictures;
        let slot = self.slots.entry(key).or_insert_with(|| {
            Slot::new(
                import::DmabufImage::Shared(Arc::new(dmabuf)),
                uv,
                alpha_mode,
                images,
                materials,
                dmabufs,
            )
        });
        slot.set_uv(uv, materials);
        let (image, material) = (slot.image.id(), slot.material.clone());
        self.now_showing(id, key, image, log);
        material
    }

    /// The dmabuf `key`, the plugin's buffer `id`, is on it now, through
    /// `image`: the one that was goes back.
    fn now_showing(
        &mut self,
        id: u64,
        key: u64,
        image: AssetId<Image>,
        log: &mut import::SharedLog,
    ) {
        let was = self.on.replace(Showing::Dmabuf { id, key });
        self.retire(was, log);
        log.shown(image);
        // Off its shared memory, if it was: those pixels are of no more use.
        self.pixels = None;
    }

    /// Puts pixels copied out of its shared memory on it: into the image of
    /// its pixels, made anew for a new size or byte order, with a material of
    /// its own. Its client has had the memory back since the copy.
    fn show_pixels(
        &mut self,
        frame: ShmFrame,
        alpha_mode: AlphaMode,
        pictures: &mut Pictures,
    ) -> Handle<StandardMaterial> {
        let size = UVec2::new(frame.width, frame.height);
        let Pictures {
            images,
            materials,
            log,
            ..
        } = pictures;
        match &self.pixels {
            Some(pixels) if pixels.size == size && pixels.layout == frame.layout => {
                if let Some(image) = images.get_mut(&pixels.image) {
                    image.into_inner().data = Some(frame.pixels);
                }
            }
            _ => {
                // The GPU's alone, as a captured frame's pixels are. A new
                // image, and a new material made with it, need no rebind.
                let image = images.add(Image::new(
                    Extent3d {
                        width: frame.width,
                        height: frame.height,
                        depth_or_array_layers: 1,
                    },
                    TextureDimension::D2,
                    frame.pixels,
                    frame.layout.srgb(),
                    RenderAssetUsages::RENDER_WORLD,
                ));
                let material = materials.add(StandardMaterial {
                    base_color_texture: Some(image.clone()),
                    alpha_mode,
                    unlit: true,
                    ..default()
                });
                self.pixels = Some(Pixels {
                    image,
                    material,
                    size,
                    layout: frame.layout,
                });
            }
        }
        let was = self.on.replace(Showing::Pixels);
        self.retire(was, log);
        self.pixels
            .as_ref()
            .map(|pixels| pixels.material.clone())
            .unwrap_or_default()
    }

    /// The dmabuf that was on it, off it: back to its client once the GPU
    /// is done with it (import.rs `SharedLog`). Shared memory went back as
    /// it was copied.
    fn retire(&self, was: Option<Showing>, log: &mut import::SharedLog) {
        if let Some(Showing::Dmabuf { id, key }) = was
            && let Some(image) = self.image_of(key)
        {
            log.retired(image, id);
        }
    }

    /// The image the dmabuf `key` is taken over and handed back by: its own,
    /// or in YUV its luma's.
    fn image_of(&self, key: u64) -> Option<AssetId<Image>> {
        self.slots
            .get(&key)
            .map(|slot| slot.image.id())
            .or_else(|| self.yuv.get(&key).map(|slot| slot.luma.id()))
    }

    /// Every material of its RGB buffers blending by `alpha_mode` from
    /// here, those it has already too: one made before a surface came below
    /// the window's own was made opaque.
    fn blend(&self, alpha_mode: AlphaMode, materials: &mut Assets<StandardMaterial>) {
        let made = self
            .slots
            .values()
            .map(|slot| &slot.material)
            .chain(self.pixels.iter().map(|pixels| &pixels.material));
        for handle in made {
            if materials
                .get(handle)
                .is_some_and(|material| material.alpha_mode != alpha_mode)
                && let Some(material) = materials.get_mut(handle)
            {
                material.into_inner().alpha_mode = alpha_mode;
            }
        }
    }

    /// Lets go of them as they go with their quad: the dmabuf on it handed
    /// back once the GPU is done with it, and their images gone, in the
    /// render world too.
    fn let_go(&self, log: &mut import::SharedLog, dmabufs: &mut import::DmabufImages) {
        self.retire(self.on, log);
        for slot in self.slots.values() {
            dmabufs.0.remove(&slot.image.id());
        }
        for slot in self.yuv.values() {
            dmabufs.0.remove(&slot.luma.id());
            dmabufs.0.remove(&slot.chroma.id());
        }
    }

    /// Lets go of them, and is empty again.
    fn forget(&mut self, log: &mut import::SharedLog, dmabufs: &mut import::DmabufImages) {
        self.let_go(log, dmabufs);
        *self = Self::default();
    }
}

/// One of a shared window's surfaces past its own (glue.rs `Surfaces`): a
/// subsurface, a popup or one of a popup's, on a quad of its own, a child
/// of the window's quad a little in front of it. Drawn from its first
/// buffer on, when it gets a material.
#[derive(Component)]
pub struct SurfaceQuad {
    /// The plugin's id for it, among the window's surfaces.
    id: u32,
    /// Its box in logical px from the window's top left, and its place
    /// among the window's surfaces counted from the window's own, 1 just
    /// above it, -1 just below.
    rect: Rect,
    place: i32,
    own: OwnBuffers,
}

/// What a frame changes: the images and materials of a captured frame's
/// slot, or of a window's own buffer.
#[derive(SystemParam)]
pub struct Pictures<'w> {
    images: ResMut<'w, Assets<Image>>,
    materials: ResMut<'w, Assets<StandardMaterial>>,
    yuv_materials: ResMut<'w, Assets<YuvMaterial>>,
    dmabufs: ResMut<'w, import::DmabufImages>,
    log: ResMut<'w, import::SharedLog>,
}

/// Moves the frames that came in onto their windows' quads, the last of
/// each window's. One before it in the same update is never drawn, so its
/// slot goes back to the capture thread at once: nothing on the GPU has it.
pub fn apply_frames(
    mut commands: Commands,
    captures: Res<Captures>,
    index: Res<WindowIndex>,
    mut windows: Query<(
        &DesktopWindow,
        &mut WindowQuad,
        &mut MeshMaterial3d<StandardMaterial>,
    )>,
    mut pictures: Pictures,
    mut stats: ResMut<Stats>,
) {
    let mut last = HashMap::new();
    for (handle, frame) in captures.0.take() {
        if let Some((window, ..)) = index
            .by_handle
            .get(&handle)
            .and_then(|&e| windows.get(e).ok())
        {
            stats.copied(window.address);
        }
        if let Some(skipped) = last.insert(handle, frame) {
            captures.0.release(handle, skipped.slot.id);
        }
    }

    for (handle, frame) in last {
        // A window that went away, or off its frame, while its frame was on
        // its way.
        let Some(&entity) = index.by_handle.get(&handle) else {
            continue;
        };
        let Ok((window, mut quad, mut look)) = windows.get_mut(entity) else {
            continue;
        };
        // A copy from before it shared its own buffers.
        if quad.shares() {
            captures.0.release(handle, frame.slot.id);
            continue;
        }
        show_slot(
            window.address,
            &mut quad,
            &mut look,
            frame,
            &mut pictures,
            &captures.0,
        );
        // Back on capture, the captured frame on it in place of its own, and
        // of its surfaces', which the frame shows: a window's children are
        // its surfaces' quads.
        if quad.unsharing {
            quad.forget_shared(&mut pictures.log, &mut pictures.dmabufs);
            commands.entity(entity).despawn_children();
        }
    }
}

/// The plugin went back to capture for a window (glue.rs `Unshared`): its
/// frames are asked for again, and its own buffers stay on its quads until
/// the first comes.
pub fn apply_unshared(
    mut unshared: MessageReader<glue::Unshared>,
    index: Res<WindowIndex>,
    mut quads: Query<&mut WindowQuad>,
) {
    for glue::Unshared(address) in unshared.read() {
        if let Some(mut quad) = index
            .by_address
            .get(address)
            .and_then(|&e| quads.get_mut(e).ok())
            && quad.own.showing()
        {
            quad.unsharing = true;
            quad.asked_first = false;
            quad.want = None;
        }
    }
}

/// Puts each shared window's surfaces past its own on quads of their own
/// (glue.rs `Surfaces`), children of its quad: a new one gets a quad, which
/// is drawn once its first buffer comes, one gone loses it, and the rest
/// move to where the plugin says they are now (`place_surfaces`). The
/// window's own surface (id 0) in the list says that those before it are
/// below it: its quad then lets them show where its own is clear.
pub fn apply_surfaces(
    mut commands: Commands,
    mut told: MessageReader<glue::Surfaces>,
    index: Res<WindowIndex>,
    mesh: Res<QuadMesh>,
    mut windows: Query<(&mut WindowQuad, Option<&Children>)>,
    mut surfaces: Query<&mut SurfaceQuad>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // The last word on each window.
    let mut said = HashMap::new();
    for glue::Surfaces { window, surfaces } in told.read() {
        said.insert(*window, surfaces);
    }

    for (address, listed) in said {
        let Some(&window) = index.by_address.get(&address) else {
            continue;
        };
        let Ok((mut quad, children)) = windows.get_mut(window) else {
            continue;
        };
        let mut had: HashMap<u32, Entity> = children
            .into_iter()
            .flatten()
            .filter_map(|&child| Some((surfaces.get(child).ok()?.id, child)))
            .collect();

        // Places counted from the window's own, wherever it is in the list.
        let own = listed.iter().position(|&(id, _)| id == 0);
        let below = own.is_some_and(|own| own > 0);
        if quad.below != below {
            quad.below = below;
            let alpha_mode = quad.alpha_mode();
            quad.own.blend(alpha_mode, &mut materials);
        }
        let first = own.map_or(-1, |own| own as i32);
        for (index, &(id, rect)) in listed.iter().enumerate().filter(|&(_, &(id, _))| id != 0) {
            let place = index as i32 - first;
            match had
                .remove(&id)
                .and_then(|child| surfaces.get_mut(child).ok())
            {
                Some(mut quad) => {
                    if quad.rect != rect || quad.place != place {
                        (quad.rect, quad.place) = (rect, place);
                    }
                }
                None => {
                    let quad = SurfaceQuad {
                        id,
                        rect,
                        place,
                        own: OwnBuffers::default(),
                    };
                    commands.spawn((quad, Mesh3d(mesh.0.clone()), ChildOf(window)));
                }
            }
        }
        for gone in had.into_values() {
            commands.entity(gone).despawn();
        }
    }
}

/// Each surface's quad where its box is on its window's: the window's quad
/// is a unit square scaled to the window, so the box goes in as a share of
/// the window's size, a little in front of the window by its place among
/// its surfaces, or behind it for one below the window's own. Its window
/// may have moved or resized since.
pub fn place_surfaces(
    windows: Query<&DesktopWindow>,
    mut surfaces: Query<(&SurfaceQuad, &ChildOf, &mut Transform)>,
) {
    for (quad, parent, mut transform) in &mut surfaces {
        if let Ok(window) = windows.get(parent.parent()) {
            transform.set_if_neq(surface_place(quad.rect, quad.place, window.rect.size()));
        }
    }
}

/// Where a surface's box (`rect`, logical px from the window's top left)
/// goes on its window's quad, the window `size` px, `place` lifts in front
/// of it (behind for a negative one): y is up on the quad and down in the
/// box.
fn surface_place(rect: Rect, place: i32, size: Vec2) -> Transform {
    let size = size.max(Vec2::ONE);
    let centre = rect.center() / size - 0.5;
    Transform::from_xyz(centre.x, -centre.y, layout::SURFACE_LIFT * place as f32)
        .with_scale((rect.size() / size).extend(1.0))
}

/// A window's quad as a buffer goes onto it, with its material, and its
/// surfaces' quads, its children.
type SharedWindow = (
    &'static mut WindowQuad,
    &'static mut MeshMaterial3d<StandardMaterial>,
    Option<&'static Children>,
);

/// A surface's quad as a buffer goes onto it, and its material, of the kind
/// it has now (`Look`).
type SharedSurface = (
    &'static mut SurfaceQuad,
    Option<&'static MeshMaterial3d<StandardMaterial>>,
    Option<&'static MeshMaterial3d<YuvMaterial>>,
);

/// Puts each surface's newest own buffer (glue.rs `Shared`) on its quad: the
/// window's own surface's in place of its captured frames, the first taking
/// it off capture, and the others' on their own quads. The buffer it
/// replaces goes back to its client once the GPU is done with it (import.rs
/// `SharedLog`). One replaced in the same update, or one for a surface with
/// no quad, goes back at once: nothing on the GPU has it.
#[allow(clippy::too_many_arguments)]
pub fn apply_shared(
    mut commands: Commands,
    mut shared: ResMut<Messages<glue::Shared>>,
    glue: Option<Res<glue::Glue>>,
    captures: Res<Captures>,
    index: Res<WindowIndex>,
    mut windows: Query<SharedWindow, Without<SurfaceQuad>>,
    mut surfaces: Query<SharedSurface, Without<WindowQuad>>,
    mut pictures: Pictures,
    mut stats: ResMut<Stats>,
) {
    // A dmabuf that never goes on goes back at once: nothing on the GPU has
    // it. Shared memory went back as it was copied.
    let give_back = |dmabuf: Option<u64>| {
        if let Some(id) = dmabuf
            && let Some(glue) = &glue
        {
            glue.send(&format!("done {id}"));
        }
    };
    let mut newest: HashMap<(u64, u32), glue::Shared> = HashMap::new();
    for shared in shared.drain() {
        if let Some(older) = newest.insert((shared.window, shared.surface), shared) {
            give_back(older.buffer.dmabuf_id());
        }
    }

    for ((address, surface), shared) in newest {
        let dmabuf = shared.buffer.dmabuf_id();
        let Some((mut quad, mut look, children)) = index
            .by_address
            .get(&address)
            .and_then(|&e| windows.get_mut(e).ok())
        else {
            give_back(dmabuf);
            continue;
        };

        if surface == 0 {
            if show_shared(
                address,
                &mut quad,
                &mut look,
                shared.buffer,
                shared.alpha,
                &mut pictures,
                &captures.0,
            ) {
                stats.shared(address, surface);
            } else {
                give_back(dmabuf);
            }
            continue;
        }
        // One of its other surfaces, on its own quad, which blends by its
        // alpha if it has one.
        let child = children
            .into_iter()
            .flatten()
            .copied()
            .find(|&child| surfaces.get(child).is_ok_and(|(q, ..)| q.id == surface));
        let Some((child, (mut on, rgb, yuv))) =
            child.and_then(|child| Some((child, surfaces.get_mut(child).ok()?)))
        else {
            give_back(dmabuf);
            continue;
        };
        let alpha_mode = if shared.alpha {
            AlphaMode::Premultiplied
        } else {
            AlphaMode::Opaque
        };
        let Some(look) = on.own.put(shared.buffer, alpha_mode, &mut pictures) else {
            give_back(dmabuf);
            continue;
        };
        stats.shared(address, surface);
        // Its material, and only the one: a surface can go from RGB to YUV
        // and back.
        let mut child = commands.entity(child);
        match look {
            Look::Rgb(material) if rgb.is_none_or(|rgb| rgb.0 != material) => {
                child
                    .remove::<MeshMaterial3d<YuvMaterial>>()
                    .insert(MeshMaterial3d(material));
            }
            Look::Yuv(material) if yuv.is_none_or(|yuv| yuv.0 != material) => {
                child
                    .remove::<MeshMaterial3d<StandardMaterial>>()
                    .insert(MeshMaterial3d(material));
            }
            _ => {}
        }
    }
}

/// A window's own surface's buffer on its quad, switched to the buffer's
/// material, off capture with the first, and with the first after the
/// plugin put it back on capture, even if no captured frame came between.
/// False for a dmabuf that cannot go on (`OwnBuffers::put`).
fn show_shared(
    address: u64,
    quad: &mut WindowQuad,
    look: &mut MeshMaterial3d<StandardMaterial>,
    buffer: glue::SharedBuffer,
    alpha: bool,
    pictures: &mut Pictures,
    captures: &capture::Capture,
) -> bool {
    // The plugin sends a window's own surface in RGB only: its quad's
    // material is a `StandardMaterial`.
    let (size, BufferFormat::Rgb(layout)) = buffer.picture() else {
        return false;
    };
    let first = !quad.shares();
    quad.alpha = alpha;
    let alpha_mode = quad.alpha_mode();
    let Some(Look::Rgb(material)) = quad.own.put(buffer, alpha_mode, pictures) else {
        return false;
    };
    quad.own.blend(alpha_mode, &mut pictures.materials);

    // Off capture: its copies and their buffers go.
    if first {
        captures.forget(quad.handle);
        quad.forget_slots(&mut pictures.dmabufs);
        quad.want = None;
    }
    quad.unsharing = false;
    if look.0 != material {
        look.0 = material;
    }
    quad.flipped = false;

    if size != quad.size || quad.layout != Some(layout) {
        landed(address, quad, size, layout);
    }
    true
}

/// The part of a `size` buffer `crop` takes, as a material's `uv_transform`.
fn cropped(crop: URect, size: UVec2) -> Affine2 {
    let size = size.as_vec2().max(Vec2::ONE);
    Affine2::from_scale_angle_translation(
        crop.size().as_vec2() / size,
        0.0,
        crop.min.as_vec2() / size,
    )
}

/// A dmabuf frame: the quad switched to the material of the slot it came in,
/// made the first time the slot comes. The pixels never come through here:
/// the render world samples the slot itself (import.rs). The slot it
/// replaces is kept from the compositor while frames on their way to the
/// GPU may still sample it, and the one before that goes back.
fn show_slot(
    address: u64,
    quad: &mut WindowQuad,
    look: &mut MeshMaterial3d<StandardMaterial>,
    frame: DmabufFrame,
    pictures: &mut Pictures,
    captures: &capture::Capture,
) {
    let size = UVec2::new(frame.width, frame.height);
    let fresh = size != quad.size || quad.layout != Some(frame.layout);
    // A new size comes in new slots: the capture thread made them for it.
    if fresh {
        quad.forget_slots(&mut pictures.dmabufs);
    }

    let Pictures {
        images,
        materials,
        dmabufs,
        ..
    } = pictures;
    let uv = upside_down(frame.y_invert);
    let slot = quad.slots.entry(frame.slot.id).or_insert_with(|| {
        Slot::new(
            import::DmabufImage::Slot {
                slot: frame.slot.clone(),
                layout: frame.layout,
            },
            uv,
            AlphaMode::Opaque,
            images,
            materials,
            dmabufs,
        )
    });
    slot.set_uv(uv, materials);
    if look.0 != slot.material {
        look.0 = slot.material.clone();
    }
    quad.flipped = frame.y_invert;
    if let Some(back) = quad.kept.show(frame.slot.id) {
        captures.release(quad.handle, back);
    }

    if fresh {
        landed(address, quad, size, frame.layout);
    }
}

/// A frame of a new size or byte order is on `quad`.
fn landed(address: u64, quad: &mut WindowQuad, size: UVec2, layout: PixelLayout) {
    if quad.size == UVec2::ZERO {
        eprintln!(
            "hypr-world: window {address:#x} is in, {}x{}",
            size.x, size.y
        );
    }
    quad.size = size;
    quad.layout = Some(layout);
}

/// How a material samples a frame: upside down for a y-inverted dmabuf, as
/// it can only be flipped where it is sampled.
fn upside_down(flipped: bool) -> Affine2 {
    if flipped {
        Affine2::from_scale_angle_translation(Vec2::new(1.0, -1.0), 0.0, Vec2::new(0.0, 1.0))
    } else {
        Affine2::IDENTITY
    }
}

/// The systems that ask what the camera sees (`Sight`).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct SightSystems;

/// The desktop's windows, their quads on the walls and their frames from the
/// capture thread, what the crosshair is on and the dwell, and, once this
/// update's view is known, the copies asked for and the windows the plugin
/// keeps awake.
pub fn plugin(app: &mut App) {
    configure_sight(app);
    crosshair(app);
    app.init_resource::<WindowIndex>()
        .add_observer(forget_capture)
        .add_observer(forget_surface)
        .add_systems(Startup, setup)
        .add_systems(PreUpdate, apply_windows.after(WorldSystems::Feed))
        .add_systems(
            Update,
            (
                sync_quads
                    .run_if(
                        glue::any_changed_or_removed::<DesktopWindow>
                            .or_else(resource_changed::<House>),
                    )
                    .in_set(WorldSystems::Walls),
                (
                    apply_unshared,
                    apply_frames,
                    apply_surfaces,
                    apply_shared,
                    place_surfaces,
                )
                    .chain()
                    .in_set(WorldSystems::Capture),
            ),
        )
        .add_systems(
            PostUpdate,
            (request_frames, announce_pace).in_set(SightSystems),
        );
}

/// The crosshair: what it is on, told the plugin while the world is in and
/// has the plugin's input, nothing once the world is left, and all afresh
/// once the input is back. The dwell follows `Aimed`, which is empty out of
/// the world.
fn crosshair(app: &mut App) {
    app.init_resource::<Aimed>()
        .init_resource::<AimedFrame>()
        .init_resource::<AimLine>()
        .add_systems(
            OnExit(WorldState::In),
            aim_at_nothing.run_if(not(in_state(InputMode::Off))),
        )
        .add_systems(OnEnter(InputMode::Off), forget_aim)
        .add_systems(
            Update,
            (
                aim.run_if(in_state(WorldState::In).and_then(not(in_state(InputMode::Off)))),
                dwell,
            )
                .chain()
                .in_set(WorldSystems::Aim),
        );
}

/// `SightSystems` in `PostUpdate`, once this update's camera frustum and
/// window boxes are in. A window that came into view is then asked for in
/// the same update, and with nothing waking the app there may be no next
/// one. Public for tests that run other modules' sight systems.
pub fn configure_sight(app: &mut App) {
    app.configure_sets(
        PostUpdate,
        SightSystems
            .after(VisibilitySystems::UpdateFrusta)
            .after(VisibilitySystems::CalculateBounds),
    );
}

/// What the app's camera sees and looks at, whether or not it draws this
/// update.
#[derive(SystemParam)]
pub struct Sight<'w, 's> {
    desktop: Res<'w, Desktop>,
    aimed: Res<'w, Aimed>,
    config: Res<'w, WorldConfig>,
    camera: Query<
        'w,
        's,
        (
            &'static Frustum,
            &'static GlobalTransform,
            &'static Projection,
        ),
        With<FlyCamera>,
    >,
}

impl Sight<'_, '_> {
    /// Whether a quad is in view: the world shown and the quad's box in the
    /// camera's frustum. Not Bevy's `ViewVisibility`, which only an active
    /// camera sets, and the camera is off in every update that draws nothing
    /// (redraw.rs).
    pub fn sees(&self, transform: &GlobalTransform, aabb: Option<&Aabb>) -> bool {
        let shown = self.desktop.in_view();
        let (Ok((frustum, ..)), Some(aabb)) = (self.camera.single(), aabb) else {
            return false;
        };
        shown && frustum.intersects_obb(aabb, &transform.affine(), true, false)
    }

    /// How large a quad is on screen: the larger of its height and width as
    /// a share of the view's, 1 filling it. A quad seen at a slant is
    /// smaller on screen than this, so it errs large.
    pub fn share(&self, transform: &GlobalTransform) -> f32 {
        let Ok((_, eye, Projection::Perspective(perspective))) = self.camera.single() else {
            return 1.0;
        };
        let depth = (transform.translation() - eye.translation()).dot(*eye.forward());
        if depth <= perspective.near {
            return 1.0;
        }
        // A quad is a unit square scaled to its size in metres.
        let size = transform.scale();
        let tall = 2.0 * depth * (perspective.fov / 2.0).tan();
        (size.y / tall).max(size.x / (tall * perspective.aspect_ratio))
    }

    /// The least time between copies of `window`, in seconds: none for the
    /// one under the crosshair, the rest by their size on screen. A window
    /// on a workspace other than the one underneath the world draws for the
    /// walls alone, so at most `other_workspace_fps` a second (15 unless
    /// set), however large or aimed at. The dwell makes a workspace the one
    /// underneath, and its windows go free.
    pub fn copy_interval(&self, window: &DesktopWindow, transform: &GlobalTransform) -> f32 {
        let interval = if self.aimed.0 == Some(window.address) {
            0.0
        } else {
            copy_interval(self.share(transform))
        };
        match self.desktop.active {
            Some(active) if window.workspace != active && self.config.other_workspace_fps > 0.0 => {
                interval.max(1.0 / self.config.other_workspace_fps)
            }
            _ => interval,
        }
    }
}

/// Tells the capture thread which windows to copy, and how often: every
/// window in view, at most at its rate (`Sight::copy_interval`), whenever it
/// drew something; none out of view, where a window keeps the last frame it
/// had. Every copy is Hyprland rendering the window again, so a still window
/// is not copied at all. The capture thread hears what drew straight from the glue thread and starts
/// each copy itself, so the app wakes only for the frames that come back.
/// Every window gets a first frame, in view or not.
pub fn request_frames(
    captures: Res<Captures>,
    sight: Sight,
    mut windows: Query<(
        &DesktopWindow,
        &mut WindowQuad,
        &GlobalTransform,
        Option<&Aabb>,
    )>,
) {
    for (window, mut quad, transform, aabb) in &mut windows {
        // Its own buffers come from the plugin.
        if quad.shares() {
            continue;
        }
        if !quad.asked_first {
            captures.0.request(quad.handle);
            quad.asked_first = true;
        }
        let want = sight
            .sees(transform, aabb)
            .then(|| Duration::from_secs_f32(sight.copy_interval(window, transform)));
        if quad.want != want {
            captures.0.want(quad.handle, want);
            quad.want = want;
        }
    }
}

/// Tells the plugin what it needs of the windows in view, each line
/// whenever it changes.
///
/// `live`: which are copied often, so it keeps them drawing at full rate
/// even on hidden workspaces. The rest are left to Hyprland, which suspends
/// the ones on hidden workspaces: a window small on screen need not draw for
/// nobody.
///
/// `pace`: how often each may draw, at the rate its copies are asked for
/// (`Sight::copy_interval`), again on each new connection: each window's
/// address and least time between its frames in ms, 0 for every refresh.
/// The plugin sends the frame callbacks of the windows whose own buffers it
/// shares at that rate, so a window on a hidden workspace keeps drawing for
/// the world, and one small on screen draws little.
pub fn announce_pace(
    glue: Res<glue::Glue>,
    sight: Sight,
    windows: Query<(&DesktopWindow, &GlobalTransform, Option<&Aabb>), With<WindowQuad>>,
    mut announced_live: Local<Option<Vec<u64>>>,
    mut announced: Local<Option<Paced>>,
) {
    if !glue.connected {
        *announced_live = None;
        return;
    }

    // Each window in view's address, least time between its frames in ms,
    // and whether it is kept awake.
    let mut seen: Vec<(u64, u32, bool)> = windows
        .iter()
        .filter(|&(_, transform, aabb)| sight.sees(transform, aabb))
        .map(|(window, transform, _)| {
            let interval = sight.copy_interval(window, transform);
            let ms = (interval * 1000.0).round() as u32;
            (window.address, ms, interval <= AWAKE)
        })
        .collect();
    seen.sort_unstable();

    let live: Vec<u64> = seen
        .iter()
        .filter(|&&(.., awake)| awake)
        .map(|&(address, ..)| address)
        .collect();
    if announced_live.as_ref() != Some(&live) {
        let mut line = String::from("live");
        for address in &live {
            line.push_str(&format!(" {address:#x}"));
        }
        glue.send(&line);
        *announced_live = Some(live);
    }

    let pace: Vec<(u64, u32)> = seen.iter().map(|&(address, ms, _)| (address, ms)).collect();
    if announced
        .as_ref()
        .is_some_and(|said| said.connection == glue.connection && said.pace == pace)
    {
        return;
    }
    let mut line = String::from("pace");
    for (address, ms) in &pace {
        line.push_str(&format!(" {address:#x} {ms}"));
    }
    glue.send(&line);
    *announced = Some(Paced {
        connection: glue.connection,
        pace,
    });
}

/// What `announce_pace` last told the plugin, and on which connection.
pub struct Paced {
    connection: u64,
    /// Each window's address and least time between its frames in ms.
    pace: Vec<(u64, u32)>,
}

/// Tells the plugin which window the crosshair is on, and where, whenever
/// that changes. During a drag the plugin holds on to the surface that was
/// pressed, and the app keeps aiming at that window's plane past its edges,
/// so a drag can leave the window the way it can in 2D.
#[allow(clippy::too_many_arguments)]
pub fn aim(
    glue: Res<glue::Glue>,
    eye: Single<Entity, With<FlyCamera>>,
    windows: Query<(Entity, &DesktopWindow, &WindowQuad)>,
    surfaces: Query<(Entity, &SurfaceQuad, &ChildOf)>,
    vacant: Query<(Entity, &Vacant)>,
    // Where the camera and the quads are in the world as of now: their
    // `GlobalTransform`s are only worked out in `PostUpdate`, and the body
    // carrying the camera, a frame or a window may have moved since.
    places: TransformHelper,
    mut aimed: ResMut<Aimed>,
    mut aimed_frame: ResMut<AimedFrame>,
    mut said: ResMut<AimLine>,
) {
    // The camera's centre ray, where the crosshair is.
    let Ok(eye) = places.compute_global_transform(*eye) else {
        return;
    };
    let ray = Ray3d::new(eye.translation(), eye.forward());

    let mut hit: Option<(f32, u64, &WindowQuad, f32, f32)> = None;
    for (entity, window, quad) in &windows {
        if quad.size == UVec2::ZERO {
            continue;
        }
        let Ok(place) = places.compute_global_transform(entity) else {
            continue;
        };
        let Some((t, u, v)) = crosshair_on(ray, &place) else {
            continue;
        };
        // The image is drawn flipped when the capture is bottom-up.
        let v = if quad.flipped { 1.0 - v } else { v };

        if glue.grab == Some(window.address) {
            hit = Some((f32::NEG_INFINITY, window.address, quad, u, v));
            break;
        }
        let on_window = (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v);
        if on_window && hit.is_none_or(|(nearest, ..)| t < nearest) {
            hit = Some((t, window.address, quad, u, v));
        }
    }

    // On one of a window's other surfaces is on the window, at the point of
    // its plane: past its edges for a popup that reaches past them, which
    // the plugin finds there.
    for (entity, surface, parent) in &surfaces {
        let Ok((window_entity, window, quad)) = windows.get(parent.parent()) else {
            continue;
        };
        if !surface.own.showing() {
            continue;
        }
        let (Ok(place), Ok(window_place)) = (
            places.compute_global_transform(entity),
            places.compute_global_transform(window_entity),
        ) else {
            continue;
        };
        let Some((t, su, sv)) = crosshair_on(ray, &place) else {
            continue;
        };
        let on_surface = (0.0..=1.0).contains(&su) && (0.0..=1.0).contains(&sv);
        if on_surface
            && hit.is_none_or(|(nearest, ..)| t < nearest)
            && let Some((_, u, v)) = crosshair_on(ray, &window_place)
        {
            hit = Some((t, window.address, quad, u, v));
        }
    }

    aimed.0 = hit.map(|(_, address, ..)| address);
    // A vacant frame has no windows, so it counts only off every window.
    aimed_frame.0 = vacant
        .iter()
        .filter(|_| hit.is_none())
        .filter_map(|(entity, vacant)| {
            let place = places.compute_global_transform(entity).ok()?;
            let (t, u, v) = crosshair_on(ray, &place)?;
            let on = (0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v);
            on.then_some((t, vacant.workspace))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, workspace)| workspace);
    let line = match hit {
        Some((_, address, quad, u, v)) => format!(
            "aim {address:#x} {u:.5} {v:.5} {} {}",
            quad.size.x, quad.size.y
        ),
        None => "aim -".to_string(),
    };
    said.say(&glue, line);
}

/// Nothing under the crosshair once the world is left: not on the flight
/// out, nor in from it, nor out.
fn aim_at_nothing(
    glue: Res<glue::Glue>,
    mut aimed: ResMut<Aimed>,
    mut aimed_frame: ResMut<AimedFrame>,
    mut said: ResMut<AimLine>,
) {
    aimed.0 = None;
    aimed_frame.0 = None;
    said.say(&glue, "aim -".to_string());
}

/// The plugin's input is off, and the plugin is told the aim afresh when it
/// is back.
fn forget_aim(
    mut aimed: ResMut<Aimed>,
    mut aimed_frame: ResMut<AimedFrame>,
    mut said: ResMut<AimLine>,
) {
    aimed.0 = None;
    aimed_frame.0 = None;
    said.0.clear();
}

/// Where `ray` meets a quad's plane, past its edges too, for a drag: the
/// distance along it and (u, v) on the quad with (0, 0) at its top-left, or
/// None if the plane is behind it or edge on.
pub fn crosshair_on(ray: Ray3d, quad: &GlobalTransform) -> Option<(f32, f32, f32)> {
    let t = ray.intersect_plane(
        quad.translation(),
        InfinitePlane3d {
            normal: quad.back(),
        },
    )?;
    // The unit quad spans -0.5..0.5 on x and y, +y up, image rows top-down.
    let local = quad.affine().inverse().transform_point3(ray.get_point(t));
    Some((t, local.x + 0.5, 0.5 - local.y))
}

/// Resting the crosshair on a window of another workspace switches to that
/// workspace, so leaving the world lands on what you were looking at. Resting
/// it on a vacant frame (scene.rs `Vacant`) makes its workspace, so a window
/// opened from the world goes on it (the plugin's App.cpp `redirectNow`).
#[allow(clippy::too_many_arguments)]
pub fn dwell(
    time: Res<Time<Real>>,
    glue: Res<glue::Glue>,
    desktop: Res<Desktop>,
    aimed: Res<Aimed>,
    aimed_frame: Res<AimedFrame>,
    index: Res<WindowIndex>,
    windows: Query<&DesktopWindow>,
    mut resting: Local<Option<Resting>>,
    mut wake_at: ResMut<WakeAt>,
) {
    let target = aimed
        .0
        .and_then(|address| windows.get(index.get(address)?).ok())
        .map(|window| (RestingOn::Window(window.address), window.workspace))
        .or_else(|| {
            aimed_frame
                .0
                .map(|workspace| (RestingOn::Frame(workspace), workspace))
        })
        .filter(|&(_, workspace)| desktop.active != Some(workspace));
    let Some((on, workspace)) = target else {
        *resting = None;
        return;
    };

    let now = Instant::now();
    let since = match resting.take() {
        Some(rest) if rest.on == on => rest.since,
        // A window or frame newly rested on: its time starts now.
        _ => now,
    };

    let left = Duration::from_secs_f32(DWELL).saturating_sub(now - since);
    if left.is_zero() {
        glue.send(&format!("workspace {workspace}"));
    } else {
        // Nothing else need wake the app when the dwell is up.
        wake_at.at(time.elapsed_secs() + left.as_secs_f32());
        *resting = Some(Resting { on, since });
    }
}

/// What the crosshair rests on, and since when. Timed by the wall clock,
/// not `Time<Real>`'s delta: with pipelined rendering that is the render
/// world's last frame, so the update that wakes the app after a pause would
/// credit the pause an update late, and a dwell nothing else wakes the app
/// for could take twice as long.
pub struct Resting {
    on: RestingOn,
    since: Instant,
}

/// What the crosshair rests on: a window, by address, or a vacant frame, by
/// its workspace.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RestingOn {
    Window(u64),
    Frame(i64),
}

#[cfg(test)]
mod tests {
    use std::f32::consts::PI;

    use bevy::{asset::AssetPlugin, camera::CameraPlugin, mesh::MeshPlugin};

    use super::*;
    use crate::glue::Monitor;

    /// A quad, and whether `Sight` saw it this update.
    #[derive(Component)]
    struct Probe(bool);

    fn look(sight: Sight, mut quads: Query<(&GlobalTransform, Option<&Aabb>, &mut Probe)>) {
        for (transform, aabb, mut probe) in &mut quads {
            probe.0 = sight.sees(transform, aabb);
        }
    }

    fn seen(app: &App, quad: Entity) -> bool {
        app.world().get::<Probe>(quad).expect("a probe").0
    }

    /// A window's entity is found by its address and by its export handle
    /// while it is there, and not once it has gone.
    #[test]
    fn the_index_follows_the_windows_entities() {
        let mut world = World::new();
        world.init_resource::<WindowIndex>();
        let window = Window2d {
            workspace: 1,
            rect: Rect::new(0.0, 0.0, 1.0, 1.0),
            stack: 1,
            kind: Kind::Tiled,
        };
        let entity = world.spawn(DesktopWindow::new(0x1_0000_00a0, &window)).id();

        let index = world.resource::<WindowIndex>();
        assert_eq!(index.get(0x1_0000_00a0), Some(entity));
        assert_eq!(
            index.by_handle.get(&0xa0),
            Some(&entity),
            "by its handle, the low half of its address"
        );

        world.despawn(entity);
        let index = world.resource::<WindowIndex>();
        assert!(
            index.by_address.is_empty() && index.by_handle.is_empty(),
            "gone with it"
        );
    }

    /// Where the crosshair meets a quad, as `aim` reads it.
    #[test]
    fn the_crosshair_meets_a_quad_in_its_plane() {
        let ray = Ray3d::new(Vec3::ZERO, Dir3::NEG_Z);

        // 2 wide and 1 high, 3 ahead and facing the eye, set up and to the
        // right: the ray meets it a quarter of the way across, three
        // quarters of the way down.
        let ahead = Transform::from_xyz(0.5, 0.25, -3.0).with_scale(Vec3::new(2.0, 1.0, 1.0));
        let (t, u, v) = crosshair_on(ray, &ahead.into()).expect("ahead");
        assert!((t - 3.0).abs() < 1e-5, "{t}");
        assert!(
            (u - 0.25).abs() < 1e-5 && (v - 0.75).abs() < 1e-5,
            "{u} {v}"
        );

        assert!(
            crosshair_on(ray, &Transform::from_xyz(0.0, 0.0, 3.0).into()).is_none(),
            "behind"
        );
        let edge_on =
            Transform::from_xyz(0.0, 0.0, -3.0).with_rotation(Quat::from_rotation_y(PI / 2.0));
        assert!(crosshair_on(ray, &edge_on.into()).is_none(), "edge on");
    }

    /// With Bevy's own frusta and bounds, headless: a quad ahead of the
    /// camera and one behind it, and the camera turned round.
    #[test]
    fn a_window_turned_into_view_is_seen_in_the_same_update() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            AssetPlugin::default(),
            MeshPlugin,
            CameraPlugin,
        ))
        .init_resource::<Desktop>()
        .init_resource::<WorldConfig>()
        .init_resource::<Aimed>()
        .init_resource::<AimedFrame>();
        configure_sight(&mut app);
        app.add_systems(PostUpdate, look.in_set(SightSystems));

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
        let ahead = app
            .world_mut()
            .spawn((
                Mesh3d(mesh.clone()),
                Transform::from_xyz(0.0, 0.0, -3.0),
                Probe(false),
            ))
            .id();
        let behind = app
            .world_mut()
            .spawn((
                Mesh3d(mesh),
                Transform::from_xyz(0.0, 0.0, 3.0),
                Probe(false),
            ))
            .id();

        // Twice: the first update works out the quads' bounds.
        app.update();
        app.update();
        assert!(
            seen(&app, ahead) && !seen(&app, behind),
            "only the quad ahead is in view"
        );

        app.world_mut()
            .get_mut::<Transform>(camera)
            .expect("the camera")
            .rotation = Quat::from_rotation_y(PI);
        app.update();
        assert!(
            !seen(&app, ahead) && seen(&app, behind),
            "turned round: seen in the same update"
        );

        // The plugin's feed says the world is out of view: nothing is seen.
        app.world_mut().resource_mut::<Desktop>().monitor = Some(Monitor {
            size: Vec2::new(1440.0, 960.0),
            scale: 2.0,
            work: Rect::new(0.0, 40.0, 1440.0, 960.0),
        });
        app.update();
        assert!(
            !seen(&app, ahead) && !seen(&app, behind),
            "out of view nothing is seen"
        );
    }
}

#[cfg(test)]
mod rates {
    use bevy::{asset::AssetPlugin, camera::CameraPlugin, mesh::MeshPlugin};

    use super::*;

    #[test]
    fn the_rate_falls_with_the_size_on_screen() {
        assert_eq!(
            copy_interval(0.6),
            0.0,
            "half the view or more: every frame"
        );
        assert_eq!(
            copy_interval(0.3),
            AWAKE,
            "a fifth: fifteen a second, kept awake"
        );
        assert_eq!(copy_interval(0.1), 0.25, "a twelfth: four a second");
        assert_eq!(copy_interval(0.01), 1.0, "below that: once a second");
    }

    /// The least time between a window's copies as `Sight` put it this
    /// update.
    #[derive(Component)]
    struct Rate(f32);

    fn rate(sight: Sight, mut quads: Query<(&DesktopWindow, &GlobalTransform, &mut Rate)>) {
        for (window, transform, mut rate) in &mut quads {
            rate.0 = sight.copy_interval(window, transform);
        }
    }

    /// With Bevy's own camera, headless: its default 45 degree view, and
    /// quads of known sizes at known depths, on workspace 1, the one
    /// underneath the world, unless said.
    #[test]
    fn a_window_is_rated_by_its_size_on_screen_the_crosshair_and_its_workspace() {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            AssetPlugin::default(),
            MeshPlugin,
            CameraPlugin,
        ))
        .init_resource::<Desktop>()
        .init_resource::<WorldConfig>()
        .init_resource::<Aimed>()
        .init_resource::<AimedFrame>()
        .init_resource::<WindowIndex>();
        configure_sight(&mut app);
        app.add_systems(PostUpdate, rate.in_set(SightSystems));

        app.world_mut().spawn((
            Camera3d::default(),
            FlyCamera {
                yaw: 0.0,
                pitch: 0.0,
            },
            Transform::default(),
        ));
        app.world_mut().resource_mut::<Desktop>().active = Some(1);
        let quad = |app: &mut App, address: u64, at: Vec3, size: f32| {
            let transform = Transform::from_translation(at).with_scale(Vec3::new(size, size, 1.0));
            let window = DesktopWindow {
                address,
                workspace: 1,
                rect: Rect::default(),
                stack: 0,
                kind: Kind::Tiled,
            };
            app.world_mut()
                .spawn((window, transform, Rate(f32::NAN)))
                .id()
        };
        // A 1 m quad 3 m away fills 1 / (6 tan 22.5) of the view: 0.40.
        let near = quad(&mut app, 1, Vec3::new(0.0, 0.0, -3.0), 1.0);
        let big = quad(&mut app, 2, Vec3::new(0.0, 0.0, -3.0), 4.0);
        let far = quad(&mut app, 3, Vec3::new(0.0, 0.0, -30.0), 1.0);
        let interval = |app: &App, quad: Entity| app.world().get::<Rate>(quad).expect("a rate").0;

        app.update();
        assert_eq!(interval(&app, big), 0.0, "filling the view: every frame");
        assert_eq!(
            interval(&app, near),
            AWAKE,
            "two fifths of it: fifteen a second"
        );
        assert_eq!(interval(&app, far), 1.0, "a twenty-fifth: once a second");

        app.world_mut().resource_mut::<Aimed>().0 = Some(3);
        app.update();
        assert_eq!(interval(&app, far), 0.0, "under the crosshair: every frame");

        // The same two on another workspace: fifteen a second at most, aimed
        // at or filling the view, and no faster than their size allows.
        for quad in [big, far] {
            app.world_mut()
                .get_mut::<DesktopWindow>(quad)
                .expect("a window")
                .workspace = 2;
        }
        app.update();
        assert_eq!(
            interval(&app, big),
            1.0 / 15.0,
            "filling the view, another workspace"
        );
        assert_eq!(
            interval(&app, far),
            1.0 / 15.0,
            "aimed at, another workspace"
        );
        app.world_mut().resource_mut::<Aimed>().0 = None;
        app.update();
        assert_eq!(
            interval(&app, far),
            1.0,
            "small, another workspace: still once a second"
        );
    }

    /// The plugin is told each window in view and the least time between
    /// its frames in ms, once while that stays the same, again when it
    /// changes and on a new connection, and nothing while disconnected.
    #[test]
    fn the_plugin_is_told_how_often_each_window_in_view_may_draw() {
        use std::{io::Read, os::unix::net::UnixStream};

        let (world_end, mut plugin) = UnixStream::pair().expect("a socket pair");
        plugin
            .set_nonblocking(true)
            .expect("a socket that does not wait");
        let glue = glue::Glue::connected_to(world_end);

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            AssetPlugin::default(),
            MeshPlugin,
            CameraPlugin,
        ))
        .insert_resource(glue)
        .init_resource::<Desktop>()
        .init_resource::<WorldConfig>()
        .init_resource::<Aimed>()
        .init_resource::<AimedFrame>()
        .init_resource::<WindowIndex>();
        configure_sight(&mut app);
        app.add_systems(PostUpdate, announce_pace.in_set(SightSystems));

        app.world_mut().spawn((
            Camera3d::default(),
            FlyCamera {
                yaw: 0.0,
                pitch: 0.0,
            },
            Transform::default(),
        ));
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Rectangle::new(1.0, 1.0));
        let mut window = |address: u64, at: Vec3| {
            let desktop = DesktopWindow {
                address,
                workspace: 1,
                rect: Rect::default(),
                stack: 0,
                kind: Kind::Tiled,
            };
            let quad = WindowQuad::new(0);
            app.world_mut().spawn((
                desktop,
                quad,
                Mesh3d(mesh.clone()),
                Transform::from_translation(at),
            ));
        };
        // A 1 m quad 3 m away is two fifths of the view, 30 m away a
        // twenty-fifth, and one behind the camera is out of view.
        window(0xa, Vec3::new(0.0, 0.0, -3.0));
        window(0xb, Vec3::new(0.0, 0.0, -30.0));
        window(0xc, Vec3::new(0.0, 0.0, 3.0));

        let told = |plugin: &mut UnixStream| {
            let mut said = String::new();
            // Everything there is so far, then WouldBlock.
            let _ = plugin.read_to_string(&mut said);
            said.lines()
                .filter(|line| line.starts_with("pace"))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };

        // Twice: the first update works out the quads' bounds.
        app.update();
        app.update();
        assert_eq!(
            told(&mut plugin).last().map(String::as_str),
            Some("pace 0xa 67 0xb 1000"),
            "fifteen a second, and one"
        );
        app.update();
        assert!(told(&mut plugin).is_empty(), "nothing changed");

        app.world_mut().resource_mut::<Aimed>().0 = Some(0xb);
        app.update();
        assert_eq!(
            told(&mut plugin),
            ["pace 0xa 67 0xb 0"],
            "under the crosshair: every refresh"
        );

        app.world_mut().resource_mut::<glue::Glue>().connection += 1;
        app.update();
        assert_eq!(
            told(&mut plugin),
            ["pace 0xa 67 0xb 0"],
            "told afresh on a new connection"
        );

        let mut glue = app.world_mut().resource_mut::<glue::Glue>();
        glue.connected = false;
        glue.connection += 1;
        app.world_mut().resource_mut::<Aimed>().0 = None;
        app.update();
        assert!(told(&mut plugin).is_empty(), "nothing while disconnected");
    }
}

/// The frames as parents, headless, with scene.rs `relayout`,
/// `sync_quads` and `aim` as the app runs them.
#[cfg(test)]
mod frames {
    use std::os::unix::net::UnixStream;

    use bevy::{asset::AssetPlugin, mesh::MeshPlugin};

    use super::*;
    use crate::{
        config::WorldConfig,
        glue::{Glue, Monitor},
        layout::{self, Layout, WALLS},
        scene::{self, Workspace},
    };

    const MONITOR: Vec2 = Vec2::new(1440.0, 960.0);
    /// Off the middle, so that a sign or an axis the wrong way round shows.
    const RECT: Rect = Rect {
        min: Vec2::new(100.0, 50.0),
        max: Vec2::new(700.0, 450.0),
    };

    /// A workspace with a window on it, box `RECT`, whose address is the
    /// workspace's id.
    fn window_on(app: &mut App, workspace: i64) -> Entity {
        app.world_mut().spawn(Workspace {
            id: workspace,
            name: String::new(),
        });
        let window = DesktopWindow {
            address: workspace as u64,
            workspace,
            rect: RECT,
            stack: 0,
            kind: Kind::Tiled,
        };
        app.world_mut().spawn(window).id()
    }

    /// Where the app hung a tiled window with box `RECT` on `frame` when it
    /// placed everything in room coordinates: along the wall and up from the
    /// frame's centre by where the window is in 2D, out from it by the
    /// window's lift, and turned to face the room.
    fn hung(layout: &Layout, frame: &layout::Frame) -> Transform {
        let wall = WALLS[frame.wall];
        let c = RECT.center() - layout.area.min;
        let translation = frame.centre
            + wall.right * (c.x - layout.area.width() / 2.0) * layout.k
            + Vec3::Y * (layout.area.height() / 2.0 - c.y) * layout.k
            + wall.normal * 0.005;
        Transform {
            translation,
            rotation: Quat::from_rotation_y(wall.yaw),
            scale: (RECT.size() * layout.k).extend(1.0),
        }
    }

    fn assert_hung(app: &App, window: Entity) {
        let &DesktopWindow {
            address, workspace, ..
        } = app.world().get::<DesktopWindow>(window).expect("a window");
        let layout = app
            .world()
            .resource::<House>()
            .layout
            .clone()
            .expect("a house");
        let expected = hung(&layout, layout.frame_of(workspace).expect("a frame"));
        let place = app.world().get::<GlobalTransform>(window).expect("a quad");
        // Position, turn and size at once: the angle between two quaternions
        // a float apart reads as a milliradian.
        assert!(
            place.affine().abs_diff_eq(expected.compute_affine(), 1e-5),
            "{address}: hangs at {:?}, not {expected:?}",
            place.compute_transform()
        );

        let frame = app
            .world()
            .resource::<scene::WorkspaceIndex>()
            .get(workspace);
        let parent = app.world().get::<ChildOf>(window).map(ChildOf::parent);
        assert!(
            parent.is_some() && parent == frame,
            "{address}: a child of its frame"
        );
    }

    /// The house, hung with whatever windows a test puts on its workspaces,
    /// and a camera. The crosshair is the test's to add.
    fn hanging(glue: Glue) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            TransformPlugin,
            AssetPlugin::default(),
            MeshPlugin,
        ))
        .init_asset::<Image>()
        .init_asset::<StandardMaterial>()
        .init_asset::<YuvMaterial>()
        .insert_resource(glue)
        .insert_resource(Captures(capture::Capture::idle()))
        .insert_resource(Desktop {
            monitor: Some(Monitor {
                size: MONITOR,
                scale: 1.0,
                work: Rect::from_corners(Vec2::ZERO, MONITOR),
            }),
            active: Some(1),
            shown: false,
        })
        .init_resource::<WorldConfig>()
        .init_resource::<House>()
        .init_resource::<scene::WorkspaceIndex>()
        .init_resource::<WindowIndex>()
        .init_resource::<import::DmabufImages>()
        .init_resource::<import::SharedLog>()
        .add_systems(Startup, (scene::setup, setup))
        .add_systems(
            Update,
            (scene::relayout, sync_quads)
                .chain()
                .before(WorldSystems::Aim),
        );
        let camera = app
            .world_mut()
            .spawn((
                FlyCamera {
                    yaw: 0.0,
                    pitch: 0.0,
                },
                Transform::default(),
            ))
            .id();
        (app, camera)
    }

    /// What the crosshair is on from a metre and a half in front of
    /// `window`, looking at its middle.
    fn aimed_from_before(app: &mut App, camera: Entity, window: Entity) -> Option<u64> {
        let place = *app.world().get::<GlobalTransform>(window).expect("a quad");
        let eye = place.translation() + place.back() * 1.5;
        *app.world_mut()
            .get_mut::<Transform>(camera)
            .expect("the camera") =
            Transform::from_translation(eye).looking_at(place.translation(), Vec3::Y);
        app.update();
        app.world().resource::<Aimed>().0
    }

    /// A window on each wall's frame hangs where it did when everything was
    /// placed in room coordinates, the spare on the fourth wall. A fourth
    /// workspace moves three frames to other walls and the fourth along its
    /// own, and each frame's window goes with it, the same entity, and the
    /// crosshair finds it there, and the spare after them. A window moved
    /// to another workspace moves to that frame.
    #[test]
    fn windows_hang_on_their_frames() {
        let (world_end, _plugin) = UnixStream::pair().expect("a socket pair");
        let (mut app, camera) = hanging(Glue::connected_to(world_end));
        app.init_resource::<Aimed>()
            .init_resource::<AimedFrame>()
            .init_resource::<AimLine>()
            .add_systems(Update, aim.in_set(WorldSystems::Aim));

        let mut windows: Vec<Entity> = (1..=3).map(|id| window_on(&mut app, id)).collect();
        app.update();
        let walls = |app: &App| {
            app.world()
                .resource::<House>()
                .layout
                .as_ref()
                .map(|l| l.frames.iter().map(|f| f.wall).collect::<Vec<_>>())
        };
        assert_eq!(walls(&app), Some(vec![0, 1, 2, 3]), "one frame a wall");
        for &window in &windows {
            assert_hung(&app, window);
        }

        windows.push(window_on(&mut app, 4));
        app.update();
        assert_eq!(
            walls(&app),
            Some(vec![0, 0, 1, 2, 3]),
            "two on the front wall"
        );
        for &window in &windows {
            assert_hung(&app, window);
            // As if its first frame had come, so that the crosshair can find it.
            app.world_mut()
                .get_mut::<WindowQuad>(window)
                .expect("a quad")
                .size = UVec2::new(600, 400);
        }
        for (address, &window) in (1..).zip(&windows) {
            assert_eq!(
                aimed_from_before(&mut app, camera, window),
                Some(address),
                "the crosshair on {address}"
            );
        }

        let spare = app
            .world_mut()
            .query::<(Entity, &Vacant)>()
            .iter(app.world())
            .map(|(entity, vacant)| (entity, vacant.workspace))
            .collect::<Vec<_>>();
        assert!(
            matches!(spare[..], [(_, 5)]),
            "the spare, vacant: {spare:?}"
        );
        assert_eq!(aimed_from_before(&mut app, camera, spare[0].0), None);
        assert_eq!(
            app.world().resource::<AimedFrame>().0,
            Some(5),
            "the crosshair on the spare"
        );

        app.world_mut()
            .get_mut::<DesktopWindow>(windows[0])
            .expect("a window")
            .workspace = 4;
        app.update();
        assert_hung(&app, windows[0]);
    }

    /// The plugin is told what the crosshair is on while the world is in and
    /// has its input, `aim -` once the world is left, and the same line again
    /// once the world is back in, and once the input is back on.
    #[test]
    fn the_plugin_is_told_the_aim_afresh() {
        use std::io::Read;

        let (world_end, mut plugin) = UnixStream::pair().expect("a socket pair");
        plugin
            .set_nonblocking(true)
            .expect("a socket that does not wait");
        let (mut app, camera) = hanging(Glue::connected_to(world_end));
        app.add_plugins(bevy::state::app::StatesPlugin)
            .init_state::<WorldState>()
            .init_state::<InputMode>()
            .init_resource::<WakeAt>();
        crosshair(&mut app);
        app.world_mut()
            .resource_mut::<NextState<InputMode>>()
            .set(InputMode::Look);

        let told = |plugin: &mut UnixStream| {
            let mut said = String::new();
            // Everything there is so far, then WouldBlock.
            let _ = plugin.read_to_string(&mut said);
            said.lines()
                .filter(|line| line.starts_with("aim"))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let go = |app: &mut App, state: WorldState| {
            app.world_mut()
                .resource_mut::<NextState<WorldState>>()
                .set(state);
            app.update();
        };
        let input = |app: &mut App, mode: InputMode| {
            app.world_mut()
                .resource_mut::<NextState<InputMode>>()
                .set(mode);
            app.update();
        };

        let window = window_on(&mut app, 1);
        app.update();
        app.world_mut()
            .get_mut::<WindowQuad>(window)
            .expect("a quad")
            .size = UVec2::new(600, 400);
        assert_eq!(aimed_from_before(&mut app, camera, window), Some(1));
        let on_it = told(&mut plugin).pop().expect("an aim");
        assert!(on_it.starts_with("aim 0x1 "), "{on_it}");

        go(&mut app, WorldState::FlyingOut);
        assert_eq!(told(&mut plugin), ["aim -"], "the world left");
        assert_eq!(app.world().resource::<Aimed>().0, None);
        go(&mut app, WorldState::In);
        assert_eq!(told(&mut plugin), [on_it.as_str()], "back in");

        input(&mut app, InputMode::Off);
        assert!(told(&mut plugin).is_empty(), "nothing with the input off");
        input(&mut app, InputMode::Look);
        assert_eq!(told(&mut plugin), [on_it.as_str()], "the input back on");
    }

    /// A window's capture is forgotten when its quad goes, off its frame or
    /// with the window, and not when the window moves to another frame.
    #[test]
    fn a_windows_capture_goes_with_its_quad() {
        let (world_end, _plugin) = UnixStream::pair().expect("a socket pair");
        let (mut app, _) = hanging(Glue::connected_to(world_end));
        let (capture, forgotten) = capture::Capture::forgetful();
        app.insert_resource(Captures(capture))
            .add_observer(forget_capture);
        let move_to = |app: &mut App, window: Entity, workspace: i64| {
            app.world_mut()
                .get_mut::<DesktopWindow>(window)
                .expect("a window")
                .workspace = workspace;
            app.update();
        };

        let windows: Vec<Entity> = (1..=3).map(|id| window_on(&mut app, id)).collect();
        app.update();
        assert!(
            windows
                .iter()
                .all(|&window| app.world().get::<WindowQuad>(window).is_some()),
            "on their frames"
        );

        move_to(&mut app, windows[0], 3);
        assert!(forgotten().is_empty(), "onto another frame, still copied");
        // A workspace the world has no frame for.
        move_to(&mut app, windows[1], 99);
        assert_eq!(forgotten(), [capture::export_handle(2)], "off its frame");
        app.world_mut().despawn(windows[2]);
        assert_eq!(forgotten(), [capture::export_handle(3)], "with the window");
    }

    /// A window's own buffers: the first takes the window off capture, each
    /// one replaced is logged to go back once the GPU is done with it, one
    /// replaced in the same update goes back at once, a buffer that comes
    /// round again is sampled as before, and the last goes back with the
    /// window.
    #[test]
    fn a_windows_own_buffers_go_back_to_their_client() {
        use std::{io::Read, os::fd::OwnedFd};

        use import::SharedEvent::{Retired, Shown};
        use rustix::fs::{MemfdFlags, memfd_create};

        let (world_end, mut plugin) = UnixStream::pair().expect("a socket pair");
        plugin
            .set_nonblocking(true)
            .expect("a socket that does not wait");
        let (mut app, _) = hanging(Glue::connected_to(world_end));
        let (capture, forgotten) = capture::Capture::forgetful();
        app.insert_resource(Captures(capture))
            .init_resource::<Stats>()
            .add_message::<glue::Shared>()
            .add_message::<glue::Unshared>()
            .add_observer(forget_capture)
            .add_systems(
                Update,
                (apply_unshared, apply_frames, apply_shared)
                    .chain()
                    .after(sync_quads),
            );

        let dmabuf = || memfd_create("dmabuf", MemfdFlags::CLOEXEC).expect("a memfd");
        let (a, b, c) = (dmabuf(), dmabuf(), dmabuf());
        let share = |app: &mut App, id: u64, file: &OwnedFd| {
            let plane = import::DmabufPlane {
                fd: file.try_clone().expect("a dup"),
                offset: 0,
                stride: 2400,
            };
            let dmabuf = import::Dmabuf {
                width: 600,
                height: 400,
                format: BufferFormat::Rgb(PixelLayout::Bgra),
                modifier: 0,
                planes: vec![plane],
            };
            let buffer = glue::SharedBuffer::Dmabuf {
                id,
                crop: URect::new(0, 0, 600, 400),
                dmabuf,
            };
            app.world_mut().write_message(glue::Shared {
                window: 1,
                surface: 0,
                alpha: false,
                buffer,
            });
        };
        let events = |app: &App| app.world().resource::<import::SharedLog>().events();
        let given_back = |plugin: &mut UnixStream| {
            let mut said = String::new();
            // Everything there is so far, then WouldBlock.
            let _ = plugin.read_to_string(&mut said);
            said.lines()
                .filter(|line| line.starts_with("done"))
                .map(str::to_string)
                .collect::<Vec<_>>()
        };

        let window = window_on(&mut app, 1);
        app.update();
        share(&mut app, 1, &a);
        app.update();
        assert_eq!(forgotten(), [capture::export_handle(1)], "off capture");
        let [Shown(image_a)] = events(&app)[..] else {
            panic!("{:?}", events(&app))
        };

        share(&mut app, 2, &b);
        app.update();
        let [_, Retired(retired, 1), Shown(image_b)] = events(&app)[..] else {
            panic!("{:?}", events(&app))
        };
        assert_eq!(retired, image_a);

        share(&mut app, 3, &c);
        share(&mut app, 4, &a);
        app.update();
        assert_eq!(given_back(&mut plugin), ["done 3"], "never shown");
        assert_eq!(
            events(&app)[3..],
            [Retired(image_b, 2), Shown(image_a)],
            "A sampled as before"
        );

        app.world_mut().despawn(window);
        assert_eq!(events(&app)[5..], [Retired(image_a, 4)], "with the window");
        assert!(given_back(&mut plugin).is_empty(), "the GPU's to say when");
    }

    /// A subsurface of a window `RECT` in size, and a popup that reaches past
    /// its bottom right corner, in px from its top left.
    const SUBSURFACE: Rect = Rect {
        min: Vec2::new(40.0, 40.0),
        max: Vec2::new(280.0, 180.0),
    };
    const POPUP: Rect = Rect {
        min: Vec2::new(500.0, 340.0),
        max: Vec2::new(740.0, 500.0),
    };

    /// The house as `hanging` hangs it, with the systems that put windows'
    /// own buffers on their quads and aim, and a capture to hand frames to.
    fn sharing(glue: Glue) -> (App, Entity, std::sync::mpsc::Sender<(u32, DmabufFrame)>) {
        let (mut app, camera) = hanging(glue);
        let (capture, copied) = capture::Capture::fed();
        app.insert_resource(Captures(capture))
            .init_resource::<Stats>()
            .init_resource::<Aimed>()
            .init_resource::<AimedFrame>()
            .init_resource::<AimLine>()
            .add_message::<glue::Shared>()
            .add_message::<glue::Surfaces>()
            .add_message::<glue::Unshared>()
            .add_observer(forget_capture)
            .add_observer(forget_surface)
            .add_systems(
                Update,
                (
                    apply_unshared,
                    apply_frames,
                    apply_surfaces,
                    apply_shared,
                    place_surfaces,
                )
                    .chain()
                    .after(sync_quads)
                    .before(WorldSystems::Aim),
            )
            .add_systems(Update, aim.in_set(WorldSystems::Aim));
        (app, camera, copied)
    }

    /// The plugin's `buffer` for surface `surface` of window 1: buffer `id`,
    /// `size` px in dmabuf `file`, all of it shown.
    fn share(
        app: &mut App,
        surface: u32,
        id: u64,
        file: &std::os::fd::OwnedFd,
        size: UVec2,
        alpha: bool,
    ) {
        let plane = import::DmabufPlane {
            fd: file.try_clone().expect("a dup"),
            offset: 0,
            stride: size.x * 4,
        };
        let dmabuf = import::Dmabuf {
            width: size.x,
            height: size.y,
            format: BufferFormat::Rgb(PixelLayout::Bgra),
            modifier: 0,
            planes: vec![plane],
        };
        let buffer = glue::SharedBuffer::Dmabuf {
            id,
            crop: URect::from_corners(UVec2::ZERO, size),
            dmabuf,
        };
        app.world_mut().write_message(glue::Shared {
            window: 1,
            surface,
            alpha,
            buffer,
        });
    }

    fn dmabuf() -> std::os::fd::OwnedFd {
        rustix::fs::memfd_create("dmabuf", rustix::fs::MemfdFlags::CLOEXEC).expect("a memfd")
    }

    /// A window's surfaces' quads, by the plugin's ids for them.
    fn surfaces_of(app: &App, window: Entity) -> Vec<(u32, Entity)> {
        let children = app
            .world()
            .get::<Children>(window)
            .map(|c| c.to_vec())
            .unwrap_or_default();
        children
            .into_iter()
            .filter_map(|child| Some((app.world().get::<SurfaceQuad>(child)?.id, child)))
            .collect()
    }

    /// A window's surfaces past its own hang on its quad where its client
    /// draws them, each in front of the one below, the popup past its corner
    /// too, and those with alpha blend. The crosshair on the popup past the
    /// window's corner aims at the window, past its edges, where the plugin
    /// finds the popup.
    #[test]
    fn a_windows_surfaces_hang_where_it_draws_them() {
        use std::io::Read;

        let (world_end, mut plugin) = UnixStream::pair().expect("a socket pair");
        plugin
            .set_nonblocking(true)
            .expect("a socket that does not wait");
        let (mut app, camera, _) = sharing(Glue::connected_to(world_end));
        let window = window_on(&mut app, 1);
        app.update();

        // The crosshair on the point `at` px from the window's top left, on
        // its plane, from a metre and a half in front of it.
        let size = RECT.size();
        let aim_at = |app: &mut App, at: Vec2| {
            let place = *app.world().get::<GlobalTransform>(window).expect("a quad");
            let on =
                place.transform_point(Vec3::new(at.x / size.x - 0.5, 0.5 - at.y / size.y, 0.0));
            *app.world_mut()
                .get_mut::<Transform>(camera)
                .expect("the camera") =
                Transform::from_translation(on + place.back() * 1.5).looking_at(on, Vec3::Y);
            app.update();
            app.world().resource::<Aimed>().0
        };
        // In the popup, past the window's corner.
        let past = Vec2::new(720.0, 480.0);
        assert_eq!(aim_at(&mut app, past), None, "nothing there yet");

        let (own, subsurface, popup) = (dmabuf(), dmabuf(), dmabuf());
        app.world_mut().write_message(glue::Surfaces {
            window: 1,
            surfaces: vec![(1, SUBSURFACE), (2, POPUP)],
        });
        share(&mut app, 0, 1, &own, size.as_uvec2(), false);
        share(
            &mut app,
            1,
            2,
            &subsurface,
            SUBSURFACE.size().as_uvec2(),
            true,
        );
        share(&mut app, 2, 3, &popup, POPUP.size().as_uvec2(), true);
        app.update();

        let quads = surfaces_of(&app, window);
        assert_eq!(
            quads.iter().map(|&(id, _)| id).collect::<Vec<_>>(),
            [1, 2],
            "a quad each"
        );
        let window_place = *app.world().get::<GlobalTransform>(window).expect("a quad");
        for (order, (rect, &(_, quad))) in [SUBSURFACE, POPUP].into_iter().zip(&quads).enumerate() {
            let place = app.world().get::<GlobalTransform>(quad).expect("placed");
            // In front of the window by its place among its surfaces.
            let lift = layout::SURFACE_LIFT * (order + 1) as f32;
            let on_window = |at: Vec2| {
                window_place.transform_point(Vec3::new(
                    at.x / size.x - 0.5,
                    0.5 - at.y / size.y,
                    lift,
                ))
            };
            for (corner, at) in [
                (Vec3::new(-0.5, 0.5, 0.0), rect.min),
                (Vec3::new(0.5, -0.5, 0.0), rect.max),
            ] {
                let (got, expected) = (place.transform_point(corner), on_window(at));
                assert!(
                    got.abs_diff_eq(expected, 1e-5),
                    "{rect:?}: a corner at {got}, not {expected}"
                );
            }
        }

        let blend = |app: &App, entity: Entity| {
            let material = &app
                .world()
                .get::<MeshMaterial3d<StandardMaterial>>(entity)
                .expect("drawn")
                .0;
            app.world()
                .resource::<Assets<StandardMaterial>>()
                .get(material)
                .expect("a material")
                .alpha_mode
        };
        assert_eq!(
            blend(&app, quads[1].1),
            AlphaMode::Premultiplied,
            "a popup with alpha blends"
        );
        assert_eq!(
            blend(&app, window),
            AlphaMode::Opaque,
            "the window's own is opaque, as a capture is"
        );

        assert_eq!(
            aim_at(&mut app, past),
            Some(1),
            "the popup past the window's corner is the window's"
        );
        let mut said = String::new();
        // Everything there is so far, then WouldBlock.
        let _ = plugin.read_to_string(&mut said);
        assert_eq!(
            said.lines().rfind(|line| line.starts_with("aim")),
            Some("aim 0x1 1.20000 1.20000 600 400"),
            "past its edges"
        );
        assert_eq!(
            aim_at(&mut app, past + Vec2::new(40.0, 0.0)),
            None,
            "past the popup too"
        );
    }

    /// A surface below the window's own, the window's own (id 0) listed
    /// after it, hangs behind the window's quad and one after it in front,
    /// and the window's quad lets it show where its own is clear: cut out,
    /// not blended, and only while a surface is below and its own buffer has
    /// alpha, its material made before the surface came below too.
    #[test]
    fn a_surface_below_shows_through_the_windows_own() {
        let (mut app, _, _) = sharing(Glue::connected_to(
            UnixStream::pair().expect("a socket pair").0,
        ));
        let window = window_on(&mut app, 1);
        app.update();

        let size = RECT.size();
        let below = Rect::new(400.0, 0.0, 560.0, 60.0);
        let (own, under, over) = (dmabuf(), dmabuf(), dmabuf());
        let look = |app: &App| {
            let material = &app
                .world()
                .get::<MeshMaterial3d<StandardMaterial>>(window)
                .expect("drawn")
                .0;
            app.world()
                .resource::<Assets<StandardMaterial>>()
                .get(material)
                .expect("a material")
                .alpha_mode
        };

        // Its own with alpha, shared before anything is below it.
        app.world_mut().write_message(glue::Surfaces {
            window: 1,
            surfaces: vec![(2, SUBSURFACE)],
        });
        share(&mut app, 0, 1, &own, size.as_uvec2(), true);
        app.update();
        assert_eq!(
            look(&app),
            AlphaMode::Opaque,
            "nothing below: opaque, as a capture is"
        );

        let whole = Rect::from_corners(Vec2::ZERO, size);
        app.world_mut().write_message(glue::Surfaces {
            window: 1,
            surfaces: vec![(1, below), (0, whole), (2, SUBSURFACE)],
        });
        share(&mut app, 1, 2, &under, below.size().as_uvec2(), false);
        app.update();
        assert_eq!(
            look(&app),
            AlphaMode::Mask(0.5),
            "the material it had, cut out over the surface below"
        );

        let quads = surfaces_of(&app, window);
        assert_eq!(
            quads.iter().map(|&(id, _)| id).collect::<Vec<_>>().len(),
            2,
            "a quad each, none for its own"
        );
        let window_place = *app.world().get::<GlobalTransform>(window).expect("a quad");
        for (id, rect, lift) in [
            (1, below, -layout::SURFACE_LIFT),
            (2, SUBSURFACE, layout::SURFACE_LIFT),
        ] {
            let quad = quads
                .iter()
                .find(|&&(quad, _)| quad == id)
                .expect("its quad")
                .1;
            let place = app.world().get::<GlobalTransform>(quad).expect("placed");
            let expected = window_place.transform_point(Vec3::new(
                rect.min.x / size.x - 0.5,
                0.5 - rect.min.y / size.y,
                lift,
            ));
            let got = place.transform_point(Vec3::new(-0.5, 0.5, 0.0));
            assert!(
                got.abs_diff_eq(expected, 1e-5),
                "surface {id}: its corner at {got}, not {expected}"
            );
        }

        // Its own without alpha hides it, as Hyprland would draw it.
        share(&mut app, 0, 3, &own, size.as_uvec2(), false);
        app.update();
        assert_eq!(look(&app), AlphaMode::Opaque, "no alpha: opaque");
        share(&mut app, 0, 4, &own, size.as_uvec2(), true);
        app.update();
        assert_eq!(look(&app), AlphaMode::Mask(0.5), "alpha again: cut out");

        // Nothing below any more: opaque again, the one above still above.
        app.world_mut().write_message(glue::Surfaces {
            window: 1,
            surfaces: vec![(2, SUBSURFACE)],
        });
        app.update();
        assert_eq!(look(&app), AlphaMode::Opaque, "nothing below: opaque again");
        let over_quad = surfaces_of(&app, window)[0].1;
        let lifted = app
            .world()
            .get::<Transform>(over_quad)
            .expect("placed")
            .translation
            .z;
        assert!(
            (lifted - layout::SURFACE_LIFT).abs() < 1e-7,
            "the one above just above it, at {lifted}"
        );
        drop(over);
    }

    /// A window's surfaces' quads go when the plugin lists them no more,
    /// when the window is back on capture and a captured frame is on it, and
    /// when the window loses its quad. Their buffers go back to their client
    /// each time, as one does when another replaces it, once the GPU is done
    /// with them.
    #[test]
    fn a_windows_surfaces_go_and_give_their_buffers_back() {
        use import::SharedEvent::{Retired, Shown};

        let (world_end, _plugin) = UnixStream::pair().expect("a socket pair");
        let (mut app, _, copied) = sharing(Glue::connected_to(world_end));
        let window = window_on(&mut app, 1);
        app.update();

        let events = |app: &App| app.world().resource::<import::SharedLog>().events();
        // Given back since `from`, by the plugin's ids, in any order.
        let retired_since = |app: &App, from: usize| {
            let mut ids: Vec<u64> = events(app)[from..]
                .iter()
                .filter_map(|e| {
                    if let Retired(_, id) = e {
                        Some(*id)
                    } else {
                        None
                    }
                })
                .collect();
            ids.sort_unstable();
            ids
        };
        let size = RECT.size().as_uvec2();
        let (own, subsurface, popup, popup_again) = (dmabuf(), dmabuf(), dmabuf(), dmabuf());
        app.world_mut().write_message(glue::Surfaces {
            window: 1,
            surfaces: vec![(1, SUBSURFACE), (2, POPUP)],
        });
        share(&mut app, 0, 1, &own, size, false);
        share(
            &mut app,
            1,
            2,
            &subsurface,
            SUBSURFACE.size().as_uvec2(),
            true,
        );
        share(&mut app, 2, 3, &popup, POPUP.size().as_uvec2(), true);
        app.update();
        assert_eq!(
            events(&app)
                .iter()
                .filter(|e| matches!(e, Shown(_)))
                .count(),
            3,
            "each surface's on its quad"
        );

        let seen = events(&app).len();
        share(&mut app, 2, 4, &popup_again, POPUP.size().as_uvec2(), true);
        app.update();
        assert_eq!(
            retired_since(&app, seen),
            [3],
            "the popup's buffer replaced"
        );

        let seen = events(&app).len();
        app.world_mut().write_message(glue::Surfaces {
            window: 1,
            surfaces: vec![(1, SUBSURFACE)],
        });
        app.update();
        assert_eq!(
            surfaces_of(&app, window)
                .iter()
                .map(|&(id, _)| id)
                .collect::<Vec<_>>(),
            [1],
            "the popup closed"
        );
        assert_eq!(retired_since(&app, seen), [4], "with its buffer");

        // Back on capture: the surfaces stay on until a captured frame comes,
        // which shows them all.
        let seen = events(&app).len();
        app.world_mut().write_message(glue::Unshared(1));
        app.update();
        assert_eq!(
            surfaces_of(&app, window).len(),
            1,
            "until a captured frame comes"
        );
        let frame = DmabufFrame::fake(100, size.x, size.y, dmabuf());
        copied
            .send((capture::export_handle(1), frame))
            .expect("the capture");
        app.update();
        assert!(
            surfaces_of(&app, window).is_empty(),
            "the captured frame shows them"
        );
        assert_eq!(
            retired_since(&app, seen),
            [1, 2],
            "the window's own and the subsurface's"
        );

        // Shared again, then off its frame.
        let seen = events(&app).len();
        app.world_mut().write_message(glue::Surfaces {
            window: 1,
            surfaces: vec![(1, SUBSURFACE)],
        });
        share(&mut app, 0, 5, &own, size, false);
        share(
            &mut app,
            1,
            6,
            &subsurface,
            SUBSURFACE.size().as_uvec2(),
            true,
        );
        app.update();
        assert_eq!(surfaces_of(&app, window).len(), 1, "shared again");
        app.world_mut()
            .get_mut::<DesktopWindow>(window)
            .expect("a window")
            .workspace = 9;
        app.update();
        assert!(
            app.world().get::<WindowQuad>(window).is_none(),
            "no frame for workspace 9"
        );
        assert!(
            surfaces_of(&app, window).is_empty(),
            "nor its surfaces' quads"
        );
        assert_eq!(retired_since(&app, seen), [5, 6], "nor their buffers");
        assert_eq!(
            app.world_mut()
                .query::<&SurfaceQuad>()
                .iter(app.world())
                .count(),
            0,
            "none left anywhere"
        );
    }

    /// A window the plugin puts back on capture and shares again before a
    /// captured frame comes is taken off capture again, or its copies would
    /// go on for nothing.
    #[test]
    fn a_window_shared_again_before_a_capture_comes_is_copied_no_more() {
        let (world_end, _plugin) = UnixStream::pair().expect("a socket pair");
        let (mut app, _, _) = sharing(Glue::connected_to(world_end));
        let (capture, forgotten) = capture::Capture::forgetful();
        app.insert_resource(Captures(capture));
        window_on(&mut app, 1);
        app.update();
        let size = RECT.size().as_uvec2();

        copied(&mut app, 0, size, 1, false);
        app.update();
        assert_eq!(forgotten(), [capture::export_handle(1)], "off capture");
        app.world_mut().write_message(glue::Unshared(1));
        app.update();
        copied(&mut app, 0, size, 2, false);
        app.update();
        assert_eq!(
            forgotten(),
            [capture::export_handle(1)],
            "off capture again"
        );
    }

    /// The plugin's `shm` for surface `surface` of window 1, as the
    /// connection thread hands it on: `size` px of `value`, copied out.
    fn copied(app: &mut App, surface: u32, size: UVec2, value: u8, alpha: bool) {
        let pixels = vec![value; (size.x * size.y * 4) as usize];
        let frame = ShmFrame {
            width: size.x,
            height: size.y,
            layout: PixelLayout::Bgra,
            pixels,
        };
        let buffer = glue::SharedBuffer::Pixels {
            frame,
            copy: Some(Duration::ZERO),
        };
        app.world_mut().write_message(glue::Shared {
            window: 1,
            surface,
            alpha,
            buffer,
        });
    }

    /// The image a quad's material samples, the image's pixels, and how the
    /// material blends.
    fn drawn_from(app: &App, quad: Entity) -> (AssetId<Image>, Option<Vec<u8>>, AlphaMode) {
        let material = &app
            .world()
            .get::<MeshMaterial3d<StandardMaterial>>(quad)
            .expect("drawn")
            .0;
        let material = app
            .world()
            .resource::<Assets<StandardMaterial>>()
            .get(material)
            .expect("a material");
        let image = material
            .base_color_texture
            .as_ref()
            .expect("a picture")
            .id();
        let pixels = app
            .world()
            .resource::<Assets<Image>>()
            .get(image)
            .and_then(|image| image.data.clone());
        (image, pixels, material.alpha_mode)
    }

    /// Pixels copied out of a surface's shared memory go onto its quad in an
    /// image of their own: the window's own surface's take it off capture as
    /// a dmabuf does, the same size goes into the same image and a new one
    /// into a new image, a dmabuf they replace goes back once the GPU is
    /// done with it, and pixels a dmabuf replaces have nothing to hand
    /// back. A surface past its own blends by its alpha in shared memory
    /// too.
    #[test]
    fn a_surface_in_shared_memory_is_drawn_from_its_pixels() {
        use import::SharedEvent::{Retired, Shown};

        let (world_end, _plugin) = UnixStream::pair().expect("a socket pair");
        let (mut app, _, _) = sharing(Glue::connected_to(world_end));
        let (capture, forgotten) = capture::Capture::forgetful();
        app.insert_resource(Captures(capture));
        let window = window_on(&mut app, 1);
        app.update();
        let events = |app: &App| app.world().resource::<import::SharedLog>().events();
        let size = RECT.size().as_uvec2();

        copied(&mut app, 0, size, 7, false);
        app.update();
        assert_eq!(
            forgotten(),
            [capture::export_handle(1)],
            "off capture with the first pixels"
        );
        let (image, pixels, blend) = drawn_from(&app, window);
        assert_eq!(
            pixels.as_deref(),
            Some(&vec![7; (size.x * size.y * 4) as usize][..]),
            "its pixels"
        );
        assert_eq!(
            blend,
            AlphaMode::Opaque,
            "the window's own is opaque, as a capture is"
        );
        assert!(events(&app).is_empty(), "nothing to hand back");

        copied(&mut app, 0, size, 8, false);
        app.update();
        assert_eq!(
            drawn_from(&app, window).0,
            image,
            "the same size into the same image"
        );
        copied(&mut app, 0, size / 2, 9, false);
        app.update();
        assert_ne!(
            drawn_from(&app, window).0,
            image,
            "a new size into a new one"
        );

        // A dmabuf in place of the pixels, then pixels in place of it.
        let own = dmabuf();
        share(&mut app, 0, 1, &own, size, false);
        app.update();
        let [Shown(on)] = events(&app)[..] else {
            panic!("{:?}", events(&app))
        };
        assert_eq!(drawn_from(&app, window).0, on, "the dmabuf on it");
        copied(&mut app, 0, size, 10, false);
        app.update();
        assert_eq!(events(&app)[1..], [Retired(on, 1)], "the dmabuf goes back");
        assert!(
            drawn_from(&app, window)
                .1
                .is_some_and(|pixels| pixels[0] == 10),
            "the pixels on it"
        );

        // A popup in shared memory, with alpha.
        app.world_mut().write_message(glue::Surfaces {
            window: 1,
            surfaces: vec![(2, POPUP)],
        });
        copied(&mut app, 2, POPUP.size().as_uvec2(), 11, true);
        app.update();
        let [(2, popup)] = surfaces_of(&app, window)[..] else {
            panic!("{:?}", surfaces_of(&app, window))
        };
        let (_, pixels, blend) = drawn_from(&app, popup);
        assert!(pixels.is_some_and(|pixels| pixels[0] == 11), "its pixels");
        assert_eq!(blend, AlphaMode::Premultiplied, "a popup with alpha blends");
    }

    /// A video subsurface in NV12 is drawn with a `YuvMaterial` sampling
    /// its two planes, each an image the render world puts its view under,
    /// through the crop the plugin said. Back in RGB it is a
    /// `StandardMaterial` again, never both. The window's own surface in YUV
    /// is never taken: its quad draws RGB only, and the buffer goes back.
    #[test]
    fn a_surface_in_yuv_is_drawn_with_the_yuv_material() {
        use std::io::Read;

        let (world_end, mut plugin) = UnixStream::pair().expect("a socket pair");
        plugin
            .set_nonblocking(true)
            .expect("a socket that does not wait");
        let (mut app, _, _) = sharing(Glue::connected_to(world_end));
        let window = window_on(&mut app, 1);
        app.update();
        let file = dmabuf();
        let nv12 = |app: &mut App, surface: u32, id: u64| {
            let plane = |offset| import::DmabufPlane {
                fd: file.try_clone().expect("a dup"),
                offset,
                stride: 320,
            };
            let dmabuf = import::Dmabuf {
                width: 320,
                height: 240,
                format: BufferFormat::Yuv(import::Yuv::Nv12),
                modifier: 0,
                planes: vec![plane(0), plane(76800)],
            };
            let buffer = glue::SharedBuffer::Dmabuf {
                id,
                crop: URect::new(80, 0, 240, 240),
                dmabuf,
            };
            app.world_mut().write_message(glue::Shared {
                window: 1,
                surface,
                alpha: false,
                buffer,
            });
        };

        share(&mut app, 0, 1, &dmabuf(), UVec2::new(600, 400), false);
        app.world_mut().write_message(glue::Surfaces {
            window: 1,
            surfaces: vec![(2, POPUP)],
        });
        app.update();
        nv12(&mut app, 2, 2);
        app.update();

        let [(2, video)] = surfaces_of(&app, window)[..] else {
            panic!("{:?}", surfaces_of(&app, window))
        };
        let world = app.world();
        assert!(
            world
                .get::<MeshMaterial3d<StandardMaterial>>(video)
                .is_none(),
            "not drawn as RGB"
        );
        let material = world
            .get::<MeshMaterial3d<YuvMaterial>>(video)
            .expect("drawn as YUV");
        let material = world
            .resource::<Assets<YuvMaterial>>()
            .get(&material.0)
            .expect("its material");
        let dmabufs = &world.resource::<import::DmabufImages>().0;
        assert!(
            matches!(
                dmabufs.get(&material.luma.id()),
                Some(import::DmabufImage::Shared(_))
            ),
            "its luma, the buffer"
        );
        assert!(
            matches!(dmabufs.get(&material.chroma.id()), Some(import::DmabufImage::Chroma { luma }) if *luma == material.luma.id()),
            "its chroma, a view of the same"
        );
        assert_eq!(
            material.params.crop,
            Vec4::new(0.5, 1.0, 0.25, 0.0),
            "the middle half across"
        );

        share(&mut app, 2, 3, &dmabuf(), UVec2::new(240, 160), false);
        app.update();
        assert!(
            app.world()
                .get::<MeshMaterial3d<YuvMaterial>>(video)
                .is_none(),
            "off YUV"
        );
        assert!(
            app.world()
                .get::<MeshMaterial3d<StandardMaterial>>(video)
                .is_some(),
            "back to RGB"
        );
        nv12(&mut app, 2, 5);
        app.update();
        assert!(
            app.world()
                .get::<MeshMaterial3d<StandardMaterial>>(video)
                .is_none(),
            "and YUV again, RGB off"
        );
        assert!(
            app.world()
                .get::<MeshMaterial3d<YuvMaterial>>(video)
                .is_some(),
            "YUV again"
        );

        let _ = plugin.read_to_string(&mut String::new());
        nv12(&mut app, 0, 4);
        app.update();
        let mut said = String::new();
        let _ = plugin.read_to_string(&mut said);
        assert!(
            said.lines().any(|line| line == "done 4"),
            "the window's own surface in YUV goes back: {said}"
        );
        let quad = app.world().get::<WindowQuad>(window).expect("its quad");
        assert!(quad.own.yuv.is_empty(), "never on the window's quad");
    }

    #[test]
    fn a_single_pixel_is_stretched_over_its_surface() {
        let (world_end, _plugin) = UnixStream::pair().expect("a socket pair");
        let (mut app, _, _) = sharing(Glue::connected_to(world_end));
        let window = window_on(&mut app, 1);
        app.update();

        // The plugin's `pixel` for the popup: half-transparent red, one
        // pixel in the app's byte order.
        app.world_mut().write_message(glue::Surfaces {
            window: 1,
            surfaces: vec![(2, POPUP)],
        });
        let frame = ShmFrame {
            width: 1,
            height: 1,
            layout: PixelLayout::Bgra,
            pixels: vec![0x00, 0x00, 0x80, 0x80],
        };
        let buffer = glue::SharedBuffer::Pixels { frame, copy: None };
        app.world_mut().write_message(glue::Shared {
            window: 1,
            surface: 2,
            alpha: true,
            buffer,
        });
        app.update();

        let [(2, popup)] = surfaces_of(&app, window)[..] else {
            panic!("{:?}", surfaces_of(&app, window))
        };
        let (image, pixels, blend) = drawn_from(&app, popup);
        assert_eq!(
            pixels.as_deref(),
            Some(&[0x00, 0x00, 0x80, 0x80][..]),
            "its one pixel"
        );
        let size = app
            .world()
            .resource::<Assets<Image>>()
            .get(image)
            .expect("an image")
            .size();
        assert_eq!(size, UVec2::ONE, "one pixel, not a box of them");
        assert_eq!(
            blend,
            AlphaMode::Premultiplied,
            "half transparent, so it blends"
        );
        // Its quad is the box, whatever the buffer's size.
        let window_size = RECT.size();
        let scale = app
            .world()
            .get::<Transform>(popup)
            .expect("placed")
            .scale
            .truncate();
        assert_eq!(scale, POPUP.size() / window_size, "stretched over the box");
    }
}
