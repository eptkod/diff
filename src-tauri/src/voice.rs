//! On-device detection of the "diff clip that" phrases.
//!
//! There are two detectors. The light one is a small keyword spotter that
//! follows the audio as it arrives. The accurate one reads each stretch of
//! speech with a larger model once it can hear the whole phrase. That model
//! is told to expect the trigger phrases, so it writes one out whenever the
//! sound fits, and only those exact words at the end of the speech count.
//! Audio is held in memory only as long as detection needs it, and is never
//! saved or sent anywhere.

use crate::settings::VoiceModel;
use sherpa_onnx::{
    KeywordSpotter, KeywordSpotterConfig, OfflineRecognizer, OfflineRecognizerConfig, OnlineStream,
};
use std::collections::VecDeque;
use std::path::Path;

pub const VOICE_SAMPLE_RATE: usize = 16_000;

/// The trigger phrases, as the tokens the light model understands.
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
/// How many readings of the audio the light model keeps open at once.
const KEYWORDS_SEARCH_WIDTH: i32 = 16;

/// Lowest RMS level that can count as speech, whatever the room sounds like.
const MIN_SPEECH_LEVEL: f32 = 0.0008;
/// Speech has to be this many times louder than the background.
const SPEECH_OVER_NOISE: f32 = 3.0;
/// How quickly the background estimate follows quiet moments.
const NOISE_TRACKING_SECONDS: f32 = 1.0;
/// How quickly it accepts a steady new noise, such as a fan starting up.
const NOISE_RISE_SECONDS: f32 = 20.0;
/// Keep decoding this long after the last speech, so a phrase can finish.
const HANGOVER_SAMPLES: usize = VOICE_SAMPLE_RATE * 3 / 2;
/// Audio retained from before speech starts, so first syllables aren't lost.
const PRE_ROLL_SAMPLES: usize = VOICE_SAMPLE_RATE / 2;

/// The speech level the model hears best, whatever the microphone delivers.
const TARGET_SPEECH_LEVEL: f32 = 0.08;
const MAX_GAIN: f32 = 40.0;
const MIN_GAIN: f32 = 0.5;
/// Frame length used to find the louder parts of a stretch of audio.
const LEVEL_FRAME_SAMPLES: usize = VOICE_SAMPLE_RATE / 40;
const LEVEL_ATTACK_SECONDS: f32 = 0.015;
const LEVEL_RELEASE_SECONDS: f32 = 1.0;

/// The trigger phrases as the accurate model writes them. "Clipped" and
/// "saved" are included because they sound the same as "clip" and "save" when
/// "that" or "it" follows. "Hey diff clip that" ends with the first phrase.
const PHRASES: [&str; 6] = [
    "DIFF CLIP THAT",
    "DIFF CLIP IT",
    "DIFF SAVE THAT",
    "DIFF CLIPPED THAT",
    "DIFF CLIPPED IT",
    "DIFF SAVED THAT",
];
/// How far the accurate model leans towards hearing a trigger phrase. "Diff"
/// isn't a word it knows, so without this it writes "if" or "they've". Higher
/// catches more unclear speech, and mistakes more similar speech for a phrase.
const PHRASE_BOOST: f32 = 3.5;
/// How many readings of the audio the accurate model keeps open at once. A
/// phrase that starts as the less likely reading needs room to win later.
const SEARCH_WIDTH: i32 = 16;
/// The lean used when asking "was the computer saying this?". Stronger, since
/// missing a phrase there lets a replayed clip trigger a new one.
const ECHO_PHRASE_BOOST: f32 = 7.0;
/// The most audio the accurate model is given at once. It fails outright on
/// much more than this, and nothing the app listens to is as long.
const MAX_READ_SAMPLES: usize = VOICE_SAMPLE_RATE * 15;
/// Sound after a phrase counts as background, such as other people talking,
/// if it's at most this fraction of the phrase's own level.
const BACKGROUND_LEVEL: f32 = 0.5;
/// After a pause, a second read covers only this much of the latest speech.
/// With less before it to account for, the model hears a phrase more easily.
const CLOSE_READ_SAMPLES: usize = VOICE_SAMPLE_RATE * 9 / 5;

