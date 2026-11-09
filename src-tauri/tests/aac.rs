//! Encodes a known signal with the real macOS AAC encoder, decodes it with
//! FFmpeg, and checks that the content and its timing survive.
#![cfg(target_os = "macos")]

use diff_lib::audio::{AacEncoder, AudioMixer, AUDIO_FORMAT};
use diff_lib::buffer::Packet;
use diff_lib::capture::{new_recording, MacAacEncoder};
use diff_lib::mux::{write_mp4, VideoFormat};
use std::process::Command;

const SAMPLE_RATE: usize = 48_000;
const SILENCE_SECONDS: f64 = 0.5;
const TOTAL_SECONDS: f64 = 2.0;

fn signal() -> Vec<f32> {
    let total = (TOTAL_SECONDS * SAMPLE_RATE as f64) as usize;
    let onset = (SILENCE_SECONDS * SAMPLE_RATE as f64) as usize;
    (0..total)
        .flat_map(|i| {
            let phase = i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32;
            let value = if i < onset { 0.0 } else { phase.sin() * 0.5 };
            [value, value]
        })
        .collect()
}

#[test]
fn encoded_audio_decodes_with_correct_timing() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipping: FFmpeg not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();

    // A real video track, so the file is a normal clip.
    let source = dir.path().join("video.mp4");
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=30", "-t", "3"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "30", "-bf", "0"])
        .arg(&source)
        .status()
        .unwrap();
    assert!(status.success());
    let file = std::fs::File::open(&source).unwrap();
    let size = file.metadata().unwrap().len();
    let mut reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(file), size).unwrap();
    let (id, track) = reader.tracks().iter().next().map(|(i, t)| (*i, t)).unwrap();
    let format = VideoFormat {
        width: track.width(),
        height: track.height(),
        sps: track.sequence_parameter_set().unwrap().to_vec(),
        pps: track.picture_parameter_set().unwrap().to_vec(),
    };
    let timescale = track.timescale() as i64;

    let recording = new_recording(60_000_000, usize::MAX);
    for n in 1..=reader.sample_count(id).unwrap() {
        let sample = reader.read_sample(id, n).unwrap().unwrap();
        let pts = sample.start_time as i64 * 1_000_000 / timescale;
        recording.lock().unwrap().ring.push_video(Packet::video(pts, sample.is_sync, sample.bytes));
    }

    let mut mixer = AudioMixer::new(Box::new(MacAacEncoder::new().unwrap()), recording.clone(), None, None);
    let priming = MacAacEncoder::new().unwrap().priming_frames();
    assert_eq!(priming, 2112);
    // Starts with extra lead-in so the first packets precede the first frame.
    for (index, block) in signal().chunks(960).enumerate() {
        mixer.push_system(index as i64 * 10_000, block);
    }

    let snapshot = recording.lock().unwrap().ring.snapshot(60_000_000).unwrap();
    assert!(!snapshot.audio.is_empty());
    let clip = dir.path().join("clip.mp4");
    write_mp4(&clip, &format, Some(&AUDIO_FORMAT), &snapshot).unwrap();

    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&clip)
        .args(["-map", "a", "-f", "f32le", "-ac", "1", "-ar", "48000", "-"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let decoded: Vec<f32> = output.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();

    let onset = decoded.iter().position(|s| s.abs() > 0.1).expect("tone present") as f64 / 48_000.0;
    let expected = SILENCE_SECONDS + (snapshot.video[0].pts_us - snapshot.audio[0].pts_us) as f64 / 1e6;
    assert!((onset - expected).abs() < 0.03, "tone started at {onset}s, expected about {expected}s");

    let peak = decoded.iter().fold(0.0f32, |max, s| max.max(s.abs()));
    // A tone that starts abruptly overshoots a little in any lossy codec.
    assert!((0.35..0.85).contains(&peak), "peak was {peak}");
}
