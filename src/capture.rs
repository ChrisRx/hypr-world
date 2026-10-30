//! Pulls Hyprland windows' contents through hyprland-toplevel-export-v1 and
//! hands finished frames to the renderer: one thread and one Wayland
//! connection for any number of windows. A window gets a new frame only when
//! the renderer asks for one, with at most one on its way, so the renderer
//! decides how often each window is copied.
//!
//! Frames come in dmabufs: a few GPU buffers per window, allocated here with
//! GBM on the compositor's own GPU, shared with Hyprland as capture targets
//! and with the renderer as textures. The compositor renders the window
//! straight into one; the renderer copies it GPU to GPU. No CPU copies at
//! all. With no render node or no GBM nothing is captured, and a window the
//! compositor offers no dmabuf for that the renderer can take, or whose
//! buffers cannot be allocated, gets no frames.

use std::{
    collections::HashMap,
    fs::File,
    os::{
        fd::{AsFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, Sender, TryRecvError, channel},
    },
    thread,
    time::{Duration, Instant},
};

use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum,
    backend::WaylandError,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_buffer, wl_registry},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use crate::wake::Wake;

pub mod protocol {
    #![allow(non_upper_case_globals, non_camel_case_types, dead_code)]
    use wayland_client;
    use wayland_client::protocol::*;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols_wlr::foreign_toplevel::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-export-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/hyprland-toplevel-export-v1.xml");
}

use protocol::{
    hyprland_toplevel_export_frame_v1::{self as frame_v1, HyprlandToplevelExportFrameV1},
    hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1,
};

/// GPU buffers each window's frames come in: the one on screen and the one
/// before it, which the renderer keeps (`Kept`), and one for the compositor
/// to write into. A frame the renderer has not shown yet holds the next copy
/// back until it is (`Counters::waited`).
const DMABUF_SLOTS: usize = 3;

/// Failed copies in a row after which a window is left alone until it is
/// asked for again (closed, most likely).
const MAX_FAILURES: u32 = 20;

/// The wait before retrying a failed copy.
const RETRY_DELAY: Duration = Duration::from_millis(250);

/// Pixels copied out of shared memory, top row first: a surface's own
/// buffer the plugin shares (glue.rs `SharedBuffer::Pixels`).
pub struct ShmFrame {
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    /// `width * 4` bytes per row, `height` rows.
    pub pixels: Vec<u8>,
}

/// One frame the compositor rendered into a GPU buffer. Valid to read once
/// it arrives: Hyprland signals ready only after its render completed.
pub struct DmabufFrame {
    pub slot: Arc<DmabufSlot>,
    pub width: u32,
    pub height: u32,
    pub layout: PixelLayout,
    /// Rows bottom to top.
    pub y_invert: bool,
}

impl DmabufFrame {
    /// A `width` by `height` frame in slot `id` onto `fd`, linear BGRA, as
    /// if the thread had copied it, for tests.
    #[cfg(test)]
    pub fn fake(id: u64, width: u32, height: u32, fd: OwnedFd) -> Self {
        let slot = DmabufSlot {
            id,
            width,
            height,
            stride: width * 4,
            offset: 0,
            modifier: u64::from(gbm::Modifier::Linear),
            fd,
        };
        Self {
            slot: Arc::new(slot),
            width,
            height,
            layout: PixelLayout::Bgra,
            y_invert: false,
        }
    }
}

/// A GPU buffer the renderer imports once and samples while it is on screen
/// (import.rs). The buffer object itself stays with the capture thread;
/// this carries a dmabuf fd onto the same memory.
pub struct DmabufSlot {
    /// Unique for the life of the process, for the renderer's import cache.
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    fd: OwnedFd,
}

