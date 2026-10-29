//! A Wayland client with a known tree of surfaces, for trying how the plugin
//! shares a window of more than one surface (README.md, "A nested session"):
//! a toplevel, a subsurface above it with a transparent half, optionally one
//! below it, and a popup that opens and closes on a timer and reaches past
//! the toplevel's right and bottom edges. Every buffer is a linear dmabuf the
//! CPU writes, or shared memory for the parts `--shm` names, and each
//! surface moves a bar one step a frame callback, so the rate each one draws
//! at shows in the app's stats.
//!
//!   cargo run --release --example surfaces -- [--still | --same] [--below] [--crop] [--popup <seconds>] [--shm <parts>] [--pixel <parts>] [--yuv <parts>] [--one-buffer] [--stubborn]
//!
//!   --still      each surface drawn once, for screenshots
//!   --same       each surface drawn every frame, the same each time: still
//!                for screenshots, but committing, as the plugin keeps only
//!                the shared memory a surface commits while it watches
//!   --below      a subsurface below the toplevel's own surface too, seen
//!                through a clear hole in it, shown while the popup is
//!                open and hidden while it is closed
//!   --crop       the upper subsurface shown through a viewport source crop
//!                of a buffer twice its size, red outside the crop
//!   --popup S    the popup open for S seconds and closed for S (3 if not
//!                given), 0 to keep it open
//!   --shm PARTS  these parts in shared memory (wl_shm), not dmabufs: any
//!                of main, above, below and popup, comma-separated
//!   --pixel PARTS these parts as one pixel each (wp_single_pixel_buffer_v1)
//!                stretched over the part by a viewport's destination, as
//!                clients draw a plain background: any of main, above,
//!                below and popup, not above with --crop
//!   --yuv PARTS  these parts in NV12 dmabufs, eight colour bars at 75%
//!                each (white, yellow, cyan, green, magenta, red, blue,
//!                black) in BT.601 at limited range, as Hyprland reads YUV:
//!                any of main, above, below and popup, not above with --crop
//!   --one-buffer each part draws into one buffer only, and a frame that
//!                comes while the compositor has it waits for its release,
//!                as a client that never makes a second one would
//!   --stubborn   ignores the compositor asking it to close, as an app with
//!                unsaved work does, so only ending its process closes it

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    os::{fd::AsFd, unix::fs::FileExt},
    time::{Duration, Instant},
};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::{ObjectId, WaylandError},
    delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_callback::{self, WlCallback},
        wl_compositor::WlCompositor,
        wl_registry::{self, WlRegistry},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_subcompositor::WlSubcompositor,
        wl_subsurface::WlSubsurface,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::client::{
            zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
            zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        },
        single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1,
        viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
    },
    xdg::shell::client::{
        xdg_popup::{self, XdgPopup},
        xdg_positioner::{self, XdgPositioner},
        xdg_surface::{self, XdgSurface},
        xdg_toplevel::{self, XdgToplevel},
        xdg_wm_base::{self, XdgWmBase},
    },
};

/// The toplevel, in logical px and buffer px alike (buffer scale 1).
const TOPLEVEL: (u32, u32) = (640, 400);
/// The upper subsurface, and where it sits on the toplevel.
const ABOVE: (u32, u32) = (240, 140);
const ABOVE_AT: (i32, i32) = (40, 40);
/// The lower one, peeking out past the toplevel's top edge.
const BELOW: (u32, u32) = (160, 100);
const BELOW_AT: (i32, i32) = (420, -40);
/// The popup, its top left corner on a point of the toplevel, so that it
/// reaches 100 px past its right and bottom edges.
const POPUP: (u32, u32) = (240, 160);
const POPUP_AT: (i32, i32) = (500, 340);
/// How far a bar moves each frame, in px.
const STEP: u32 = 4;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Part {
    Toplevel,
    Above,
    Below,
    Popup,
}

impl Part {
    /// By the name `--shm` takes.
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "main" => Part::Toplevel,
            "above" => Part::Above,
            "below" => Part::Below,
            "popup" => Part::Popup,
            _ => return None,
        })
    }

    fn size(self) -> (u32, u32) {
        match self {
            Part::Toplevel => TOPLEVEL,
            Part::Above => ABOVE,
            Part::Below => BELOW,
            Part::Popup => POPUP,
        }
    }

    /// With alpha for the parts with something to see through: the
    /// toplevel's hole over the lower subsurface (`--below`), the upper
    /// subsurface's clear half, the popup's band.
    fn format(self) -> gbm::Format {
        match self {
            Part::Below => gbm::Format::Xrgb8888,
            Part::Toplevel | Part::Above | Part::Popup => gbm::Format::Argb8888,
        }
    }
}

