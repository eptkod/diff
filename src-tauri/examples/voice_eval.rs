//! Counts trigger detections for each 16 kHz mono WAV listed on the command line.
//! Usage: cargo run --example voice_eval -- <model dir> <wav>...
//! Set VOICE_SCORE and VOICE_THRESHOLD to try other sensitivities.

use diff_lib::voice::{VoiceSpotter, VOICE_SAMPLE_RATE};
use std::path::Path;

fn read_samples(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("readable wav");
    let data = bytes.windows(4).position(|w| w == b"data").expect("data chunk") + 8;
    bytes[data..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0)
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let setting = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<f32>().ok());
    let mut spotter = match (setting("VOICE_SCORE"), setting("VOICE_THRESHOLD")) {
        (Some(score), Some(threshold)) => VoiceSpotter::with_tuning(Path::new(&args[0]), score, threshold),
        _ => VoiceSpotter::new(Path::new(&args[0])),
    }
    .expect("model loads");
    let chunk = VOICE_SAMPLE_RATE / 10;
    let silence = vec![0.0f32; chunk];

    for path in &args[1..] {
        let mut hits = 0;
        for block in read_samples(path).chunks(chunk) {
            hits += spotter.feed(block) as u32;
        }
        // Enough silence to flush the phrase and reset between files.
        for _ in 0..25 {
            hits += spotter.feed(&silence) as u32;
        }
        println!("{hits} {path}");
    }
}
