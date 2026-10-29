//! Counters for what the app does and why: updates
//! and frames drawn, what woke each update, the plugin's lines by verb, the
//! windows it said drew something, copies asked for and received, layer
//! pictures, and the GPU time of a frame by render pass. Logged as rates
//! every few seconds while the app is in view, so each change to how often
//! it draws can be measured. `stats = false` in the app's settings stops
//! the log.

use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

use bevy::{
    diagnostic::DiagnosticsStore,
    ecs::system::SystemParam,
    platform::time::Instant,
    prelude::*,
    window::{RequestRedraw, WindowEvent},
};

use crate::{FlyCamera, capture::Totals, config::WorldConfig, glue::Desktop, windows::Captures};

/// How long each report covers, in seconds of wall time.
const PERIOD: f32 = 5.0;

pub fn plugin(app: &mut App) {
    app.init_resource::<Stats>().add_systems(Last, tally);
}

/// What happened since the last report. Systems add to it as they work, and
/// `tally` closes each update.
#[derive(Resource, Default)]
pub struct Stats {
    /// Plugin lines and finished copies in this update so far.
    lines: u32,
    copies: u32,
    /// The last update wrote a `RequestRedraw`, so this one runs for it.
    redraw_asked: bool,
    /// The capture thread's totals at the last report.
    counted: Totals,
    /// The newest GPU timing taken in.
    gpu_seen: Option<Instant>,
    /// When this period began, by the wall clock. Not by adding up
    /// `Time<Real>`: with pipelined rendering that is the instant the render
    /// world finished its last frame (bevy_time `lib.rs:178`), so a pause
    /// would be counted an update late.
    began: Option<Instant>,
    period: Period,
}

#[derive(Default)]
struct Period {
    seconds: f32,
    updates: u32,
    drawn: u32,
    /// Frames drawn by why (redraw.rs). A frame can have more than one
    /// reason.
    drawn_for: BTreeMap<&'static str, u32>,
    /// Redraws asked for, by why (player.rs, flight.rs).
    redraws_for: BTreeMap<&'static str, u32>,
    /// Updates by what woke them. An update can have more than one reason,
    /// and one with none ran because the reactive wait ran out.
    by_plugin: u32,
    by_copy: u32,
    by_redraw: u32,
    by_window: u32,
    by_wait: u32,
    lines: BTreeMap<&'static str, u32>,
    damaged_in_view: u32,
    damaged_out_of_view: u32,
    asked: u32,
    /// Copies held back for a slot (capture.rs `Held`).
    waited: u32,
    /// Finished copies by window address.
    copies: HashMap<u64, u32>,
    /// Windows' own buffers shown, by window address and surface (plugin
    /// `buffer` and `shm`).
    shared: HashMap<(u64, u32), u32>,
    /// Shared memory copied out of clients' pools (plugin `shm`): how many,
    /// how many bytes, and how long the copies took all told and at most.
    shm_copies: u32,
    shm_bytes: u64,
    shm_time: Duration,
    shm_slowest: Duration,
    /// How long a copy took from start to `ready`, on average and at most,
    /// in milliseconds.
    copy_ms: f32,
    slowest_ms: f32,
    pictures: u32,
    /// GPU time by render pass (Bevy's top-level spans, which hold the
    /// nested ones): milliseconds all told, and the frames it ran in.
    gpu: BTreeMap<String, (f64, u32)>,
    /// The camera's samples a pixel at the report, 1 for no MSAA.
    msaa: u32,
}

impl Stats {
    /// A line from the plugin, by its verb.
    pub fn line(&mut self, verb: &'static str) {
        self.lines += 1;
        *self.period.lines.entry(verb).or_default() += 1;
    }

    /// A finished copy of the window at `address`.
    pub fn copied(&mut self, address: u64) {
        self.copies += 1;
        *self.period.copies.entry(address).or_default() += 1;
    }

    /// A buffer of one of a window's surfaces shown on its quad, in place
    /// of a copy: 0 the window's own surface, else the plugin's id for it.
    pub fn shared(&mut self, address: u64, surface: u32) {
        *self.period.shared.entry((address, surface)).or_default() += 1;
    }

