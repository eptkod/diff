//! Owns the running capture and microphone, and turns triggers into clips.

use crate::audio::{MicQueue, Microphone};
use crate::buffer::Snapshot;
use crate::capture::follow::{FollowStep, WindowCandidate, WindowFollower};
use crate::capture::{
    self, new_recording, Capture, CaptureBackend, CaptureError, CaptureEvents, SharedRecording,
    Source,
};
use crate::clip::{save_snapshot, take_snapshot, ClipError, Debouncer, TRIGGER_COOLDOWN};
use crate::echo::EchoGuard;
use crate::mux::{AudioFormat, VideoFormat};
use crate::settings::Settings;
use crate::voice::VoiceSpotter;
use chrono::Local;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Status {
    Stopped,
    Recording,
    Paused,
    NeedsPermission,
    Error { message: String },
}

/// Everything needed to write a clip, detached from the live buffer.
pub struct PendingClip {
    snapshot: Snapshot,
    video_format: VideoFormat,
    audio_format: Option<AudioFormat>,
    folder: PathBuf,
    /// What Automatic mode was recording, if that's the mode in use.
    pub window_name: Option<String>,
}

impl PendingClip {
    pub fn save(&self) -> Result<PathBuf, ClipError> {
        save_snapshot(
            &self.snapshot,
            &self.folder,
            &self.video_format,
            self.audio_format.as_ref(),
            Local::now(),
        )
    }
}

/// How the engine reaches back into the app. All are called off the main thread.
pub struct EngineHooks {
    pub on_voice_trigger: Arc<dyn Fn() + Send + Sync>,
    pub on_notice: Arc<dyn Fn(&str, &str) + Send + Sync>,
    pub on_status_changed: Arc<dyn Fn() + Send + Sync>,
}

/// The microphone and its detection thread. Dropping the microphone closes
/// the audio channel, which ends the thread.
struct MicRuntime {
    microphone: Option<Microphone>,
    voice_thread: Option<JoinHandle<()>>,
}

impl Drop for MicRuntime {
    fn drop(&mut self) {
        self.microphone.take();
        if let Some(thread) = self.voice_thread.take() {
            let _ = thread.join();
        }
    }
}

/// How often Automatic mode looks at which windows are open.
const FOLLOW_INTERVAL: Duration = Duration::from_secs(1);
const MIC_SILENCE_GRACE: Duration = Duration::from_secs(4);
const MIC_SILENT_MESSAGE: &str =
    "diff can't hear your microphone. Check that it's allowed in your privacy settings and isn't muted.";

pub struct Engine {
    recording: SharedRecording,
    capture: Mutex<Option<Capture>>,
    /// Held while starting or stopping capture, so a window switch can't
    /// resume recording after a pause.
    follower: Mutex<WindowFollower>,
    following_name: Mutex<Option<String>>,
    mic_runtime: Mutex<Option<MicRuntime>>,
    mic_queue: Arc<MicQueue>,
    echo_guard: Arc<EchoGuard>,
    voice_model_dir: PathBuf,
    voice_issue: Mutex<Option<String>>,
    mic_started: Mutex<Option<Instant>>,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    debouncer: Mutex<Debouncer>,
    status: Mutex<Status>,
    hooks: EngineHooks,
}

