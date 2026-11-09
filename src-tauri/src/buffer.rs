//! In-memory rolling buffer of already-encoded video and audio.
//!
//! Video is stored as whole GOPs so that every clip can start on a keyframe
//! without re-encoding. Nothing here touches the disk.

use bytes::Bytes;
use std::collections::VecDeque;

/// Audio may start this far away from the first video frame and still count as
/// aligned. Roughly one AAC frame at 48 kHz.
const AUDIO_ALIGN_TOLERANCE_US: i64 = 25_000;

/// One encoded frame. Timestamps are microseconds on a shared clock.
/// Video is expected to have no B-frames, so presentation order is decode order.
#[derive(Debug, Clone)]
pub struct Packet {
    pub pts_us: i64,
    pub duration_us: i64,
    pub keyframe: bool,
    pub data: Bytes,
}

impl Packet {
    pub fn video(pts_us: i64, keyframe: bool, data: impl Into<Bytes>) -> Self {
        Self { pts_us, duration_us: 0, keyframe, data: data.into() }
    }

    pub fn audio(pts_us: i64, duration_us: i64, data: impl Into<Bytes>) -> Self {
        Self { pts_us, duration_us, keyframe: true, data: data.into() }
    }

    fn end_us(&self) -> i64 {
        self.pts_us + self.duration_us
    }
}

#[derive(Debug)]
struct Gop {
    start_us: i64,
    bytes: usize,
    packets: Vec<Packet>,
}

/// A point-in-time copy of part of the buffer. Packet payloads are shared, so
/// taking one is cheap and recording can continue while it is written out.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    pub video: Vec<Packet>,
    pub audio: Vec<Packet>,
}

#[derive(Debug)]
pub struct RingBuffer {
    window_us: i64,
    max_bytes: usize,
    gops: VecDeque<Gop>,
    audio: VecDeque<Packet>,
    audio_bytes: usize,
    video_bytes: usize,
    newest_video_us: i64,
}

impl RingBuffer {
    /// `window_us` is how much history to retain. `max_bytes` is a hard ceiling
    /// that protects memory if the bitrate spikes; it takes priority over the
    /// window, except that the newest GOP is always kept.
    pub fn new(window_us: i64, max_bytes: usize) -> Self {
        Self {
            window_us,
            max_bytes,
            gops: VecDeque::new(),
            audio: VecDeque::new(),
            audio_bytes: 0,
            video_bytes: 0,
            newest_video_us: 0,
        }
    }

    pub fn push_video(&mut self, packet: Packet) {
        if packet.keyframe {
            self.gops.push_back(Gop {
                start_us: packet.pts_us,
                bytes: 0,
                packets: Vec::with_capacity(64),
            });
        }
        // Frames before the first keyframe can't be decoded, so drop them.
        let Some(gop) = self.gops.back_mut() else { return };
        self.newest_video_us = packet.pts_us;
        gop.bytes += packet.data.len();
        self.video_bytes += packet.data.len();
        gop.packets.push(packet);
        self.evict();
    }

    pub fn push_audio(&mut self, packet: Packet) {
        self.audio_bytes += packet.data.len();
        self.audio.push_back(packet);
        self.evict();
    }

    pub fn memory_bytes(&self) -> usize {
        self.video_bytes + self.audio_bytes
    }

    pub fn clear(&mut self) {
        self.gops.clear();
        self.audio.clear();
        self.audio_bytes = 0;
        self.video_bytes = 0;
        self.newest_video_us = 0;
    }

    /// Changes how much history is retained. Shrinking takes effect immediately.
    pub fn set_window(&mut self, window_us: i64, max_bytes: usize) {
        self.window_us = window_us;
        self.max_bytes = max_bytes;
        self.evict();
    }

    fn evict(&mut self) {
        // The oldest GOP stays until the next one alone covers the window.
        while self.gops.len() > 1
            && self.newest_video_us - self.gops[1].start_us >= self.window_us
        {
            self.pop_gop();
        }
        while self.gops.len() > 1 && self.memory_bytes() > self.max_bytes {
            self.pop_gop();
        }

        let floor = match (self.gops.front(), self.audio.back()) {
            (Some(gop), _) => gop.start_us,
            // Audio arrived before any video: only keep one window of it.
            (None, Some(newest)) => newest.pts_us - self.window_us,
            (None, None) => return,
        };
        while self.audio.front().is_some_and(|p| p.end_us() <= floor) {
            let dropped = self.audio.pop_front().expect("front checked above");
            self.audio_bytes -= dropped.data.len();
        }
    }