impl DmabufSlot {
    /// A new fd onto the buffer, for an importer that takes ownership.
    pub fn dup_fd(&self) -> std::io::Result<OwnedFd> {
        self.fd.try_clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixelLayout {
    Bgra,
    Rgba,
}

impl PixelLayout {
    /// The byte order of a DRM fourcc, or None for one the renderer cannot
    /// take as-is.
    fn of_drm(format: gbm::Format) -> Option<Self> {
        crate::import::layout_of(format as u32)
    }

    /// Its texture format, as sRGB, so that sampling decodes its colors.
    pub fn srgb(self) -> wgpu::TextureFormat {
        match self {
            Self::Bgra => wgpu::TextureFormat::Bgra8UnormSrgb,
            Self::Rgba => wgpu::TextureFormat::Rgba8UnormSrgb,
        }
    }
}

/// The export handle of the window at `address`, as `hyprctl clients`
/// prints it: its low 32 bits.
pub fn export_handle(address: u64) -> u32 {
    (address & 0xffff_ffff) as u32
}

/// A Wayland connection to the compositor at `display`, a socket's path, else
/// to $WAYLAND_DISPLAY's: a backend's compositor is not the one the app's
/// window is on (glue.rs `Backend`).
pub fn connect(display: Option<&Path>) -> Result<Connection, String> {
    let Some(path) = display else {
        return Connection::connect_to_env().map_err(|e| format!("wayland connect: {e}"));
    };
    let stream = UnixStream::connect(path)
        .map_err(|e| format!("wayland connect {}: {e}", path.display()))?;
    Connection::from_socket(stream).map_err(|e| format!("wayland connect {}: {e}", path.display()))
}

/// The renderer's end of the capture thread.
pub struct Capture {
    requests: Sender<Request>,
    /// Wakes the thread from its poll when a request is queued.
    wake: Arc<OwnedFd>,
    frames: Mutex<Receiver<(u32, DmabufFrame)>>,
    counters: Arc<Counters>,
}

enum Request {
    /// A frame now, wanted or not: a window's first.
    Frame(u32),
    /// Copies whenever the window draws, at most this often; none when
    /// `None`.
    Want(u32, Option<Duration>),
    /// These windows drew something.
    Damaged(Vec<u32>),
    Forget(u32),
    /// The renderer is done with this slot of a window (`DmabufSlot::id`).
    Release(u32, u64),
}

/// What the capture thread did, as running totals, for the stats.
#[derive(Default)]
pub struct Counters {
    started: AtomicU64,
    damaged_wanted: AtomicU64,
    damaged_unwanted: AtomicU64,
    copied: AtomicU64,
    copy_micros: AtomicU64,
    waited: AtomicU64,
    /// The longest copy since `take_slowest`, in microseconds.
    slowest_micros: AtomicU64,
}

impl Counters {
    pub fn totals(&self) -> Totals {
        let read = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Totals {
            started: read(&self.started),
            damaged_wanted: read(&self.damaged_wanted),
            damaged_unwanted: read(&self.damaged_unwanted),
            copied: read(&self.copied),
            copy_micros: read(&self.copy_micros),
            waited: read(&self.waited),
        }
    }

    /// The longest copy since the last call, in microseconds.
    pub fn take_slowest(&self) -> u64 {
        self.slowest_micros.swap(0, Ordering::Relaxed)
    }
}

/// A reading of the `Counters`, or what changed between two.
#[derive(Clone, Copy, Default)]
pub struct Totals {
    /// Copies started.
    pub started: u64,
    /// Windows that drew something while they were wanted, and while not.
    pub damaged_wanted: u64,
    pub damaged_unwanted: u64,
    /// Copies that came back, and the time they took all told, from start
    /// to `ready`.
    pub copied: u64,
    pub copy_micros: u64,
    /// Copies that waited for a slot: all of the window's were with the
    /// renderer, or being written into.
    pub waited: u64,
}

impl Totals {
    /// What happened from `earlier` until these.
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            started: self.started - earlier.started,
            damaged_wanted: self.damaged_wanted - earlier.damaged_wanted,
            damaged_unwanted: self.damaged_unwanted - earlier.damaged_unwanted,
            copied: self.copied - earlier.copied,
            copy_micros: self.copy_micros - earlier.copy_micros,
            waited: self.waited - earlier.waited,
        }
    }
}

/// Where the glue thread hands the plugin's damage, straight to the capture
/// thread: the app need not wake to pass it on.
#[derive(Clone)]
pub struct Damage {
    requests: Sender<Request>,
    wake: Arc<OwnedFd>,
}

impl Damage {
    /// The windows at `addresses` drew something.
    pub fn report(&self, addresses: &[u64]) {
        let handles = addresses
            .iter()
            .map(|&address| export_handle(address))
            .collect();
        send(&self.requests, &self.wake, Request::Damaged(handles));
    }
}

/// Queues a request and wakes the thread from its poll.
fn send(requests: &Sender<Request>, wake: &OwnedFd, request: Request) {
    if requests.send(request).is_ok() {
        let _ = rustix::io::write(wake, &1u64.to_ne_bytes());
    }
}

