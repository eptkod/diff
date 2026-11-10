//! Counts trigger detections for each 16 kHz mono WAV listed on the command line.
//! Usage: cargo run --example voice_eval -- <voice models dir> <wav>...
//! Set VOICE_MODEL to light or accurate (the default). For the light model,
//! set VOICE_SCORE and VOICE_THRESHOLD to try other sensitivities, and
//! VOICE_KEYWORDS to the path of a keywords file to try other phrases.

use diff_lib::settings::VoiceModel;
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
    let root = Path::new(&args[0]);
    let light = std::env::var("VOICE_MODEL").as_deref() == Ok("light");
    let setting = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<f32>().ok());
    let mut spotter = match (light, setting("VOICE_SCORE"), setting("VOICE_THRESHOLD")) {
        (true, Some(score), Some(threshold)) => {
            let keywords = std::env::var("VOICE_KEYWORDS")
                .ok()
                .map(|path| std::fs::read_to_string(path).expect("readable keywords"));
            VoiceSpotter::light_with_tuning(&root.join("light"), score, threshold, keywords)
        }
        (true, _, _) => VoiceSpotter::new(root, VoiceModel::Light),
        (false, _, _) => VoiceSpotter::new(root, VoiceModel::Accurate),
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