/// A part drawn as one pixel (`--pixel`): its colour, premultiplied R, G,
/// B, A, see-through where the part is.
fn plain(part: Part) -> [u8; 4] {
    match part {
        Part::Toplevel => [0x30, 0x50, 0xf0, 0xff],
        Part::Below => [0x30, 0xc0, 0x30, 0xff],
        // Yellow and cyan at half alpha.
        Part::Above => [0x78, 0x68, 0x10, 0x80],
        Part::Popup => [0x18, 0x68, 0x68, 0x80],
    }
}

/// The pixel at (x, y) of a part at `step`, as premultiplied B, G, R, A
/// bytes: solid colours, a moving dark bar, and the parts to see through,
/// the toplevel's hole over the lower subsurface only with one (`hole`).
fn pixel(part: Part, step: u32, x: u32, y: u32, hole: bool) -> [u8; 4] {
    let (width, _) = part.size();
    let opaque = |[r, g, b]: [u8; 3]| [b, g, r, 0xff];
    let over_below = |x: u32, y: u32| {
        let (bx, by) = (BELOW_AT.0.max(0) as u32, BELOW_AT.1.max(0) as u32);
        let (bw, bh) = (
            (BELOW_AT.0 + BELOW.0 as i32).max(0) as u32,
            (BELOW_AT.1 + BELOW.1 as i32).max(0) as u32,
        );
        (bx..bw).contains(&x) && (by..bh).contains(&y)
    };
    if part == Part::Toplevel && hole && over_below(x, y) {
        return [0, 0, 0, 0];
    }
    if x.abs_diff(step * STEP % width) < 4 {
        return opaque([0x20, 0x20, 0x20]);
    }
    match part {
        // Four quadrants, each its own colour, to find them in a picture.
        Part::Toplevel => opaque(match (x < TOPLEVEL.0 / 2, y < TOPLEVEL.1 / 2) {
            (true, true) => [0xe0, 0x30, 0x30],
            (false, true) => [0x30, 0xc0, 0x30],
            (true, false) => [0x30, 0x50, 0xf0],
            (false, false) => [0xd0, 0xd0, 0xd0],
        }),
        // The left half clear, the right yellow.
        Part::Above if x < ABOVE.0 / 2 => [0, 0, 0, 0],
        Part::Above => opaque([0xf0, 0xd0, 0x20]),
        Part::Below => opaque([0xe0, 0x30, 0xe0]),
        // Cyan, with a band across it at half alpha.
        Part::Popup if (60..100).contains(&y) => [0x68, 0x68, 0x18, 0x80],
        Part::Popup => opaque([0x30, 0xd0, 0xd0]),
    }
}

/// The bar at x of a part `width` wide, eight of them at 75% across it, as
/// B, G, R, A bytes.
fn bar(x: u32, width: u32) -> [u8; 4] {
    const BARS: [[u8; 3]; 8] = [
        [191, 191, 191],
        [191, 191, 0],
        [0, 191, 191],
        [0, 191, 0],
        [191, 0, 191],
        [191, 0, 0],
        [0, 0, 191],
        [0, 0, 0],
    ];
    let [r, g, b] = BARS[(x * 8 / width.max(1)) as usize % 8];
    [b, g, r, 0xff]
}

/// R'G'B' (B, G, R, A bytes) as BT.601 Y'CbCr at limited range: luma 16 to
/// 235, chroma 16 to 240 around 128.
fn bt601([b, g, r, _]: [u8; 4]) -> (u8, u8, u8) {
    let (r, g, b) = (
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
    );
    let y = 0.299 * r + 0.587 * g + 0.114 * b;
    let (cb, cr) = ((b - y) / 1.772, (r - y) / 1.402);
    let byte = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    (
        byte(16.0 + 219.0 * y),
        byte(128.0 + 224.0 * cb),
        byte(128.0 + 224.0 * cr),
    )
}

/// One of a part's buffers: its memory, the compositor's handle on it, and
/// whether the compositor has it.
struct Slot {
    memory: Memory,
    buffer: WlBuffer,
    busy: bool,
}