impl Capture {
    /// Starts the capture thread, on the compositor at `display` (a Wayland
    /// socket's path), else $WAYLAND_DISPLAY's. If it cannot run (no
    /// Wayland, not Hyprland, no dmabufs) it says why, and no frames ever
    /// come.
    pub fn start(app: Wake, display: Option<PathBuf>) -> Self {
        let (requests, asked) = channel();
        let (sender, frames) = channel();
        let wake =
            Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).expect("eventfd"));
        let counters = Arc::new(Counters::default());

        let (thread_wake, thread_counters) = (wake.clone(), counters.clone());
        thread::spawn(move || {
            if let Err(e) = run(asked, &thread_wake, sender, app, thread_counters, display) {
                eprintln!("hypr-world: capture stopped: {e}");
            }
        });

        Self {
            requests,
            wake,
            frames: Mutex::new(frames),
            counters,
        }
    }

    /// With no thread, for tests: what is asked goes nowhere, and no frames
    /// come.
    #[cfg(test)]
    pub fn idle() -> Self {
        let (requests, _) = channel();
        let (_, frames) = channel();
        Self {
            requests,
            wake: Arc::new(
                eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).expect("eventfd"),
            ),
            frames: Mutex::new(frames),
            counters: Arc::default(),
        }
    }

    /// As `idle`, with a way to see the handles it has been told to forget
    /// since last looked.
    #[cfg(test)]
    pub fn forgetful() -> (Self, impl Fn() -> Vec<u32>) {
        let (requests, asked) = channel();
        let forgotten = move || {
            asked
                .try_iter()
                .filter_map(|request| match request {
                    Request::Forget(handle) => Some(handle),
                    _ => None,
                })
                .collect()
        };
        (
            Self {
                requests,
                ..Self::idle()
            },
            forgotten,
        )
    }

    /// As `idle`, with a way to hand it frames as if the thread had copied
    /// them.
    #[cfg(test)]
    pub fn fed() -> (Self, Sender<(u32, DmabufFrame)>) {
        let (copied, frames) = channel();
        (
            Self {
                frames: Mutex::new(frames),
                ..Self::idle()
            },
            copied,
        )
    }

    /// Asks for the next frame of the window with export handle `handle`
    /// (`export_handle`), wanted or not: a window's first. Asked for while
    /// one is on its way, it is started once that one is done.
    pub fn request(&self, handle: u32) {
        self.send(Request::Frame(handle));
    }

    /// Copies of `handle` whenever it draws something (`Damage`), at most
    /// one per `every`, or none. The thread starts each copy itself.
    pub fn want(&self, handle: u32, every: Option<Duration>) {
        self.send(Request::Want(handle, every));
    }

    /// Done with a window: its copy in flight and its buffers go.
    pub fn forget(&self, handle: u32) {
        self.send(Request::Forget(handle));
    }

    /// The renderer is done with `handle`'s slot `id` (`Kept`): the
    /// compositor may write into it again.
    pub fn release(&self, handle: u32, id: u64) {
        self.send(Request::Release(handle, id));
    }

    /// For the glue thread, which hears which windows drew something.
    pub fn damage(&self) -> Damage {
        Damage {
            requests: self.requests.clone(),
            wake: self.wake.clone(),
        }
    }

    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// The frames that came in since the last call, oldest first, each with
    /// its window's handle.
    pub fn take(&self) -> Vec<(u32, DmabufFrame)> {
        self.frames
            .lock()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default()
    }

    /// Waits for the next frame, for --probe.
    pub fn wait(&self) -> Option<(u32, DmabufFrame)> {
        self.frames.lock().ok()?.recv().ok()
    }

    fn send(&self, request: Request) {
        send(&self.requests, &self.wake, request);
    }
}

/// The compositor's dmabuf offer (the `linux_dmabuf` event).
#[derive(Clone, Copy, PartialEq, Eq)]
struct DmabufOffer {
    format: gbm::Format,
    width: u32,
    height: u32,
}

/// One GPU buffer: the buffer object, the compositor's handle on it, and
/// the renderer's.
struct GpuBuffer {
    _bo: gbm::BufferObject<()>,
    buffer: wl_buffer::WlBuffer,
    slot: Arc<DmabufSlot>,
}

impl Drop for GpuBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

/// The dmabuf transport: GBM on the compositor's render node.
struct Gpu {
    linux_dmabuf: ZwpLinuxDmabufV1,
    gbm: gbm::Device<File>,
}

static NEXT_SLOT_ID: AtomicU64 = AtomicU64::new(1);

/// A linear, renderable buffer for `offer`. Asking for the LINEAR modifier
/// explicitly is what makes GBM report it; drivers without modifier support
/// fall back to the LINEAR usage flag.
fn allocate_linear(
    gbm: &gbm::Device<File>,
    offer: DmabufOffer,
) -> Result<gbm::BufferObject<()>, String> {
    gbm.create_buffer_object_with_modifiers2::<()>(
        offer.width,
        offer.height,
        offer.format,
        std::iter::once(gbm::Modifier::Linear),
        gbm::BufferObjectFlags::RENDERING,
    )
    .or_else(|_| {
        gbm.create_buffer_object::<()>(
            offer.width,
            offer.height,
            offer.format,
            gbm::BufferObjectFlags::RENDERING | gbm::BufferObjectFlags::LINEAR,
        )
    })
    .map_err(|e| format!("gbm allocation: {e}"))
}

