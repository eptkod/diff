//! Mixes system audio with the optional microphone, encodes it to AAC, and
//! captures the microphone for both clips and voice detection.

use crate::buffer::Packet;
use crate::capture::SharedRecording;
use crate::echo::EchoGuard;
use crate::mux::AudioFormat;
use crate::voice::VOICE_SAMPLE_RATE;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

pub const AUDIO_FORMAT: AudioFormat =
    AudioFormat { sample_rate: 48_000, channels: 2, bitrate: 160_000 };
const SAMPLE_RATE: i64 = AUDIO_FORMAT.sample_rate as i64;
const CHANNELS: usize = AUDIO_FORMAT.channels as usize;
/// Samples (both channels) in one AAC frame.
const AAC_FRAME_SAMPLES: usize = 1024 * CHANNELS;
const AAC_FRAME_US: i64 = 1024 * 1_000_000 / SAMPLE_RATE;

/// Smaller timing differences are normal jitter and are ignored.
const GAP_TOLERANCE_US: i64 = 50_000;
/// A longer gap than this means the stream restarted; don't fill it all.
const MAX_GAP_US: i64 = 10_000_000;
/// System audio can arrive this late, so silence is only filled in behind it.
const PUMP_MARGIN_US: i64 = 150_000;

/// How much microphone audio may wait to be mixed, as mono 48 kHz samples.
const MIC_QUEUE_LIMIT: usize = SAMPLE_RATE as usize / 2;
/// Mic audio beyond this much backlog is dropped to keep it in sync.
const MIC_MAX_BACKLOG: usize = SAMPLE_RATE as usize / 5;
const VOICE_CHANNEL_CHUNKS: usize = 64;

pub trait AacEncoder: Send {
    /// Encodes exactly one AAC frame of interleaved stereo samples. Returns
    /// `None` while the encoder is still filling its internal delay.
    fn encode(&mut self, pcm: &[f32]) -> Result<Option<Vec<u8>>, String>;

    /// Frames of silence the encoder adds to the start of its output.
    fn priming_frames(&self) -> i64;
}

/// Microphone audio waiting to be mixed in, as mono 48 kHz.
#[derive(Debug, Default)]
pub struct MicQueue {
    samples: Mutex<VecDeque<f32>>,
    heard_signal: AtomicBool,
}

impl MicQueue {
    pub fn push(&self, samples: &[f32]) {
        // A blocked or muted microphone delivers perfect digital silence.
        if samples.iter().any(|sample| *sample != 0.0) {
            self.heard_signal.store(true, Ordering::Relaxed);
        }
        let mut queue = self.samples.lock().expect("mic queue lock");
        queue.extend(samples);
        let excess = queue.len().saturating_sub(MIC_QUEUE_LIMIT);
        queue.drain(..excess);
    }

    /// Removes up to `count` of the most timely samples.
    fn take(&self, count: usize) -> Vec<f32> {
        let mut queue = self.samples.lock().expect("mic queue lock");
        let excess = queue.len().saturating_sub(count + MIC_MAX_BACKLOG);
        queue.drain(..excess);
        let available = count.min(queue.len());
        queue.drain(..available).collect()
    }

    /// True once any real audio has arrived since the last reset.
    pub fn heard_signal(&self) -> bool {
        self.heard_signal.load(Ordering::Relaxed)
    }

    pub fn reset_signal(&self) {
        self.heard_signal.store(false, Ordering::Relaxed);
    }

    pub fn clear(&self) {
        self.samples.lock().expect("mic queue lock").clear();
    }
}

/// Builds one continuous AAC stream. Backends call [`AudioMixer::push_system`]
/// with whatever audio the OS delivers, and gaps become silence so that audio
/// and video stay aligned.
pub struct AudioMixer {
    encoder: Box<dyn AacEncoder>,
    recording: SharedRecording,
    mic: Option<Arc<MicQueue>>,
    echo: Option<Arc<EchoGuard>>,
    origin_us: Option<i64>,
    written_frames: i64,
    packets_emitted: i64,
    pending: Vec<f32>,
}

impl AudioMixer {
    pub fn new(
        encoder: Box<dyn AacEncoder>,
        recording: SharedRecording,
        mic: Option<Arc<MicQueue>>,
        echo: Option<Arc<EchoGuard>>,
    ) -> Self {
        Self {
            encoder,
            recording,
            mic,
            echo,
            origin_us: None,
            written_frames: 0,
            packets_emitted: 0,
            pending: Vec::with_capacity(AAC_FRAME_SAMPLES * 4),
        }
    }