/// What a buffer's pixels are in.
enum Memory {
    /// A linear buffer object, mapped to be written.
    Gpu(gbm::BufferObject<()>),
    /// A memfd the compositor maps as a `wl_shm` pool, written whole.
    Shared(File),
    /// A linear buffer object of one byte a texel, half as tall again as the
    /// picture: its luma rows, then its chroma rows, Cb and Cr in turn a
    /// texel each for every two pixels across and down (NV12).
    Yuv(gbm::BufferObject<()>),
}

impl Memory {
    /// Every pixel of a `width` x `height` buffer, `colour` at (x, y), as B,
    /// G, R, A bytes.
    fn paint(
        &mut self,
        (width, height): (u32, u32),
        colour: impl Fn(u32, u32) -> [u8; 4],
    ) -> Result<(), String> {
        let fill = |bytes: &mut [u8], stride: usize| {
            for y in 0..height {
                for x in 0..width {
                    let at = y as usize * stride + x as usize * 4;
                    bytes[at..at + 4].copy_from_slice(&colour(x, y));
                }
            }
        };
        match self {
            Memory::Gpu(bo) => bo
                .map_mut(0, 0, width, height, |mapped| {
                    let stride = mapped.stride() as usize;
                    fill(mapped.buffer_mut(), stride);
                })
                .map_err(|e| format!("mapping a buffer: {e}")),
            Memory::Shared(file) => {
                let mut bytes = vec![0; (width * height * 4) as usize];
                fill(&mut bytes, width as usize * 4);
                file.write_all_at(&bytes, 0)
                    .map_err(|e| format!("writing shared memory: {e}"))
            }
            // Each 2 x 2 pixels' chroma from the top left of them.
            Memory::Yuv(bo) => bo
                .map_mut(0, 0, width, height * 3 / 2, |mapped| {
                    let stride = mapped.stride() as usize;
                    let bytes = mapped.buffer_mut();
                    for y in 0..height {
                        for x in 0..width {
                            let (luma, cb, cr) = bt601(colour(x, y));
                            bytes[y as usize * stride + x as usize] = luma;
                            if x % 2 == 0 && y % 2 == 0 {
                                let at = (height + y / 2) as usize * stride + x as usize;
                                bytes[at] = cb;
                                bytes[at + 1] = cr;
                            }
                        }
                    }
                })
                .map_err(|e| format!("mapping a buffer: {e}")),
        }
    }
}

/// The buffers a part draws into in turn, one more whenever the compositor
/// has them all, and the step its bar is at. A part's pool outlives its
/// surface, so a popup opened again takes back the buffers of the last.
struct Pool {
    slots: Vec<Slot>,
    step: u32,
    /// With one buffer only, a frame came while the compositor had it.
    waiting: bool,
}

/// GBM on the compositor's GPU, and the protocols that hand it buffers:
/// dmabufs, and shared memory for the parts in `shm`.
struct Gpu {
    gbm: gbm::Device<File>,
    dmabuf: ZwpLinuxDmabufV1,
    wl_shm: WlShm,
    shm: HashSet<Part>,
    /// The parts in NV12.
    yuv: HashSet<Part>,
}

