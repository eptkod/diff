//! Writes encoded packets to an MP4 file without re-encoding.

use crate::buffer::{Packet, Snapshot};
use mp4::{
    AacConfig, AudioObjectType, AvcConfig, ChannelConfig, MediaConfig, Mp4Config, Mp4Sample,
    Mp4Writer, SampleFreqIndex, TrackConfig, TrackType,
};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

const VIDEO_TIMESCALE: u32 = 90_000;
const VIDEO_TRACK: u32 = 1;
const AUDIO_TRACK: u32 = 2;
/// Used only when a clip has a single video frame and nothing to measure from.
const FALLBACK_FRAME_US: i64 = 16_667;

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("nothing to save: the clip has no video")]
    Empty,
    #[error("unsupported audio format: {0}")]
    UnsupportedAudio(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Mp4(#[from] mp4::Error),
}

/// H.264 stream parameters. SPS and PPS have no start codes or length prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFormat {
    pub width: u16,
    pub height: u16,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

/// AAC-LC stream parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate: u32,
}

/// Writes a snapshot to `path`. Video samples must be AVCC (length-prefixed)
/// and audio samples raw AAC. The file appears at `path` only once complete,
/// so the library never sees a half-written clip.
pub fn write_mp4(
    path: &Path,
    video_format: &VideoFormat,
    audio_format: Option<&AudioFormat>,
    snapshot: &Snapshot,
) -> Result<(), MuxError> {
    if snapshot.video.is_empty() {
        return Err(MuxError::Empty);
    }
    let audio_format = audio_format.filter(|_| !snapshot.audio.is_empty());
    let audio_config = audio_format.map(aac_config).transpose()?;

    let partial = partial_path(path);
    let result = write_file(&partial, video_format, audio_format, audio_config, snapshot)
        .and_then(|()| fs::rename(&partial, path).map_err(MuxError::from));
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

fn write_file(
    path: &Path,
    video_format: &VideoFormat,
    audio_format: Option<&AudioFormat>,
    audio_config: Option<AacConfig>,
    snapshot: &Snapshot,
) -> Result<(), MuxError> {
    let config = Mp4Config {
        major_brand: "isom".parse().expect("valid fourcc"),
        minor_version: 512,
        compatible_brands: ["isom", "iso2", "avc1", "mp41"]
            .iter()
            .map(|b| b.parse().expect("valid fourcc"))
            .collect(),
        timescale: 1000,
    };
    let file = BufWriter::with_capacity(1 << 20, File::create(path)?);
    let mut writer = Mp4Writer::write_start(file, &config)?;

    writer.add_track(&TrackConfig {
        track_type: TrackType::Video,
        timescale: VIDEO_TIMESCALE,
        language: "und".into(),
        media_conf: MediaConfig::AvcConfig(AvcConfig {
            width: video_format.width,
            height: video_format.height,
            seq_param_set: video_format.sps.clone(),
            pic_param_set: video_format.pps.clone(),
        }),
    })?;
    if let (Some(format), Some(aac)) = (audio_format, audio_config) {
        writer.add_track(&TrackConfig {
            track_type: TrackType::Audio,
            timescale: format.sample_rate,
            language: "und".into(),
            media_conf: MediaConfig::AacConfig(aac),
        })?;
    }

    // Tracks are written one after the other. Their `moov` tables hold the
    // offsets, so players still interleave correctly when reading.
    write_track(&mut writer, VIDEO_TRACK, VIDEO_TIMESCALE, &snapshot.video)?;
    if let Some(format) = audio_format {
        write_track(&mut writer, AUDIO_TRACK, format.sample_rate, &snapshot.audio)?;
    }

    writer.write_end()?;
    let mut file = writer.into_writer();
    std::io::Write::flush(&mut file)?;
    Ok(())
}

fn write_track<W: std::io::Write + std::io::Seek>(
    writer: &mut Mp4Writer<W>,
    track: u32,
    timescale: u32,
    packets: &[Packet],
) -> Result<(), MuxError> {
    let origin = packets[0].pts_us;
    // Converting absolute timestamps stops rounding from accumulating into drift.
    let ticks = |pts_us: i64| ((pts_us - origin) as i128 * timescale as i128 / 1_000_000) as u64;

    let mut last_duration = last_duration_us(packets);
    for (index, packet) in packets.iter().enumerate() {
        let start = ticks(packet.pts_us);
        let end = match packets.get(index + 1) {
            Some(next) => ticks(next.pts_us),
            None => {
                if packet.duration_us > 0 {
                    last_duration = packet.duration_us;
                }
                ticks(packet.pts_us + last_duration)
            }
        };
        writer.write_sample(
            track,
            &Mp4Sample {
                start_time: start,
                duration: end.saturating_sub(start).max(1) as u32,
                rendering_offset: 0,
                is_sync: packet.keyframe,
                bytes: packet.data.clone(),
            },
        )?;
    }
    Ok(())
}

fn last_duration_us(packets: &[Packet]) -> i64 {
    match packets {
        [.., previous, last] => (last.pts_us - previous.pts_us).max(1),
        _ => FALLBACK_FRAME_US,
    }
}

fn aac_config(format: &AudioFormat) -> Result<AacConfig, MuxError> {
    let freq_index = match format.sample_rate {
        48_000 => SampleFreqIndex::Freq48000,
        44_100 => SampleFreqIndex::Freq44100,
        other => return Err(MuxError::UnsupportedAudio(format!("{other} Hz"))),
    };
    let chan_conf = match format.channels {
        1 => ChannelConfig::Mono,
        2 => ChannelConfig::Stereo,
        other => return Err(MuxError::UnsupportedAudio(format!("{other} channels"))),
    };
    Ok(AacConfig {
        bitrate: format.bitrate,
        profile: AudioObjectType::AacLowComplexity,
        freq_index,
        chan_conf,
    })
}

fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_format() -> VideoFormat {
        VideoFormat { width: 16, height: 16, sps: vec![0x67, 0x42, 0, 0x1e], pps: vec![0x68, 0xce] }
    }

    #[test]
    fn empty_snapshot_is_rejected_and_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        let result = write_mp4(&path, &video_format(), None, &Snapshot::default());
        assert!(matches!(result, Err(MuxError::Empty)));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn unsupported_audio_is_rejected_and_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        let snapshot = Snapshot {
            video: vec![Packet::video(0, true, vec![0u8; 8])],
            audio: vec![Packet::audio(0, 20_000, vec![0u8; 8])],
        };
        let audio = AudioFormat { sample_rate: 8_000, channels: 2, bitrate: 96_000 };
        let result = write_mp4(&path, &video_format(), Some(&audio), &snapshot);
        assert!(matches!(result, Err(MuxError::UnsupportedAudio(_))));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn successful_write_leaves_only_the_final_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        let snapshot = Snapshot {
            video: vec![
                Packet::video(1_000, true, vec![0u8; 8]),
                Packet::video(34_333, false, vec![0u8; 8]),
            ],
            audio: vec![],
        };
        write_mp4(&path, &video_format(), None, &snapshot).unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("clip.mp4")]);
    }
}
