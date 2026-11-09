//! The clip library. The save folder is the source of truth; there's no database.
//! Stars live beside the clips, in a small file listing the starred file names.

use mp4::Mp4Reader;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

const STARS_FILE: &str = ".diff-stars.json";

/// Commands run concurrently, and each star change rewrites the whole file.
static STARS_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ClipInfo {
    pub path: PathBuf,
    pub name: String,
    pub size_bytes: u64,
    pub duration_seconds: f64,
    /// Milliseconds since the Unix epoch.
    pub modified_ms: u64,
    pub starred: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("that name can't be used for a file")]
    InvalidName,
    #[error("a clip with that name already exists")]
    NameTaken,
    #[error("that file isn't in your clips folder")]
    OutsideFolder,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Lists MP4s in `folder`, newest first. A missing folder is an empty library.
pub fn list_clips(folder: &Path) -> io::Result<Vec<ClipInfo>> {
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let stars = read_stars(folder);
    let mut clips: Vec<ClipInfo> = entries
        .flatten()
        .filter_map(|entry| clip_info(&entry.path(), &stars))
        .collect();
    clips.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms).then_with(|| a.name.cmp(&b.name)));
    Ok(clips)
}

fn clip_info(path: &Path, stars: &BTreeSet<String>) -> Option<ClipInfo> {
    let is_mp4 = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
    if !is_mp4 {
        return None;
    }
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let modified_ms = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    Some(ClipInfo {
        path: path.to_path_buf(),
        name: path.file_stem()?.to_string_lossy().into_owned(),
        size_bytes: metadata.len(),
        duration_seconds: read_duration(path).unwrap_or(0.0),
        modified_ms,
        starred: star_key(path).is_some_and(|key| stars.contains(&key)),
    })
}

fn star_key(path: &Path) -> Option<String> {
    Some(path.file_name()?.to_string_lossy().into_owned())
}

