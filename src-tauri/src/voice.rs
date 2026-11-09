//! On-device detection of the "diff clip that" phrases.
//!
//! Audio is held in memory only as long as detection needs it, and is never
//! saved or sent anywhere.

use sherpa_onnx::{KeywordSpotter, KeywordSpotterConfig, OnlineStream};
use std::collections::VecDeque;
use std::path::Path;

pub const VOICE_SAMPLE_RATE: usize = 16_000;

/// The trigger phrases, as the tokens the model understands.
const KEYWORDS: &str = "\
▁DI FF ▁C LI P ▁THAT @diff_clip_that
▁DI FF ▁C LI P ▁IT @diff_clip_it
▁DI FF ▁SA VE ▁THAT @diff_save_that
▁HE Y ▁DI FF ▁C LI P ▁THAT @hey_diff_clip_that
";

/// Used to ask "was the computer saying this?". Deliberately broad, since a
/// false match only matters if the microphone heard the phrase at that moment.
const ECHO_KEYWORDS: &str = "\
▁DI FF ▁C LI P ▁THAT :4.0 #0.03 @a/\
▁DI FF ▁C LI P ▁IT :4.0 #0.03 @b/\
▁DI FF ▁SA VE ▁THAT :4.0 #0.03 @c/\
▁HE Y ▁DI FF ▁C LI P ▁THAT :4.0 #0.03 @d/\
▁DI FF ▁C LI P :4.0 #0.03 @e/\
▁DI FF ▁SA VE :4.0 #0.03 @f";
const ECHO_TRAILING_SILENCE: usize = VOICE_SAMPLE_RATE;

const KEYWORDS_SCORE: f32 = 3.0;
const KEYWORDS_THRESHOLD: f32 = 0.05;

/// RMS level that counts as someone speaking.
const SPEECH_LEVEL: f32 = 0.004;
/// Keep decoding this long after the last speech, so a phrase can finish.
const HANGOVER_SAMPLES: usize = VOICE_SAMPLE_RATE * 3 / 2;
/// Audio retained from before speech starts, so first syllables aren't lost.
const PRE_ROLL_SAMPLES: usize = VOICE_SAMPLE_RATE / 2;

#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("the voice model is missing or couldn't be loaded")]
    ModelUnavailable,
}

/// Skips decoding during silence, which is what keeps CPU use near zero.
#[derive(Debug, Default)]
pub struct SpeechGate {
    pre_roll: VecDeque<f32>,
    hangover: usize,
}