    fn timeline_us(&self, origin: i64, frames: i64) -> i64 {
        origin + frames * 1_000_000 / SAMPLE_RATE
    }

    /// Adds interleaved stereo system audio captured at `pts_us`.
    pub fn push_system(&mut self, pts_us: i64, interleaved: &[f32]) {
        if let Some(echo) = &self.echo {
            echo.push_system(interleaved);
        }
        let origin = *self.origin_us.get_or_insert(pts_us);
        let gap = pts_us - self.timeline_us(origin, self.written_frames);
        if gap > GAP_TOLERANCE_US {
            self.fill_silence(origin, pts_us, gap);
        }
        // Audio older than the timeline was already covered by `pump`.
        let overlap = if gap < -GAP_TOLERANCE_US {
            ((-gap * SAMPLE_RATE / 1_000_000) as usize * CHANNELS).min(interleaved.len())
        } else {
            0
        };
        self.append(&interleaved[overlap..]);
    }

    /// Keeps the audio moving when the OS delivers nothing, as it may while no
    /// sound is playing. Without this, the microphone would go unrecorded
    /// during silence. Call regularly with the current capture-clock time.
    pub fn pump(&mut self, now_us: i64) {
        let target = now_us - PUMP_MARGIN_US;
        let origin = *self.origin_us.get_or_insert(target);
        let behind = target - self.timeline_us(origin, self.written_frames);
        if behind > 0 {
            self.fill_silence(origin, target, behind);
        }
    }

    fn fill_silence(&mut self, origin: i64, target_us: i64, gap: i64) {
        let frames = (gap.min(MAX_GAP_US) * SAMPLE_RATE / 1_000_000) as usize;
        self.append(&vec![0.0; frames * CHANNELS]);
        if gap > MAX_GAP_US {
            // Skip the rest of the gap on the timeline without encoding it.
            self.written_frames = (target_us - origin) * SAMPLE_RATE / 1_000_000;
            self.packets_emitted = self.written_frames / 1024;
            self.pending.clear();
        }
    }

    fn append(&mut self, interleaved: &[f32]) {
        self.written_frames += (interleaved.len() / CHANNELS) as i64;
        let start = self.pending.len();
        self.pending.extend_from_slice(interleaved);
        if let Some(mic) = &self.mic {
            let voice = mic.take(interleaved.len() / CHANNELS);
            for (frame, sample) in self.pending[start..].chunks_exact_mut(CHANNELS).zip(voice) {
                for channel in frame {
                    *channel = (*channel + sample).clamp(-1.0, 1.0);
                }
            }
        }
        self.drain_frames();
    }

    fn drain_frames(&mut self) {
        let Some(origin) = self.origin_us else { return };
        let mut consumed = 0;
        while self.pending.len() - consumed >= AAC_FRAME_SAMPLES {
            let frame = &self.pending[consumed..consumed + AAC_FRAME_SAMPLES];
            consumed += AAC_FRAME_SAMPLES;
            let Ok(Some(bytes)) = self.encoder.encode(frame) else { continue };
            // Each output packet covers a fixed slice of the input, offset
            // by the silence the encoder prepends.
            let position = self.packets_emitted * 1024 - self.encoder.priming_frames();
            self.packets_emitted += 1;
            let pts = self.timeline_us(origin, position);
            if let Ok(mut recording) = self.recording.lock() {
                recording.ring.push_audio(Packet::audio(pts, AAC_FRAME_US, bytes));
            }
        }
        self.pending.drain(..consumed);
    }
}

/// Simple streaming sample-rate converter. Downsampling averages the input
/// between outputs, which is enough to stop speech frequencies from aliasing.
#[derive(Debug)]
pub struct Resampler {
    step: f64,
    position: f64,
    last: f32,
    sum: f32,
    count: u32,
}

impl Resampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            step: input_rate as f64 / output_rate as f64,
            position: 1.0,
            last: 0.0,
            sum: 0.0,
            count: 0,
        }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut output = Vec::with_capacity((input.len() as f64 / self.step) as usize + 2);
        for &sample in input {
            self.sum += sample;
            self.count += 1;
            while self.position <= 1.0 {
                output.push(if self.step > 1.0 {
                    self.sum / self.count.max(1) as f32
                } else {
                    self.last + (sample - self.last) * self.position as f32
                });
                self.position += self.step;
                self.sum = 0.0;
                self.count = 0;
            }
            self.position -= 1.0;
            self.last = sample;
        }
        output
    }
}