    /// Shared memory copied out of a client's pool on the connection
    /// thread, `bytes` of it, taking `time`.
    pub fn shm_copied(&mut self, bytes: usize, time: Duration) {
        let period = &mut self.period;
        period.shm_copies += 1;
        period.shm_bytes += bytes as u64;
        period.shm_time += time;
        period.shm_slowest = period.shm_slowest.max(time);
    }

    /// A layer's new picture.
    pub fn picture(&mut self) {
        self.period.pictures += 1;
    }

    /// A frame drawn, for one of its reasons.
    pub fn drawn_for(&mut self, reason: &'static str) {
        *self.period.drawn_for.entry(reason).or_default() += 1;
    }

    /// A redraw asked for, for one of its reasons.
    pub fn redraw_for(&mut self, reason: &'static str) {
        *self.period.redraws_for.entry(reason).or_default() += 1;
    }
}

/// Another update asked for, counted by why.
#[derive(SystemParam)]
pub struct Redraw<'w> {
    stats: ResMut<'w, Stats>,
    redraws: MessageWriter<'w, RequestRedraw>,
}

impl Redraw<'_> {
    /// A redraw asked for, for each of `reasons`.
    pub fn ask(&mut self, reasons: impl IntoIterator<Item = &'static str>) {
        for reason in reasons {
            self.stats.redraw_for(reason);
        }
        self.redraws.write(RequestRedraw);
    }
}

/// Closes the update: what woke it and whether it draws, the GPU timings
/// that came in, and the report once the period is over. Last, after
/// everything that adds to the stats.
#[allow(clippy::too_many_arguments)]
fn tally(
    config: Res<WorldConfig>,
    desktop: Res<Desktop>,
    captures: Res<Captures>,
    diagnostics: Res<DiagnosticsStore>,
    camera: Single<(&Camera, &Msaa), With<FlyCamera>>,
    mut redraws: MessageReader<RequestRedraw>,
    mut window: MessageReader<WindowEvent>,
    stats: ResMut<Stats>,
) {
    let stats = stats.into_inner();
    let (camera, msaa) = *camera;
    let plugin = std::mem::take(&mut stats.lines) > 0;
    let copy = std::mem::take(&mut stats.copies) > 0;
    let redraw = std::mem::replace(&mut stats.redraw_asked, redraws.read().count() > 0);
    let window = window.read().count() > 0;

    let now = Instant::now();
    let began = *stats.began.get_or_insert(now);
    let period = &mut stats.period;
    period.seconds = (now - began).as_secs_f32();
    period.updates += 1;
    period.drawn += u32::from(camera.is_active);
    period.by_plugin += u32::from(plugin);
    period.by_copy += u32::from(copy);
    period.by_redraw += u32::from(redraw);
    period.by_window += u32::from(window);
    period.by_wait += u32::from(!(plugin || copy || redraw || window));

    // A frame's GPU time by pass, from `RenderDiagnosticsPlugin`: it comes a
    // few frames late, all of a frame's passes at once, stamped alike.
    let mut newest = stats.gpu_seen;
    for diagnostic in diagnostics.iter() {
        let path: Vec<&str> = diagnostic.path().components().collect();
        // A top-level pass. A nested span's time is in its parent's.
        let ["render", pass, "elapsed_gpu"] = path[..] else {
            continue;
        };
        for measurement in diagnostic
            .measurements()
            .filter(|m| stats.gpu_seen.is_none_or(|seen| m.time > seen))
        {
            let (ms, frames) = period.gpu.entry(pass.to_owned()).or_default();
            *ms += measurement.value;
            *frames += 1;
            newest = newest.max(Some(measurement.time));
        }
    }
    stats.gpu_seen = newest;

    if period.seconds < PERIOD {
        return;
    }

    // What the capture thread did meanwhile, on its own: the plugin's
    // damage goes to it, and it starts the copies.
    let counters = captures.0.counters();
    let totals = counters.totals();
    let done = totals.since(&stats.counted);
    stats.counted = totals;
    let period = &mut stats.period;
    period.asked = done.started as u32;
    period.waited = done.waited as u32;
    period.damaged_in_view = done.damaged_wanted as u32;
    period.damaged_out_of_view = done.damaged_unwanted as u32;
    period.copy_ms = done.copy_micros as f32 / 1000.0 / done.copied.max(1) as f32;
    period.slowest_ms = counters.take_slowest() as f32 / 1000.0;
    period.msaa = msaa.samples();
    // Out of view a report is worth it only when the app kept updating
    // anyway, more than once a second, which nothing out of view should
    // make it do. The counting starts afresh either way.
    let period = std::mem::take(&mut stats.period);
    stats.began = Some(now);
    let busy = period.updates as f32 > period.seconds;
    if (desktop.shown || busy) && config.stats {
        info!("{}", period.report(desktop.shown));
    }
}