impl SpeechGate {
    /// Returns the audio to decode for this chunk, which is empty during
    /// silence. The second value is true on the chunk where speech ended.
    pub fn process(&mut self, chunk: &[f32]) -> (Vec<f32>, bool) {
        let was_active = self.hangover > 0;
        if rms(chunk) >= SPEECH_LEVEL {
            self.hangover = HANGOVER_SAMPLES;
            let mut output: Vec<f32> = if was_active {
                Vec::with_capacity(chunk.len())
            } else {
                self.pre_roll.drain(..).collect()
            };
            output.extend_from_slice(chunk);
            return (output, false);
        }
        if was_active {
            self.hangover = self.hangover.saturating_sub(chunk.len());
            return (chunk.to_vec(), self.hangover == 0);
        }
        self.pre_roll.extend(chunk);
        let excess = self.pre_roll.len().saturating_sub(PRE_ROLL_SAMPLES);
        self.pre_roll.drain(..excess);
        (Vec::new(), false)
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

pub struct VoiceSpotter {
    spotter: KeywordSpotter,
    stream: OnlineStream,
    gate: SpeechGate,
}

// SAFETY: the spotter and stream are used only by the one thread that owns
// this value, and the underlying library objects aren't tied to a thread.
unsafe impl Send for VoiceSpotter {}

impl VoiceSpotter {
    /// `model_dir` holds the encoder, decoder, joiner and tokens files.
    pub fn new(model_dir: &Path) -> Result<Self, VoiceError> {
        Self::with_tuning(model_dir, KEYWORDS_SCORE, KEYWORDS_THRESHOLD)
    }

    /// Like [`VoiceSpotter::new`] with explicit sensitivity, for accuracy tests.
    pub fn with_tuning(model_dir: &Path, score: f32, threshold: f32) -> Result<Self, VoiceError> {
        let file = |name: &str| {
            let path = model_dir.join(name);
            path.exists()
                .then(|| path.to_string_lossy().into_owned())
                .ok_or(VoiceError::ModelUnavailable)
        };

        let mut config = KeywordSpotterConfig::default();
        config.model_config.transducer.encoder = Some(file("encoder.int8.onnx")?);
        config.model_config.transducer.decoder = Some(file("decoder.int8.onnx")?);
        config.model_config.transducer.joiner = Some(file("joiner.int8.onnx")?);
        config.model_config.tokens = Some(file("tokens.txt")?);
        config.model_config.num_threads = 1;
        config.model_config.provider = Some("cpu".into());
        config.keywords_buf = Some(KEYWORDS.into());
        config.keywords_score = score;
        config.keywords_threshold = threshold;
        config.max_active_paths = 4;
        config.num_trailing_blanks = 1;

        let spotter = KeywordSpotter::create(&config).ok_or(VoiceError::ModelUnavailable)?;
        let stream = spotter.create_stream();
        Ok(Self { spotter, stream, gate: SpeechGate::default() })
    }

    /// True if a trigger phrase occurs in `audio` (16 kHz mono). Takes a
    /// fraction of a second, and only runs after the microphone hears a phrase.
    pub fn contains_phrase(&self, audio: &[f32]) -> bool {
        if audio.is_empty() {
            return false;
        }
        let stream = self.spotter.create_stream_with_keywords(ECHO_KEYWORDS);
        stream.accept_waveform(VOICE_SAMPLE_RATE as i32, audio);
        stream.accept_waveform(VOICE_SAMPLE_RATE as i32, &vec![0.0; ECHO_TRAILING_SILENCE]);
        stream.input_finished();
        while self.spotter.is_ready(&stream) {
            self.spotter.decode(&stream);
            if self.spotter.get_result(&stream).is_some_and(|result| !result.keyword.is_empty()) {
                return true;
            }
        }
        false
    }

    /// Feeds 16 kHz mono audio. Returns true if a trigger phrase just finished.
    pub fn feed(&mut self, chunk: &[f32]) -> bool {
        let (speech, ended) = self.gate.process(chunk);
        let mut detected = false;
        if !speech.is_empty() {
            self.stream.accept_waveform(VOICE_SAMPLE_RATE as i32, &speech);
            while self.spotter.is_ready(&self.stream) {
                self.spotter.decode(&self.stream);
                let hit = self
                    .spotter
                    .get_result(&self.stream)
                    .is_some_and(|result| !result.keyword.is_empty());
                if hit {
                    detected = true;
                    self.spotter.reset(&self.stream);
                }
            }
        }
        if ended {
            self.spotter.reset(&self.stream);
        }
        detected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: usize = 1_600;

    #[test]
    fn silence_is_not_decoded() {
        let mut gate = SpeechGate::default();
        for _ in 0..50 {
            assert!(gate.process(&[0.0; CHUNK]).0.is_empty());
        }
    }

    #[test]
    fn speech_includes_the_audio_just_before_it() {
        let mut gate = SpeechGate::default();
        gate.process(&[0.0; CHUNK]);
        let (output, _) = gate.process(&[0.1; CHUNK]);
        assert_eq!(output.len(), CHUNK * 2);
    }

    #[test]
    fn pre_roll_is_bounded() {
        let mut gate = SpeechGate::default();
        for _ in 0..100 {
            gate.process(&[0.0; CHUNK]);
        }
        assert_eq!(gate.pre_roll.len(), PRE_ROLL_SAMPLES);
    }

    #[test]
    fn keeps_decoding_briefly_after_speech_then_reports_the_end() {
        let mut gate = SpeechGate::default();
        gate.process(&[0.1; CHUNK]);
        let mut ended_after = None;
        for index in 1..100 {
            let (output, ended) = gate.process(&[0.0; CHUNK]);
            assert_eq!(output.len(), CHUNK);
            if ended {
                ended_after = Some(index);
                break;
            }
        }
        assert_eq!(ended_after, Some(HANGOVER_SAMPLES.div_ceil(CHUNK)));
        assert!(gate.process(&[0.0; CHUNK]).0.is_empty());
    }
}