struct MicSinks {
    downmix: Vec<f32>,
    clip_resampler: Resampler,
    voice_resampler: Resampler,
    clip: Arc<MicQueue>,
    voice: Option<SyncSender<Vec<f32>>>,
    channels: usize,
}

impl MicSinks {
    fn handle<T>(&mut self, data: &[T])
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        self.downmix.clear();
        self.downmix.extend(data.chunks_exact(self.channels).map(|frame| {
            frame.iter().map(|s| f32::from_sample_(*s)).sum::<f32>() / self.channels as f32
        }));
        self.clip.push(&self.clip_resampler.process(&self.downmix));
        if let Some(voice) = &self.voice {
            // If detection falls behind, drop audio rather than block capture.
            let chunk = self.voice_resampler.process(&self.downmix);
            if let Err(TrySendError::Disconnected(_)) = voice.try_send(chunk) {
                self.voice = None;
            }
        }
    }
}

/// A running microphone capture. Stops when dropped.
pub struct Microphone {
    stop: Option<SyncSender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Microphone {
    /// Opens the default input device. Audio is delivered to `clip` always and
    /// to the returned receiver (16 kHz mono) when `with_voice` is set.
    pub fn start(
        clip: Arc<MicQueue>,
        with_voice: bool,
    ) -> Result<(Self, Option<Receiver<Vec<f32>>>), String> {
        let (voice_tx, voice_rx) = if with_voice {
            let (tx, rx) = sync_channel(VOICE_CHANNEL_CHUNKS);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let (ready_tx, ready_rx) = sync_channel(1);
        let (stop_tx, stop_rx) = sync_channel::<()>(1);

        // The stream isn't `Send` on every OS, so it lives on its own thread.
        let thread = std::thread::spawn(move || match open_stream(clip, voice_tx) {
            Ok(stream) => {
                let _ = ready_tx.send(Ok(()));
                let _ = stop_rx.recv();
                drop(stream);
            }
            Err(message) => {
                let _ = ready_tx.send(Err(message));
            }
        });

        match ready_rx.recv() {
            Ok(Ok(())) => {
                Ok((Self { stop: Some(stop_tx), thread: Some(thread) }, voice_rx))
            }
            Ok(Err(message)) => Err(message),
            Err(_) => Err("the microphone thread stopped unexpectedly".into()),
        }
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn open_stream(
    clip: Arc<MicQueue>,
    voice: Option<SyncSender<Vec<f32>>>,
) -> Result<cpal::Stream, String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("no microphone was found")?;
    let config = device.default_input_config().map_err(|e| e.to_string())?;
    let rate = config.sample_rate();
    let sinks = MicSinks {
        downmix: Vec::new(),
        clip_resampler: Resampler::new(rate, AUDIO_FORMAT.sample_rate),
        voice_resampler: Resampler::new(rate, VOICE_SAMPLE_RATE as u32),
        clip,
        voice,
        channels: config.channels().max(1) as usize,
    };
    let format = config.sample_format();
    let config = config.config();

    fn build<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        mut sinks: MicSinks,
    ) -> Result<cpal::Stream, cpal::Error>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        device.build_input_stream::<T, _, _>(
            *config,
            move |data, _| sinks.handle(data),
            |_| {},
            None,
        )
    }

    let stream = match format {
        SampleFormat::F32 => build::<f32>(&device, &config, sinks),
        SampleFormat::I16 => build::<i16>(&device, &config, sinks),
        SampleFormat::U16 => build::<u16>(&device, &config, sinks),
        SampleFormat::I32 => build::<i32>(&device, &config, sinks),
        other => return Err(format!("unsupported microphone format: {other}")),
    }
    .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::new_recording;
    use crate::buffer::Packet as BufferPacket;

    /// Records each frame's first sample instead of encoding.
    struct FakeEncoder {
        frames: Arc<Mutex<Vec<Vec<f32>>>>,
        priming: i64,
        delay_packets: usize,
    }

    impl AacEncoder for FakeEncoder {
        fn encode(&mut self, pcm: &[f32]) -> Result<Option<Vec<u8>>, String> {
            self.frames.lock().unwrap().push(pcm.to_vec());
            if self.delay_packets > 0 {
                self.delay_packets -= 1;
                return Ok(None);
            }
            Ok(Some(vec![1, 2, 3]))
        }

        fn priming_frames(&self) -> i64 {
            self.priming
        }
    }

    fn mixer(mic: Option<Arc<MicQueue>>, priming: i64) -> (AudioMixer, SharedRecording, Arc<Mutex<Vec<Vec<f32>>>>) {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let recording = new_recording(60_000_000, usize::MAX);
        {
            // One long GOP, so no audio in these tests falls outside the video.
            let ring = &mut recording.lock().unwrap().ring;
            ring.push_video(BufferPacket::video(0, true, vec![0u8]));
            ring.push_video(BufferPacket::video(4_000_000_000, false, vec![0u8]));
        }
        let encoder = FakeEncoder { frames: frames.clone(), priming, delay_packets: 0 };
        (AudioMixer::new(Box::new(encoder), recording.clone(), mic, None), recording, frames)
    }

    fn audio_pts(recording: &SharedRecording) -> Vec<i64> {
        recording.lock().unwrap().ring.snapshot(i64::MAX / 2).unwrap().audio.iter().map(|p| p.pts_us).collect()
    }

    #[test]
    fn audio_is_cut_into_exact_aac_frames() {
        let (mut mixer, _, frames) = mixer(None, 0);
        mixer.push_system(0, &vec![0.25; 5000]);
        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.len() == AAC_FRAME_SAMPLES));
    }

    #[test]
    fn leftover_samples_carry_into_the_next_push() {
        let (mut mixer, _, frames) = mixer(None, 0);
        mixer.push_system(0, &vec![0.25; 1500]);
        assert_eq!(frames.lock().unwrap().len(), 0);
        mixer.push_system(15_625, &vec![0.25; 1500]);
        assert_eq!(frames.lock().unwrap().len(), 1);
    }

    #[test]
    fn timestamps_account_for_encoder_priming() {
        let (mut mixer, recording, _) = mixer(None, 2112);
        mixer.push_system(44_000, &vec![0.0; AAC_FRAME_SAMPLES * 3]);
        let pts = audio_pts(&recording);
        assert_eq!(pts[0], 0);
        assert!((pts[1] - pts[0] - 21_333).abs() <= 1, "spacing was {}", pts[1] - pts[0]);
    }

    #[test]
    fn gaps_are_filled_with_silence() {
        let (mut mixer, _, frames) = mixer(None, 0);
        mixer.push_system(0, &vec![0.5; AAC_FRAME_SAMPLES]);
        mixer.push_system(500_000, &vec![0.5; AAC_FRAME_SAMPLES]);
        let frames = frames.lock().unwrap();
        // 21 ms of audio, then 479 ms of silence, then 21 ms more.
        assert_eq!(frames.len(), 24);
        assert_eq!(frames[0][0], 0.5);
        assert_eq!(frames[10][0], 0.0);
    }

    #[test]
    fn small_timing_jitter_is_ignored() {
        let (mut mixer, _, frames) = mixer(None, 0);
        mixer.push_system(0, &vec![0.5; AAC_FRAME_SAMPLES]);
        mixer.push_system(21_333 + GAP_TOLERANCE_US - 1, &vec![0.5; AAC_FRAME_SAMPLES]);
        assert_eq!(frames.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_very_long_gap_does_not_encode_forever() {
        let (mut mixer, recording, frames) = mixer(None, 0);
        mixer.push_system(0, &vec![0.5; AAC_FRAME_SAMPLES]);
        mixer.push_system(3_600_000_000, &vec![0.5; AAC_FRAME_SAMPLES]);
        assert!(frames.lock().unwrap().len() < 500);
        let last = *audio_pts(&recording).last().unwrap();
        assert!((last - 3_600_000_000).abs() < 50_000, "last packet at {last}");
    }

    #[test]
    fn microphone_is_mixed_into_both_channels_and_clamped() {
        let mic = Arc::new(MicQueue::default());
        mic.push(&vec![0.75; 1024]);
        let (mut mixer, _, frames) = mixer(Some(mic), 0);
        mixer.push_system(0, &vec![0.5; AAC_FRAME_SAMPLES]);
        let frames = frames.lock().unwrap();
        assert!(frames[0].iter().all(|s| *s == 1.0));
    }

    #[test]
    fn a_quiet_microphone_leaves_system_audio_untouched() {
        let mic = Arc::new(MicQueue::default());
        let (mut mixer, _, frames) = mixer(Some(mic), 0);
        mixer.push_system(0, &vec![0.5; AAC_FRAME_SAMPLES]);
        assert!(frames.lock().unwrap()[0].iter().all(|s| *s == 0.5));
    }

    #[test]
    fn pump_records_the_microphone_while_no_system_audio_arrives() {
        let mic = Arc::new(MicQueue::default());
        let (mut mixer, _, frames) = mixer(Some(mic.clone()), 0);
        mixer.pump(0);
        for tick in 1..=20 {
            mic.push(&vec![0.25; 4800]);
            mixer.pump(tick * 100_000);
        }
        let frames = frames.lock().unwrap();
        // 2 seconds of audio, and the microphone is audible in it.
        assert!((90..=94).contains(&frames.len()), "{} frames", frames.len());
        assert!(frames[50].iter().all(|s| *s == 0.25));
    }

    #[test]
    fn pump_does_nothing_while_system_audio_is_keeping_up() {
        let (mut mixer, _, frames) = mixer(None, 0);
        mixer.push_system(0, &vec![0.5; AAC_FRAME_SAMPLES * 4]);
        mixer.pump(85_000);
        assert_eq!(frames.lock().unwrap().len(), 4);
    }

    #[test]
    fn system_audio_after_a_pump_lands_at_the_right_time() {
        let (mut mixer, recording, frames) = mixer(None, 0);
        mixer.pump(150_000);
        mixer.pump(1_150_000);
        // Starts 300 ms ahead of the pumped silence, which is a normal gap.
        mixer.push_system(1_300_000, &vec![0.5; AAC_FRAME_SAMPLES]);
        let frames = frames.lock().unwrap();
        let first_loud = frames.iter().position(|f| f.iter().any(|s| *s != 0.0)).unwrap();
        let pts = audio_pts(&recording);
        assert!((pts[first_loud] - 1_300_000).abs() < 25_000, "landed at {}", pts[first_loud]);
    }

    #[test]
    fn late_system_audio_does_not_stretch_the_timeline() {
        let (mut mixer, _, frames) = mixer(None, 0);
        mixer.pump(150_000);
        mixer.pump(1_150_000);
        // Fully inside the second that was already filled with silence.
        mixer.push_system(100_000, &vec![0.5; AAC_FRAME_SAMPLES]);
        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 46);
        assert!(frames.iter().all(|f| f.iter().all(|s| *s == 0.0)));
    }

    #[test]
    fn digital_silence_does_not_count_as_hearing_the_microphone() {
        let queue = MicQueue::default();
        queue.push(&[0.0; 100]);
        assert!(!queue.heard_signal());
        queue.push(&[0.0, 0.001]);
        assert!(queue.heard_signal());
        queue.reset_signal();
        assert!(!queue.heard_signal());
    }

    #[test]
    fn mic_queue_drops_stale_audio_instead_of_lagging() {
        let queue = MicQueue::default();
        queue.push(&vec![0.1; MIC_QUEUE_LIMIT]);
        queue.push(&[0.9; 100]);
        let taken = queue.take(100);
        assert_eq!(taken.len(), 100);
        assert_eq!(queue.samples.lock().unwrap().len(), MIC_MAX_BACKLOG);
    }

    #[test]
    fn resampler_produces_the_expected_amount() {
        let mut down = Resampler::new(48_000, 16_000);
        let mut up = Resampler::new(44_100, 48_000);
        let (mut down_count, mut up_count) = (0, 0);
        for _ in 0..100 {
            down_count += down.process(&[0.5; 480]).len();
            up_count += up.process(&[0.5; 441]).len();
        }
        assert!((down_count as i64 - 16_000).abs() <= 2, "{down_count}");
        assert!((up_count as i64 - 48_000).abs() <= 2, "{up_count}");
    }

    #[test]
    fn resampler_preserves_a_constant_signal() {
        let mut down = Resampler::new(48_000, 16_000);
        assert!(down.process(&[0.5; 960])[1..].iter().all(|s| (s - 0.5).abs() < 1e-5));
        let mut same = Resampler::new(48_000, 48_000);
        assert!(same.process(&[0.5; 960])[1..].iter().all(|s| (s - 0.5).abs() < 1e-5));
    }
}