    fn pop_gop(&mut self) {
        if let Some(gop) = self.gops.pop_front() {
            self.video_bytes -= gop.bytes;
        }
    }

    /// Copies out at least `length_us` of history, or everything available if
    /// less has been recorded. Starts on the nearest keyframe at or before the
    /// requested start. Returns `None` until a keyframe has been recorded.
    pub fn snapshot(&self, length_us: i64) -> Option<Snapshot> {
        if self.gops.is_empty() {
            return None;
        }
        let target = self.newest_video_us - length_us;
        let mut first = self
            .gops
            .iter()
            .rposition(|gop| gop.start_us <= target)
            .unwrap_or(0);

        // If audio begins after the first frame, an MP4 written from durations
        // alone would play it early. Move to a keyframe that has audio, or
        // leave the audio out if there isn't one yet.
        let mut include_audio = !self.audio.is_empty();
        if let Some(audio_start) = self.audio.front().map(|p| p.pts_us) {
            if audio_start > self.gops[first].start_us + AUDIO_ALIGN_TOLERANCE_US {
                match self
                    .gops
                    .iter()
                    .position(|g| g.start_us + AUDIO_ALIGN_TOLERANCE_US >= audio_start)
                {
                    Some(aligned) => first = first.max(aligned),
                    None => include_audio = false,
                }
            }
        }

        let video: Vec<Packet> = self
            .gops
            .iter()
            .skip(first)
            .flat_map(|gop| gop.packets.iter().cloned())
            .collect();

        let audio = if include_audio {
            select_audio(self.audio.iter(), self.gops[first].start_us, self.newest_video_us)
        } else {
            Vec::new()
        };

        Some(Snapshot { video, audio })
    }
}