impl Gpu {
    /// A buffer of `width` x `height` for `part`, as a `wl_buffer` whose
    /// release says which slot it is: linear, or shared memory.
    fn allocate(
        &self,
        part: Part,
        slot: usize,
        (width, height): (u32, u32),
        qh: &QueueHandle<State>,
    ) -> Result<Slot, String> {
        if self.shm.contains(&part) {
            let file = File::from(
                rustix::fs::memfd_create("hypr-world-surfaces", rustix::fs::MemfdFlags::CLOEXEC)
                    .map_err(|e| format!("a memfd: {e}"))?,
            );
            let len = width * height * 4;
            file.set_len(u64::from(len))
                .map_err(|e| format!("sizing a memfd: {e}"))?;
            let format = match part.format() {
                gbm::Format::Argb8888 => wl_shm::Format::Argb8888,
                _ => wl_shm::Format::Xrgb8888,
            };
            let pool = self.wl_shm.create_pool(file.as_fd(), len as i32, qh, ());
            let buffer = pool.create_buffer(
                0,
                width as i32,
                height as i32,
                width as i32 * 4,
                format,
                qh,
                (part, slot),
            );
            // The buffer keeps what it needs of the pool.
            pool.destroy();
            return Ok(Slot {
                memory: Memory::Shared(file),
                buffer,
                busy: false,
            });
        }

        if self.yuv.contains(&part) {
            // Both planes in one buffer object, the chroma's rows after the
            // luma's, as a video decoder hands out NV12.
            let bo = self
                .gbm
                .create_buffer_object_with_modifiers2::<()>(
                    width,
                    height * 3 / 2,
                    gbm::Format::R8,
                    std::iter::once(gbm::Modifier::Linear),
                    gbm::BufferObjectFlags::RENDERING,
                )
                .map_err(|e| format!("allocating a linear NV12 buffer: {e}"))?;
            let fd = bo.fd().map_err(|e| format!("exporting a buffer: {e}"))?;
            let modifier = u64::from(gbm::Modifier::Linear);
            let (offset, stride) = (bo.offset(0), bo.stride());
            let params = self.dmabuf.create_params(qh, ());
            for (plane, at) in [(0, offset), (1, offset + stride * height)] {
                params.add(
                    fd.as_fd(),
                    plane,
                    at,
                    stride,
                    (modifier >> 32) as u32,
                    modifier as u32,
                );
            }
            let buffer = params.create_immed(
                width as i32,
                height as i32,
                gbm::Format::Nv12 as u32,
                zwp_linux_buffer_params_v1::Flags::empty(),
                qh,
                (part, slot),
            );
            params.destroy();
            return Ok(Slot {
                memory: Memory::Yuv(bo),
                buffer,
                busy: false,
            });
        }

        let format = part.format();
        let bo = self
            .gbm
            .create_buffer_object_with_modifiers2::<()>(
                width,
                height,
                format,
                std::iter::once(gbm::Modifier::Linear),
                gbm::BufferObjectFlags::RENDERING,
            )
            .map_err(|e| format!("allocating a linear buffer: {e}"))?;
        let fd = bo.fd().map_err(|e| format!("exporting a buffer: {e}"))?;
        let modifier = u64::from(gbm::Modifier::Linear);

        let params = self.dmabuf.create_params(qh, ());
        params.add(
            fd.as_fd(),
            0,
            bo.offset(0),
            bo.stride(),
            (modifier >> 32) as u32,
            modifier as u32,
        );
        let buffer = params.create_immed(
            width as i32,
            height as i32,
            format as u32,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
            (part, slot),
        );
        params.destroy();
        Ok(Slot {
            memory: Memory::Gpu(bo),
            buffer,
            busy: false,
        })
    }
}

struct State {
    gpu: Gpu,
    compositor: WlCompositor,
    wm_base: XdgWmBase,
    toplevel: XdgSurface,
    /// The surfaces there are now, by part.
    surfaces: HashMap<Part, WlSurface>,
    pools: HashMap<Part, Pool>,
    popup: Option<(XdgSurface, XdgPopup)>,
    still: bool,
    /// Each frame drawn as the last, the bars where they were.
    same: bool,
    /// The upper subsurface's buffers are twice its size, cropped.
    crop: bool,
    /// A subsurface below the toplevel's own, seen through a hole in it.
    below: bool,
    /// Each part draws into one buffer only (`Pool::waiting`).
    one_buffer: bool,
    /// It stays open when asked to close.
    stubborn: bool,
    /// The parts drawn as one pixel, with what they take: the manager, the
    /// viewport that stretches each over its part, and each one's buffer,
    /// made once and attached again, as its pixel never changes.
    plain: HashSet<Part>,
    single_pixel: Option<WpSinglePixelBufferManagerV1>,
    viewporter: WpViewporter,
    viewports: HashMap<Part, WpViewport>,
    pixels: HashMap<Part, WlBuffer>,
    mapped: bool,
    running: bool,
}

impl State {
    /// Draws `part` at its step into a free buffer and commits it, asking
    /// for the next frame unless still.
    fn draw(&mut self, part: Part, qh: &QueueHandle<Self>) {
        if let Err(e) = self.try_draw(part, qh) {
            eprintln!("surfaces: {e}");
            self.running = false;
        }
    }

