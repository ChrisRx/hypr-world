//! Sound, through rodio, with the output open only while something plays.
//!
//! Bevy's own audio opens the output as the app starts and holds it until
//! the app exits, sending silence in between, so PipeWire never lets the
//! sink idle and suspend. A USB DAC clocked off the bus, feeding an amp over
//! S/PDIF, drifts against it over hours of that unbroken stream until the
//! sound pops, clicks and lags. Any restart of the stream clears it.
//!
//! Here a thread of its own holds the output. It opens it for the first
//! sound, with a moment of silence ahead of it, so that a sink coming back
//! from suspend, and whatever is downstream locking on again, does not clip
//! the start. It closes it once nothing has played for a while.

use std::{
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use bevy::{
    asset::{AssetLoader, LoadContext, io::Reader},
    prelude::*,
};
use rodio::{ChannelCount, Decoder, DeviceSinkBuilder, MixerDeviceSink, SampleRate, Source};

/// How long the output stays open with nothing playing.
const IDLE: Duration = Duration::from_secs(3);
/// The silence ahead of the first sound on a newly opened output.
const LEAD_IN: Duration = Duration::from_millis(150);
/// How often the output's thread looks whether it is idle.
const POLL: Duration = Duration::from_millis(250);

pub fn plugin(app: &mut App) {
    app.init_asset::<Sound>()
        .init_asset_loader::<SoundLoader>()
        .init_resource::<Speaker>()
        .add_systems(PostUpdate, (start, finish));
}

/// A sound file's bytes, decoded as it plays.
#[derive(Asset, TypePath, Clone)]
pub struct Sound {
    pub bytes: Arc<[u8]>,
}

#[derive(Default, TypePath)]
struct SoundLoader;

impl AssetLoader for SoundLoader {
    type Asset = Sound;
    type Settings = ();
    type Error = std::io::Error;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &(),
        _load_context: &mut LoadContext<'_>,
    ) -> Result<Sound, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        Ok(Sound {
            bytes: bytes.into(),
        })
    }

    fn extensions(&self) -> &[&str] {
        &["wav"]
    }
}

/// A sound to play on this entity: once, the entity going when it is done,
/// or looped for as long as the entity is there.
#[derive(Component, Clone)]
pub struct Play {
    pub sound: Handle<Sound>,
    pub looped: bool,
    pub volume: f32,
}

impl Play {
    pub fn once(sound: Handle<Sound>) -> Self {
        Self {
            sound,
            looped: false,
            volume: 1.0,
        }
    }

    pub fn looped(sound: Handle<Sound>, volume: f32) -> Self {
        Self {
            sound,
            looped: true,
            volume,
        }
    }
}

/// The sound playing, put on a `Play`'s entity once it starts. Dropping it,
/// with the entity, stops the sound.
#[derive(Component)]
pub struct Playing(Arc<Control>);

impl Playing {
    pub fn volume(&self) -> f32 {
        self.0.volume()
    }

    pub fn set_volume(&self, volume: f32) {
        self.0.volume.store(volume.to_bits(), Ordering::Relaxed);
    }
}

/// What the app and the playing sound share: its volume, and whether it has
/// come to its end.
struct Control {
    volume: AtomicU32,
    done: AtomicBool,
}

impl Control {
    fn new(volume: f32) -> Self {
        Self {
            volume: AtomicU32::new(volume.to_bits()),
            done: AtomicBool::new(false),
        }
    }

    fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }
}

/// A sound for the output's thread to play.
struct Request {
    bytes: Arc<[u8]>,
    looped: bool,
    control: Arc<Control>,
}

/// The way to the output's thread, which runs as long as this is kept.
#[derive(Resource)]
struct Speaker(mpsc::Sender<Request>);

impl Default for Speaker {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("audio".into())
            .spawn(move || speak(rx))
            .expect("spawning the audio thread");
        Self(tx)
    }
}

/// Starts each `Play` whose sound has loaded.
fn start(
    mut commands: Commands,
    speaker: Res<Speaker>,
    sounds: Res<Assets<Sound>>,
    queued: Query<(Entity, &Play), Without<Playing>>,
) {
    for (entity, play) in &queued {
        let Some(sound) = sounds.get(&play.sound) else {
            continue;
        };
        let control = Arc::new(Control::new(play.volume));
        let _ = speaker.0.send(Request {
            bytes: sound.bytes.clone(),
            looped: play.looped,
            control: control.clone(),
        });
        commands.entity(entity).insert(Playing(control));
    }
}

/// Despawns each sound played once that has finished.
fn finish(mut commands: Commands, playing: Query<(Entity, &Play, &Playing)>) {
    for (entity, play, playing) in &playing {
        if !play.looped && playing.0.done.load(Ordering::Relaxed) {
            commands.entity(entity).despawn();
        }
    }
}