/// Keeps audio packets that overlap `[start_us, end_us]`. The first packet may
/// straddle `start_us`, which leaves the tracks less than one AAC frame apart.
pub fn select_audio<'a>(
    audio: impl Iterator<Item = &'a Packet>,
    start_us: i64,
    end_us: i64,
) -> Vec<Packet> {
    audio
        .filter(|p| p.end_us() > start_us && p.pts_us <= end_us)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: i64 = 1_000_000;
    const FRAME: i64 = SECOND / 10;

    /// Feeds `seconds` of 10 fps video with a keyframe every second.
    fn feed_video(ring: &mut RingBuffer, seconds: i64) {
        for i in 0..seconds * 10 {
            ring.push_video(Packet::video(i * FRAME, i % 10 == 0, vec![0u8; 100]));
        }
    }

    fn feed_audio(ring: &mut RingBuffer, from_us: i64, to_us: i64) {
        let step = 20_000;
        let mut pts = from_us;
        while pts < to_us {
            ring.push_audio(Packet::audio(pts, step, vec![0u8; 10]));
            pts += step;
        }
    }

    #[test]
    fn empty_buffer_has_no_snapshot() {
        assert!(RingBuffer::new(5 * SECOND, usize::MAX).snapshot(SECOND).is_none());
    }

    #[test]
    fn frames_before_first_keyframe_are_dropped() {
        let mut ring = RingBuffer::new(5 * SECOND, usize::MAX);
        ring.push_video(Packet::video(0, false, vec![1u8; 10]));
        assert!(ring.snapshot(SECOND).is_none());
        assert_eq!(ring.memory_bytes(), 0);
    }

    #[test]
    fn evicts_old_gops_but_still_covers_the_window() {
        let mut ring = RingBuffer::new(3 * SECOND, usize::MAX);
        feed_video(&mut ring, 20);
        let snap = ring.snapshot(3 * SECOND).unwrap();
        let start = snap.video.first().unwrap().pts_us;
        let end = snap.video.last().unwrap().pts_us;
        assert!(end - start >= 3 * SECOND, "covers only {}us", end - start);
        assert!(end - start < 4 * SECOND + FRAME, "kept {}us", end - start);
        assert_eq!(ring.gops.len(), 4);
    }

    #[test]
    fn snapshot_starts_on_a_keyframe() {
        let mut ring = RingBuffer::new(10 * SECOND, usize::MAX);
        feed_video(&mut ring, 8);
        let snap = ring.snapshot(2_500_000).unwrap();
        assert!(snap.video[0].keyframe);
        // The newest frame is 7.9s, so the target is 5.4s and the GOP is 5.0s.
        assert_eq!(snap.video[0].pts_us, 5 * SECOND);
    }

    #[test]
    fn snapshot_returns_everything_when_history_is_short() {
        let mut ring = RingBuffer::new(30 * SECOND, usize::MAX);
        feed_video(&mut ring, 2);
        assert_eq!(ring.snapshot(30 * SECOND).unwrap().video.len(), 20);
    }

    #[test]
    fn byte_limit_overrides_window_but_keeps_newest_gop() {
        let mut ring = RingBuffer::new(100 * SECOND, 1);
        feed_video(&mut ring, 5);
        assert_eq!(ring.gops.len(), 1);
        assert_eq!(ring.snapshot(100 * SECOND).unwrap().video[0].pts_us, 4 * SECOND);
    }

    #[test]
    fn memory_accounting_returns_to_zero_on_clear() {
        let mut ring = RingBuffer::new(2 * SECOND, usize::MAX);
        feed_video(&mut ring, 10);
        feed_audio(&mut ring, 0, 10 * SECOND);
        let video: usize = ring.gops.iter().map(|g| g.bytes).sum();
        let audio: usize = ring.audio.iter().map(|p| p.data.len()).sum();
        assert_eq!(ring.memory_bytes(), video + audio);
        ring.clear();
        assert_eq!(ring.memory_bytes(), 0);
    }

    #[test]
    fn shrinking_the_window_evicts_immediately() {
        let mut ring = RingBuffer::new(30 * SECOND, usize::MAX);
        feed_video(&mut ring, 20);
        ring.set_window(2 * SECOND, usize::MAX);
        assert_eq!(ring.gops.len(), 3);
    }

    #[test]
    fn audio_is_trimmed_to_the_video_range() {
        let mut ring = RingBuffer::new(10 * SECOND, usize::MAX);
        feed_video(&mut ring, 5);
        feed_audio(&mut ring, 0, 10 * SECOND);
        let snap = ring.snapshot(2 * SECOND).unwrap();
        let start = snap.video[0].pts_us;
        let end = snap.video.last().unwrap().pts_us;
        let first = snap.audio.first().unwrap();
        assert!(first.pts_us <= start && first.pts_us + first.duration_us > start);
        assert!(snap.audio.last().unwrap().pts_us <= end);
    }

    #[test]
    fn late_audio_moves_the_clip_to_an_aligned_keyframe() {
        let mut ring = RingBuffer::new(10 * SECOND, usize::MAX);
        feed_video(&mut ring, 5);
        feed_audio(&mut ring, 2_500_000, 5 * SECOND);
        let snap = ring.snapshot(10 * SECOND).unwrap();
        assert_eq!(snap.video[0].pts_us, 3 * SECOND);
        assert!(!snap.audio.is_empty());
    }

    #[test]
    fn audio_is_dropped_when_no_keyframe_can_align_with_it() {
        let mut ring = RingBuffer::new(10 * SECOND, usize::MAX);
        feed_video(&mut ring, 5);
        feed_audio(&mut ring, 4_500_000, 5 * SECOND);
        let snap = ring.snapshot(10 * SECOND).unwrap();
        assert_eq!(snap.video[0].pts_us, 0);
        assert!(snap.audio.is_empty());
    }

    #[test]
    fn audio_without_video_stays_bounded() {
        let mut ring = RingBuffer::new(SECOND, usize::MAX);
        feed_audio(&mut ring, 0, 60 * SECOND);
        assert!(ring.audio.len() <= 51, "held {} packets", ring.audio.len());
    }
}