/// How much recent speech the accurate model reads each time.
const WINDOW_SAMPLES: usize = VOICE_SAMPLE_RATE * 7 / 2;
/// A pause this long means a phrase may have just finished.
const PAUSE_SAMPLES: usize = VOICE_SAMPLE_RATE * 3 / 10;
/// Shorter bursts of sound can't be a whole phrase.
const MIN_SPEECH_SAMPLES: usize = VOICE_SAMPLE_RATE * 3 / 10;
/// How often to look during speech with no pauses, such as a loud room.
const CONTINUOUS_SPEECH_INTERVAL: usize = VOICE_SAMPLE_RATE * 6 / 5;

#[derive(Debug, thiserror::Error)]
pub enum VoiceError {
    #[error("the voice model is missing or couldn't be loaded")]
    ModelUnavailable,
}

/// The level of the speech in a stretch of audio, judged by its louder parts
/// so pauses don't drag the estimate down.
fn speech_level(audio: &[f32]) -> f32 {
    let mut energies: Vec<f32> = audio
        .chunks(LEVEL_FRAME_SAMPLES)
        .map(|frame| frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32)
        .collect();
    if energies.is_empty() {
        return 0.0;
    }
    energies.sort_by(f32::total_cmp);
    let loud = &energies[energies.len() / 3..];
    (loud.iter().sum::<f32>() / loud.len() as f32).sqrt()
}

