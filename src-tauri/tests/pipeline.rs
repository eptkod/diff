//! Runs real H.264/AAC through the buffer, muxer and trimmer, then checks the
//! results with FFmpeg. Skipped when FFmpeg isn't installed, since the app
//! itself doesn't depend on it.

use diff_lib::buffer::{Packet, RingBuffer};
use diff_lib::mux::{write_mp4, AudioFormat, VideoFormat};
use diff_lib::trim::trim_mp4;
use mp4::{Mp4Reader, TrackType};
use serde_json::Value;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::process::Command;

const SECOND: i64 = 1_000_000;

fn ffmpeg_available() -> bool {
    let works = |tool: &str| {
        Command::new(tool).arg("-version").output().is_ok_and(|o| o.status.success())
    };
    works("ffmpeg") && works("ffprobe")
}

/// Encodes 8 seconds of test video with a 1s GOP and no B-frames, like the app.
fn make_source(path: &Path) {
    let status = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-t", "8", "-c:v", "libx264", "-profile:v", "main", "-pix_fmt", "yuv420p"])
        .args(["-g", "30", "-keyint_min", "30", "-sc_threshold", "0", "-bf", "0"])
        .args(["-c:a", "aac", "-ac", "2", "-b:a", "128k"])
        .arg(path)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg failed to create the source");
}

struct Source {
    video_format: VideoFormat,
    audio_format: AudioFormat,
    video: Vec<Packet>,
    audio: Vec<Packet>,
}

fn read_source(path: &Path) -> Source {
    let file = File::open(path).unwrap();
    let size = file.metadata().unwrap().len();
    let mut reader = Mp4Reader::read_header(BufReader::new(file), size).unwrap();

    let mut video_info = None;
    let mut audio_info = None;
    for (id, track) in reader.tracks() {
        match track.track_type().unwrap() {
            TrackType::Video => {
                video_info = Some((
                    *id,
                    track.timescale(),
                    VideoFormat {
                        width: track.width(),
                        height: track.height(),
                        sps: track.sequence_parameter_set().unwrap().to_vec(),
                        pps: track.picture_parameter_set().unwrap().to_vec(),
                    },
                ));
            }
            TrackType::Audio => audio_info = Some((*id, track.timescale())),
            _ => {}
        }
    }
    let (video_id, video_timescale, video_format) = video_info.expect("video track");
    let (audio_id, audio_timescale) = audio_info.expect("audio track");

    let mut read = |id: u32, timescale: u32| -> Vec<Packet> {
        let count = reader.sample_count(id).unwrap();
        (1..=count)
            .map(|n| {
                let sample = reader.read_sample(id, n).unwrap().unwrap();
                let pts = (sample.start_time as i128 * SECOND as i128 / timescale as i128) as i64;
                let duration = (sample.duration as i128 * SECOND as i128 / timescale as i128) as i64;
                Packet { pts_us: pts, duration_us: duration, keyframe: sample.is_sync, data: sample.bytes }
            })
            .collect()
    };
    let video = read(video_id, video_timescale);
    let audio = read(audio_id, audio_timescale);

    Source {
        video_format,
        audio_format: AudioFormat { sample_rate: 48_000, channels: 2, bitrate: 128_000 },
        video,
        audio,
    }
}

fn probe(path: &Path) -> Value {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn stream<'a>(probe: &'a Value, kind: &str) -> &'a Value {
    probe["streams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["codec_type"] == kind)
        .unwrap_or_else(|| panic!("no {kind} stream"))
}

fn seconds(value: &Value) -> f64 {
    value.as_str().unwrap().parse().unwrap()
}

/// Fully decodes the file. Any corruption shows up as FFmpeg errors.
fn assert_decodes_cleanly(path: &Path) {
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success() && errors.trim().is_empty(), "decode errors: {errors}");
}

#[test]
fn clip_and_trim_produce_valid_synchronized_files() {
    if !ffmpeg_available() {
        eprintln!("skipping: FFmpeg not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source_path = dir.path().join("source.mp4");
    make_source(&source_path);
    let source = read_source(&source_path);

    // Interleave packets by time, as live capture would deliver them.
    let mut ring = RingBuffer::new(5 * SECOND, usize::MAX);
    let (mut v, mut a) = (0, 0);
    while v < source.video.len() || a < source.audio.len() {
        let take_video = a >= source.audio.len()
            || (v < source.video.len() && source.video[v].pts_us <= source.audio[a].pts_us);
        if take_video {
            ring.push_video(source.video[v].clone());
            v += 1;
        } else {
            ring.push_audio(source.audio[a].clone());
            a += 1;
        }
    }

    let snapshot = ring.snapshot(5 * SECOND).expect("snapshot");
    let clip_path = dir.path().join("clip.mp4");
    write_mp4(&clip_path, &source.video_format, Some(&source.audio_format), &snapshot).unwrap();

    let clip = probe(&clip_path);
    let duration = seconds(&clip["format"]["duration"]);
    assert!((5.0..=6.1).contains(&duration), "clip duration was {duration}");

    let video = stream(&clip, "video");
    assert_eq!(video["codec_name"], "h264");
    assert_eq!(video["width"], 640);
    assert_eq!(video["height"], 360);
    let (numerator, denominator) = video["avg_frame_rate"].as_str().unwrap().split_once('/').unwrap();
    let frame_rate = numerator.parse::<f64>().unwrap() / denominator.parse::<f64>().unwrap();
    assert!((frame_rate - 30.0).abs() < 0.01, "frame rate was {frame_rate}");

    let audio = stream(&clip, "audio");
    assert_eq!(audio["codec_name"], "aac");
    assert_eq!(audio["channels"], 2);
    assert_eq!(audio["sample_rate"], "48000");

    let drift = (seconds(&video["duration"]) - seconds(&audio["duration"])).abs();
    assert!(drift < 0.05, "video and audio differ by {drift}s");
    assert_decodes_cleanly(&clip_path);

    // 2.2s snaps back to the 2.0s keyframe, relative to the clip's own start.
    let trimmed_path = dir.path().join("trimmed.mp4");
    let result = trim_mp4(&clip_path, &trimmed_path, 2_200_000, 4 * SECOND).unwrap();
    assert_eq!(result.start_us, 2 * SECOND);

    let trimmed = probe(&trimmed_path);
    let duration = seconds(&trimmed["format"]["duration"]);
    assert!((1.9..=2.1).contains(&duration), "trimmed duration was {duration}");
    let drift = (seconds(&stream(&trimmed, "video")["duration"])
        - seconds(&stream(&trimmed, "audio")["duration"]))
    .abs();
    assert!(drift < 0.05, "trimmed tracks differ by {drift}s");
    assert_decodes_cleanly(&trimmed_path);

    // The original must be untouched.
    assert_eq!(seconds(&probe(&clip_path)["format"]["duration"]), seconds(&clip["format"]["duration"]));
}