/// A missing or unreadable stars file means nothing is starred.
fn read_stars(folder: &Path) -> BTreeSet<String> {
    fs::read(folder.join(STARS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Applies `change` to the stars in `folder`, saving only if it returns true.
fn update_stars(folder: &Path, change: impl FnOnce(&mut BTreeSet<String>) -> bool) -> io::Result<()> {
    let _guard = STARS_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut stars = read_stars(folder);
    if !change(&mut stars) {
        return Ok(());
    }
    // Replacing the file in one step means a crash can't leave half a list.
    let temporary = folder.join(format!("{STARS_FILE}.tmp"));
    fs::write(&temporary, serde_json::to_vec(&stars)?)?;
    fs::rename(temporary, folder.join(STARS_FILE))
}

/// Stars or unstars a clip.
pub fn set_starred(path: &Path, starred: bool) -> Result<(), LibraryError> {
    let folder = path.parent().ok_or(LibraryError::InvalidName)?;
    let key = star_key(path).ok_or(LibraryError::InvalidName)?;
    update_stars(folder, |stars| if starred { stars.insert(key) } else { stars.remove(&key) })?;
    Ok(())
}

fn read_duration(path: &Path) -> Option<f64> {
    let file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let reader = Mp4Reader::read_header(BufReader::new(file), size).ok()?;
    Some(reader.duration().as_secs_f64())
}

/// Confirms `path` is directly inside `folder`, resolving any `..` or links.
/// Every command that modifies a clip goes through this.
pub fn ensure_in_folder(path: &Path, folder: &Path) -> Result<PathBuf, LibraryError> {
    let resolved = path.canonicalize()?;
    let folder = folder.canonicalize()?;
    if resolved.parent() == Some(folder.as_path()) {
        Ok(resolved)
    } else {
        Err(LibraryError::OutsideFolder)
    }
}

/// Resolves a batch of clips with `ensure_in_folder`, dropping any that no
/// longer exist so one stale selection can't fail the whole batch.
pub fn existing_in_folder(paths: &[PathBuf], folder: &Path) -> Result<Vec<PathBuf>, LibraryError> {
    let mut resolved = Vec::with_capacity(paths.len());
    for path in paths {
        match ensure_in_folder(path, folder) {
            Ok(path) => resolved.push(path),
            Err(LibraryError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(resolved)
}

/// Renames a clip within its folder and returns the new path.
pub fn rename_clip(path: &Path, new_name: &str) -> Result<PathBuf, LibraryError> {
    let name = validate_name(new_name)?;
    let parent = path.parent().ok_or(LibraryError::InvalidName)?;
    let destination = parent.join(format!("{name}.mp4"));
    if destination == path {
        return Ok(destination);
    }
    if destination.exists() {
        return Err(LibraryError::NameTaken);
    }
    fs::rename(path, &destination)?;
    if let (Some(old), Some(new)) = (star_key(path), star_key(&destination)) {
        // The rename already happened, so losing the star shouldn't report a failure.
        let _ = update_stars(parent, |stars| stars.remove(&old) && stars.insert(new));
    }
    Ok(destination)
}

fn validate_name(name: &str) -> Result<&str, LibraryError> {
    let name = name.trim();
    let name = name.strip_suffix(".mp4").unwrap_or(name).trim_end();
    let forbidden = |c: char| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control();
    let reserved_windows_name = {
        let upper = name.to_ascii_uppercase();
        let stem = upper.split('.').next().unwrap_or("");
        matches!(stem, "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit())
    };
    if name.is_empty()
        || name.len() > 200
        || name.starts_with('.')
        || name.ends_with('.')
        || name.contains(forbidden)
        || reserved_windows_name
    {
        return Err(LibraryError::InvalidName);
    }
    Ok(name)
}

/// Picks an unused `<name>-trim.mp4` path next to `source`.
pub fn trimmed_path(source: &Path) -> Option<PathBuf> {
    let stem = source.file_stem()?.to_string_lossy().into_owned();
    let parent = source.parent()?;
    let first = parent.join(format!("{stem}-trim.mp4"));
    if !first.exists() {
        return Some(first);
    }
    (2u32..)
        .map(|n| parent.join(format!("{stem}-trim-{n}.mp4")))
        .find(|candidate| !candidate.exists())
}

/// A thumbnail's cache file name, tied to the clip's path and modified time so
/// that a renamed or replaced clip gets a fresh one.
pub fn thumbnail_file_name(clip: &ClipInfo) -> String {
    // FNV-1a: stable across runs, unlike the standard library's default hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let path = clip.path.to_string_lossy();
    for byte in path.bytes().chain(clip.modified_ms.to_le_bytes()) {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}.jpg")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        fs::write(path, b"not really a video").unwrap();
    }

    #[test]
    fn missing_folder_is_an_empty_library() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_clips(&dir.path().join("none")).unwrap().is_empty());
    }

    #[test]
    fn lists_only_mp4s_and_tolerates_unreadable_ones() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.mp4"));
        touch(&dir.path().join("b.MP4"));
        touch(&dir.path().join("c.mp4.part"));
        touch(&dir.path().join("notes.txt"));
        fs::create_dir(dir.path().join("folder.mp4")).unwrap();

        let clips = list_clips(dir.path()).unwrap();
        let mut names: Vec<_> = clips.iter().map(|c| c.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["a", "b"]);
        assert!(clips.iter().all(|c| c.duration_seconds == 0.0));
    }

    #[test]
    fn rename_keeps_the_extension_and_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("clip.mp4");
        touch(&original);
        let renamed = rename_clip(&original, "  best play.mp4 ").unwrap();
        assert_eq!(renamed, dir.path().join("best play.mp4"));
        assert!(renamed.exists() && !original.exists());
    }

    #[test]
    fn rename_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("a.mp4"), dir.path().join("b.mp4"));
        touch(&first);
        touch(&second);
        assert!(matches!(rename_clip(&first, "b"), Err(LibraryError::NameTaken)));
        assert!(first.exists());
    }

    #[test]
    fn rename_to_the_same_name_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.mp4");
        touch(&path);
        assert_eq!(rename_clip(&path, "a").unwrap(), path);
        assert!(path.exists());
    }

    #[test]
    fn rejects_names_that_could_escape_or_are_invalid_on_windows() {
        for bad in ["", "   ", "../evil", "a/b", "a\\b", ".hidden", "a:b", "con", "COM1", "nul.txt", "dot."] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
        for good in ["clip", "best play 2", "console", "com10"] {
            assert!(validate_name(good).is_ok(), "{good:?} should be allowed");
        }
    }

    #[test]
    fn ensure_in_folder_blocks_paths_outside() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("clips");
        fs::create_dir(&folder).unwrap();
        let inside = folder.join("a.mp4");
        let outside = dir.path().join("secret.mp4");
        touch(&inside);
        touch(&outside);

        assert!(ensure_in_folder(&inside, &folder).is_ok());
        assert!(matches!(ensure_in_folder(&outside, &folder), Err(LibraryError::OutsideFolder)));
        assert!(matches!(
            ensure_in_folder(&folder.join("../secret.mp4"), &folder),
            Err(LibraryError::OutsideFolder)
        ));
    }

    #[test]
    fn batch_skips_missing_clips_but_refuses_outside_ones() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("clips");
        fs::create_dir(&folder).unwrap();
        let inside = folder.join("a.mp4");
        let outside = dir.path().join("secret.mp4");
        touch(&inside);
        touch(&outside);

        let resolved = existing_in_folder(&[inside.clone(), folder.join("gone.mp4")], &folder).unwrap();
        assert_eq!(resolved, [inside.canonicalize().unwrap()]);
        assert!(matches!(
            existing_in_folder(&[inside, outside], &folder),
            Err(LibraryError::OutsideFolder)
        ));
    }

    fn is_starred(folder: &Path, name: &str) -> bool {
        list_clips(folder).unwrap().iter().any(|clip| clip.name == name && clip.starred)
    }

    #[test]
    fn stars_can_be_set_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("a.mp4"), dir.path().join("b.mp4"));
        touch(&first);
        touch(&second);

        set_starred(&first, true).unwrap();
        assert!(is_starred(dir.path(), "a"));
        assert!(!is_starred(dir.path(), "b"));

        set_starred(&first, false).unwrap();
        assert!(!is_starred(dir.path(), "a"));
    }

    #[test]
    fn a_renamed_clip_keeps_its_star() {
        let dir = tempfile::tempdir().unwrap();
        let (starred, plain) = (dir.path().join("a.mp4"), dir.path().join("b.mp4"));
        touch(&starred);
        touch(&plain);
        set_starred(&starred, true).unwrap();

        rename_clip(&starred, "kept").unwrap();
        rename_clip(&plain, "still plain").unwrap();
        assert!(is_starred(dir.path(), "kept"));
        assert!(!is_starred(dir.path(), "still plain"));
    }

    #[test]
    fn a_new_clip_does_not_inherit_a_deleted_clips_star() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.mp4");
        touch(&path);
        set_starred(&path, true).unwrap();

        set_starred(&path, false).unwrap();
        fs::remove_file(&path).unwrap();
        touch(&path);
        assert!(!is_starred(dir.path(), "a"));
    }

    #[test]
    fn a_damaged_stars_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.mp4");
        touch(&path);
        fs::write(dir.path().join(STARS_FILE), b"{ not json").unwrap();
        assert!(!is_starred(dir.path(), "a"));

        set_starred(&path, true).unwrap();
        assert!(is_starred(dir.path(), "a"));
    }

    #[test]
    fn trimmed_path_never_collides() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("clip.mp4");
        assert_eq!(trimmed_path(&source).unwrap(), dir.path().join("clip-trim.mp4"));
        touch(&dir.path().join("clip-trim.mp4"));
        assert_eq!(trimmed_path(&source).unwrap(), dir.path().join("clip-trim-2.mp4"));
    }

    #[test]
    fn thumbnail_name_changes_with_the_clip() {
        let clip = ClipInfo {
            path: "/a.mp4".into(),
            name: "a".into(),
            size_bytes: 1,
            duration_seconds: 1.0,
            modified_ms: 5,
            starred: false,
        };
        let touched = ClipInfo { modified_ms: 6, ..clip.clone() };
        assert_eq!(thumbnail_file_name(&clip), thumbnail_file_name(&clip.clone()));
        assert_ne!(thumbnail_file_name(&clip), thumbnail_file_name(&touched));
    }
}