/// Scales a whole stretch of audio so its speech is at the target level.
fn normalize_speech(audio: &mut [f32]) {
    let level = speech_level(audio);
    if level <= 0.0 {
        return;
    }
    let gain = (TARGET_SPEECH_LEVEL / level).clamp(MIN_GAIN, MAX_GAIN);
    for sample in audio.iter_mut() {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
}

fn smoothing(samples: usize, seconds: f32) -> f32 {
    1.0 - (-(samples as f32) / (seconds * VOICE_SAMPLE_RATE as f32)).exp()
}

#[derive(Debug, PartialEq)]
pub struct GateOutput {
    /// The audio to decode for this chunk, which is empty during silence.
    pub audio: Vec<f32>,
    /// True on the chunk where speech ended.
    pub ended: bool,
    /// True if this chunk is speech, rather than the pause after it.
    pub voiced: bool,
}

/// Skips decoding during silence, which is what keeps CPU use near zero. The
/// bar adapts to the room, so quiet voices count in a quiet room and a noisy
/// room doesn't keep the decoder running.
#[derive(Debug, Default)]
pub struct SpeechGate {
    pre_roll: VecDeque<f32>,
    hangover: usize,
    noise: f32,
}

impl SpeechGate {
    pub fn process(&mut self, chunk: &[f32]) -> GateOutput {
        let was_active = self.hangover > 0;
        let level = rms(chunk);
        let voiced = level >= (self.noise * SPEECH_OVER_NOISE).max(MIN_SPEECH_LEVEL);
        let tracking = if voiced {
            NOISE_RISE_SECONDS
        } else {
            NOISE_TRACKING_SECONDS
        };
        self.noise += (level - self.noise) * smoothing(chunk.len(), tracking);

        if voiced {
            self.hangover = HANGOVER_SAMPLES;
            let mut audio: Vec<f32> = if was_active {
                Vec::with_capacity(chunk.len())
            } else {
                self.pre_roll.drain(..).collect()
            };
            audio.extend_from_slice(chunk);
            return GateOutput {
                audio,
                ended: false,
                voiced: true,
            };
        }
        if was_active {
            self.hangover = self.hangover.saturating_sub(chunk.len());
            return GateOutput {
                audio: chunk.to_vec(),
                ended: self.hangover == 0,
                voiced: false,
            };
        }
        self.pre_roll.extend(chunk);
        let excess = self.pre_roll.len().saturating_sub(PRE_ROLL_SAMPLES);
        self.pre_roll.drain(..excess);
        GateOutput {
            audio: Vec::new(),
            ended: false,
            voiced: false,
        }
    }
}

/// Brings speech to a steady level. The model's features depend on loudness,
/// so a quiet microphone otherwise looks like a different kind of audio.
#[derive(Debug)]
pub struct LevelControl {
    level: f32,
    gain: f32,
}

impl Default for LevelControl {
    fn default() -> Self {
        Self {
            level: 0.0,
            gain: 1.0,
        }
    }
}

impl LevelControl {
    /// Scales `audio` in place. Only `voiced` audio updates the level, so
    /// pauses aren't turned up into noise.
    pub fn apply(&mut self, audio: &mut [f32], voiced: bool) {
        if audio.is_empty() {
            return;
        }
        if voiced {
            let measured = rms(audio);
            let rate = if measured > self.level {
                smoothing(audio.len(), LEVEL_ATTACK_SECONDS)
            } else {
                smoothing(audio.len(), LEVEL_RELEASE_SECONDS)
            };
            self.level += (measured - self.level) * rate;
        }
        let target = if self.level > 0.0 {
            (TARGET_SPEECH_LEVEL / self.level).clamp(MIN_GAIN, MAX_GAIN)
        } else {
            self.gain
        };
        let step = (target - self.gain) / audio.len() as f32;
        for sample in audio.iter_mut() {
            self.gain += step;
            *sample = (*sample * self.gain).clamp(-1.0, 1.0);
        }
        self.gain = target;
    }

    pub fn reset(&mut self) {
        self.level = 0.0;
    }
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

/// A word the accurate model heard, and when it started.
#[derive(Debug, Clone, PartialEq)]
struct Word {
    text: String,
    start_seconds: f32,
}

/// Joins the model's pieces of words back into words. A piece that starts a
/// word is marked with a leading space or underline.
fn words_from_tokens(tokens: &[String], times: &[f32]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let piece = token.trim_start_matches([' ', '\u{2581}']);
        match words.last_mut() {
            Some(word) if piece.len() == token.len() => word.text.push_str(piece),
            _ => words.push(Word {
                text: piece.to_string(),
                start_seconds: times.get(index).copied().unwrap_or(0.0),
            }),
        }
    }
    words.retain(|word| !word.text.is_empty());
    words
}

/// Where each trigger phrase in `words` starts and ends, as word positions.
/// Only whole words count: "cardiff clip that" isn't a phrase.
fn phrase_positions(words: &[Word]) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for phrase in PHRASES {
        let phrase: Vec<&str> = phrase.split(' ').collect();
        for (start, run) in words.windows(phrase.len()).enumerate() {
            let matches = run
                .iter()
                .zip(&phrase)
                .all(|(word, expected)| word.text.eq_ignore_ascii_case(expected));
            if matches {
                found.push((start, start + phrase.len()));
            }
        }
    }
    found.sort_unstable();
    found
}

/// How a trigger phrase sits in what was heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// Nothing was said after it.
    Last,
    /// Words follow, but much more quietly, so someone else said them.
    BeforeBackground,
}

/// Finds a trigger phrase that finishes what was said in `audio`, as a spoken
/// command does. "Diff clip that was the old phrase" doesn't count, because
/// the same voice carries on.
fn finished_phrase(words: &[Word], audio: &[f32]) -> Option<Ending> {
    let (start, end) = *phrase_positions(words).last()?;
    let Some(next) = words.get(end) else {
        return Some(Ending::Last);
    };
    let sample = |seconds: f32| ((seconds * VOICE_SAMPLE_RATE as f32) as usize).min(audio.len());
    let (phrase_start, phrase_end) = (
        sample(words[start].start_seconds),
        sample(next.start_seconds),
    );
    let phrase_level = speech_level(&audio[phrase_start.min(phrase_end)..phrase_end]);
    let later_level = speech_level(&audio[phrase_end..]);
    (phrase_level > 0.0 && later_level <= phrase_level * BACKGROUND_LEVEL)
        .then_some(Ending::BeforeBackground)
}

