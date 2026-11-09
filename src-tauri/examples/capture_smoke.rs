//! Records the primary display for a few seconds and saves a clip.
//! Usage: cargo run --example capture_smoke -- <output folder> [--mic]
//! With --mic, the microphone is mixed into the clip.

use chrono::Local;
use diff_lib::audio::{MicQueue, Microphone};
use diff_lib::capture::{self, new_recording, Capture, CaptureBackend, CaptureConfig, CaptureEvents};
use diff_lib::clip::{save_snapshot, take_snapshot};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const SECOND: i64 = 1_000_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let folder = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    if !capture::has_permission() {
        capture::request_permission();
        return Err("Grant Screen Recording to this terminal, restart it, and run again".into());
    }

    let recording = new_recording(5 * SECOND, 512 * 1024 * 1024);
    let use_mic = std::env::args().any(|arg| arg == "--mic");
    let queue = Arc::new(MicQueue::default());
    let microphone = if use_mic { Some(Microphone::start(queue.clone(), false)?.0) } else { None };
    let mic = use_mic.then(|| queue.clone());
    let config = CaptureConfig { source: None, max_height: 1080, fps: 60, bitrate: 15_000_000, mic, echo: None };
    let events = CaptureEvents { on_source_lost: Arc::new(|| println!("source lost")) };
    let capture = Capture::start(&config, recording.clone(), events)?;
    std::thread::sleep(Duration::from_secs(7));
    capture.stop();
    drop(microphone);

    let state = recording.lock().unwrap();
    println!("buffer memory: {} KB", state.ring.memory_bytes() / 1024);
    let format = state.video_format.as_ref().ok_or("no frames were captured")?;
    println!("format: {}x{}", format.width, format.height);
    let snapshot = take_snapshot(&state.ring, 4 * SECOND)?;
    println!("frames in clip: {}", snapshot.video.len());
    println!("audio packets in clip: {}", snapshot.audio.len());
    let path = save_snapshot(&snapshot, &folder, format, state.audio_format.as_ref(), Local::now())?;
    println!("saved: {}", path.display());
    Ok(())
}
