//! User settings, stored as one small JSON file.

use crate::capture::{CaptureConfig, Source};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const CLIP_LENGTHS: [u32; 5] = [15, 30, 60, 120, 300];
const DEFAULT_CLIP_SECONDS: u32 = 30;
/// Extra history kept so a clip still covers its full length after snapping
/// back to a keyframe.
const BUFFER_MARGIN_SECONDS: u32 = 2;
/// Headroom for bitrate spikes before the buffer starts dropping old video.
const MEMORY_HEADROOM: u64 = 2;
const MEMORY_CEILING: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Quality {
    Low,
    Medium,
    High,
}

impl Quality {
    pub fn max_height(self) -> u32 {
        match self {
            Quality::Low => 720,
            Quality::Medium | Quality::High => 1080,
        }
    }

    pub fn fps(self) -> u32 {
        match self {
            Quality::Low | Quality::Medium => 30,
            Quality::High => 60,
        }
    }

    pub fn bitrate(self) -> u32 {
        match self {
            Quality::Low => 5_000_000,
            Quality::Medium => 10_000_000,
            Quality::High => 15_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// `None` records the primary display.
    pub source: Option<Source>,
    pub clip_seconds: u32,
    pub hotkey: String,
    pub voice_enabled: bool,
    pub include_mic: bool,
    pub quality: Quality,
    pub save_folder: PathBuf,
    pub start_at_login: bool,
    pub setup_complete: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            source: None,
            clip_seconds: DEFAULT_CLIP_SECONDS,
            hotkey: "Alt+Shift+KeyC".into(),
            voice_enabled: true,
            include_mic: true,
            quality: Quality::High,
            save_folder: PathBuf::new(),
            start_at_login: false,
            setup_complete: false,
        }
    }
}

impl Settings {
    /// Reads settings, falling back to defaults if the file is missing or
    /// damaged so that a bad file can't stop the app from starting.
    pub fn load(path: &Path, default_folder: &Path) -> Self {
        let mut settings: Settings = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        if settings.save_folder.as_os_str().is_empty() {
            settings.save_folder = default_folder.to_path_buf();
        }
        if !CLIP_LENGTHS.contains(&settings.clip_seconds) {
            settings.clip_seconds = DEFAULT_CLIP_SECONDS;
        }
        settings
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&temporary, path)
    }

    pub fn capture_config(&self) -> CaptureConfig {
        CaptureConfig {
            source: self.source.clone(),
            max_height: self.quality.max_height(),
            fps: self.quality.fps(),
            bitrate: self.quality.bitrate(),
            mic: None,
            echo: None,
        }
    }

    pub fn clip_length_us(&self) -> i64 {
        self.clip_seconds as i64 * 1_000_000
    }

    pub fn buffer_window_us(&self) -> i64 {
        (self.clip_seconds + BUFFER_MARGIN_SECONDS) as i64 * 1_000_000
    }

    pub fn buffer_memory_limit(&self) -> usize {
        let seconds = (self.clip_seconds + BUFFER_MARGIN_SECONDS) as u64;
        let expected = self.quality.bitrate() as u64 / 8 * seconds;
        (expected * MEMORY_HEADROOM).min(MEMORY_CEILING) as usize
    }

    /// True if moving from `self` to `other` requires restarting capture.
    pub fn capture_differs(&self, other: &Settings) -> bool {
        self.source != other.source
            || self.quality != other.quality
            || self.clip_seconds != other.clip_seconds
            || self.include_mic != other.include_mic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults_with_the_default_folder() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings::load(&dir.path().join("nope.json"), Path::new("/videos/diff"));
        assert_eq!(settings.save_folder, Path::new("/videos/diff"));
        assert_eq!(settings.clip_seconds, 30);
        assert!(settings.include_mic);
    }

    #[test]
    fn damaged_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "{ not json").unwrap();
        assert_eq!(Settings::load(&path, Path::new("/v")).clip_seconds, 30);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.json");
        let settings = Settings {
            source: Some(Source::Region { display_id: 2, x: 1.0, y: 2.0, width: 3.0, height: 4.0 }),
            clip_seconds: 60,
            hotkey: "F9".into(),
            quality: Quality::Low,
            save_folder: "/clips".into(),
            setup_complete: true,
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path, Path::new("/other")), settings);
    }

    #[test]
    fn unknown_clip_length_falls_back_to_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"clipSeconds": 17}"#).unwrap();
        assert_eq!(Settings::load(&path, Path::new("/v")).clip_seconds, 30);
    }

    #[test]
    fn memory_limit_has_headroom_and_a_ceiling() {
        let short = Settings { clip_seconds: 30, quality: Quality::Low, ..Settings::default() };
        assert_eq!(short.buffer_memory_limit(), 5_000_000 / 8 * 32 * 2);

        let long = Settings { clip_seconds: 300, quality: Quality::High, ..Settings::default() };
        assert!(long.buffer_memory_limit() as u64 <= MEMORY_CEILING);
    }

    #[test]
    fn only_capture_related_changes_restart_capture() {
        let base = Settings::default();
        assert!(!base.capture_differs(&Settings { hotkey: "F9".into(), ..base.clone() }));
        assert!(base.capture_differs(&Settings { clip_seconds: 60, ..base.clone() }));
        assert!(base.capture_differs(&Settings { quality: Quality::Low, ..base.clone() }));
    }
}