/// Why the window is being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
    /// Speech just paused, so a phrase may have finished.
    AfterPause,
    /// Speech is still going. A phrase-like ending may be the start of a
    /// longer sentence, so it needs confirming by the next read.
    DuringSpeech,
}

/// Holds recent speech and decides when it's worth reading.
#[derive(Debug, Default)]
struct UtteranceWindow {
    audio: VecDeque<f32>,
    /// Speech heard since the window was last cleared.
    speech: usize,
    /// Speech heard since the last read after a pause.
    unread_speech: usize,
    pause: usize,
    since_read: usize,
}

impl UtteranceWindow {
    /// Adds gated audio, of which `spoken` samples were speech. Says when the
    /// window should be read now.
    fn push(&mut self, audio: &[f32], spoken: usize) -> Option<Read> {
        self.audio.extend(audio);
        let excess = self.audio.len().saturating_sub(WINDOW_SAMPLES);
        self.audio.drain(..excess);

        self.since_read += audio.len();
        self.speech += spoken;
        self.unread_speech += spoken;
        self.pause = if spoken > 0 {
            0
        } else {
            self.pause + audio.len()
        };

        if self.speech < MIN_SPEECH_SAMPLES {
            return None;
        }
        let read = if self.pause >= PAUSE_SAMPLES && self.unread_speech > 0 {
            Read::AfterPause
        } else if spoken > 0 && self.since_read >= CONTINUOUS_SPEECH_INTERVAL {
            Read::DuringSpeech
        } else {
            return None;
        };
        if read == Read::AfterPause {
            self.unread_speech = 0;
        }
        self.since_read = 0;
        Some(read)
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Follows the audio as it arrives. Light, but only catches clear speech.
struct LightDetector {
    spotter: KeywordSpotter,
    stream: OnlineStream,
}

impl LightDetector {
    fn new(
        model_dir: &Path,
        score: f32,
        threshold: f32,
        search_width: i32,
        keywords: Option<String>,
    ) -> Result<Self, VoiceError> {
        let mut config = KeywordSpotterConfig::default();
        config.model_config.transducer.encoder = Some(model_file(model_dir, "encoder.int8.onnx")?);
        config.model_config.transducer.decoder = Some(model_file(model_dir, "decoder.int8.onnx")?);
        config.model_config.transducer.joiner = Some(model_file(model_dir, "joiner.int8.onnx")?);
        config.model_config.tokens = Some(model_file(model_dir, "tokens.txt")?);
        config.model_config.num_threads = 1;
        config.model_config.provider = Some("cpu".into());
        config.keywords_buf = Some(keywords.unwrap_or_else(|| KEYWORDS.into()));
        config.keywords_score = score;
        config.keywords_threshold = threshold;
        config.max_active_paths = search_width;
        config.num_trailing_blanks = 1;

        let spotter = KeywordSpotter::create(&config).ok_or(VoiceError::ModelUnavailable)?;
        let stream = spotter.create_stream();
        Ok(Self { spotter, stream })
    }

    fn accept(&mut self, audio: &[f32]) -> bool {
        let mut detected = false;
        self.stream.accept_waveform(VOICE_SAMPLE_RATE as i32, audio);
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
        detected
    }

    fn reset(&mut self) {
        self.spotter.reset(&self.stream);
    }

    fn contains_phrase(&self, audio: &[f32]) -> bool {
        let stream = self.spotter.create_stream_with_keywords(ECHO_KEYWORDS);
        stream.accept_waveform(VOICE_SAMPLE_RATE as i32, audio);
        stream.accept_waveform(VOICE_SAMPLE_RATE as i32, &vec![0.0; ECHO_TRAILING_SILENCE]);
        stream.input_finished();
        while self.spotter.is_ready(&stream) {
            self.spotter.decode(&stream);
            if self
                .spotter
                .get_result(&stream)
                .is_some_and(|result| !result.keyword.is_empty())
            {
                return true;
            }
        }
        false
    }
}

/// Reads whole stretches of speech with a larger model.
struct AccurateDetector {
    recognizer: OfflineRecognizer,
    window: UtteranceWindow,
    /// The last read during speech ended like a phrase.
    possible_phrase: bool,
}

impl AccurateDetector {
    fn new(model_dir: &Path, boost: f32, search_width: i32) -> Result<Self, VoiceError> {
        let mut config = OfflineRecognizerConfig::default();
        config.model_config.transducer.encoder = Some(model_file(model_dir, "encoder.int8.onnx")?);
        config.model_config.transducer.decoder = Some(model_file(model_dir, "decoder.int8.onnx")?);
        config.model_config.transducer.joiner = Some(model_file(model_dir, "joiner.int8.onnx")?);
        config.model_config.tokens = Some(model_file(model_dir, "tokens.txt")?);
        config.model_config.num_threads = 1;
        config.model_config.provider = Some("cpu".into());
        // Needed to turn the phrases into the pieces of words the model uses.
        config.model_config.modeling_unit = Some("bpe".into());
        config.model_config.bpe_vocab = Some(model_file(model_dir, "bpe.vocab")?);
        config.decoding_method = Some("modified_beam_search".into());
        config.max_active_paths = search_width;
        config.hotwords_score = boost;

        let recognizer = OfflineRecognizer::create(&config).ok_or(VoiceError::ModelUnavailable)?;
        Ok(Self {
            recognizer,
            window: UtteranceWindow::default(),
            possible_phrase: false,
        })
    }

    /// The words in `audio`. `boost` overrides the usual lean towards the
    /// trigger phrases.
    fn words(&self, audio: &[f32], boost: Option<f32>) -> Vec<Word> {
        let phrases: Vec<String> = PHRASES
            .iter()
            .map(|phrase| match boost {
                Some(boost) => format!("{phrase} :{boost}"),
                None => phrase.to_string(),
            })
            .collect();
        let audio = &audio[audio.len().saturating_sub(MAX_READ_SAMPLES)..];
        let stream = self
            .recognizer
            .create_stream_with_hotwords(&phrases.join("/"));
        stream.accept_waveform(VOICE_SAMPLE_RATE as i32, audio);
        self.recognizer.decode(&stream);
        stream
            .get_result()
            .map(|result| words_from_tokens(&result.tokens, &result.timestamps.unwrap_or_default()))
            .unwrap_or_default()
    }

    fn accept(&mut self, audio: &[f32], spoken: usize) -> bool {
        let Some(read) = self.window.push(audio, spoken) else {
            return false;
        };
        let mut recent: Vec<f32> = self.window.audio.iter().copied().collect();
        normalize_speech(&mut recent);
        let mut ending = finished_phrase(&self.words(&recent, None), &recent);
        if ending.is_none() && read == Read::AfterPause && recent.len() > CLOSE_READ_SAMPLES {
            let close = &recent[recent.len() - CLOSE_READ_SAMPLES..];
            ending = finished_phrase(&self.words(close, None), close);
        }
        let detected = match (ending, read) {
            (None, _) => false,
            (Some(_), Read::AfterPause) | (Some(Ending::BeforeBackground), _) => true,
            // The speaker may be carrying on, which the next read will show.
            (Some(Ending::Last), Read::DuringSpeech) => self.possible_phrase,
        };
        self.possible_phrase = ending.is_some() && !detected;
        if detected {
            self.reset();
        }
        detected
    }

    fn reset(&mut self) {
        self.window.clear();
        self.possible_phrase = false;
    }

    fn contains_phrase(&self, audio: &[f32]) -> bool {
        let heard = |audio: &[f32]| {
            let mut levelled = audio.to_vec();
            normalize_speech(&mut levelled);
            !phrase_positions(&self.words(&levelled, Some(ECHO_PHRASE_BOOST))).is_empty()
        };
        // Anything longer than the model can take is read in overlapping
        // stretches, so a phrase can't be lost across a join.
        let step = MAX_READ_SAMPLES - WINDOW_SAMPLES;
        let whole = (0..audio.len())
            .step_by(step)
            .any(|start| heard(&audio[start..(start + MAX_READ_SAMPLES).min(audio.len())]));
        // The phrase the microphone just heard is at the end, so look there
        // again on its own.
        whole
            || (audio.len() > CLOSE_READ_SAMPLES
                && heard(&audio[audio.len() - CLOSE_READ_SAMPLES..]))
    }
}

fn model_file(model_dir: &Path, name: &str) -> Result<String, VoiceError> {
    let path = model_dir.join(name);
    path.exists()
        .then(|| path.to_string_lossy().into_owned())
        .ok_or(VoiceError::ModelUnavailable)
}

enum Detector {
    Light(LightDetector),
    Accurate(AccurateDetector),
}

pub struct VoiceSpotter {
    detector: Detector,
    gate: SpeechGate,
    level: LevelControl,
}

// SAFETY: the detector is used only by the one thread that owns this value,
// and the underlying library objects aren't tied to a thread.
unsafe impl Send for VoiceSpotter {}

impl VoiceSpotter {
    /// `models_dir` holds a folder for each [`VoiceModel`].
    pub fn new(models_dir: &Path, model: VoiceModel) -> Result<Self, VoiceError> {
        let detector = match model {
            VoiceModel::Light => Detector::Light(LightDetector::new(
                &models_dir.join("light"),
                KEYWORDS_SCORE,
                KEYWORDS_THRESHOLD,
                KEYWORDS_SEARCH_WIDTH,
                None,
            )?),
            VoiceModel::Accurate => Detector::Accurate(AccurateDetector::new(
                &models_dir.join("accurate"),
                PHRASE_BOOST,
                SEARCH_WIDTH,
            )?),
        };
        Ok(Self::with_detector(detector))
    }

    /// The light detector with explicit sensitivity and phrases, for accuracy
    /// tests. `model_dir` is the light model's own folder.
    pub fn light_with_tuning(
        model_dir: &Path,
        score: f32,
        threshold: f32,
        search_width: i32,
        keywords: Option<String>,
    ) -> Result<Self, VoiceError> {
        let detector = LightDetector::new(model_dir, score, threshold, search_width, keywords)?;
        Ok(Self::with_detector(Detector::Light(detector)))
    }

    /// The accurate detector with an explicit lean towards the phrases, for
    /// accuracy tests. `model_dir` is the accurate model's own folder.
    pub fn accurate_with_tuning(
        model_dir: &Path,
        boost: f32,
        search_width: i32,
    ) -> Result<Self, VoiceError> {
        let detector = AccurateDetector::new(model_dir, boost, search_width)?;
        Ok(Self::with_detector(Detector::Accurate(detector)))
    }

    fn with_detector(detector: Detector) -> Self {
        Self {
            detector,
            gate: SpeechGate::default(),
            level: LevelControl::default(),
        }
    }

    /// True if a trigger phrase occurs in `audio` (16 kHz mono). Takes a
    /// fraction of a second, and only runs after the microphone hears a phrase.
    pub fn contains_phrase(&self, audio: &[f32]) -> bool {
        if audio.is_empty() {
            return false;
        }
        match &self.detector {
            Detector::Light(detector) => detector.contains_phrase(audio),
            Detector::Accurate(detector) => detector.contains_phrase(audio),
        }
    }

    /// Feeds 16 kHz mono audio. Returns true if a trigger phrase just finished.
    pub fn feed(&mut self, chunk: &[f32]) -> bool {
        let GateOutput {
            mut audio,
            ended,
            voiced,
        } = self.gate.process(chunk);
        let detected = !audio.is_empty()
            && match &mut self.detector {
                Detector::Light(detector) => {
                    // The light model follows the audio live, so it's levelled as it arrives.
                    self.level.apply(&mut audio, voiced);
                    detector.accept(&audio)
                }
                Detector::Accurate(detector) => {
                    detector.accept(&audio, if voiced { chunk.len() } else { 0 })
                }
            };
        if ended {
            self.level.reset();
            match &mut self.detector {
                Detector::Light(detector) => detector.reset(),
                Detector::Accurate(detector) => detector.reset(),
            }
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
            assert!(gate.process(&[0.0; CHUNK]).audio.is_empty());
        }
    }

    #[test]
    fn speech_includes_the_audio_just_before_it() {
        let mut gate = SpeechGate::default();
        gate.process(&[0.0; CHUNK]);
        assert_eq!(gate.process(&[0.1; CHUNK]).audio.len(), CHUNK * 2);
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
            let output = gate.process(&[0.0; CHUNK]);
            assert_eq!(output.audio.len(), CHUNK);
            if output.ended {
                ended_after = Some(index);
                break;
            }
        }
        assert_eq!(ended_after, Some(HANGOVER_SAMPLES.div_ceil(CHUNK)));
        assert!(gate.process(&[0.0; CHUNK]).audio.is_empty());
    }

    #[test]
    fn quiet_speech_opens_the_gate_in_a_quiet_room() {
        let mut gate = SpeechGate::default();
        for _ in 0..30 {
            gate.process(&[0.0001; CHUNK]);
        }
        assert!(gate.process(&[0.002; CHUNK]).voiced);
    }

    #[test]
    fn steady_room_noise_stops_counting_as_speech() {
        let mut gate = SpeechGate::default();
        for _ in 0..400 {
            gate.process(&[0.01; CHUNK]);
        }
        assert!(!gate.process(&[0.01; CHUNK]).voiced);
        assert!(gate.process(&[0.05; CHUNK]).voiced);
    }

    #[test]
    fn level_control_brings_quiet_speech_up_to_the_target() {
        let mut level = LevelControl::default();
        let mut audio = vec![0.0];
        for _ in 0..20 {
            audio = vec![0.004; CHUNK];
            level.apply(&mut audio, true);
        }
        assert!(
            (rms(&audio) - TARGET_SPEECH_LEVEL).abs() < 0.01,
            "{}",
            rms(&audio)
        );
    }

    #[test]
    fn level_control_does_not_clip_past_full_scale() {
        let mut level = LevelControl {
            level: 0.001,
            gain: MAX_GAIN,
        };
        let mut audio = vec![0.9; CHUNK];
        level.apply(&mut audio, false);
        assert!(audio.iter().all(|sample| sample.abs() <= 1.0));
    }

    #[test]
    fn normalizing_ignores_pauses_when_judging_the_level() {
        let mut audio = vec![0.0; CHUNK];
        audio.extend([0.01, -0.01].repeat(CHUNK));
        normalize_speech(&mut audio);
        assert_eq!(audio[0], 0.0);
        assert!((audio.last().unwrap().abs() - TARGET_SPEECH_LEVEL).abs() < 0.001);
    }

    #[test]
    fn pauses_do_not_change_the_level() {
        let mut level = LevelControl::default();
        level.apply(&mut vec![0.05; CHUNK], true);
        let before = level.level;
        level.apply(&mut vec![0.0001; CHUNK], false);
        assert_eq!(level.level, before);
    }

    /// Words spaced half a second apart, as the model would report them.
    fn heard(text: &str) -> Vec<Word> {
        text.split(' ')
            .filter(|word| !word.is_empty())
            .enumerate()
            .map(|(index, word)| Word {
                text: word.to_string(),
                start_seconds: index as f32 * 0.5,
            })
            .collect()
    }

    /// Audio with each half-second word of `levels` at the given level.
    fn spoken(levels: &[f32]) -> Vec<f32> {
        levels
            .iter()
            .flat_map(|level| [*level, -*level].repeat(VOICE_SAMPLE_RATE / 4))
            .collect()
    }

    #[test]
    fn pieces_of_words_are_joined_into_words() {
        let tokens: Vec<String> = [" HE", "Y", " DI", "FF", " C", "LI", "P", " THAT"]
            .iter()
            .map(|token| token.to_string())
            .collect();
        let times = [0.1, 0.2, 0.4, 0.5, 0.7, 0.8, 0.9, 1.1];
        let words = words_from_tokens(&tokens, &times);
        let text: Vec<&str> = words.iter().map(|word| word.text.as_str()).collect();
        assert_eq!(text, ["HEY", "DIFF", "CLIP", "THAT"]);
        assert_eq!(words[1].start_seconds, 0.4);
    }

    #[test]
    fn every_way_of_saying_the_phrase_matches() {
        for text in [
            "DIFF CLIP THAT",
            "HEY DIFF CLIP THAT",
            "diff clip that",
            "DIFF SAVE THAT",
            "DIFF CLIP IT",
            "DIFF CLIPPED THAT",
            "OH MY GOD DIFF CLIP THAT",
        ] {
            assert_eq!(
                finished_phrase(&heard(text), &[]),
                Some(Ending::Last),
                "{text}"
            );
        }
    }

    #[test]
    fn other_speech_does_not_match() {
        for text in [
            "",
            "CLIP THAT",
            "SAVE THAT",
            "IF CLIP THAT",
            "DID YOU CLIP THAT",
            "DUDE CLIP THAT",
            "THAT WAS DIFFICULT",
            "IT'S A BIT DIFFERENT",
            "THIS IS THE DEFAULT",
            "THAT WAS THE FIRST",
            "CARDIFF CLIP THAT",
            "DIFF CLIP THAT'S",
            "NICE SHOT MAN THAT WAS REALLY CLOSE",
            "THAT WAS A DIFFICULT CLIP TO WATCH",
        ] {
            assert!(phrase_positions(&heard(text)).is_empty(), "{text}");
        }
    }

    #[test]
    fn a_phrase_the_speaker_carries_on_from_is_not_a_command() {
        let words = heard("DIFF CLIP THAT YESTERDAY WAS GREAT");
        assert_eq!(finished_phrase(&words, &spoken(&[0.1; 6])), None);
        // The computer's own sound is checked for the phrase anywhere.
        assert_eq!(phrase_positions(&words), [(0, 3)]);
    }

    #[test]
    fn quieter_talk_after_a_phrase_is_background() {
        let words = heard("DIFF CLIP THAT YESTERDAY WAS GREAT");
        let audio = spoken(&[0.1, 0.1, 0.1, 0.02, 0.02, 0.02]);
        assert_eq!(
            finished_phrase(&words, &audio),
            Some(Ending::BeforeBackground)
        );
    }

    #[test]
    fn window_reads_once_after_a_pause() {
        let mut window = UtteranceWindow::default();
        assert_eq!(window.push(&[0.1; 8_000], 8_000), None);
        assert_eq!(window.push(&[0.0; 1_600], 0), None);
        assert_eq!(
            window.push(&[0.0; PAUSE_SAMPLES], 0),
            Some(Read::AfterPause)
        );
        assert_eq!(window.push(&[0.0; PAUSE_SAMPLES], 0), None);
    }

    #[test]
    fn window_ignores_short_noises() {
        let mut window = UtteranceWindow::default();
        window.push(&[0.1; 1_600], 1_600);
        assert_eq!(window.push(&[0.0; PAUSE_SAMPLES], 0), None);
    }

    #[test]
    fn window_reads_during_speech_with_no_pauses() {
        let mut window = UtteranceWindow::default();
        let reads = (0..30)
            .filter(|_| window.push(&[0.1; CHUNK], CHUNK) == Some(Read::DuringSpeech))
            .count();
        assert_eq!(reads, 30 * CHUNK / CONTINUOUS_SPEECH_INTERVAL);
    }

    #[test]
    fn a_pause_after_a_read_during_speech_still_reads() {
        let mut window = UtteranceWindow::default();
        let mut reads = Vec::new();
        for _ in 0..14 {
            reads.extend(window.push(&[0.1; CHUNK], CHUNK));
        }
        reads.extend(window.push(&[0.0; PAUSE_SAMPLES], 0));
        assert_eq!(reads, [Read::DuringSpeech, Read::AfterPause]);
    }

    #[test]
    fn window_is_bounded() {
        let mut window = UtteranceWindow::default();
        for _ in 0..500 {
            window.push(&[0.1; CHUNK], CHUNK);
        }
        assert_eq!(window.audio.len(), WINDOW_SAMPLES);
    }
}
