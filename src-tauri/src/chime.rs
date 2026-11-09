//! The sound played when a clip is saved. It's synthesized for the output
//! device's own sample rate, so it sounds the same on macOS and Windows.
//!
//! Two quick synth hits rising a fourth onto a suspended chord, which reads as
//! confirmation without sounding cheerful. Gameplay audio is dense at low
//! frequencies, so the harmonics and the attack tick carry it through the mix.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use std::f64::consts::TAU;
use std::time::Duration;

const E4: f64 = 329.63;
const B4: f64 = 493.88;
const E5: f64 = 659.25;
const FS5: f64 = 739.99;
const B5: f64 = 987.77;
const FS6: f64 = 1479.98;

const SECOND_HIT: f64 = 0.14;

const ATTACK: f64 = 0.002;
const RELEASE: f64 = 0.04;
const DETUNE: f64 = 1.0018;
/// Relative level of each harmonic, starting at the fundamental.
const HARMONICS: [f64; 5] = [1.0, 0.55, 0.3, 0.18, 0.1];
/// Higher harmonics fade sooner, so each hit starts bright and then softens.
const HARMONIC_FADE: f64 = 0.09;

const TICK_GAIN: f64 = 0.7;
const TICK_ATTACK: f64 = 0.0005;
const TICK_FADE: f64 = 0.003;
const TICK_LENGTH: f64 = 0.02;

const DRIVE: f64 = 1.5;
const PEAK: f32 = 0.89;
/// Device buffers still hold audio when the last sample has been handed over.
const OUTPUT_TAIL: Duration = Duration::from_millis(250);

struct Note {
    start: f64,
    hz: f64,
    /// Time for the note to fade to about a third of its level.
    ring: f64,
    length: f64,
    gain: f64,
}

const fn first_hit(hz: f64, gain: f64) -> Note {
    Note { start: 0.0, hz, ring: 0.09, length: 0.18, gain }
}

const fn second_hit(hz: f64, gain: f64) -> Note {
    Note { start: SECOND_HIT, hz, ring: 0.22, length: 0.7, gain }
}

const NOTES: [Note; 6] = [
    first_hit(B4, 0.7),
    first_hit(FS5, 1.0),
    second_hit(E4, 0.45),
    second_hit(E5, 0.75),
    second_hit(B5, 1.0),
    second_hit(FS6, 0.55),
];

fn tone(hz: f64, t: f64) -> f64 {
    HARMONICS
        .iter()
        .enumerate()
        .map(|(i, level)| level * (-t * i as f64 / HARMONIC_FADE).exp() * (TAU * hz * (i + 1) as f64 * t).sin())
        .sum()
}

fn add_note(mix: &mut [f64], rate: f64, note: &Note) {
    let first = (note.start * rate) as usize;
    let count = (note.length * rate) as usize;
    for (i, sample) in mix[first..].iter_mut().take(count).enumerate() {
        let t = i as f64 / rate;
        let attack = (t / ATTACK).min(1.0);
        let release = ((note.length - t) / RELEASE).clamp(0.0, 1.0);
        let level = note.gain * attack * release * (-t / note.ring).exp();
        *sample += level * (tone(note.hz, t) + tone(note.hz * DETUNE, t));
    }
}

/// A burst of high-passed noise that gives a hit its sharp front edge.
fn add_tick(mix: &mut [f64], rate: f64, start: f64) {
    let first = (start * rate) as usize;
    let count = (TICK_LENGTH * rate) as usize;
    let mut state = 0x2545_f491_u32;
    let mut previous = 0.0;
    for (i, sample) in mix[first..].iter_mut().take(count).enumerate() {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let noise = state as f64 / u32::MAX as f64 * 2.0 - 1.0;
        let t = i as f64 / rate;
        let level = TICK_GAIN * (t / TICK_ATTACK).min(1.0) * (-t / TICK_FADE).exp();
        *sample += level * (noise - previous);
        previous = noise;
    }
}

/// The sound as mono samples at `sample_rate`.
pub fn render(sample_rate: u32) -> Vec<f32> {
    let rate = sample_rate as f64;
    let duration = NOTES.iter().map(|n| n.start + n.length).fold(0.0, f64::max);
    let mut mix = vec![0.0; (duration * rate).ceil() as usize];
    for note in &NOTES {
        add_note(&mut mix, rate, note);
    }
    add_tick(&mut mix, rate, 0.0);
    add_tick(&mut mix, rate, SECOND_HIT);
    // Saturating the peaks makes it louder for the same maximum level.
    let peak = mix.iter().fold(f64::EPSILON, |max, s: &f64| max.max(s.abs()));
    let shaped: Vec<f64> = mix.iter().map(|s| (DRIVE * s / peak).tanh()).collect();
    let shaped_peak = shaped.iter().fold(f64::EPSILON, |max, s| max.max(s.abs()));
    shaped.iter().map(|s| (s / shaped_peak) as f32 * PEAK).collect()
}

/// Plays the sound without blocking. A confirmation isn't worth failing over,
/// and the notification still appears if there's no usable output.
pub fn play() {
    std::thread::spawn(|| {
        let _ = play_to_end();
    });
}

fn play_to_end() -> Result<(), String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no audio output was found")?;
    let config = device.default_output_config().map_err(|e| e.to_string())?;
    let format = config.sample_format();
    let config = config.config();
    let samples = render(config.sample_rate);
    let duration = Duration::from_secs_f64(samples.len() as f64 / config.sample_rate as f64);

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, samples),
        SampleFormat::I16 => build::<i16>(&device, &config, samples),
        SampleFormat::U16 => build::<u16>(&device, &config, samples),
        SampleFormat::I32 => build::<i32>(&device, &config, samples),
        other => return Err(format!("unsupported output format: {other}")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    std::thread::sleep(duration + OUTPUT_TAIL);
    Ok(())
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    samples: Vec<f32>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels.max(1) as usize;
    let mut sound = samples.into_iter();
    device.build_output_stream::<T, _, _>(
        *config,
        move |data: &mut [T], _| {
            for frame in data.chunks_exact_mut(channels) {
                let value = T::from_sample_(sound.next().unwrap_or(0.0));
                frame.fill(value);
            }
        },
        |_| {},
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0, |max, s| max.max(s.abs()))
    }

    #[test]
    fn lasts_the_same_at_any_sample_rate() {
        let seconds = |rate: u32| render(rate).len() as f64 / rate as f64;
        assert!((seconds(44_100) - seconds(48_000)).abs() < 0.001);
        assert!((0.6..1.1).contains(&seconds(48_000)), "{}", seconds(48_000));
    }

    #[test]
    fn is_loud_without_clipping() {
        let samples = render(48_000);
        assert!((peak(&samples) - PEAK).abs() < 1e-4, "peak {}", peak(&samples));
    }

    #[test]
    fn starts_and_ends_without_a_click() {
        let samples = render(48_000);
        assert!(samples[0].abs() < 0.01);
        assert!(samples.last().unwrap().abs() < 0.01);
    }
}
