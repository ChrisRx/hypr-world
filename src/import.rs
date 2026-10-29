//! Render-world half of the dmabuf transport. Each GPU buffer the capture
//! thread rotates a window's frames through (a slot) is imported into wgpu
//! once, as an sRGB texture, and put in `RenderAssets<GpuImage>` under an
//! image id of its own that no main-world `Image` has
//! (`Assets::reserve_handle`). A window's quad samples it through a material
//! of its own, and a new frame only switches the quad's material
//! (windows.rs `show_slot`): no copy, and nothing to prepare again. The
//! capture thread keeps a slot from the compositor while it is on screen
//! (capture.rs `release`).

use std::{
    collections::{HashMap, HashSet},
    os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use ash::vk;

use bevy::{
    pbr::MeshMaterial3d,
    prelude::*,
    render::{
        Render, RenderApp, RenderSystems,
        erased_render_asset::prepare_erased_assets,
        extract_resource::{ExtractResource, ExtractResourcePlugin},
        render_asset::{RenderAssets, prepare_assets},
        render_resource::Sampler,
        renderer::{RenderDevice, RenderQueue},
        texture::{DefaultImageSampler, GpuImage},
    },
};

use wgpu_hal::api::Vulkan;

use crate::{
    capture::{DmabufSlot, PixelLayout},
    glue,
    materials::YuvMaterial,
};

pub fn plugin(app: &mut App) {
    app.init_resource::<DmabufImages>()
        .init_resource::<SharedLog>()
        .init_resource::<SampleLines>()
        .add_plugins((
            ExtractResourcePlugin::<DmabufImages>::default(),
            ExtractResourcePlugin::<SharedLog>::default(),
        ))
        .add_systems(Update, send_samples);

    let samples = app.world().resource::<SampleLines>().clone();
    let sender = app
        .world()
        .get_resource::<glue::Glue>()
        .map(glue::Glue::sender);
    if let Some(render_app) = app.get_sub_app_mut(RenderApp) {
        render_app
            .insert_resource(samples)
            .init_resource::<SharedTextures>();
        if let Some(sender) = sender {
            render_app.insert_resource(sender);
        }
        // After Bevy's own images, which drops those whose last handle went,
        // and before the materials, which look theirs up as they are made.
        render_app.add_systems(
            Render,
            (find_samples, insert_slots, hand_over_shared)
                .chain()
                .in_set(RenderSystems::PrepareAssets)
                .after(prepare_assets::<GpuImage>)
                .before(prepare_erased_assets::<MeshMaterial3d<StandardMaterial>>)
                .before(prepare_erased_assets::<MeshMaterial3d<YuvMaterial>>),
        );
    }
}

/// The GPU buffers the quads sample, by the image id each is inserted
/// under, mirrored into the render world when they change.
#[derive(Resource, Clone, Default)]
pub struct DmabufImages(pub HashMap<AssetId<Image>, DmabufImage>);

#[derive(Clone)]
pub enum DmabufImage {
    /// One of the capture thread's slots.
    Slot {
        slot: Arc<DmabufSlot>,
        layout: PixelLayout,
    },
    /// A window's own buffer, as its client drew it (glue.rs `Shared`). In
    /// YUV, its luma plane.
    Shared(Arc<Dmabuf>),
    /// The chroma plane of the YUV buffer that is under `luma`.
    Chroma { luma: AssetId<Image> },
}

/// Windows' own buffers going onto quads and off them, in order, for the
/// render world: each is taken over from its client before a frame samples
/// it (`Shown`), and handed back after the last frame that did, once the
/// GPU has finished with it, when the plugin is told `done` (`Retired`).
/// The log goes over with the update it was written in, so a buffer is
/// never handed back while a frame from before still samples it. The
/// render world says how far it has acted, and the rest is trimmed.
#[derive(Resource, Clone, Default)]
pub struct SharedLog {
    entries: Vec<(u64, SharedEvent)>,
    last: u64,
    acted: Arc<AtomicU64>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SharedEvent {
    Shown(AssetId<Image>),
    Retired(AssetId<Image>, u64),
}

impl SharedLog {
    pub fn shown(&mut self, image: AssetId<Image>) {
        self.push(SharedEvent::Shown(image));
    }

    /// The buffer under `image`, the plugin's `id`, is off its quad.
    pub fn retired(&mut self, image: AssetId<Image>, id: u64) {
        self.push(SharedEvent::Retired(image, id));
    }

    fn push(&mut self, event: SharedEvent) {
        let acted = self.acted.load(Ordering::Acquire);
        self.entries.retain(|&(seq, _)| seq > acted);
        self.last += 1;
        self.entries.push((self.last, event));
    }

    /// What there is to act on after `seq`, in order.
    fn after(&self, seq: u64) -> impl Iterator<Item = (u64, SharedEvent)> + '_ {
        self.entries
            .iter()
            .copied()
            .filter(move |&(entry, _)| entry > seq)
    }
}

#[cfg(test)]
impl SharedLog {
    /// Everything not acted on yet, in order.
    pub fn events(&self) -> Vec<SharedEvent> {
        self.entries.iter().map(|&(_, event)| event).collect()
    }
}

impl ExtractResource<RenderApp> for SharedLog {
    type Source = Self;

    fn extract_resource(source: &Self) -> Self {
        source.clone()
    }
}

/// The `samples` lines for the plugin, worked out once the render device is
/// there: each format the app takes a window's own buffer in, with the
/// modifiers it samples it through.
#[derive(Resource, Clone, Default)]
pub struct SampleLines(Arc<OnceLock<Vec<String>>>);

/// The textures of windows' own buffers, kept past their images until they
/// are handed back.
#[derive(Resource, Default)]
struct SharedTextures(HashMap<AssetId<Image>, wgpu::Texture>);

/// The byte order of a DRM format the app samples, by its fourcc: ARGB8888
/// and XRGB8888 are B, G, R, A in memory, ABGR8888 and XBGR8888 R, G, B, A.
/// The X byte comes in as alpha, which the opaque quads ignore.
pub fn layout_of(fourcc: u32) -> Option<PixelLayout> {
    match &fourcc.to_le_bytes() {
        b"AR24" | b"XR24" => Some(PixelLayout::Bgra),
        b"AB24" | b"XB24" => Some(PixelLayout::Rgba),
        _ => None,
    }
}

/// What a client's buffer of DRM format `fourcc` is to the app, or None
/// for one it cannot sample: RGB in a byte order, or YUV in two planes.
pub fn format_of(fourcc: u32) -> Option<BufferFormat> {
    match &fourcc.to_le_bytes() {
        b"NV12" => Some(BufferFormat::Yuv(Yuv::Nv12)),
        b"P010" => Some(BufferFormat::Yuv(Yuv::P010)),
        _ => layout_of(fourcc).map(BufferFormat::Rgb),
    }
}

/// Whether a format's fourth byte is alpha (ARGB8888, ABGR8888), to blend
/// by, rather than padding (the X formats). Premultiplied, as Wayland's
/// buffers are.
pub fn has_alpha(fourcc: u32) -> bool {
    matches!(&fourcc.to_le_bytes(), b"AR24" | b"AB24")
}

/// `samples` lines, once: what the app samples, for the plugin to share.
fn find_samples(device: Res<RenderDevice>, lines: Res<SampleLines>) {
    if lines.0.get().is_some() {
        return;
    }
    let mut found = Vec::new();
    for fourcc in [b"AR24", b"XR24", b"AB24", b"XB24", b"NV12", b"P010"] {
        let fourcc = u32::from_le_bytes(*fourcc);
        let Some(format) = format_of(fourcc) else {
            continue;
        };
        let Ok(modifiers) = sampling_modifiers(device.wgpu_device(), format) else {
            continue;
        };
        if modifiers.is_empty() {
            continue;
        }
        let modifiers: Vec<String> = modifiers
            .iter()
            .map(|(modifier, _)| format!("{modifier:#x}"))
            .collect();
        found.push(format!("samples {fourcc:#x} {}", modifiers.join(" ")));
    }
    let _ = lines.0.set(found);
}

/// The `samples` lines to the plugin, once a connection, as soon as both
/// are there.
fn send_samples(glue: Option<Res<glue::Glue>>, lines: Res<SampleLines>, mut sent: Local<u64>) {
    let Some(glue) = glue else { return };
    if !glue.connected || *sent == glue.connection {
        return;
    }
    let Some(lines) = lines.0.get() else { return };
    for line in lines {
        glue.send(line);
    }
    *sent = glue.connection;
}

impl ExtractResource<RenderApp> for DmabufImages {
    type Source = Self;

    fn extract_resource(source: &Self) -> Self {
        source.clone()
    }
}

#[derive(Default)]
struct Inserted {
    ids: HashSet<AssetId<Image>>,
    /// Imports that failed, not tried again.
    failed: HashSet<AssetId<Image>>,
}

/// Each GPU buffer in `RenderAssets<GpuImage>` under its id, imported the
/// first time, and out again once the main world lets it go. Bevy takes it
/// out too when its last handle goes (`AssetEvent::Unused`).
fn insert_slots(
    dmabufs: Res<DmabufImages>,
    mut images: ResMut<RenderAssets<GpuImage>>,
    mut shared: ResMut<SharedTextures>,
    device: Res<RenderDevice>,
    sampler: Res<DefaultImageSampler>,
    mut inserted: Local<Inserted>,
) {
    let Inserted { ids, failed } = &mut *inserted;
    ids.retain(|&id| {
        let kept = dmabufs.0.contains_key(&id);
        if !kept {
            images.remove(id);
        }
        kept
    });
    failed.retain(|id| dmabufs.0.contains_key(id));

    // Luma first, the chroma planes then on the textures just made.
    let (planes, whole): (Vec<_>, Vec<_>) = dmabufs
        .0
        .iter()
        .partition(|(_, dmabuf)| matches!(dmabuf, DmabufImage::Chroma { .. }));
    for (&id, dmabuf) in whole.into_iter().chain(planes) {
        if images.get(id).is_some() || failed.contains(&id) {
            continue;
        }
        let image = match dmabuf {
            // Its contents come from outside wgpu, which is what UNINITIALIZED
            // declares: wgpu makes no assumption about its layout. Linear, it
            // has no compression metadata to lose on the way out of undefined.
            DmabufImage::Slot { slot, layout } => slot_dmabuf(slot, *layout)
                .and_then(|buffer| {
                    import_dmabuf(
                        device.wgpu_device(),
                        &buffer,
                        wgpu::TextureUsages::TEXTURE_BINDING,
                        wgpu::TextureUses::UNINITIALIZED,
                    )
                })
                .map(|texture| gpu_image(texture, &sampler)),
            DmabufImage::Shared(buffer) => {
                let texture = import_dmabuf(
                    device.wgpu_device(),
                    buffer,
                    wgpu::TextureUsages::TEXTURE_BINDING,
                    wgpu::TextureUses::RESOURCE,
                );
                if let Ok(texture) = &texture {
                    shared.0.insert(id, texture.clone());
                }
                texture.map(|texture| match buffer.format {
                    BufferFormat::Rgb(_) => gpu_image(texture, &sampler),
                    BufferFormat::Yuv(_) => plane_image(texture, 0, &sampler),
                })
            }
            // Its luma not imported (yet, or ever): nothing to view.
            DmabufImage::Chroma { luma } => match shared.0.get(luma) {
                Some(texture) => Ok(plane_image(texture.clone(), 1, &sampler)),
                None => continue,
            },
        };
        match image {
            Ok(image) => {
                images.insert(id, image);
                ids.insert(id);
            }
            Err(e) => {
                error_once!("dmabuf import failed: {e}");
                failed.insert(id);
            }
        }
    }
}

/// Where a sampled texture is left between frames (wgpu's `RESOURCE`).
const SAMPLED: vk::ImageLayout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;

/// Takes each window's own buffer over from its client as it comes onto a
/// quad, and hands it back as it goes off one (`SharedLog`): queue family
/// barriers, submitted ahead of this frame, then `done` to the plugin once
/// the GPU has finished everything submitted so far, the frames that
/// sampled the buffer among it.
fn hand_over_shared(
    log: Res<SharedLog>,
    dmabufs: Res<DmabufImages>,
    device: Res<RenderDevice>,
    queue: Res<RenderQueue>,
    sender: Option<Res<glue::PluginSender>>,
    mut shared: ResMut<SharedTextures>,
    mut acted: Local<u64>,
) {
    let device = device.wgpu_device();
    let mut encoder = None;
    let mut done = Vec::new();
    for (seq, event) in log.after(*acted) {
        let (image, handover) = match event {
            SharedEvent::Shown(image) => (image, Handover::In(SAMPLED)),
            SharedEvent::Retired(image, id) => {
                done.push(id);
                (image, Handover::Out(SAMPLED))
            }
        };
        // None for a buffer whose import failed: nothing to hand over.
        if let Some(texture) = shared.0.get(&image) {
            let encoder = encoder.get_or_insert_with(|| {
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("windows' own buffers"),
                })
            });
            if let Err(e) = hand_over(device, encoder, texture, handover) {
                error_once!("handing a window's own buffer over: {e}");
            }
        }
        *acted = seq;
    }

    if let Some(encoder) = encoder {
        queue.submit([encoder.finish()]);
    }
    if !done.is_empty()
        && let Some(sender) = sender
    {
        let sender = sender.clone();
        queue.on_submitted_work_done(move || {
            for id in done {
                sender.send(&format!("done {id}"));
            }
        });
    }
    log.acted.store(*acted, Ordering::Release);
    // Handed back by now, the textures whose images went can go.
    shared.0.retain(|id, _| dmabufs.0.contains_key(id));
}