impl Engine {
    pub fn new(
        settings_path: PathBuf,
        default_folder: &Path,
        voice_model_dir: PathBuf,
        hooks: EngineHooks,
    ) -> Self {
        let settings = Settings::load(&settings_path, default_folder);
        let recording = new_recording(settings.buffer_window_us(), settings.buffer_memory_limit());
        Self {
            recording,
            capture: Mutex::new(None),
            follower: Mutex::new(WindowFollower::default()),
            following_name: Mutex::new(None),
            mic_runtime: Mutex::new(None),
            mic_queue: Arc::new(MicQueue::default()),
            echo_guard: Arc::new(EchoGuard::new()),
            voice_model_dir,
            voice_issue: Mutex::new(None),
            mic_started: Mutex::new(None),
            settings: Mutex::new(settings),
            settings_path,
            debouncer: Mutex::new(Debouncer::new(TRIGGER_COOLDOWN)),
            status: Mutex::new(Status::Stopped),
            hooks,
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().expect("settings lock").clone()
    }

    pub fn status(&self) -> Status {
        self.status.lock().expect("status lock").clone()
    }

    /// The window Automatic mode is recording right now.
    pub fn following_name(&self) -> Option<String> {
        self.following_name.lock().expect("following name lock").clone()
    }

    fn set_following_name(&self, name: Option<String>) {
        *self.following_name.lock().expect("following name lock") = name;
    }

    /// Runs Automatic mode for as long as the engine exists. The thread sits
    /// idle unless that mode is selected and recording.
    pub fn spawn_window_follower(self: &Arc<Self>) {
        let engine = Arc::downgrade(self);
        std::thread::spawn(move || loop {
            std::thread::sleep(FOLLOW_INTERVAL);
            let Some(engine) = engine.upgrade() else { return };
            engine.follow_active_window();
        });
    }

    /// Why voice clipping or the microphone isn't working, if it isn't.
    pub fn voice_issue(&self) -> Option<String> {
        let reported = self.voice_issue.lock().expect("voice issue lock").clone();
        reported.or_else(|| self.microphone_is_silent().then(|| MIC_SILENT_MESSAGE.to_string()))
    }

    /// The OS gives a blocked app silence rather than an error, so the only
    /// sign of it is that nothing but zeros ever arrives.
    fn microphone_is_silent(&self) -> bool {
        let started = *self.mic_started.lock().expect("mic started lock");
        started.is_some_and(|at| at.elapsed() >= MIC_SILENCE_GRACE)
            && !self.mic_queue.heard_signal()
    }

    fn set_status(&self, status: Status) {
        *self.status.lock().expect("status lock") = status;
    }

    /// Saves settings to disk, then restarts only what the change affects.
    pub fn update_settings(self: &Arc<Self>, next: Settings) -> std::io::Result<()> {
        next.save(&self.settings_path)?;
        let previous = self.settings();
        *self.settings.lock().expect("settings lock") = next.clone();

        let running = next.setup_complete && self.status() != Status::Paused;
        if running && previous.capture_differs(&next) {
            self.start();
        } else if running
            && (previous.voice_enabled != next.voice_enabled || previous.voice_model != next.voice_model)
        {
            self.restart_microphone(&next);
        }
        Ok(())
    }

    /// Starts or restarts recording with the current settings. Failures are
    /// reported through [`Engine::status`].
    pub fn start(self: &Arc<Self>) {
        let mut follower = self.follower.lock().expect("follower lock");
        let mut settings = self.settings();
        let result = if settings.source == Some(Source::ActiveWindow) {
            self.start_following(&mut follower, &settings)
        } else {
            follower.clear();
            self.set_following_name(None);
            self.start_capture(&settings, settings.source.clone())
        };
        // A saved display that isn't connected any more shouldn't stop
        // recording altogether.
        let result = match (result, &settings.source) {
            (Err(CaptureError::SourceNotFound), Some(Source::Display { .. } | Source::Region { .. })) => {
                settings.source = None;
                (self.hooks.on_notice)("Display not found", "Recording your main display instead.");
                *self.settings.lock().expect("settings lock") = settings.clone();
                let _ = settings.save(&self.settings_path);
                self.start_capture(&settings, None)
            }
            (result, _) => result,
        };

        match result {
            Ok(()) => {
                self.set_status(Status::Recording);
                self.restart_microphone(&settings);
            }
            Err(error) => self.set_capture_failure(error),
        }
        drop(follower);
        (self.hooks.on_status_changed)();
    }

    fn set_capture_failure(&self, error: CaptureError) {
        self.set_status(match error {
            CaptureError::PermissionDenied => Status::NeedsPermission,
            error => Status::Error { message: error.to_string() },
        });
    }

    /// Begins Automatic mode. With no windows open it just waits for one.
    fn start_following(
        self: &Arc<Self>,
        follower: &mut WindowFollower,
        settings: &Settings,
    ) -> Result<(), CaptureError> {
        follower.clear();
        let windows = capture::front_windows()?;
        if let FollowStep::Switch(window) = follower.advance(&windows, false, Instant::now()) {
            return self.follow_into(follower, settings, window);
        }
        self.stop_capture(settings);
        self.set_following_name(None);
        Ok(())
    }

    /// Moves recording to `window`. One that vanished in the meantime isn't a
    /// failure: the next look at the open windows finds something else.
    fn follow_into(
        self: &Arc<Self>,
        follower: &mut WindowFollower,
        settings: &Settings,
        window: WindowCandidate,
    ) -> Result<(), CaptureError> {
        match self.start_capture(settings, Some(Source::Window { id: window.id })) {
            Ok(()) => {
                if window.is_game {
                    (self.hooks.on_notice)(
                        &format!("Recording {}", window.name),
                        "diff will stay on this game until you close it.",
                    );
                }
                self.set_following_name(Some(window.name));
                Ok(())
            }
            Err(CaptureError::SourceNotFound) => {
                follower.clear();
                self.set_following_name(None);
                Ok(())
            }
            Err(error) => {
                follower.clear();
                self.set_following_name(None);
                Err(error)
            }
        }
    }

    fn follow_active_window(self: &Arc<Self>) {
        let mut follower = self.follower.lock().expect("follower lock");
        let settings = self.settings();
        if settings.source != Some(Source::ActiveWindow) || self.status() != Status::Recording {
            return;
        }
        // A failed lookup says nothing about the windows, so wait for the next.
        let Ok(windows) = capture::front_windows() else { return };
        let current_open = follower.current().is_some_and(|current| {
            windows.iter().any(|window| window.id == current.id) || capture::window_exists(current.id)
        });
        match follower.advance(&windows, current_open, Instant::now()) {
            FollowStep::Keep => return,
            FollowStep::Renamed(name) => self.set_following_name(Some(name)),
            FollowStep::Switch(window) => {
                if let Err(error) = self.follow_into(&mut follower, &settings, window) {
                    self.set_capture_failure(error);
                }
            }
            FollowStep::Lost => {
                self.stop_capture(&settings);
                self.set_following_name(None);
            }
        }
        drop(follower);
        (self.hooks.on_status_changed)();
    }

    fn start_capture(
        self: &Arc<Self>,
        settings: &Settings,
        source: Option<Source>,
    ) -> Result<(), CaptureError> {
        let mut slot = self.capture.lock().expect("capture lock");
        if let Some(previous) = slot.take() {
            previous.stop();
        }
        self.reset_recording(settings);

        let mut config = settings.capture_config();
        config.source = source;
        config.echo = Some(self.echo_guard.clone());
        if settings.include_mic {
            self.mic_queue.clear();
            config.mic = Some(self.mic_queue.clone());
        }
        let engine: Weak<Self> = Arc::downgrade(self);
        let events = CaptureEvents {
            on_source_lost: Arc::new(move || {
                let engine = engine.clone();
                // Runs on its own thread because handling this stops capture,
                // which waits for the thread that reports it.
                std::thread::spawn(move || {
                    if let Some(engine) = engine.upgrade() {
                        engine.handle_source_lost();
                    }
                });
            }),
        };
        *slot = Some(Capture::start(&config, self.recording.clone(), events)?);
        Ok(())
    }

    fn handle_source_lost(self: &Arc<Self>) {
        let settings = self.settings();
        match settings.source {
            // The follower notices within a moment and picks another window.
            Some(Source::ActiveWindow) => {}
            Some(Source::Window { .. }) => {
                self.pause();
                (self.hooks.on_notice)(
                    "Recording paused",
                    "The window you were recording closed. Choose another in Settings.",
                );
                (self.hooks.on_status_changed)();
            }
            Some(_) => {
                (self.hooks.on_notice)(
                    "Display disconnected",
                    "Recording your main display instead.",
                );
                let _ = self.update_settings(Settings { source: None, ..settings });
            }
            None => {}
        }
    }

    /// Stops recording and listening, and discards the buffer.
    pub fn pause(&self) {
        let mut follower = self.follower.lock().expect("follower lock");
        follower.clear();
        self.set_following_name(None);
        let settings = self.settings();
        *self.mic_runtime.lock().expect("mic lock") = None;
        *self.mic_started.lock().expect("mic started lock") = None;
        self.stop_capture(&settings);
        self.set_status(Status::Paused);
    }

    fn stop_capture(&self, settings: &Settings) {
        let mut slot = self.capture.lock().expect("capture lock");
        if let Some(previous) = slot.take() {
            previous.stop();
        }
        self.reset_recording(settings);
    }

    fn reset_recording(&self, settings: &Settings) {
        let mut recording = self.recording.lock().expect("recording lock");
        recording.ring.clear();
        recording.ring.set_window(settings.buffer_window_us(), settings.buffer_memory_limit());
        recording.video_format = None;
        recording.audio_format = None;
        recording.problem = None;
    }

    /// Explains in plain words why there's nothing to clip.
    pub fn nothing_recorded_reason(&self) -> String {
        let automatic = self.settings().source == Some(Source::ActiveWindow);
        if automatic && self.status() == Status::Recording && self.following_name().is_none() {
            return "diff is waiting for a window to record.".into();
        }
        match self.status() {
            Status::Error { message } => message,
            Status::NeedsPermission => "diff needs permission to record your screen.".into(),
            Status::Paused => "diff is paused.".into(),
            Status::Stopped => "diff isn't recording yet. Finish setup to start.".into(),
            Status::Recording => self
                .recording
                .lock()
                .expect("recording lock")
                .problem
                .clone()
                .unwrap_or_else(|| "diff has started recording, but no video has arrived yet.".into()),
        }
    }

    fn set_voice_issue(&self, issue: Option<String>) {
        *self.voice_issue.lock().expect("voice issue lock") = issue;
    }

    fn restart_microphone(&self, settings: &Settings) {
        let mut slot = self.mic_runtime.lock().expect("mic lock");
        *slot = None;
        *self.mic_started.lock().expect("mic started lock") = None;
        self.set_voice_issue(None);
        if !settings.voice_enabled && !settings.include_mic {
            return;
        }

        // A voice problem must not take microphone-in-clips down with it.
        let spotter = if settings.voice_enabled {
            match VoiceSpotter::new(&self.voice_model_dir, settings.voice_model) {
                Ok(spotter) => Some(spotter),
                Err(error) => {
                    self.set_voice_issue(Some(format!("Voice clipping is off: {error}.")));
                    None
                }
            }
        } else {
            None
        };

        match Microphone::start(self.mic_queue.clone(), spotter.is_some()) {
            Ok((microphone, voice_audio)) => {
                let voice_thread = spotter.zip(voice_audio).map(|(spotter, audio)| {
                    let trigger = self.hooks.on_voice_trigger.clone();
                    let echo = self.echo_guard.clone();
                    std::thread::spawn(move || run_voice_detection(spotter, audio, echo, trigger))
                });
                *slot = Some(MicRuntime { microphone: Some(microphone), voice_thread });
                self.mic_queue.reset_signal();
                *self.mic_started.lock().expect("mic started lock") = Some(Instant::now());
            }
            Err(message) => {
                self.set_voice_issue(Some(format!("Couldn't use the microphone: {message}.")));
            }
        }
    }

    /// Handles a hotkey or voice trigger. `Ok(None)` means it was debounced.
    /// The buffer lock is held only for the snapshot, so recording isn't
    /// disturbed while the file is written.
    pub fn prepare_clip(&self) -> Result<Option<PendingClip>, ClipError> {
        if !self.debouncer.lock().expect("debouncer lock").accept(Instant::now()) {
            return Ok(None);
        }
        let settings = self.settings();
        let recording = self.recording.lock().expect("recording lock");
        let video_format = recording.video_format.clone().ok_or(ClipError::NothingRecorded)?;
        let snapshot = take_snapshot(&recording.ring, settings.clip_length_us())?;
        Ok(Some(PendingClip {
            snapshot,
            video_format,
            audio_format: recording.audio_format,
            folder: settings.save_folder,
            window_name: self.following_name(),
        }))
    }
}

fn run_voice_detection(
    mut spotter: VoiceSpotter,
    audio: Receiver<Vec<f32>>,
    echo: Arc<EchoGuard>,
    trigger: Arc<dyn Fn() + Send + Sync>,
) {
    while let Ok(chunk) = audio.recv() {
        // Only a phrase spoken in the room counts, not one from the speakers.
        if spotter.feed(&chunk) && !spotter.contains_phrase(&echo.recent_audio()) {
            trigger();
        }
    }
}

/// Re-exported so callers don't need to know which backend is active.
pub fn has_capture_permission() -> bool {
    capture::has_permission()
}