/// One window being captured.
#[derive(Default)]
struct Window {
    /// The copy in flight.
    frame: Option<HyprlandToplevelExportFrameV1>,
    /// Asked for again while a copy was in flight.
    again: bool,
    // The compositor's description of the copy in flight.
    dmabuf_offer: Option<DmabufOffer>,
    /// The slot the copy in flight goes into.
    target: Option<usize>,
    y_invert: bool,
    /// The GPU buffers its frames come in, all made for `offer`, and which
    /// of them the compositor may write into next.
    offer: Option<DmabufOffer>,
    buffers: Vec<GpuBuffer>,
    held: Held,
    /// A copy is to start once a slot comes back.
    waiting: bool,
    /// The compositor offered no dmabuf the renderer can take, or its
    /// buffers could not be allocated: said once, and no copy starts again.
    unusable: bool,
    failures: u32,
    retry_at: Option<Instant>,
    /// Copies whenever it draws, at most this often; none when `None`.
    wanted: Option<Duration>,
    /// Drew something since its last copy started.
    dirty: bool,
    /// When its last copy started, for its rate.
    started_at: Option<Instant>,
    /// The app is done with it, and it goes once the copy on its way is
    /// answered (`State::forget`).
    forgotten: bool,
}

impl Window {
    /// When the next copy may start: wanted, drew something, none on its way
    /// and no failure being waited out, it can be captured at all, and its
    /// rate allows it then.
    fn due_at(&self) -> Option<Instant> {
        let every = self.wanted?;
        if !self.dirty
            || self.frame.is_some()
            || self.retry_at.is_some()
            || self.unusable
            || !self.held.any_free()
        {
            return None;
        }
        Some(self.started_at.map_or_else(Instant::now, |at| at + every))
    }

    /// Allocates the rotation for `offer`, unless it already matches.
    fn ensure(
        &mut self,
        gpu: &Gpu,
        offer: DmabufOffer,
        qh: &QueueHandle<State>,
    ) -> Result<(), String> {
        if self.offer == Some(offer) && self.buffers.len() == DMABUF_SLOTS {
            return Ok(());
        }

        self.buffers.clear();
        self.held = Held::default();
        self.offer = None;

        for _ in 0..DMABUF_SLOTS {
            // Linear: the one layout both the compositor's GL and Vulkan are
            // sure to agree on without negotiating modifiers.
            let bo = allocate_linear(&gpu.gbm, offer)?;

            let fd = bo.fd().map_err(|e| format!("gbm export: {e}"))?;
            let stride = bo.stride();
            let offset = bo.offset(0);

            // The buffer is linear either way, and Vulkan can only import it
            // with an explicit modifier. A plain gbm_bo_create reports
            // DRM_FORMAT_MOD_INVALID ("implicit layout") even for a linear
            // buffer, and the import rejects that.
            let modifier = match bo.modifier() {
                gbm::Modifier::Invalid => u64::from(gbm::Modifier::Linear),
                reported => u64::from(reported),
            };
            if modifier != u64::from(gbm::Modifier::Linear) {
                return Err(format!(
                    "gbm gave a non-linear buffer (modifier {modifier:#x})"
                ));
            }

            let params = gpu.linux_dmabuf.create_params(qh, ());
            params.add(
                fd.as_fd(),
                0,
                offset,
                stride,
                (modifier >> 32) as u32,
                (modifier & 0xffff_ffff) as u32,
            );
            let buffer = params.create_immed(
                offer.width as i32,
                offer.height as i32,
                offer.format as u32,
                zwp_linux_buffer_params_v1::Flags::empty(),
                qh,
                (),
            );
            params.destroy();

            let slot = Arc::new(DmabufSlot {
                id: NEXT_SLOT_ID.fetch_add(1, Ordering::Relaxed),
                width: offer.width,
                height: offer.height,
                stride,
                offset,
                modifier,
                fd,
            });

            self.buffers.push(GpuBuffer {
                _bo: bo,
                buffer,
                slot,
            });
        }

        self.held = Held::new(self.buffers.len());
        self.offer = Some(offer);
        Ok(())
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.destroy();
        }
    }
}

/// Which of a window's GPU buffers the compositor may write into next: not
/// one it is writing into, nor one the renderer has until it gives it back
/// (`Capture::release`), so the frame on screen is never written over.
#[derive(Default)]
struct Held(Vec<bool>);