    fn try_draw(&mut self, part: Part, qh: &QueueHandle<Self>) -> Result<(), String> {
        let Some(surface) = self.surfaces.get(&part) else {
            return Ok(());
        };
        if self.plain.contains(&part) {
            self.draw_plain(part, qh);
            return Ok(());
        }
        let crop = self.crop && part == Part::Above;
        let (width, height) = part.size();
        let size = if crop {
            (2 * width, 2 * height)
        } else {
            (width, height)
        };
        let pool = self.pools.entry(part).or_insert(Pool {
            slots: Vec::new(),
            step: 0,
            waiting: false,
        });

        let free = pool.slots.iter().position(|slot| !slot.busy);
        let index = match free {
            Some(index) => index,
            // Drawn when it comes back.
            None if self.one_buffer && !pool.slots.is_empty() => {
                pool.waiting = true;
                return Ok(());
            }
            None => {
                pool.slots
                    .push(self.gpu.allocate(part, pool.slots.len(), size, qh)?);
                pool.slots.len() - 1
            }
        };
        let step = pool.step;
        let yuv = self.gpu.yuv.contains(&part);
        let hole = self.below;
        let slot = &mut pool.slots[index];
        slot.memory.paint(size, |x, y| match crop {
            false if yuv => bar(x, width),
            false => pixel(part, step, x, y, hole),
            // Cropped: the middle of the buffer, red around it.
            true => match (x.checked_sub(width / 2), y.checked_sub(height / 2)) {
                (Some(x), Some(y)) if x < width && y < height => pixel(part, step, x, y, hole),
                _ => [0x20, 0x20, 0xf0, 0xff],
            },
        })?;
        slot.busy = true;

        surface.attach(Some(&slot.buffer), 0, 0);
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        if !self.still {
            surface.frame(qh, (part, surface.id()));
        }
        surface.commit();
        Ok(())
    }

    /// `part` as one pixel stretched over it, committed, asking for the next
    /// frame unless still.
    fn draw_plain(&mut self, part: Part, qh: &QueueHandle<Self>) {
        let (Some(surface), Some(manager)) = (self.surfaces.get(&part), &self.single_pixel) else {
            return;
        };
        let (width, height) = part.size();
        self.viewports.entry(part).or_insert_with(|| {
            let viewport = self.viewporter.get_viewport(surface, qh, ());
            viewport.set_destination(width as i32, height as i32);
            viewport
        });
        let buffer = self.pixels.entry(part).or_insert_with(|| {
            // Each channel's byte stretched over the whole u32 range.
            let [r, g, b, a] = plain(part).map(|byte| u32::from(byte) * 0x0101_0101);
            manager.create_u32_rgba_buffer(r, g, b, a, qh, (part, usize::MAX))
        });
        surface.attach(Some(buffer), 0, 0);
        surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        if !self.still {
            surface.frame(qh, (part, surface.id()));
        }
        surface.commit();
    }