/// What Bevy would have made of an `Image` the size of `texture`, sampled
/// with the default sampler.
pub fn gpu_image(texture: wgpu::Texture, sampler: &Sampler) -> GpuImage {
    let (format, size) = (texture.format(), texture.size());
    image_of(texture, wgpu::TextureAspect::All, format, size, sampler)
}

/// One plane of a YUV texture as Bevy would have made an image of it: the
/// luma (0) at full size in one channel, or the chroma (1) at half size each
/// way in two.
pub fn plane_image(texture: wgpu::Texture, plane: u32, sampler: &Sampler) -> GpuImage {
    let (aspect, format) = match (plane, texture.format()) {
        (0, wgpu::TextureFormat::P010) => {
            (wgpu::TextureAspect::Plane0, wgpu::TextureFormat::R16Unorm)
        }
        (_, wgpu::TextureFormat::P010) => {
            (wgpu::TextureAspect::Plane1, wgpu::TextureFormat::Rg16Unorm)
        }
        (0, _) => (wgpu::TextureAspect::Plane0, wgpu::TextureFormat::R8Unorm),
        _ => (wgpu::TextureAspect::Plane1, wgpu::TextureFormat::Rg8Unorm),
    };
    let size = texture.size();
    let size = match plane {
        0 => size,
        _ => wgpu::Extent3d {
            width: size.width.div_ceil(2),
            height: size.height.div_ceil(2),
            depth_or_array_layers: 1,
        },
    };
    image_of(texture, aspect, format, size, sampler)
}