impl Period {
    /// Everything a second, over the period.
    fn report(&self, in_view: bool) -> String {
        let rate = |n: u32| n as f32 / self.seconds;
        let list = |items: &mut dyn Iterator<Item = (String, u32)>| {
            let items: Vec<String> = items
                .map(|(name, n)| format!("{name} {:.1}", rate(n)))
                .collect();
            if items.is_empty() {
                "none".to_string()
            } else {
                items.join(", ")
            }
        };

        // The busiest first.
        let by_count = |counts: Vec<(String, u32)>| {
            let mut counts = counts;
            counts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            counts
        };
        let copies = by_count(
            self.copies
                .iter()
                .map(|(&address, &n)| (format!("{address:#x}"), n))
                .collect(),
        );
        // A window's own surface by its address, its others with their id.
        let shared = by_count(
            self.shared
                .iter()
                .map(|(&(address, surface), &n)| match surface {
                    0 => (format!("{address:#x}"), n),
                    _ => (format!("{address:#x}/{surface}"), n),
                })
                .collect(),
        );

        format!(
            "stats over {:.1} s{}, msaa {}, a second:\n  \
             updates {:.1}, drawn {:.1}\n  \
             drawn for: {}\n  \
             redraws asked for: {}\n  \
             woken by the plugin {:.1}, a copy {:.1}, a redraw {:.1}, the window {:.1}, the wait {:.1}\n  \
             plugin lines: {}\n  \
             windows that drew: {:.1} in view, {:.1} out of view\n  \
             copies asked {:.1}, got {:.1}, waited for a slot {:.1}, {:.1} ms each, at most {:.1}: {}\n  \
             windows' own buffers shown {:.1}: {}\n  \
             shared memory copied {:.1}, {:.1} MB, {:.2} ms each, at most {:.2}\n  \
             layer pictures {:.1}\n  \
             gpu a frame {}",
            self.seconds,
            if in_view { "" } else { " out of view" },
            self.msaa,
            rate(self.updates),
            rate(self.drawn),
            list(&mut self.drawn_for.iter().map(|(why, &n)| (why.to_string(), n))),
            list(
                &mut self
                    .redraws_for
                    .iter()
                    .map(|(why, &n)| (why.to_string(), n))
            ),
            rate(self.by_plugin),
            rate(self.by_copy),
            rate(self.by_redraw),
            rate(self.by_window),
            rate(self.by_wait),
            list(&mut self.lines.iter().map(|(verb, &n)| (verb.to_string(), n))),
            rate(self.damaged_in_view),
            rate(self.damaged_out_of_view),
            rate(self.asked),
            rate(self.copies.values().sum()),
            rate(self.waited),
            self.copy_ms,
            self.slowest_ms,
            list(&mut copies.into_iter().take(4)),
            rate(self.shared.values().sum()),
            list(&mut shared.into_iter().take(6)),
            rate(self.shm_copies),
            self.shm_bytes as f32 / 1e6 / self.seconds,
            self.shm_time.as_secs_f32() * 1000.0 / self.shm_copies.max(1) as f32,
            self.shm_slowest.as_secs_f32() * 1000.0,
            rate(self.pictures),
            self.gpu(),
        )
    }