    /// The popup, open: its surface and role now, its first buffer once it
    /// is configured.
    fn open_popup(&mut self, qh: &QueueHandle<Self>) {
        let surface = self.compositor.create_surface(qh, ());
        let xdg = self.wm_base.get_xdg_surface(&surface, qh, ());
        let positioner = self.wm_base.create_positioner(qh, ());
        positioner.set_size(POPUP.0 as i32, POPUP.1 as i32);
        positioner.set_anchor_rect(POPUP_AT.0, POPUP_AT.1, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        positioner.set_constraint_adjustment(xdg_positioner::ConstraintAdjustment::empty());
        let popup = xdg.get_popup(Some(&self.toplevel), &positioner, qh, ());
        positioner.destroy();
        surface.commit();

        self.surfaces.insert(Part::Popup, surface);
        self.popup = Some((xdg, popup));
    }

    fn close_popup(&mut self) {
        if let Some((xdg, popup)) = self.popup.take() {
            popup.destroy();
            xdg.destroy();
        }
        if let Some(viewport) = self.viewports.remove(&Part::Popup) {
            viewport.destroy();
        }
        if let Some(surface) = self.surfaces.remove(&Part::Popup) {
            surface.destroy();
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("surfaces: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let (mut still, mut same, mut below, mut crop, mut period) =
        (false, false, false, false, Duration::from_secs(3));
    let mut shm = HashSet::new();
    let mut one_buffer = false;
    let mut stubborn = false;
    let mut plain = HashSet::new();
    let mut yuv = HashSet::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--yuv" => {
                let parts = args.next().ok_or("--yuv takes parts")?;
                for name in parts.split(',') {
                    yuv.insert(Part::named(name).ok_or_else(|| format!("no part {name}"))?);
                }
            }
            "--pixel" => {
                let parts = args.next().ok_or("--pixel takes parts")?;
                for name in parts.split(',') {
                    plain.insert(Part::named(name).ok_or_else(|| format!("no part {name}"))?);
                }
            }
            "--one-buffer" => one_buffer = true,
            "--stubborn" => stubborn = true,
            "--shm" => {
                let parts = args.next().ok_or("--shm takes parts")?;
                for name in parts.split(',') {
                    shm.insert(Part::named(name).ok_or_else(|| format!("no part {name}"))?);
                }
            }
            "--still" => still = true,
            "--same" => same = true,
            "--below" => below = true,
            "--crop" => crop = true,
            "--popup" => {
                let seconds: f64 = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or("--popup takes seconds")?;
                period = Duration::from_secs_f64(seconds);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if crop && plain.contains(&Part::Above) {
        return Err(
            "--crop takes the upper subsurface's viewport, which --pixel above needs".into(),
        );
    }
    if crop && yuv.contains(&Part::Above) {
        return Err("--crop draws the upper subsurface red around its crop, not in bars: no --yuv above with it".into());
    }

    let conn = Connection::connect_to_env().map_err(|e| format!("connecting: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("registry: {e}"))?;
    let qh = queue.handle();
    let bound = |name: &'static str| move |e| format!("{name}: {e}");
    let compositor: WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .map_err(bound("wl_compositor"))?;
    let subcompositor: WlSubcompositor = globals
        .bind(&qh, 1..=1, ())
        .map_err(bound("wl_subcompositor"))?;
    let wm_base: XdgWmBase = globals.bind(&qh, 1..=6, ()).map_err(bound("xdg_wm_base"))?;
    let dmabuf: ZwpLinuxDmabufV1 = globals
        .bind(&qh, 3..=5, ())
        .map_err(bound("zwp_linux_dmabuf_v1"))?;
    let viewporter: WpViewporter = globals
        .bind(&qh, 1..=1, ())
        .map_err(bound("wp_viewporter"))?;
    let wl_shm: WlShm = globals.bind(&qh, 1..=1, ()).map_err(bound("wl_shm"))?;
    let single_pixel: Option<WpSinglePixelBufferManagerV1> = match plain.is_empty() {
        true => None,
        false => Some(
            globals
                .bind(&qh, 1..=1, ())
                .map_err(bound("wp_single_pixel_buffer_manager_v1"))?,
        ),
    };

    // The nested session's compositor draws on the one GPU there is.
    let node = std::fs::read_dir("/dev/dri")
        .map_err(|e| format!("/dev/dri: {e}"))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("renderD"))
        })
        .min()
        .ok_or("no render node")?;
    let file = File::options()
        .read(true)
        .write(true)
        .open(&node)
        .map_err(|e| format!("{}: {e}", node.display()))?;
    let gbm = gbm::Device::new(file).map_err(|e| format!("gbm: {e}"))?;

    let surface = compositor.create_surface(&qh, ());
    let toplevel = wm_base.get_xdg_surface(&surface, &qh, ());
    let role = toplevel.get_toplevel(&qh, ());
    role.set_title("hypr-world-surfaces".into());
    role.set_app_id("hypr-world-surfaces".into());

    // The subsurfaces draw on their own (desync). Their places apply with
    // the toplevel's first commit.
    let mut surfaces = HashMap::from([(Part::Toplevel, surface.clone())]);
    let mut subsurface = |part: Part, (x, y): (i32, i32), below: bool| {
        let child = compositor.create_surface(&qh, ());
        let role = subcompositor.get_subsurface(&child, &surface, &qh, ());
        role.set_position(x, y);
        role.set_desync();
        if below {
            role.place_below(&surface);
        }
        surfaces.insert(part, child.clone());
        child
    };
    let above = subsurface(Part::Above, ABOVE_AT, false);
    if below {
        subsurface(Part::Below, BELOW_AT, true);
    }
    if crop {
        let viewport = viewporter.get_viewport(&above, &qh, ());
        let (width, height) = ABOVE;
        viewport.set_source(
            f64::from(width / 2),
            f64::from(height / 2),
            f64::from(width),
            f64::from(height),
        );
        viewport.set_destination(width as i32, height as i32);
    }
    surface.commit();

    let mut state = State {
        gpu: Gpu {
            gbm,
            dmabuf,
            wl_shm,
            shm,
            yuv,
        },
        compositor,
        wm_base,
        toplevel,
        surfaces,
        pools: HashMap::new(),
        popup: None,
        still,
        same,
        crop,
        below,
        one_buffer,
        stubborn,
        plain,
        single_pixel,
        viewporter,
        viewports: HashMap::new(),
        pixels: HashMap::new(),
        mapped: false,
        running: true,
    };

    // The popup opens once the toplevel is up, then opens and closes on the
    // period.
    let mut toggle_at: Option<Instant> = None;
    let mut opened = false;
    while state.running {
        queue
            .dispatch_pending(&mut state)
            .map_err(|e| format!("dispatch: {e}"))?;
        let now = Instant::now();
        if state.mapped && !opened {
            state.open_popup(&qh);
            opened = true;
            toggle_at = (!period.is_zero()).then(|| now + period);
        } else if let Some(at) = toggle_at
            && now >= at
        {
            if state.popup.is_some() {
                state.close_popup();
                // Below with the popup: hidden, as an empty commit unmaps it.
                if let Some(below) = state.surfaces.get(&Part::Below) {
                    below.attach(None, 0, 0);
                    below.commit();
                }
            } else {
                state.open_popup(&qh);
                state.draw(Part::Below, &qh);
            }
            toggle_at = Some(at + period);
        }
        queue.flush().map_err(|e| format!("flush: {e}"))?;

        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let timeout = toggle_at
            .and_then(|at| Timespec::try_from(at.saturating_duration_since(Instant::now())).ok());
        let readable = {
            let fd = guard.connection_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            match poll(&mut fds, timeout.as_ref()) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(format!("poll: {e}")),
            }
            !fds[0].revents().is_empty()
        };
        if readable {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(format!("read: {e}")),
            }
        }
    }
    Ok(())
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let xdg_surface::Event::Configure { serial } = event else {
            return;
        };
        xdg.ack_configure(serial);
        if *xdg == state.toplevel {
            // The first configure maps it: every part's first buffer, the
            // subsurfaces' before the toplevel's commit puts them in place.
            if !state.mapped {
                for part in [Part::Above, Part::Below, Part::Toplevel] {
                    state.draw(part, qh);
                }
                state.mapped = true;
            }
        } else if state.popup.as_ref().is_some_and(|(popup, _)| popup == xdg) {
            state.draw(Part::Popup, qh);
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Its size stays its own, whatever the configure says.
        if let xdg_toplevel::Event::Close = event
            && !state.stubborn
        {
            state.running = false;
        }
    }
}

impl Dispatch<XdgPopup, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgPopup,
        event: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Dismissed by the compositor: opened again on the period.
        if let xdg_popup::Event::PopupDone = event {
            state.close_popup();
        }
    }
}