/// `aspect` of `texture` as an image of `format` and `size`, sampled with
/// the default sampler.
fn image_of(
    texture: wgpu::Texture,
    aspect: wgpu::TextureAspect,
    format: wgpu::TextureFormat,
    size: wgpu::Extent3d,
    sampler: &Sampler,
) -> GpuImage {
    let view = texture.create_view(&wgpu::TextureViewDescriptor {
        format: Some(format),
        aspect,
        ..Default::default()
    });
    GpuImage {
        texture_descriptor: wgpu::TextureDescriptor {
            label: None,
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: texture.usage(),
            view_formats: &[],
        },
        texture: texture.into(),
        texture_view: view.into(),
        sampler: sampler.clone(),
        texture_view_descriptor: None,
        had_data: true,
    }
}

/// One captured GPU buffer as a one-plane `Dmabuf`, for `import_dmabuf`:
/// linear, its byte order the slot's, on a new fd onto it.
fn slot_dmabuf(slot: &DmabufSlot, layout: PixelLayout) -> Result<Dmabuf, String> {
    Ok(Dmabuf {
        width: slot.width,
        height: slot.height,
        format: BufferFormat::Rgb(layout),
        modifier: slot.modifier,
        planes: vec![DmabufPlane {
            fd: slot.dup_fd().map_err(|e| format!("dup: {e}"))?,
            offset: slot.offset,
            stride: slot.stride,
        }],
    })
}

