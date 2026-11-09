//! Keeps a few seconds of what the computer is playing, so a phrase heard by
//! the microphone can be checked against it.
//!
//! The microphone hears whatever the speakers play, so a replayed clip saying
//! "diff clip that" looks like a real command. Storing the audio costs almost
//! nothing; the recognizer only runs on it after the microphone hears a phrase.

use crate::audio::Resampler;
use crate::voice::VOICE_SAMPLE_RATE;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Long enough to hold a whole phrase plus the delay before it's detected.
const HISTORY: Duration = Duration::from_millis(3_500);
const SYSTEM_SAMPLE_RATE: u32 = 48_000;
const SYSTEM_CHANNELS: usize = 2;

#[derive(Debug)]
struct History {
    resampler: Resampler,
    /// Blocks are timestamped because the OS may deliver nothing during
    /// silence, and old audio must not linger as if it were recent.
    blocks: VecDeque<(Instant, Vec<f32>)>,
}

#[derive(Debug)]
pub struct EchoGuard {
    history: Mutex<History>,
}

impl Default for EchoGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl EchoGuard {
    pub fn new() -> Self {
        Self {
            history: Mutex::new(History {
                resampler: Resampler::new(SYSTEM_SAMPLE_RATE, VOICE_SAMPLE_RATE as u32),
                blocks: VecDeque::new(),
            }),
        }
    }

    /// Adds system audio (48 kHz interleaved stereo) that has just arrived.
    pub fn push_system(&self, interleaved: &[f32]) {
        self.push_system_at(Instant::now(), interleaved);
    }

    fn push_system_at(&self, now: Instant, interleaved: &[f32]) {
        let mono: Vec<f32> = interleaved
            .chunks_exact(SYSTEM_CHANNELS)
            .map(|frame| frame.iter().sum::<f32>() / SYSTEM_CHANNELS as f32)
            .collect();
        let mut history = self.history.lock().expect("echo lock");
        let block = history.resampler.process(&mono);
        history.blocks.push_back((now, block));
        while history.blocks.front().is_some_and(|(at, _)| now.duration_since(*at) > HISTORY) {
            history.blocks.pop_front();
        }
    }

    /// What the computer played recently, as 16 kHz mono.
    pub fn recent_audio(&self) -> Vec<f32> {
        self.recent_audio_at(Instant::now())
    }

    fn recent_audio_at(&self, now: Instant) -> Vec<f32> {
        let history = self.history.lock().expect("echo lock");
        history
            .blocks
            .iter()
            .filter(|(at, _)| now.saturating_duration_since(*at) <= HISTORY)
            .flat_map(|(_, block)| block.iter().copied())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_SECOND_STEREO: usize = 96_000;

    #[test]
    fn stores_system_audio_as_16khz_mono() {
        let guard = EchoGuard::new();
        guard.push_system(&vec![0.5; ONE_SECOND_STEREO]);
        let audio = guard.recent_audio();
        assert!((audio.len() as i64 - 16_000).abs() <= 1, "{}", audio.len());
        assert!((audio[100] - 0.5).abs() < 1e-5);
    }

    #[test]
    fn nothing_played_means_nothing_to_compare() {
        assert!(EchoGuard::new().recent_audio().is_empty());
    }

    #[test]
    fn audio_expires_even_if_nothing_new_arrives() {
        let guard = EchoGuard::new();
        let start = Instant::now();
        guard.push_system_at(start, &vec![0.5; ONE_SECOND_STEREO]);
        assert!(!guard.recent_audio_at(start + Duration::from_secs(3)).is_empty());
        assert!(guard.recent_audio_at(start + Duration::from_secs(4)).is_empty());
    }

    #[test]
    fn history_is_bounded_while_audio_keeps_arriving() {
        let guard = EchoGuard::new();
        let start = Instant::now();
        for second in 0..60 {
            guard.push_system_at(start + Duration::from_secs(second), &vec![0.1; ONE_SECOND_STEREO]);
        }
        let held = guard.history.lock().unwrap().blocks.len();
        assert!(held <= 4, "held {held} blocks");
    }
}
