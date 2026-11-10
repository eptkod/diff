//! Opens the microphone and reports what it heard.
//! Usage: cargo run --example mic_check -- [voice models dir]
//! Set VOICE_MODEL=light to listen with the light model.
//! With a model folder, also records system audio and, for 20 seconds, reports
//! each trigger phrase and whether it was rejected as coming from the speakers.

use diff_lib::audio::{MicQueue, Microphone};
use diff_lib::capture::{new_recording, Capture, CaptureBackend, CaptureConfig, CaptureEvents};
use diff_lib::echo::EchoGuard;
use diff_lib::settings::VoiceModel;
use diff_lib::voice::VoiceSpotter;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const LISTEN: Duration = Duration::from_secs(20);
const CHECK: Duration = Duration::from_secs(3);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let queue = Arc::new(MicQueue::default());
    let model = std::env::args().nth(1);
    let (microphone, audio) = Microphone::start(queue.clone(), model.is_some())?;

    match (model, audio) {
        (Some(model), Some(audio)) => {
            let echo = Arc::new(EchoGuard::new());
            let config = CaptureConfig {
                source: None,
                max_height: 720,
                fps: 30,
                bitrate: 5_000_000,
                mic: None,
                echo: Some(echo.clone()),
            };
            let events = CaptureEvents { on_source_lost: Arc::new(|| {}) };
            let capture = Capture::start(&config, new_recording(5_000_000, 64 << 20), events)?;

            let mut spotter = VoiceSpotter::new(Path::new(&model), voice_model())?;
            let start = Instant::now();
            while start.elapsed() < LISTEN {
                if let Ok(chunk) = audio.recv_timeout(Duration::from_millis(200)) {
                    if spotter.feed(&chunk) {
                        let started = Instant::now();
                        let rejected = spotter.contains_phrase(&echo.recent_audio());
                        println!("phrase: rejected={rejected} (checked in {:?})", started.elapsed());
                    }
                }
            }
            capture.stop();
        }
        _ => std::thread::sleep(CHECK),
    }

    drop(microphone);
    println!("heard a signal: {}", queue.heard_signal());
    Ok(())
}

fn voice_model() -> VoiceModel {
    let light = std::env::var("VOICE_MODEL").as_deref() == Ok("light");
    if light { VoiceModel::Light } else { VoiceModel::Accurate }
}