const NOT_VULKAN: &str = "the renderer is not running on Vulkan";

/// A GPU buffer as a client hands it to the compositor (linux-dmabuf): its
/// size, byte order and DRM format modifier, and its memory planes. A
/// compressed buffer has several: the pixels, then the compression
/// metadata (on AMD, DCC, and a copy of it laid out for the display).
pub struct Dmabuf {
    pub width: u32,
    pub height: u32,
    pub format: BufferFormat,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
}

pub struct DmabufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// What a client's buffer holds: 8-bit RGB in a byte order, or YUV in two
/// planes, the luma at full size and the chroma at half each way.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BufferFormat {
    Rgb(PixelLayout),
    Yuv(Yuv),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Yuv {
    /// NV12: 8 bits a sample.
    Nv12,
    /// P010: 10 bits a sample, in the top of 16.
    P010,
}

/// The texture format for `format`, in Vulkan's terms and wgpu's: RGB as
/// sRGB, so that sampling decodes its colors, and YUV as it is, its two
/// planes sampled through views of their own (`plane_image`) and turned
/// into RGB in the shader (shaders/yuv.wesl).
fn texture_formats(format: BufferFormat) -> (vk::Format, wgpu::TextureFormat) {
    match format {
        BufferFormat::Rgb(layout) => (
            match layout {
                PixelLayout::Bgra => vk::Format::B8G8R8A8_SRGB,
                PixelLayout::Rgba => vk::Format::R8G8B8A8_SRGB,
            },
            layout.srgb(),
        ),
        BufferFormat::Yuv(Yuv::Nv12) => (
            vk::Format::G8_B8R8_2PLANE_420_UNORM,
            wgpu::TextureFormat::NV12,
        ),
        BufferFormat::Yuv(Yuv::P010) => (
            vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
            wgpu::TextureFormat::P010,
        ),
    }
}

/// The DRM format modifiers `device` can sample `format` through, each with
/// its number of memory planes: what a client's buffer has to use for the
/// app to take it as it is.
pub fn sampling_modifiers(
    device: &wgpu::Device,
    format: BufferFormat,
) -> Result<Vec<(u64, u32)>, String> {
    // SAFETY: only asks the physical device about a format.
    let hal = unsafe { device.as_hal::<Vulkan>() }.ok_or(NOT_VULKAN)?;
    let instance = hal.shared_instance().raw_instance();
    let physical = hal.raw_physical_device();
    let (format, _) = texture_formats(format);

    // Asked twice: how many there are, then what they are.
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut properties = vk::FormatProperties2::default().push_next(&mut list);
    unsafe { instance.get_physical_device_format_properties2(physical, format, &mut properties) };
    let mut modifiers = vec![
        vk::DrmFormatModifierPropertiesEXT::default();
        list.drm_format_modifier_count as usize
    ];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut modifiers);
    let mut properties = vk::FormatProperties2::default().push_next(&mut list);
    unsafe { instance.get_physical_device_format_properties2(physical, format, &mut properties) };

    Ok(modifiers
        .iter()
        .filter(|m| {
            m.drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
        })
        .map(|m| (m.drm_format_modifier, m.drm_format_modifier_plane_count))
        .collect())
}

