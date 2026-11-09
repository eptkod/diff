//! Measures echo rejection. For each phrase, simulates the microphone hearing
//! it from the speakers, then checks whether a detection would be rejected.
//! Usage: cargo run --example echo_eval -- <model dir> <phrase wavs dir> <other speech dir>...

use diff_lib::voice::{VoiceSpotter, VOICE_SAMPLE_RATE};
use std::fs;
use std::path::{Path, PathBuf};

const CHUNK: usize = VOICE_SAMPLE_RATE / 10;
const TRAILING_SILENCE_CHUNKS: usize = 25;

fn read(path: &Path) -> Vec<f32> {
    let bytes = fs::read(path).expect("readable wav");
    let data = bytes.windows(4).position(|w| w == b"data").expect("data chunk") + 8;
    bytes[data..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32_768.0)
        .collect()
}

fn wavs(dir: &str) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    files
}

/// Speaker-to-microphone: quieter, with reflections and background noise.
fn through_the_room(audio: &[f32], gain: f32, seed: u32) -> Vec<f32> {
    let mut state = seed.wrapping_add(7);
    (0..audio.len())
        .map(|i| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = ((state >> 16) as f32 / 65_536.0 - 0.5) * 0.002;
            let early = if i >= 480 { audio[i - 480] * 0.35 } else { 0.0 };
            let late = if i >= 1_600 { audio[i - 1_600] * 0.2 } else { 0.0 };
            (audio[i] + early + late) * gain + noise
        })
        .collect()
}

/// Real history always has audio before the phrase, unlike these short files.
fn with_lead_in(audio: &[f32]) -> Vec<f32> {
    let mut padded = vec![0.0; VOICE_SAMPLE_RATE];
    padded.extend_from_slice(audio);
    padded
}

fn detected(spotter: &mut VoiceSpotter, audio: &[f32]) -> bool {
    let mut hit = false;
    for block in audio.chunks(CHUNK) {
        hit |= spotter.feed(block);
    }
    for _ in 0..TRAILING_SILENCE_CHUNKS {
        hit |= spotter.feed(&[0.0; CHUNK]);
    }
    hit
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut spotter = VoiceSpotter::new(Path::new(&args[0])).expect("model loads");

    for (gain, label) in [(1.5, "loud speakers"), (0.8, "normal speakers")] {
        let (mut heard, mut rejected, mut total) = (0, 0, 0);
        for (index, path) in wavs(&args[1]).iter().enumerate() {
            let clean = read(path);
            total += 1;
            if detected(&mut spotter, &through_the_room(&clean, gain, index as u32)) {
                heard += 1;
                rejected += spotter.contains_phrase(&with_lead_in(&clean)) as u32;
            }
        }
        println!("{label}: mic detected {heard}/{total}, rejected as echo {rejected}/{heard}");
    }

    for dir in &args[2..] {
        let files = wavs(dir);
        let wrongly = files.iter().filter(|p| spotter.contains_phrase(&with_lead_in(&read(p)))).count();
        println!("{dir}: unrelated audio mistaken for the phrase {wrongly}/{}", files.len());
    }
}