    /// A drawn frame's GPU time on average, all told and by its largest
    /// passes. Each pass is averaged over the frames it ran in, not over
    /// every update that brought a timing: some passes run in updates that
    /// draw nothing, which would water the others down. The frame count is
    /// the largest pass's.
    fn gpu(&self) -> String {
        let mut passes: Vec<(&str, f64, u32)> = self
            .gpu
            .iter()
            .map(|(pass, &(ms, frames))| (pass.as_str(), ms / f64::from(frames), frames))
            .collect();
        passes.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        let Some(&(_, _, frames)) = passes.first() else {
            return "none measured".to_string();
        };
        let total: f64 = passes.iter().map(|&(_, ms, _)| ms).sum();
        let largest: Vec<String> = passes
            .iter()
            .take(5)
            .map(|(pass, ms, _)| format!("{pass} {ms:.2}"))
            .collect();
        format!("{total:.2} ms over {frames} frames: {}", largest.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_is_rates_a_second() {
        let mut stats = Stats::default();
        stats.line("window");
        stats.line("window");
        stats.copied(0x2a);
        stats.shared(0x2b, 0);
        stats.shared(0x2b, 3);
        stats.shm_copied(4_000_000, Duration::from_millis(2));
        stats.shm_copied(2_000_000, Duration::from_millis(1));
        stats.picture();
        stats.redraw_for("falling");

        // From the capture thread's counters, at the report.
        let (asked, damaged_out_of_view) = (1, 1);
        let period = Period {
            seconds: 2.0,
            updates: 10,
            drawn: 10,
            by_wait: 4,
            asked,
            waited: 1,
            damaged_out_of_view,
            copy_ms: 6.0,
            slowest_ms: 17.0,
            // Ten frames drawn, 3 ms of opaque pass and half a ms of UI
            // each, and a pass that also ran in twenty updates that drew
            // nothing: each is averaged over its own frames.
            gpu: BTreeMap::from([
                ("main_opaque_pass_3d".to_string(), (30.0, 10)),
                ("ui".to_string(), (5.0, 10)),
                ("uniform_allocation".to_string(), (0.3, 30)),
            ]),
            msaa: 2,
            ..std::mem::take(&mut stats.period)
        };
        let report = period.report(true);

        assert!(
            report.starts_with("stats over 2.0 s, msaa 2, a second"),
            "{report}"
        );
        assert!(
            period
                .report(false)
                .starts_with("stats over 2.0 s out of view, msaa 2, a second")
        );
        assert!(
            report.contains("gpu a frame 3.51 ms over 10 frames: main_opaque_pass_3d 3.00, ui 0.50, uniform_allocation 0.01"),
            "{report}"
        );
        let unmeasured = Period {
            seconds: 1.0,
            ..Period::default()
        }
        .report(true);
        assert!(
            unmeasured.contains("gpu a frame none measured"),
            "{unmeasured}"
        );
        assert!(
            report.contains("redraws asked for: falling 0.5"),
            "{report}"
        );
        assert!(report.contains("updates 5.0, drawn 5.0"), "{report}");
        assert!(report.contains("the wait 2.0"), "{report}");
        assert!(report.contains("plugin lines: window 1.0"), "{report}");
        assert!(report.contains("0.0 in view, 0.5 out of view"), "{report}");
        assert!(report.contains("copies asked 0.5, got 0.5, waited for a slot 0.5, 6.0 ms each, at most 17.0: 0x2a 0.5"), "{report}");
        assert!(
            report.contains("windows' own buffers shown 1.0: 0x2b 0.5, 0x2b/3 0.5"),
            "{report}"
        );
        assert!(
            report.contains("shared memory copied 1.0, 3.0 MB, 1.50 ms each, at most 2.00"),
            "{report}"
        );
        assert!(report.contains("layer pictures 0.5"), "{report}");
    }
}