/// The output's thread: plays what it is sent, opening the output for it if
/// it is closed, and closes it after `IDLE` with nothing playing.
fn speak(requests: mpsc::Receiver<Request>) {
    let live = Arc::new(AtomicUsize::new(0));
    let mut output: Option<(MixerDeviceSink, Instant)> = None;
    let mut quiet_since: Option<Instant> = None;
    loop {
        match requests.recv_timeout(POLL) {
            Ok(request) => {
                if output.is_none() {
                    output = open().map(|sink| (sink, Instant::now()));
                }
                match &output {
                    // Each sound in the lead-in waits it out.
                    Some((sink, opened)) => play(
                        sink,
                        request,
                        LEAD_IN.saturating_sub(opened.elapsed()),
                        &live,
                    ),
                    None => request.control.done.store(true, Ordering::Relaxed),
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if output.is_none() || live.load(Ordering::Acquire) > 0 {
            quiet_since = None;
        } else if quiet_since.get_or_insert_with(Instant::now).elapsed() >= IDLE {
            output = None;
            quiet_since = None;
        }
    }
}

fn open() -> Option<MixerDeviceSink> {
    DeviceSinkBuilder::open_default_sink()
        .inspect_err(|e| warn!("no audio output: {e}"))
        .map(|mut sink| {
            sink.log_on_drop(false);
            sink
        })
        .ok()
}

fn play(sink: &MixerDeviceSink, request: Request, delay: Duration, live: &Arc<AtomicUsize>) {
    let Request {
        bytes,
        looped,
        control,
    } = request;
    let data = Cursor::new(bytes);
    let decoded = if looped {
        Decoder::new_looped(data).map(|d| {
            sink.mixer()
                .add(Voice::new(d.delay(delay), control.clone(), live))
        })
    } else {
        Decoder::new(data).map(|d| {
            sink.mixer()
                .add(Voice::new(d.delay(delay), control.clone(), live))
        })
    };
    if let Err(e) = decoded {
        warn!("cannot play a sound: {e}");
        control.done.store(true, Ordering::Relaxed);
    }
}

/// A sound in the mix, at its `Control`'s volume. It ends early once the
/// app lets go of its `Control`, and counts itself in `live` while the
/// mixer holds it.
struct Voice<S> {
    inner: S,
    control: Arc<Control>,
    live: Arc<AtomicUsize>,
}

impl<S> Voice<S> {
    fn new(inner: S, control: Arc<Control>, live: &Arc<AtomicUsize>) -> Self {
        live.fetch_add(1, Ordering::AcqRel);
        Self {
            inner,
            control,
            live: live.clone(),
        }
    }
}

impl<S> Drop for Voice<S> {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
    }
}

impl<S: Source> Iterator for Voice<S> {
    type Item = rodio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        // The app's `Playing` gone, with its entity.
        if Arc::strong_count(&self.control) == 1 {
            return None;
        }
        match self.inner.next() {
            Some(sample) => Some(sample * self.control.volume()),
            None => {
                self.control.done.store(true, Ordering::Relaxed);
                None
            }
        }
    }
}

impl<S: Source> Source for Voice<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> ChannelCount {
        self.inner.channels()
    }

    fn sample_rate(&self) -> SampleRate {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZero;

    use rodio::buffer::SamplesBuffer;

    use super::*;

    fn buffer() -> SamplesBuffer {
        SamplesBuffer::new(
            NonZero::new(1).unwrap(),
            NonZero::new(44100).unwrap(),
            vec![1.0, 0.5, -1.0],
        )
    }

    /// At the control's volume, changed as it plays, and done at its end.
    #[test]
    fn a_voice_plays_at_its_volume_to_the_end() {
        let live = Arc::new(AtomicUsize::new(0));
        let control = Arc::new(Control::new(0.5));
        let mut voice = Voice::new(buffer(), control.clone(), &live);
        assert_eq!(live.load(Ordering::Acquire), 1, "counted while held");
        assert_eq!(voice.next(), Some(0.5));
        Playing(control.clone()).set_volume(2.0);
        assert_eq!(voice.next(), Some(1.0), "the new volume");
        assert_eq!(voice.next(), Some(-2.0));
        assert!(!control.done.load(Ordering::Relaxed));
        assert_eq!(voice.next(), None);
        assert!(control.done.load(Ordering::Relaxed), "done at its end");
        drop(voice);
        assert_eq!(live.load(Ordering::Acquire), 0, "uncounted once dropped");
    }

    /// Ended as soon as the app lets go of it.
    #[test]
    fn a_voice_stops_with_its_entity() {
        let live = Arc::new(AtomicUsize::new(0));
        let playing = Playing(Arc::new(Control::new(1.0)));
        let mut voice = Voice::new(buffer(), playing.0.clone(), &live);
        assert_eq!(voice.next(), Some(1.0));
        drop(playing);
        assert_eq!(voice.next(), None);
    }

    /// On the real output, for a look at it from outside (`wpctl status`,
    /// `/proc/asound/card*/pcm*p/sub0/status`): the party blower at once,
    /// again at 8s, and 8s more to go quiet. The output should be open only
    /// from each to `IDLE` after its end.
    #[test]
    #[ignore = "plays through the audio output"]
    fn the_output_is_open_only_while_playing() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default(), plugin));
        let sound = app.world_mut().resource_mut::<Assets<Sound>>().add(Sound {
            bytes: include_bytes!("button/partyblower.wav").as_slice().into(),
        });
        let begun = Instant::now();
        for at in [0, 8, 16] {
            while begun.elapsed() < Duration::from_secs(at) {
                app.update();
                thread::sleep(Duration::from_millis(10));
            }
            if at < 16 {
                eprintln!("{:.2} play", begun.elapsed().as_secs_f32());
                app.world_mut().spawn(Play::once(sound.clone()));
            }
        }
    }

    /// Once and done, the entity goes; looped, it stays.
    #[test]
    fn a_finished_sound_played_once_despawns() {
        let mut app = App::new();
        app.add_systems(Update, finish);
        let control = Arc::new(Control::new(1.0));
        let once = app
            .world_mut()
            .spawn((Play::once(Handle::default()), Playing(control.clone())))
            .id();
        let looped = app
            .world_mut()
            .spawn((
                Play::looped(Handle::default(), 1.0),
                Playing(Arc::new(Control::new(1.0))),
            ))
            .id();
        app.update();
        assert!(app.world().get_entity(once).is_ok(), "still playing");
        control.done.store(true, Ordering::Relaxed);
        app.update();
        assert!(app.world().get_entity(once).is_err(), "done");
        assert!(app.world().get_entity(looped).is_ok());
    }
}
