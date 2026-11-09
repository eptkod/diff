//! Screen capture. Each OS has a backend that pushes encoded packets into a
//! shared [`Recording`].

use crate::audio::MicQueue;
use crate::buffer::RingBuffer;
use crate::echo::EchoGuard;
use crate::mux::{AudioFormat, VideoFormat};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
mod macos_aac;
#[cfg(target_os = "macos")]
pub use macos_aac::MacAacEncoder;
#[cfg(target_os = "macos")]
pub use macos::{
    front_windows, has_permission, list_sources, request_permission, window_exists,
    MacCapture as Capture,
};

pub mod annexb;
pub mod follow;

#[cfg(not(target_os = "macos"))]
mod windows;
#[cfg(not(target_os = "macos"))]
mod windows_mf;
#[cfg(not(target_os = "macos"))]
pub use windows::{
    front_windows, has_permission, list_sources, request_permission, window_exists,
    WindowsCapture as Capture,
};

/// What to record. Region coordinates are in display points, from the top left.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Source {
    Display { id: u32 },
    Window { id: u32 },
    /// Follows the game or window in use. The engine resolves this to a
    /// [`Source::Window`], so capture backends never record it directly.
    ActiveWindow,
    Region { display_id: u32, x: f64, y: f64, width: f64, height: f64 },
}

/// A display or window offered in the source picker. Position and size are in
/// points, on the desktop's global coordinate space.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub source: Source,
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// `None` uses the primary display.
    pub source: Option<Source>,
    pub max_height: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// Set when the microphone should be mixed into clips.
    pub mic: Option<Arc<MicQueue>>,
    /// Receives system audio so voice detection can recognize the speakers.
    pub echo: Option<Arc<EchoGuard>>,
}

/// Things a backend reports back while running.
#[derive(Clone)]
pub struct CaptureEvents {
    /// The recorded window closed or the display was disconnected. Called from
    /// a backend thread, at most once.
    pub on_source_lost: Arc<dyn Fn() + Send + Sync>,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("screen recording permission hasn't been granted")]
    PermissionDenied,
    #[error("the selected display or window is no longer available")]
    SourceNotFound,
    #[error("screen capture failed: {0}")]
    Backend(String),
    #[error("couldn't start the video encoder (error {0})")]
    Encoder(i32),
    #[error("screen capture isn't supported on this platform yet")]
    Unsupported,
}

/// The live recording state that capture writes to and clipping reads from.
#[derive(Debug)]
pub struct Recording {
    pub ring: RingBuffer,
    pub video_format: Option<VideoFormat>,
    pub audio_format: Option<AudioFormat>,
    /// The first thing that went wrong while recording, if frames stopped.
    pub problem: Option<String>,
}

pub type SharedRecording = Arc<Mutex<Recording>>;

pub fn new_recording(window_us: i64, max_bytes: usize) -> SharedRecording {
    Arc::new(Mutex::new(Recording {
        ring: RingBuffer::new(window_us, max_bytes),
        video_format: None,
        audio_format: None,
        problem: None,
    }))
}

/// Implemented once per OS.
pub trait CaptureBackend: Sized {
    fn start(
        config: &CaptureConfig,
        recording: SharedRecording,
        events: CaptureEvents,
    ) -> Result<Self, CaptureError>;
    fn stop(self);
}

/// Whether a new frame should be recorded, given the time of the last one.
/// Displays can refresh faster than the chosen frame rate.
pub fn frame_is_due(time: i64, last: Option<i64>, interval: i64) -> bool {
    last.is_none_or(|last| time - last >= interval * 9 / 10)
}

/// Scales a source down to `max_height`, keeping its shape. H.264 needs even
/// dimensions, and sources are never scaled up.
pub fn output_size(source_width: u32, source_height: u32, max_height: u32) -> (u32, u32) {
    let height = source_height.min(max_height).max(2);
    let width = (source_width as u64 * height as u64 / source_height.max(1) as u64) as u32;
    (width.max(2) & !1, height & !1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_frame_is_always_due() {
        assert!(frame_is_due(0, None, 100));
        assert!(frame_is_due(1_234_567_890_123, None, 166_666));
    }

    #[test]
    fn frames_arriving_too_soon_are_skipped() {
        assert!(!frame_is_due(1_050, Some(1_000), 100));
        assert!(frame_is_due(1_090, Some(1_000), 100));
        assert!(frame_is_due(1_500, Some(1_000), 100));
    }

    #[test]
    fn scales_down_preserving_aspect() {
        assert_eq!(output_size(3840, 2160, 1080), (1920, 1080));
    }

    #[test]
    fn never_scales_up() {
        assert_eq!(output_size(1280, 720, 1080), (1280, 720));
    }

    #[test]
    fn output_is_always_even() {
        let (width, height) = output_size(1001, 777, 1080);
        assert_eq!((width % 2, height % 2), (0, 0));
    }

    #[test]
    fn tiny_or_degenerate_sources_stay_valid() {
        assert_eq!(output_size(1, 1, 1080), (2, 2));
        assert_eq!(output_size(0, 0, 1080), (2, 2));
    }
}