impl Held {
    fn new(slots: usize) -> Self {
        Self(vec![false; slots])
    }

    /// True too while there are no buffers yet: the first copy makes them.
    fn any_free(&self) -> bool {
        self.0.is_empty() || self.0.contains(&false)
    }

    /// A slot to write into, held from now on.
    fn take(&mut self) -> Option<usize> {
        let i = self.0.iter().position(|&held| !held)?;
        self.0[i] = true;
        Some(i)
    }

    fn give_back(&mut self, i: usize) {
        if let Some(held) = self.0.get_mut(i) {
            *held = false;
        }
    }
}

/// The renderer's half: of the slots a window's frames came in, it keeps the
/// one on screen and the one before it, which frames already on their way to
/// the GPU may still sample, and gives back the rest (windows.rs
/// `show_slot`).
#[derive(Default)]
pub struct Kept {
    shown: Option<u64>,
    before: Option<u64>,
}

impl Kept {
    /// Slot `id` is on screen now: the one to give back, if any.
    pub fn show(&mut self, id: u64) -> Option<u64> {
        if self.shown == Some(id) {
            return None;
        }
        let back = self.before.take();
        self.before = self.shown.replace(id);
        back
    }
}

struct State {
    manager: HyprlandToplevelExportManagerV1,
    /// Opened once at startup (`open_gpu`): there is no capture without it.
    gpu: Option<Gpu>,
    /// By export handle.
    windows: HashMap<u32, Window>,
    frames: Sender<(u32, DmabufFrame)>,
    /// Wakes the app for each frame handed over.
    app: Wake,
    counters: Arc<Counters>,
    /// The renderer went away: time to stop.
    hung_up: bool,
    // dmabuf feedback, read once at startup.
    main_device: Option<u64>,
    feedback_done: bool,
}

fn run(
    asked: Receiver<Request>,
    wake: &OwnedFd,
    frames: Sender<(u32, DmabufFrame)>,
    app: Wake,
    counters: Arc<Counters>,
    display: Option<PathBuf>,
) -> Result<(), String> {
    let conn = connect(display.as_deref())?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&conn).map_err(|e| format!("wayland registry: {e}"))?;
    let qh = queue.handle();

    let manager: HyprlandToplevelExportManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| format!("hyprland_toplevel_export_manager_v1 (not Hyprland?): {e}"))?;

    let mut state = State {
        manager,
        gpu: None,
        windows: HashMap::new(),
        frames,
        app,
        counters,
        hung_up: false,
        main_device: None,
        feedback_done: false,
    };

    let gpu = open_gpu(&globals, &qh, &mut queue, &mut state)
        .map_err(|e| format!("no dmabuf capture ({e}), so no capture at all"))?;
    state.gpu = Some(gpu);

    loop {
        // What the renderer asked for since the last round.
        let mut count = [0u8; 8];
        let _ = rustix::io::read(wake, &mut count);
        loop {
            match asked.try_recv() {
                Ok(Request::Frame(handle)) => state.ask(handle, &qh),
                // A window's entry comes with its first frame, asked for
                // before anything else.
                Ok(Request::Want(handle, every)) => {
                    if let Some(window) = state.windows.get_mut(&handle) {
                        window.wanted = every;
                    }
                }
                Ok(Request::Damaged(handles)) => state.damaged(&handles),
                Ok(Request::Forget(handle)) => state.forget(handle),
                Ok(Request::Release(handle, id)) => state.release(handle, id, &qh),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // Failed copies whose wait is over.
        let now = Instant::now();
        let due: Vec<u32> = state
            .windows
            .iter()
            .filter(|(_, w)| w.retry_at.is_some_and(|at| at <= now))
            .map(|(&handle, _)| handle)
            .collect();
        for handle in due {
            if let Some(window) = state.windows.get_mut(&handle) {
                window.retry_at = None;
            }
            state.start(handle, &qh);
        }

        // Windows that drew something and whose rate allows a copy now.
        let due: Vec<u32> = state
            .windows
            .iter()
            .filter(|(_, w)| w.due_at().is_some_and(|at| at <= now))
            .map(|(&handle, _)| handle)
            .collect();
        for handle in due {
            state.start(handle, &qh);
        }

        queue
            .dispatch_pending(&mut state)
            .map_err(|e| format!("wayland dispatch: {e}"))?;
        if state.hung_up {
            return Ok(());
        }
        queue.flush().map_err(|e| format!("wayland flush: {e}"))?;

        // Sleep until the compositor or the renderer has something for us,
        // or the next retry or held-back copy is due.
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let timeout = state
            .windows
            .values()
            .filter_map(|w| w.retry_at.into_iter().chain(w.due_at()).min())
            .min()
            .and_then(|at| Timespec::try_from(at.saturating_duration_since(Instant::now())).ok());

        let readable = {
            let fd = guard.connection_fd();
            let mut fds = [
                PollFd::new(&fd, PollFlags::IN),
                PollFd::new(wake, PollFlags::IN),
            ];
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
                Err(e) => return Err(format!("wayland read: {e}")),
            }
        }
    }
}

