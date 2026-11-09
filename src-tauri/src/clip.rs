//! Turns a trigger (hotkey or voice) into a saved clip.

use crate::buffer::{RingBuffer, Snapshot};
use crate::mux::{write_mp4, AudioFormat, MuxError, VideoFormat};
use chrono::{DateTime, Local};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Saying the phrase while also pressing the key shouldn't produce two clips.
pub const TRIGGER_COOLDOWN: Duration = Duration::from_secs(2);

/// Accepts the first trigger, then ignores others until the cooldown passes.
#[derive(Debug)]
pub struct Debouncer {
    cooldown: Duration,
    last_accepted: Option<Instant>,
}

impl Debouncer {
    pub fn new(cooldown: Duration) -> Self {
        Self { cooldown, last_accepted: None }
    }

    pub fn accept(&mut self, now: Instant) -> bool {
        let ready = self
            .last_accepted
            .is_none_or(|last| now.duration_since(last) >= self.cooldown);
        if ready {
            self.last_accepted = Some(now);
        }
        ready
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClipError {
    #[error("nothing has been recorded yet")]
    NothingRecorded,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Mux(#[from] MuxError),
}

/// Copies the latest `length_us` out of the buffer. This is the only step that
/// needs the buffer, so the lock around it can be released before writing.
pub fn take_snapshot(ring: &RingBuffer, length_us: i64) -> Result<Snapshot, ClipError> {
    ring.snapshot(length_us).ok_or(ClipError::NothingRecorded)
}

/// Writes a snapshot into `folder` and returns the new file's path.
pub fn save_snapshot(
    snapshot: &Snapshot,
    folder: &Path,
    video_format: &VideoFormat,
    audio_format: Option<&AudioFormat>,
    now: DateTime<Local>,
) -> Result<PathBuf, ClipError> {
    std::fs::create_dir_all(folder)?;
    let path = unique_clip_path(folder, now);
    write_mp4(&path, video_format, audio_format, snapshot)?;
    Ok(path)
}

fn unique_clip_path(folder: &Path, now: DateTime<Local>) -> PathBuf {
    let stem = format!("diff-{}", now.format("%Y-%m-%d_%H-%M-%S"));
    let first = folder.join(format!("{stem}.mp4"));
    if !first.exists() {
        return first;
    }
    (2u32..)
        .map(|n| folder.join(format!("{stem}-{n}.mp4")))
        .find(|candidate| !candidate.exists())
        .expect("an unused file name exists")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Packet;
    use chrono::TimeZone;

    #[test]
    fn debouncer_accepts_first_and_rejects_within_cooldown() {
        let mut debouncer = Debouncer::new(Duration::from_secs(2));
        let start = Instant::now();
        assert!(debouncer.accept(start));
        assert!(!debouncer.accept(start + Duration::from_millis(1_999)));
    }

    #[test]
    fn debouncer_accepts_again_after_cooldown() {
        let mut debouncer = Debouncer::new(Duration::from_secs(2));
        let start = Instant::now();
        assert!(debouncer.accept(start));
        assert!(debouncer.accept(start + Duration::from_secs(2)));
    }

    #[test]
    fn rejected_triggers_do_not_extend_the_cooldown() {
        let mut debouncer = Debouncer::new(Duration::from_secs(2));
        let start = Instant::now();
        assert!(debouncer.accept(start));
        assert!(!debouncer.accept(start + Duration::from_secs(1)));
        assert!(debouncer.accept(start + Duration::from_secs(2)));
    }

    #[test]
    fn empty_buffer_reports_nothing_recorded() {
        let ring = RingBuffer::new(1_000_000, usize::MAX);
        assert!(matches!(take_snapshot(&ring, 1_000_000), Err(ClipError::NothingRecorded)));
    }

    #[test]
    fn saves_with_a_dated_name_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("nested");
        let format = VideoFormat { width: 16, height: 16, sps: vec![0x67, 0x42, 0, 0x1e], pps: vec![0x68] };
        let snapshot = Snapshot { video: vec![Packet::video(0, true, vec![0u8; 8])], audio: vec![] };
        let now = Local.with_ymd_and_hms(2026, 11, 9, 20, 15, 32).unwrap();

        let first = save_snapshot(&snapshot, &folder, &format, None, now).unwrap();
        let second = save_snapshot(&snapshot, &folder, &format, None, now).unwrap();

        assert_eq!(first.file_name().unwrap(), "diff-2026-11-09_20-15-32.mp4");
        assert_eq!(second.file_name().unwrap(), "diff-2026-11-09_20-15-32-2.mp4");
    }
}
