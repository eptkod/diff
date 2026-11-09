//! Cuts a saved clip without re-encoding.

use crate::buffer::{select_audio, Packet, Snapshot};
use crate::mux::{write_mp4, AudioFormat, MuxError, VideoFormat};
use mp4::{AudioObjectType, ChannelConfig, Mp4Reader, SampleFreqIndex, TrackType};
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum TrimError {
    #[error("the selected range is empty")]
    EmptyRange,
    #[error("this file has no H.264 video track")]
    NoVideo,
    #[error("this file's audio can't be trimmed: {0}")]
    UnsupportedAudio(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Mp4(#[from] mp4::Error),
    #[error(transparent)]
    Mux(#[from] MuxError),
}

/// Where a trim actually landed, after snapping the start to a keyframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrimResult {
    pub start_us: i64,
    pub end_us: i64,
}

/// Copies `[start_us, end_us]` of `source` to `destination`. Video can only
/// start on a keyframe, so the start snaps to the nearest one. `source` is
/// left untouched.
pub fn trim_mp4(
    source: &Path,
    destination: &Path,
    start_us: i64,
    end_us: i64,
) -> Result<TrimResult, TrimError> {
    if end_us <= start_us {
        return Err(TrimError::EmptyRange);
    }
    let file = File::open(source)?;
    let size = file.metadata()?.len();
    let mut reader = Mp4Reader::read_header(BufReader::new(file), size)?;

    let tracks = find_tracks(&reader)?;
    let frames = read_packets(&mut reader, tracks.video_id, tracks.video_timescale)?;

    let start_index = nearest_keyframe(&frames, start_us).ok_or(TrimError::NoVideo)?;
    let start = frames[start_index].pts_us;
    let video: Vec<Packet> = frames
        .into_iter()
        .skip(start_index)
        .take_while(|frame| frame.pts_us < end_us)
        .collect();
    let Some(last_pts) = video.last().map(|frame| frame.pts_us) else {
        return Err(TrimError::EmptyRange);
    };

    let audio = match tracks.audio {
        Some((id, timescale, _)) => {
            let packets = read_packets(&mut reader, id, timescale)?;
            select_audio(packets.iter(), start, last_pts)
        }
        None => Vec::new(),
    };

    let audio_format = tracks.audio.map(|(_, _, format)| format);
    write_mp4(
        destination,
        &tracks.video_format,
        audio_format.as_ref(),
        &Snapshot { video, audio },
    )?;
    Ok(TrimResult { start_us: start, end_us: last_pts })
}

struct Tracks {
    video_id: u32,
    video_timescale: u32,
    video_format: VideoFormat,
    audio: Option<(u32, u32, AudioFormat)>,
}

fn find_tracks<R: Read + Seek>(reader: &Mp4Reader<R>) -> Result<Tracks, TrimError> {
    let mut video = None;
    let mut audio = None;

    for (id, track) in reader.tracks() {
        match track.track_type()? {
            TrackType::Video if video.is_none() => {
                let format = VideoFormat {
                    width: track.width(),
                    height: track.height(),
                    sps: track.sequence_parameter_set().map_err(|_| TrimError::NoVideo)?.to_vec(),
                    pps: track.picture_parameter_set().map_err(|_| TrimError::NoVideo)?.to_vec(),
                };
                video = Some((*id, track.timescale(), format));
            }
            TrackType::Audio if audio.is_none() => {
                if track.audio_profile()? != AudioObjectType::AacLowComplexity {
                    return Err(TrimError::UnsupportedAudio("not AAC-LC".into()));
                }
                let sample_rate = match track.sample_freq_index()? {
                    SampleFreqIndex::Freq48000 => 48_000,
                    SampleFreqIndex::Freq44100 => 44_100,
                    other => return Err(TrimError::UnsupportedAudio(format!("{other:?}"))),
                };
                let channels = match track.channel_config()? {
                    ChannelConfig::Mono => 1,
                    ChannelConfig::Stereo => 2,
                    other => return Err(TrimError::UnsupportedAudio(format!("{other:?}"))),
                };
                let format = AudioFormat { sample_rate, channels, bitrate: track.bitrate() };
                audio = Some((*id, track.timescale(), format));
            }
            _ => {}
        }
    }

    let (video_id, video_timescale, video_format) = video.ok_or(TrimError::NoVideo)?;
    Ok(Tracks { video_id, video_timescale, video_format, audio })
}

fn read_packets<R: Read + Seek>(
    reader: &mut Mp4Reader<R>,
    track: u32,
    timescale: u32,
) -> Result<Vec<Packet>, TrimError> {
    let to_us = |ticks: u64| (ticks as i128 * 1_000_000 / timescale as i128) as i64;
    let count = reader.sample_count(track)?;
    let mut packets = Vec::with_capacity(count as usize);
    for sample_id in 1..=count {
        if let Some(sample) = reader.read_sample(track, sample_id)? {
            let pts_us = to_us(sample.start_time);
            packets.push(Packet {
                pts_us,
                duration_us: to_us(sample.start_time + sample.duration as u64) - pts_us,
                keyframe: sample.is_sync,
                data: sample.bytes,
            });
        }
    }
    Ok(packets)
}

fn nearest_keyframe(frames: &[Packet], target_us: i64) -> Option<usize> {
    frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame.keyframe)
        .min_by_key(|(_, frame)| (frame.pts_us - target_us).abs())
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_keyframe_picks_the_closest_in_either_direction() {
        let frames: Vec<Packet> = (0..30)
            .map(|i| Packet::video(i * 100_000, i % 10 == 0, vec![0u8]))
            .collect();
        assert_eq!(nearest_keyframe(&frames, 400_000), Some(0));
        assert_eq!(nearest_keyframe(&frames, 600_000), Some(10));
        assert_eq!(nearest_keyframe(&frames, 99_000_000), Some(20));
    }

    #[test]
    fn nearest_keyframe_is_none_without_keyframes() {
        assert_eq!(nearest_keyframe(&[Packet::video(0, false, vec![0u8])], 0), None);
    }

    #[test]
    fn empty_range_is_rejected_before_opening_the_file() {
        let result = trim_mp4(Path::new("/no/such"), Path::new("/no/out"), 5, 5);
        assert!(matches!(result, Err(TrimError::EmptyRange)));
    }
}