/// Wraps a GPU buffer, a client's or a capture slot, as a texture the app
/// samples as sRGB, through raw Vulkan: an image of the buffer's modifier
/// with each plane where the buffer has it, on the buffer's own memory.
/// wgpu-hal's import takes one plane only. wgpu takes the texture to be in
/// `initial`: for a client's buffer, the state `acquire` puts it in, so that
/// wgpu never moves it out of an undefined layout, which would reset the
/// compression metadata under the pixels. The fds stay the caller's.
pub fn import_dmabuf(
    device: &wgpu::Device,
    buffer: &Dmabuf,
    usage: wgpu::TextureUsages,
    initial: wgpu::TextureUses,
) -> Result<wgpu::Texture, String> {
    let (vk_format, format) = texture_formats(buffer.format);
    let (vk_usage, uses) = usages(usage);
    let size = wgpu::Extent3d {
        width: buffer.width,
        height: buffer.height,
        depth_or_array_layers: 1,
    };
    let described = |e: String| {
        format!(
            "{e} ({}x{} {format:?}, modifier {:#x})",
            buffer.width, buffer.height, buffer.modifier
        )
    };

    // Without it the external memory functions below are not loaded.
    if !device
        .features()
        .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
    {
        return Err(described("no dmabuf import on this device".into()));
    }
    let [first, ..] = buffer.planes.as_slice() else {
        return Err(described("no planes".into()));
    };
    // Mesa hands out a buffer's planes as one dmabuf at different offsets.
    // Planes in dmabufs of their own (disjoint) would each need an import.
    if !buffer
        .planes
        .iter()
        .all(|plane| same_dmabuf(&plane.fd, &first.fd))
    {
        return Err(described("planes in separate dmabufs".into()));
    }

    let hal_texture = {
        // SAFETY: the image and memory made here are handed to this same
        // device whole.
        let hal = unsafe { device.as_hal::<Vulkan>() }.ok_or(NOT_VULKAN)?;
        let raw = hal.raw_device();
        let memory_fd =
            ash::khr::external_memory_fd::Device::new(hal.shared_instance().raw_instance(), raw);

        let planes: Vec<vk::SubresourceLayout> = buffer
            .planes
            .iter()
            .map(|plane| vk::SubresourceLayout {
                offset: plane.offset.into(),
                row_pitch: plane.stride.into(),
                ..Default::default()
            })
            .collect();
        let mut modifier = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(buffer.modifier)
            .plane_layouts(&planes);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // Its planes are viewed in formats of their own, which Vulkan allows
        // an image made mutable.
        let flags = match buffer.format {
            BufferFormat::Yuv(_) => vk::ImageCreateFlags::MUTABLE_FORMAT,
            BufferFormat::Rgb(_) => vk::ImageCreateFlags::empty(),
        };
        let info = vk::ImageCreateInfo::default()
            .flags(flags)
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width: buffer.width,
                height: buffer.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk_usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut modifier);
        // SAFETY: a valid create info, its chain alive for the call.
        let image = unsafe { raw.create_image(&info, None) }
            .map_err(|e| described(format!("vkCreateImage: {e}")))?;
        let memory = import_memory(raw, &memory_fd, image, &first.fd).map_err(|e| {
            // SAFETY: made above, never used.
            unsafe { raw.destroy_image(image, None) };
            described(e)
        })?;

        let desc = wgpu_hal::TextureDescriptor {
            label: Some("dmabuf"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: uses,
            memory_flags: wgpu_hal::MemoryFlags::empty(),
            view_formats: Vec::new(),
        };
        // SAFETY: made above to this descriptor, its memory bound, and both
        // handed over: wgpu-hal destroys them with the texture.
        unsafe {
            hal.texture_from_raw(
                image,
                &desc,
                None,
                wgpu_hal::vulkan::TextureMemory::Dedicated(memory),
            )
        }
    };

    // SAFETY: created from this device's own hal device to the same
    // descriptor, in the state the caller puts it in before any use.
    Ok(unsafe {
        device.create_texture_from_hal::<Vulkan>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("dmabuf"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            },
            initial,
        )
    })
}

/// The Vulkan usage for `usage`, and wgpu-hal's name for it.
fn usages(usage: wgpu::TextureUsages) -> (vk::ImageUsageFlags, wgpu::TextureUses) {
    let pairs = [
        (
            wgpu::TextureUsages::TEXTURE_BINDING,
            vk::ImageUsageFlags::SAMPLED,
            wgpu::TextureUses::RESOURCE,
        ),
        (
            wgpu::TextureUsages::COPY_SRC,
            vk::ImageUsageFlags::TRANSFER_SRC,
            wgpu::TextureUses::COPY_SRC,
        ),
        (
            wgpu::TextureUsages::COPY_DST,
            vk::ImageUsageFlags::TRANSFER_DST,
            wgpu::TextureUses::COPY_DST,
        ),
        (
            wgpu::TextureUsages::RENDER_ATTACHMENT,
            vk::ImageUsageFlags::COLOR_ATTACHMENT,
            wgpu::TextureUses::COLOR_TARGET,
        ),
    ];
    pairs
        .iter()
        .filter(|(wanted, ..)| usage.contains(*wanted))
        .fold(
            (vk::ImageUsageFlags::empty(), wgpu::TextureUses::empty()),
            |(vk_usage, uses), &(_, v, u)| (vk_usage | v, uses | u),
        )
}

/// Both fds are the same dmabuf.
fn same_dmabuf(a: &OwnedFd, b: &OwnedFd) -> bool {
    match (rustix::fs::fstat(a), rustix::fs::fstat(b)) {
        (Ok(a), Ok(b)) => (a.st_dev, a.st_ino) == (b.st_dev, b.st_ino),
        _ => false,
    }
}