/// Binds linux-dmabuf, asks its default feedback for the compositor's main
/// device, and opens GBM on that device's render node.
fn open_gpu(
    globals: &wayland_client::globals::GlobalList,
    qh: &QueueHandle<State>,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
) -> Result<Gpu, String> {
    // 4 and 5 have the feedback object with its main_device event.
    let linux_dmabuf: ZwpLinuxDmabufV1 = globals
        .bind(qh, 4..=5, ())
        .map_err(|e| format!("zwp_linux_dmabuf_v1 v4: {e}"))?;

    let feedback = linux_dmabuf.get_default_feedback(qh, ());
    while !state.feedback_done {
        queue
            .blocking_dispatch(state)
            .map_err(|e| format!("wayland dispatch: {e}"))?;
    }
    feedback.destroy();

    let device = state.main_device.ok_or("compositor named no main device")?;
    let path =
        render_node(device).ok_or_else(|| format!("no render node for device {device:#x}"))?;

    let file = File::options()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("opening {path}: {e}"))?;
    let gbm = gbm::Device::new(file).map_err(|e| format!("gbm on {path}: {e}"))?;

    eprintln!("hypr-world: capturing through dmabufs on {path}");

    Ok(Gpu { linux_dmabuf, gbm })
}

/// The /dev/dri render node whose device number is `device`.
fn render_node(device: u64) -> Option<String> {
    std::fs::read_dir("/dev/dri")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("renderD"))
        })
        .find(|p| rustix::fs::stat(p).is_ok_and(|s| s.st_rdev == device))
        .map(|p| p.to_string_lossy().into_owned())
}

impl State {
    /// The renderer asked for the next frame of `handle`.
    fn ask(&mut self, handle: u32, qh: &QueueHandle<Self>) {
        let window = self.windows.entry(handle).or_default();
        // Wanted again before it went.
        window.forgotten = false;
        if window.frame.is_some() {
            window.again = true;
            return;
        }
        // Already waiting out a failure.
        if window.retry_at.is_some() {
            return;
        }
        // One given up on gets another go when it is asked for again.
        window.failures = window.failures.min(MAX_FAILURES - 1);
        self.start(handle, qh);
    }

    /// The app is done with `handle`: its state goes, buffers and all. A
    /// copy on its way keeps it until the compositor answers it (`finish`):
    /// Hyprland takes a frame destroyed before its first event for a
    /// protocol error, and ends the whole connection.
    fn forget(&mut self, handle: u32) {
        match self.windows.get_mut(&handle) {
            Some(window) if window.frame.is_some() => {
                window.forgotten = true;
                window.wanted = None;
                window.again = false;
            }
            _ => {
                self.windows.remove(&handle);
            }
        }
    }