/// A frame callback, by the part and the surface it was asked for: one for
/// a popup since closed is dropped.
impl Dispatch<WlCallback, (Part, ObjectId)> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        (part, surface): &(Part, ObjectId),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event
            && state
                .surfaces
                .get(part)
                .is_some_and(|now| now.id() == *surface)
        {
            if let Some(pool) = state.pools.get_mut(part)
                && !state.same
            {
                pool.step += 1;
            }
            state.draw(*part, qh);
        }
    }
}

impl Dispatch<WlBuffer, (Part, usize)> for State {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        &(part, index): &(Part, usize),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(pool) = state.pools.get_mut(&part)
            && let Some(slot) = pool.slots.get_mut(index)
        {
            slot.busy = false;
            // The frame that waited for it.
            if std::mem::take(&mut pool.waiting) {
                state.draw(part, qh);
            }
        }
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        event: zwp_linux_buffer_params_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_buffer_params_v1::Event::Failed = event {
            eprintln!("surfaces: the compositor refused a buffer");
            state.running = false;
        }
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: WlSubcompositor);
delegate_noop!(State: WlSubsurface);
delegate_noop!(State: XdgPositioner);
delegate_noop!(State: WpViewporter);
delegate_noop!(State: WpViewport);
delegate_noop!(State: WpSinglePixelBufferManagerV1);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore ZwpLinuxDmabufV1);