/// The dmabuf's memory, imported and bound to `image`.
fn import_memory(
    raw: &ash::Device,
    memory_fd: &ash::khr::external_memory_fd::Device,
    image: vk::Image,
    fd: &OwnedFd,
) -> Result<vk::DeviceMemory, String> {
    let fd = fd.try_clone().map_err(|e| format!("dup: {e}"))?;
    let dmabuf = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
    let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
    // SAFETY: a valid dmabuf fd, only looked at.
    unsafe { memory_fd.get_memory_fd_properties(dmabuf, fd.as_raw_fd(), &mut fd_properties) }
        .map_err(|e| format!("vkGetMemoryFdPropertiesKHR: {e}"))?;
    // SAFETY: an image of this device with no memory yet.
    let requirements = unsafe { raw.get_image_memory_requirements(image) };
    let types = requirements.memory_type_bits & fd_properties.memory_type_bits;
    if types == 0 {
        return Err("no memory type holds both the image and the dmabuf".into());
    }

    // Vulkan owns the fd once the allocation succeeds, and only then.
    let fd = fd.into_raw_fd();
    let mut import = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(dmabuf)
        .fd(fd);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let info = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(types.trailing_zeros())
        .push_next(&mut import)
        .push_next(&mut dedicated);
    // SAFETY: a valid allocate info, its chain alive for the call.
    let memory = unsafe { raw.allocate_memory(&info, None) }.map_err(|e| {
        // SAFETY: not consumed, so still ours to close.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
        format!("vkAllocateMemory: {e}")
    })?;
    // SAFETY: the image's own dedicated memory, at its start.
    unsafe { raw.bind_image_memory(image, memory, 0) }.map_err(|e| {
        // SAFETY: allocated above and bound to nothing.
        unsafe { raw.free_memory(memory, None) };
        format!("vkBindImageMemory: {e}")
    })?;
    Ok(memory)
}

enum Handover {
    In(vk::ImageLayout),
    Out(vk::ImageLayout),
}