    /// The glue thread says these windows drew something: copied when their
    /// rate allows, if they are wanted (the run loop).
    fn damaged(&mut self, handles: &[u32]) {
        for handle in handles {
            let wanted = self.windows.get_mut(handle).is_some_and(|window| {
                window.dirty = true;
                window.wanted.is_some()
            });
            let counter = if wanted {
                &self.counters.damaged_wanted
            } else {
                &self.counters.damaged_unwanted
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The renderer gave back `handle`'s slot `id`: the compositor may write
    /// into it again, and a copy waiting for a slot starts.
    fn release(&mut self, handle: u32, id: u64, qh: &QueueHandle<Self>) {
        let Some(window) = self.windows.get_mut(&handle) else {
            return;
        };
        // Not one of its slots any more: they were made again for a new size.
        if let Some(i) = window.buffers.iter().position(|b| b.slot.id == id) {
            window.held.give_back(i);
        }
        if std::mem::take(&mut window.waiting) {
            self.start(handle, qh);
        }
    }

    /// Asks the compositor for a copy of `handle`. It takes in everything the
    /// window drew until now.
    fn start(&mut self, handle: u32, qh: &QueueHandle<Self>) {
        let Some(window) = self.windows.get_mut(&handle) else {
            return;
        };
        // Said once (`copy`), and nothing to copy into.
        if window.unusable {
            return;
        }
        // Every slot on screen, about to be, or being written into: once one
        // comes back (`release`).
        if !window.held.any_free() {
            if !window.waiting {
                self.counters.waited.fetch_add(1, Ordering::Relaxed);
            }
            window.waiting = true;
            return;
        }
        window.dirty = false;
        window.started_at = Some(Instant::now());
        self.counters.started.fetch_add(1, Ordering::Relaxed);
        window.dmabuf_offer = None;
        window.target = None;
        window.y_invert = false;
        window.frame = Some(self.manager.capture_toplevel(0, handle, qh, handle));
    }

    /// Picks a GPU slot for the copy the compositor just described and asks
    /// for it. False when there is none: the compositor offered no dmabuf
    /// the renderer can take, or the window's buffers could not be
    /// allocated, which is said once and ends its copies (`unusable`).
    fn copy(
        &mut self,
        handle: u32,
        frame: &HyprlandToplevelExportFrameV1,
        qh: &QueueHandle<Self>,
    ) -> bool {
        let Self { gpu, windows, .. } = self;
        let (Some(gpu), Some(window)) = (gpu.as_ref(), windows.get_mut(&handle)) else {
            return false;
        };
        // Answered now, so it can go (`finish`).
        if window.forgotten {
            return false;
        }

        let ready = window
            .dmabuf_offer
            .filter(|offer| PixelLayout::of_drm(offer.format).is_some())
            .ok_or_else(|| "the compositor offered no dmabuf the renderer can take".to_string())
            .and_then(|offer| window.ensure(gpu, offer, qh));
        if let Err(e) = ready {
            eprintln!("hypr-world: no capture of window {handle:#x}: {e}");
            window.unusable = true;
            return false;
        }

        // `start` saw one free, and only this copy takes one.
        let Some(i) = window.held.take() else {
            return false;
        };
        // Ignoring damage: Hyprland redraws the window's monitor for the copy
        // at once, rather than the copy waiting for that monitor's next
        // frame, which comes only when something on it changes.
        frame.copy(&window.buffers[i].buffer, 1);
        window.target = Some(i);
        true
    }

    /// The copy of `handle` in flight ended: hands the frame to the renderer
    /// when it worked, schedules a retry when it did not.
    fn finish(&mut self, handle: u32, ready: bool, qh: &QueueHandle<Self>) {
        let Self {
            windows,
            frames,
            hung_up,
            app,
            counters,
            ..
        } = self;
        let Some(window) = windows.get_mut(&handle) else {
            return;
        };
        if let Some(frame) = window.frame.take() {
            frame.destroy();
        }
        if window.forgotten {
            windows.remove(&handle);
            return;
        }

        if !ready {
            if let Some(i) = window.target {
                window.held.give_back(i);
            }
            // Not a failure to retry: no copy can work (`copy`).
            if window.unusable {
                window.again = false;
                return;
            }
            window.failures += 1;
            if window.failures >= MAX_FAILURES {
                // Said once: a window given up on that keeps drawing gets a
                // go at its rate, and each of those may fail too.
                if window.failures == MAX_FAILURES {
                    eprintln!(
                        "hypr-world: capture of window {handle:#x} keeps failing; is it still open?"
                    );
                }
                window.again = false;
            } else {
                window.retry_at = Some(Instant::now() + RETRY_DELAY);
            }
            return;
        }
        window.failures = 0;
        if let Some(started) = window.started_at {
            let micros = started.elapsed().as_micros() as u64;
            counters.copied.fetch_add(1, Ordering::Relaxed);
            counters.copy_micros.fetch_add(micros, Ordering::Relaxed);
            counters.slowest_micros.fetch_max(micros, Ordering::Relaxed);
        }

        let Some(i) = window.target else {
            return;
        };
        let offer = window.offer.expect("gpu target without offer");
        let out = DmabufFrame {
            slot: window.buffers[i].slot.clone(),
            width: offer.width,
            height: offer.height,
            layout: PixelLayout::of_drm(offer.format).expect("checked at offer"),
            y_invert: window.y_invert,
        };
        if frames.send((handle, out)).is_err() {
            *hung_up = true;
            return;
        }
        app.wake();

        if std::mem::take(&mut window.again) {
            self.start(handle, qh);
        }
    }
}

impl Dispatch<HyprlandToplevelExportFrameV1, u32> for State {
    fn event(
        state: &mut Self,
        frame: &HyprlandToplevelExportFrameV1,
        event: frame_v1::Event,
        handle: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let handle = *handle;
        let Some(window) = state.windows.get_mut(&handle) else {
            return;
        };

        match event {
            frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                if window.dmabuf_offer.is_none()
                    && let Ok(format) = gbm::Format::try_from(format)
                {
                    window.dmabuf_offer = Some(DmabufOffer {
                        format,
                        width,
                        height,
                    });
                }
            }
            frame_v1::Event::BufferDone => {
                if !state.copy(handle, frame, qh) {
                    state.finish(handle, false, qh);
                }
            }
            frame_v1::Event::Flags { flags } => {
                window.y_invert =
                    matches!(flags, WEnum::Value(f) if f.contains(frame_v1::Flags::YInvert));
            }
            frame_v1::Event::Ready { .. } => state.finish(handle, true, qh),
            frame_v1::Event::Failed => state.finish(handle, false, qh),
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_dmabuf_feedback_v1::Event::MainDevice { device } => {
                // A dev_t in host byte order.
                if let Ok(bytes) = <[u8; 8]>::try_from(device.as_slice()) {
                    state.main_device = Some(u64::from_ne_bytes(bytes));
                }
            }
            zwp_linux_dmabuf_feedback_v1::Event::Done => state.feedback_done = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! ignore_events {
    ($($proxy:ty),*) => {$(
        impl Dispatch<$proxy, ()> for State {
            fn event(_: &mut Self, _: &$proxy, _: <$proxy as wayland_client::Proxy>::Event,
                     _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}

ignore_events!(
    wl_buffer::WlBuffer,
    ZwpLinuxDmabufV1,
    ZwpLinuxBufferParamsV1,
    HyprlandToplevelExportManagerV1
);

#[cfg(test)]
mod tests {
    use super::*;

    /// When a window's next copy may start, by its rate: the rule the
    /// capture thread copies by, on its own.
    #[test]
    fn a_copy_is_due_when_wanted_drawn_and_its_rate_allows() {
        let mut window = Window::default();
        window.dirty = true;
        assert_eq!(window.due_at(), None, "not wanted: never");

        window.wanted = Some(Duration::from_secs(1));
        let now = Instant::now();
        assert!(
            window.due_at().is_some_and(|at| at >= now),
            "never copied: now"
        );

        let started = Instant::now();
        window.started_at = Some(started);
        assert_eq!(
            window.due_at(),
            Some(started + Duration::from_secs(1)),
            "a second after the last"
        );

        window.dirty = false;
        assert_eq!(window.due_at(), None, "drew nothing since: never");

        window.dirty = true;
        window.retry_at = Some(started);
        assert_eq!(
            window.due_at(),
            None,
            "waiting out a failure: not by its rate"
        );
    }

    #[test]
    fn the_export_handle_is_the_low_half_of_the_address() {
        assert_eq!(export_handle(0x600e_cbc4_4de0), 0xcbc4_4de0);
    }

    /// The compositor never writes into a slot the renderer still has: the
    /// one on screen, the one before it, which frames on their way to the
    /// GPU may sample, or a frame it has not shown yet. The renderer is late
    /// now and then, as an update can be, and copies still come.
    #[test]
    fn a_slot_on_screen_is_never_written_over() {
        const IDS: [u64; DMABUF_SLOTS] = [7, 8, 9];
        let mut held = Held::new(DMABUF_SLOTS);
        let mut kept = Kept::default();
        // Frames handed to the renderer and not shown yet, oldest first. A
        // window's first ones come before the renderer's first update: the
        // only time it skips any, with two slots kept.
        let mut handed: Vec<usize> = (0..DMABUF_SLOTS)
            .map(|_| held.take().expect("a free slot"))
            .collect();
        let (mut copies, mut waits) = (DMABUF_SLOTS, 0);
        let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
        for step in 0..10_000 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            if !rng.is_multiple_of(3) {
                // A copy, done: into a free slot, or none free to wait for.
                let Some(i) = held.take() else {
                    waits += 1;
                    continue;
                };
                let renderer_has =
                    handed.contains(&i) || [kept.shown, kept.before].contains(&Some(IDS[i]));
                assert!(
                    !renderer_has,
                    "step {step}: slot {i} written over while the renderer has it"
                );
                handed.push(i);
                copies += 1;
            } else if let Some(last) = handed.pop() {
                // The renderer's update: the last frame shown, and those it
                // skipped given back at once (windows.rs `apply_frames`).
                for skipped in handed.drain(..) {
                    held.give_back(skipped);
                }
                if let Some(back) = kept.show(IDS[last]) {
                    held.give_back(
                        IDS.iter()
                            .position(|&id| id == back)
                            .expect("one of its slots"),
                    );
                }
            }
        }
        // Stuck, it would have made three.
        assert!(
            copies > 1_000 && waits > 0,
            "{copies} copies, {waits} waits"
        );
    }
}
