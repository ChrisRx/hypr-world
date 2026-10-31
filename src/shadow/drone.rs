//! The shadow realm's sound, made here rather than read from a file: a
//! loop of tones and bands of hiss, each swelling and fading on its own
//! slow beat, as 16-bit WAV. A faint chord of wavering voices a semitone
//! apart, over a low throb, with whispers and a high shimmer. Every tone and swell goes
//! round a whole number of times in the loop, so it loops without a seam.

use std::f32::consts::TAU;

/// Samples a second.
const RATE: u32 = 16_000;
/// The loop's length, in seconds.
const LOOP: f32 = 20.0;
/// The loudest sample, of full scale.
const PEAK: f32 = 0.9;

/// The loop, as a WAV file.
pub fn wav() -> Vec<u8> {
    let mut out = vec![0.0; (RATE as f32 * LOOP) as usize];
    // The same each time.
    let mut rng = fastrand::Rng::with_seed(0x5ead);
    tone(&mut out, 55.0, 0.0, (0.0, 0), |_| 0.3);
    tone(&mut out, 55.35, 1.0, (0.0, 0), |_| 0.25);
    // Semitones and a tritone, each voice wavering and coming and
    // going on its own.
    let voices = [196.0, 207.65, 277.18, 293.66, 392.0, 415.3];
    for (i, hz) in voices.into_iter().enumerate() {
        let phase = i as f32 * 1.1;
        tone(&mut out, hz, phase, (2.0, 7 + i as u32), move |t| {
            0.12 * swell(t, 1 + i as u32 % 3, phase).powi(2)
        });
    }
    band(&mut out, &mut rng, (500.0, 1400.0), 100, |t| {
        0.12 * swell(t, 5, 0.0).powi(3)
    });
    band(&mut out, &mut rng, (2500.0, 4500.0), 80, |t| {
        0.05 * (0.4 + 0.6 * swell(t, 2, 0.5))
    });
    encode(&out)
}

/// The nearest frequency that goes round the loop a whole number of times.
fn fit(hz: f32) -> f64 {
    (f64::from(hz) * f64::from(LOOP)).round() / f64::from(LOOP)
}

/// From 0 to 1 and back, `n` times a loop.
fn swell(t: f32, n: u32, phase: f32) -> f32 {
    0.5 - 0.5 * (TAU * n as f32 * t / LOOP + phase).cos()
}

/// A tone of `hz`, `amp(t)` loud, its pitch wavering by `waver.0` radians,
/// `waver.1` times a loop.
fn tone(out: &mut [f32], hz: f32, phase: f32, waver: (f32, u32), amp: impl Fn(f32) -> f32) {
    let hz = fit(hz);
    for (i, sample) in out.iter_mut().enumerate() {
        let t = i as f64 / f64::from(RATE);
        // The turns so far less the whole ones, so a long loop keeps its
        // precision.
        let turns = (hz * t).fract() as f32;
        let t = t as f32;
        let wave = waver.0 * (TAU * waver.1 as f32 * t / LOOP).sin();
        *sample += amp(t) * (TAU * turns + phase + wave).sin();
    }
}

/// A band of hiss between `hz`, out of `count` tones there at random,
/// `amp(t)` loud.
fn band(
    out: &mut [f32],
    rng: &mut fastrand::Rng,
    (low, high): (f32, f32),
    count: usize,
    amp: impl Fn(f32) -> f32,
) {
    let mut hiss = vec![0.0; out.len()];
    let each = 1.0 / (count as f32).sqrt();
    for _ in 0..count {
        let hz = low + rng.f32() * (high - low);
        tone(&mut hiss, hz, rng.f32() * TAU, (0.0, 0), |_| each);
    }
    for (i, (sample, hiss)) in out.iter_mut().zip(hiss).enumerate() {
        *sample += amp(i as f32 / RATE as f32) * hiss;
    }
}

/// Mono 16-bit PCM WAV, scaled to `PEAK`.
fn encode(samples: &[f32]) -> Vec<u8> {
    let loudest = samples
        .iter()
        .fold(0.0_f32, |m, s| m.max(s.abs()))
        .max(1e-6);
    let scale = PEAK * f32::from(i16::MAX) / loudest;
    let data = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    // PCM, one channel.
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    // Two bytes a frame, 16 bits a sample.
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&((sample * scale) as i16).to_le_bytes());
    }
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A whole loop, as WAV, loud but not clipped, and ending where it
    /// starts.
    #[test]
    fn it_loops() {
        let wav = wav();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(wav.len(), 44 + 2 * (RATE as f32 * LOOP) as usize);
        let samples: Vec<i16> = wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b))
            .collect();
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > i16::MAX as u16 / 2, "peaks at {peak}");
        // The last sample runs on into the first, as the next would.
        let (first, last) = (samples[0], samples[samples.len() - 1]);
        assert!(
            (i32::from(first) - i32::from(last)).abs() < 2000,
            "{last} then {first}"
        );
    }
}
