//! Waking the app from the glue and capture threads. The app only updates
//! when something happens: the plugin said something, a window's frame came
//! in, or something on screen is still moving. The first two happen off the
//! main thread, and this is how they set the event loop going again. It also
//! tells whether the app is answering, for the plugin's watchdog (glue.rs),
//! and wakes it at a time a system asks for (`WakeAt`).

use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use bevy::{
    prelude::*,
    winit::{EventLoopProxyWrapper, UpdateMode, WinitSettings, WinitUserEvent},
};

/// With nothing happening the app never wakes on a timer: a wait this long
/// overflows, and Bevy then waits for an event with no deadline. The glue
/// thread feeds the plugin's watchdog by itself.
const IDLE: Duration = Duration::MAX;

/// An update only when something happens: the plugin said something, a
/// window's frame came in, or something is still moving and asks for the
/// next. Device events (relative mouse motion) count too, for looking around
/// without the plugin. The `Wake` itself comes from main.rs, which hands it
/// to the threads.
pub fn plugin(app: &mut App) {
    app.insert_resource(WinitSettings {
        focused_mode: UpdateMode::reactive(IDLE),
        unfocused_mode: UpdateMode::reactive(IDLE),
    })
    .init_resource::<WakeAt>()
    .add_systems(Startup, wire_wake)
    // After every system that may ask for a wake.
    .add_systems(Last, schedule_wake);
}

/// Lets the glue and capture threads wake the event loop.
fn wire_wake(wake: Res<Wake>, proxy: Res<EventLoopProxyWrapper>) {
    let proxy = (**proxy).clone();
    wake.set(move || {
        let _ = proxy.send_event(WinitUserEvent::WakeUp);
    });
}

/// An update asked for at a time, by a system that has something due then
/// and nothing else that would wake the app for it: a copy held back for
/// its window's rate, a dwell running out. The earliest of them becomes the
/// event loop's wait (`schedule_wake`).
#[derive(Resource, Default)]
pub struct WakeAt {
    /// The earliest asked for this update, in seconds on `Time<Real>`.
    earliest: Option<f32>,
    /// The one the event loop was given.
    scheduled: Option<f32>,
}

impl WakeAt {
    /// Asks for an update at `seconds` on `Time<Real>`.
    pub fn at(&mut self, seconds: f32) {
        self.earliest = Some(
            self.earliest
                .map_or(seconds, |earliest| earliest.min(seconds)),
        );
    }
}

/// At the end of each update, makes the earliest wake asked for in it the
/// event loop's wait, or no wait at all. Bevy forces an update of its own
/// whenever the update mode changes, so the mode is set only when the time
/// asked for moved: written every update, it would never sleep.
pub fn schedule_wake(
    time: Res<Time<Real>>,
    wake_at: ResMut<WakeAt>,
    mut winit: ResMut<WinitSettings>,
) {
    let wake_at = wake_at.into_inner();
    let earliest = wake_at.earliest.take();
    if earliest == wake_at.scheduled {
        return;
    }
    wake_at.scheduled = earliest;

    let wait = earliest.map_or(IDLE, |at| {
        Duration::from_secs_f32((at - time.elapsed_secs()).max(0.0))
    });
    winit.focused_mode = UpdateMode::reactive(wait);
    winit.unfocused_mode = UpdateMode::reactive(wait);
}

#[derive(Resource, Clone, Default)]
pub struct Wake(Arc<Inner>);

struct Inner {
    poke: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// Since when a wake has waited for the app to look, in milliseconds
    /// after `start`, plus one so that 0 can mean none waits.
    waiting_since: AtomicU64,
    start: Instant,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            poke: OnceLock::new(),
            waiting_since: AtomicU64::new(0),
            start: Instant::now(),
        }
    }
}

impl Wake {
    /// How to poke the event loop, once there is one.
    pub fn set(&self, poke: impl Fn() + Send + Sync + 'static) {
        let _ = self.0.poke.set(Box::new(poke));
    }

    /// From another thread: there is something for the app to look at.
    /// A burst of these is one poke, until the app looks.
    pub fn wake(&self) {
        let now = self.now();
        if self
            .0
            .waiting_since
            .compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && let Some(poke) = self.0.poke.get()
        {
            poke();
        }
    }

    /// From the app, before it looks.
    pub fn seen(&self) {
        self.0.waiting_since.store(0, Ordering::Release);
    }

    /// A wake has waited longer than `limit` for the app to look: the app is
    /// stuck, or still starting.
    pub fn stuck(&self, limit: Duration) -> bool {
        let since = self.0.waiting_since.load(Ordering::Acquire);
        since != 0 && self.now().saturating_sub(since) > limit.as_millis() as u64
    }

    fn now(&self) -> u64 {
        self.0.start.elapsed().as_millis() as u64 + 1
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::AtomicU32, thread};

    use super::*;

    #[test]
    fn pokes_once_until_the_app_looks() {
        let wake = Wake::default();
        let pokes = Arc::new(AtomicU32::new(0));
        let counted = pokes.clone();

        // Before the event loop exists nothing pokes, and the app's first
        // look clears it.
        wake.wake();
        wake.seen();

        wake.set(move || {
            counted.fetch_add(1, Ordering::Relaxed);
        });
        wake.wake();
        wake.wake();
        assert_eq!(
            pokes.load(Ordering::Relaxed),
            1,
            "a burst of events is one wake-up"
        );

        wake.seen();
        wake.wake();
        assert_eq!(
            pokes.load(Ordering::Relaxed),
            2,
            "after a look, the next event wakes it again"
        );
    }

    /// The earliest time asked for becomes the wait, and the mode is left
    /// alone while it does not move.
    #[test]
    fn the_earliest_wake_becomes_the_wait() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<WakeAt>()
            .insert_resource(WinitSettings {
                focused_mode: UpdateMode::reactive(Duration::MAX),
                ..default()
            })
            .add_systems(Last, schedule_wake);
        let wait = |app: &App| match app.world().resource::<WinitSettings>().focused_mode {
            UpdateMode::Reactive { wait, .. } => wait,
            UpdateMode::Continuous => Duration::ZERO,
        };

        app.world_mut().resource_mut::<WakeAt>().at(60.0);
        app.world_mut().resource_mut::<WakeAt>().at(30.0);
        app.update();
        let first = wait(&app);
        assert!(
            first > Duration::from_secs(29) && first <= Duration::from_secs(30),
            "the earlier one: {first:?}"
        );

        app.world_mut().resource_mut::<WakeAt>().at(30.0);
        app.update();
        assert_eq!(
            wait(&app),
            first,
            "the same time asked again leaves the mode as it was"
        );

        app.update();
        assert_eq!(wait(&app), Duration::MAX, "nothing asked: no wait at all");
    }

    #[test]
    fn a_wake_left_waiting_is_stuck() {
        let wake = Wake::default();
        assert!(!wake.stuck(Duration::ZERO), "nothing waits");

        wake.wake();
        thread::sleep(Duration::from_millis(5));
        assert!(
            wake.stuck(Duration::from_millis(1)),
            "waited past the limit"
        );
        assert!(
            !wake.stuck(Duration::from_secs(60)),
            "but not past this one"
        );

        wake.seen();
        assert!(!wake.stuck(Duration::ZERO), "the app looked");
    }
}