fn hand_over(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    handover: Handover,
) -> Result<(), String> {
    let (raw, family) = {
        // SAFETY: only the device's function table and queue family taken.
        let hal = unsafe { device.as_hal::<Vulkan>() }.ok_or(NOT_VULKAN)?;
        (hal.raw_device().clone(), hal.queue_family_index())
    };
    // SAFETY: the image is used only in the barrier below, while the
    // texture is alive.
    let image = unsafe { texture.as_hal::<Vulkan>() }
        .map(|t| unsafe { t.raw_handle() })
        .ok_or(NOT_VULKAN)?;

    let barrier = vk::ImageMemoryBarrier::default()
        .image(image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        });
    let (barrier, from, to) = match handover {
        Handover::In(layout) => (
            barrier
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(layout)
                .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                .dst_queue_family_index(family),
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::ALL_COMMANDS,
        ),
        Handover::Out(layout) => (
            barrier
                .src_access_mask(
                    vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                )
                .old_layout(layout)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL),
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
        ),
    };
    // SAFETY: recorded into the encoder's own open command buffer.
    unsafe {
        encoder.as_hal_mut::<Vulkan, _, _>(|hal_encoder| {
            let hal_encoder = hal_encoder.ok_or(NOT_VULKAN)?;
            raw.cmd_pipeline_barrier(
                hal_encoder.raw_handle(),
                from,
                to,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The render world acts on each entry once, in order, and the main
    /// world keeps only what it has not acted on yet.
    #[test]
    fn the_shared_log_is_acted_on_once_in_order() {
        let image = |n| AssetId::<Image>::Uuid {
            uuid: bevy::asset::uuid::Uuid::from_u128(n),
        };
        let (a, b) = (image(1), image(2));
        let mut log = SharedLog::default();
        log.shown(a);
        log.retired(a, 7);
        assert_eq!(
            log.after(0).collect::<Vec<_>>(),
            [(1, SharedEvent::Shown(a)), (2, SharedEvent::Retired(a, 7))]
        );

        // The render world has acted on the first.
        log.acted.store(1, Ordering::Release);
        log.shown(b);
        assert_eq!(
            log.events(),
            [SharedEvent::Retired(a, 7), SharedEvent::Shown(b)],
            "the first trimmed"
        );
        assert_eq!(
            log.after(2).collect::<Vec<_>>(),
            [(3, SharedEvent::Shown(b))],
            "only what is new"
        );
    }
    use std::fs::File;

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 400;

    /// A device on the GPU that imports dmabufs and views NV12's planes, as
    /// the app's does (Bevy asks for every feature the adapter has).
    fn gpu() -> (wgpu::Device, wgpu::Queue) {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let adapter =
            bevy::tasks::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                ..Default::default()
            }))
            .expect("a GPU");
        bevy::tasks::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_features: wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF
                | wgpu::Features::TEXTURE_FORMAT_NV12,
            ..Default::default()
        }))
        .expect("a device that imports dmabufs")
    }

    /// A compressed buffer, allocated as a client's driver allocates one,
    /// drawn through one import and read through another, as a client draws
    /// a frame and the app samples it: the same bytes come out.
    #[test]
    #[ignore = "needs the GPU, and /dev/dri is not in the sandbox"]
    fn a_compressed_buffer_survives_the_handover() {
        let (device, queue) = gpu();
        for (modifier, planes) in sampling_modifiers(&device, BufferFormat::Rgb(PixelLayout::Bgra))
            .expect("the modifiers")
        {
            eprintln!("samples {modifier:#018x}, {planes} planes");
        }
        let buffer = drawn(&device, &queue, WIDTH, HEIGHT).expect("a buffer drawn and handed over");
        let planes: Vec<(u32, u32)> = buffer.planes.iter().map(|p| (p.offset, p.stride)).collect();
        eprintln!(
            "picked {:#018x}, planes at (offset, stride) {planes:?}",
            buffer.modifier
        );
        assert!(
            buffer.planes.len() > 1,
            "a compressed buffer, its metadata in planes of its own"
        );

        // Taken over, as the app would to sample it, and copied out here.
        let reader = import_dmabuf(
            &device,
            &buffer,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            wgpu::TextureUses::COPY_SRC,
        )
        .expect("an import to read through");
        let row = (WIDTH * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(row * HEIGHT),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut handover = device.create_command_encoder(&Default::default());
        acquire(
            &device,
            &mut handover,
            &reader,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        )
        .expect("an acquire");
        let mut copy = device.create_command_encoder(&Default::default());
        copy.copy_texture_to_buffer(
            reader.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([handover.finish(), copy.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |mapped| {
            mapped.expect("the readback mapped")
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("the copy done");

        let read = readback
            .slice(..)
            .get_mapped_range()
            .expect("the readback's bytes");
        let pixels = pattern(WIDTH, HEIGHT);
        let width = (WIDTH * 4) as usize;
        let wrong = (0..HEIGHT as usize)
            .filter(|&y| read[y * row as usize..][..width] != pixels[y * width..][..width])
            .count();
        assert_eq!(wrong, 0, "rows out of {HEIGHT} that came back different");
    }

    /// An NV12 buffer as a video decoder hands one out, linear, its chroma
    /// rows after its luma rows in one buffer, taken over as the app takes
    /// a client's: each plane, read back through a view of its own, holds
    /// the bytes written into it.
    #[test]
    #[ignore = "needs the GPU, and /dev/dri is not in the sandbox"]
    fn an_nv12_buffer_comes_through_in_its_planes() {
        use std::os::fd::AsFd;

        let (device, queue) = gpu();
        assert!(
            device
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_NV12),
            "NV12 textures"
        );
        let nv12 = BufferFormat::Yuv(Yuv::Nv12);
        for (modifier, planes) in sampling_modifiers(&device, nv12).expect("the modifiers") {
            eprintln!("samples NV12 through {modifier:#018x}, {planes} planes");
        }

        let gbm = gbm::Device::new(render_node().expect("a render node")).expect("GBM");
        let bo = gbm
            .create_buffer_object_with_modifiers2::<()>(
                WIDTH,
                HEIGHT * 3 / 2,
                gbm::Format::R8,
                std::iter::once(gbm::Modifier::Linear),
                gbm::BufferObjectFlags::RENDERING,
            )
            .expect("a linear buffer");
        let stride = bo.stride();
        let luma = |x: u32, y: u32| ((x + 3 * y) % 251) as u8;
        let chroma = |x: u32, y: u32| ((7 * x + y) % 241) as u8;
        let mut bo = bo;
        bo.map_mut(0, 0, WIDTH, HEIGHT * 3 / 2, |mapped| {
            let bytes = mapped.buffer_mut();
            for y in 0..HEIGHT * 3 / 2 {
                for x in 0..WIDTH {
                    let value = if y < HEIGHT {
                        luma(x, y)
                    } else {
                        chroma(x, y - HEIGHT)
                    };
                    bytes[(y * stride + x) as usize] = value;
                }
            }
        })
        .expect("the buffer written");
        let fd = bo.fd().expect("its dmabuf");
        let plane = |offset| DmabufPlane {
            fd: fd.as_fd().try_clone_to_owned().expect("a dup"),
            offset,
            stride,
        };
        let buffer = Dmabuf {
            width: WIDTH,
            height: HEIGHT,
            format: nv12,
            modifier: 0,
            planes: vec![plane(bo.offset(0)), plane(bo.offset(0) + stride * HEIGHT)],
        };

        let texture = import_dmabuf(
            &device,
            &buffer,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            wgpu::TextureUses::COPY_SRC,
        )
        .expect("an NV12 import");
        let mut handover = device.create_command_encoder(&Default::default());
        acquire(
            &device,
            &mut handover,
            &texture,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        )
        .expect("an acquire");

        // Each plane into a texture of its own format, then out to memory.
        let mut copy = device.create_command_encoder(&Default::default());
        let planes = [
            (
                wgpu::TextureAspect::Plane0,
                wgpu::TextureFormat::R8Unorm,
                WIDTH,
                HEIGHT,
            ),
            (
                wgpu::TextureAspect::Plane1,
                wgpu::TextureFormat::Rg8Unorm,
                WIDTH / 2,
                HEIGHT / 2,
            ),
        ];
        let readbacks: Vec<(wgpu::Buffer, u32)> = planes
            .iter()
            .map(|&(aspect, format, width, height)| {
                let size = wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                };
                let target = device.create_texture(&wgpu::TextureDescriptor {
                    label: None,
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                copy.copy_texture_to_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect,
                    },
                    target.as_image_copy(),
                    size,
                );
                let row = (WIDTH).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
                let readback = device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: u64::from(row * height),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                copy.copy_texture_to_buffer(
                    target.as_image_copy(),
                    wgpu::TexelCopyBufferInfo {
                        buffer: &readback,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(row),
                            rows_per_image: None,
                        },
                    },
                    size,
                );
                (readback, row)
            })
            .collect();
        queue.submit([handover.finish(), copy.finish()]);
        for (readback, _) in &readbacks {
            readback.slice(..).map_async(wgpu::MapMode::Read, |mapped| {
                mapped.expect("a readback mapped")
            });
        }
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("the copies done");

        let wrong =
            |(readback, row): &(wgpu::Buffer, u32), height: u32, value: &dyn Fn(u32, u32) -> u8| {
                let read = readback.slice(..).get_mapped_range().expect("the bytes");
                (0..height)
                    .filter(|&y| (0..WIDTH).any(|x| read[(y * row + x) as usize] != value(x, y)))
                    .count()
            };
        assert_eq!(
            wrong(&readbacks[0], HEIGHT, &luma),
            0,
            "luma rows that came back different"
        );
        // The chroma plane's rows: Cb and Cr for every two pixels across,
        // WIDTH bytes.
        assert_eq!(
            wrong(&readbacks[1], HEIGHT / 2, &chroma),
            0,
            "chroma rows that came back different"
        );
    }

    /// Takes a buffer over from whoever drew it, into `layout`: a queue family
    /// ownership acquire from outside the device, recorded in `encoder`, which
    /// is to be submitted on its own before anything that uses the texture.
    /// The driver then keeps the compression metadata as the other side left
    /// it.
    fn acquire(
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        layout: vk::ImageLayout,
    ) -> Result<(), String> {
        hand_over(device, encoder, texture, Handover::In(layout))
    }

    /// Hands a buffer back for whoever draws it next, from `layout`, where the
    /// last use left it: the release that pairs with the other side's acquire,
    /// submitted after everything that used the texture.
    fn release(
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        layout: vk::ImageLayout,
    ) -> Result<(), String> {
        hand_over(device, encoder, texture, Handover::Out(layout))
    }

    /// A buffer allocated as a client's driver allocates one, from the
    /// modifiers `device` can sample, drawn with `pattern` through an import of
    /// its own and handed over, as a client hands over a frame.
    fn drawn(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
    ) -> Result<Dmabuf, String> {
        let gbm = gbm::Device::new(render_node()?).map_err(|e| format!("GBM: {e}"))?;
        let modifiers = sampling_modifiers(device, BufferFormat::Rgb(PixelLayout::Bgra))?;
        let bo = gbm
            .create_buffer_object_with_modifiers2::<()>(
                width,
                height,
                gbm::Format::Argb8888,
                modifiers
                    .iter()
                    .map(|&(modifier, _)| gbm::Modifier::from(modifier)),
                gbm::BufferObjectFlags::RENDERING,
            )
            .map_err(|e| format!("GBM allocation: {e}"))?;
        let buffer = dmabuf_of(&bo)?;

        let writer = import_dmabuf(
            device,
            &buffer,
            wgpu::TextureUsages::COPY_DST,
            wgpu::TextureUses::UNINITIALIZED,
        )?;
        queue.write_texture(
            writer.as_image_copy(),
            &pattern(width, height),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let mut handover = device.create_command_encoder(&Default::default());
        release(
            device,
            &mut handover,
            &writer,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        )?;
        queue.submit([handover.finish()]);
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| format!("drawing: {e}"))?;
        Ok(buffer)
    }

    /// The first render node: the one GPU here.
    fn render_node() -> Result<File, String> {
        let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
            .map_err(|e| format!("/dev/dri: {e}"))?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
            })
            .collect();
        nodes.sort();
        let node = nodes.first().ok_or("no render node")?;
        File::options()
            .read(true)
            .write(true)
            .open(node)
            .map_err(|e| format!("{}: {e}", node.display()))
    }

    /// The buffer as its client would hand it over.
    fn dmabuf_of(bo: &gbm::BufferObject<()>) -> Result<Dmabuf, String> {
        let planes = (0..bo.plane_count() as i32)
            .map(|plane| {
                Ok(DmabufPlane {
                    fd: bo
                        .fd_for_plane(plane)
                        .map_err(|e| format!("plane {plane}: {e}"))?,
                    offset: bo.offset(plane),
                    stride: bo.stride_for_plane(plane),
                })
            })
            .collect::<Result<_, String>>()?;
        Ok(Dmabuf {
            width: bo.width(),
            height: bo.height(),
            format: BufferFormat::Rgb(PixelLayout::Bgra),
            modifier: bo.modifier().into(),
            planes,
        })
    }

    /// Red, green, blue and white quarters from the top left, crossed by a
    /// one-pixel grid every 7 pixels, BGRA: flat runs compress, and every edge
    /// has to come through as well.
    fn pattern(width: u32, height: u32) -> Vec<u8> {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let bgra = if x % 7 == 0 || y % 7 == 0 {
                    [0x20, 0x20, 0x20, 0xff]
                } else {
                    match (x < width / 2, y < height / 2) {
                        (true, true) => [0x00, 0x00, 0xff, 0xff],
                        (false, true) => [0x00, 0xff, 0x00, 0xff],
                        (true, false) => [0xff, 0x00, 0x00, 0xff],
                        (false, false) => [0xff, 0xff, 0xff, 0xff],
                    }
                };
                pixels.extend_from_slice(&bgra);
            }
        }
        pixels
    }
}
