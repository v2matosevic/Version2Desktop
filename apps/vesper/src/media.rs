use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

pub const QUEUE_LIMIT: usize = 2048;
pub const HISTORY_LIMIT: usize = 256;
pub const HISTORY_BYTES: u64 = 1024 * 1024;

fn local_namespace(path: &Path) -> Result<(), String> {
    let raw = path.to_string_lossy();
    let upper = raw.to_ascii_uppercase();
    if let Some(rest) = upper.strip_prefix("\\\\?\\") {
        let bytes = rest.as_bytes();
        if bytes.len() < 3
            || !bytes[0].is_ascii_alphabetic()
            || bytes[1] != b':'
            || bytes[2] != b'\\'
        {
            return Err("Only local drive filesystem paths are supported.".into());
        }
    } else if upper.starts_with("\\\\") || upper.starts_with("//") {
        return Err("Network and device paths are unavailable in this offline player.".into());
    }
    Ok(())
}

/// Check every directory before traversal can enter a network/cloud reparse target.
fn direct_local_path(path: &Path) -> Result<(), String> {
    local_namespace(path)?;
    let absolute = std::path::absolute(path).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDriveTypeW(root: *const u16) -> u32;
        }
        let text = absolute.to_string_lossy();
        let text = text.strip_prefix("\\\\?\\").unwrap_or(&text);
        if text.as_bytes().get(1) == Some(&b':') {
            let root: Vec<u16> = format!("{}\\", &text[..2])
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            if unsafe { GetDriveTypeW(root.as_ptr()) } == 4 {
                return Err("Mapped network drives are unavailable in this offline player.".into());
            }
        }
        let mut prefix = PathBuf::new();
        for component in absolute.components() {
            prefix.push(component.as_os_str());
            if prefix.parent().is_none() {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&prefix).map_err(|e| e.to_string())?;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err("Choose a direct local path. Symlinks, junctions and cloud placeholders are unavailable.".into());
            }
        }
    }
    Ok(())
}

/// Validate actual local files. mpv never receives a URL, playlist or device path.
pub fn local_input(path: &Path) -> Result<PathBuf, String> {
    direct_local_path(path)?;
    let resolved = path.canonicalize().map_err(|e| e.to_string())?;
    local_namespace(&resolved)?;
    Ok(resolved)
}
pub fn local_file(path: &Path) -> Result<PathBuf, String> {
    direct_local_path(path)?;
    let meta = std::fs::metadata(path).map_err(|e| format!("Cannot open file: {e}"))?;
    if !meta.is_file() {
        return Err("Choose a regular local file.".into());
    }
    if meta.len() == 0 {
        return Err("This file is empty.".into());
    }
    if meta.len() > 1_099_511_627_776 {
        return Err("Files above 1 TiB are outside Vesper's safety limit.".into());
    }
    if matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "m3u" | "m3u8" | "pls" | "url" | "lnk" | "cue" | "ffconcat"
    ) {
        return Err("Playlists, shortcuts and external references are unavailable.".into());
    }
    let resolved = path
        .canonicalize()
        .map_err(|e| format!("Cannot resolve file: {e}"))?;
    local_namespace(&resolved)?;
    Ok(resolved)
}

/// A local file that passed [`validate`]: canonical path plus the identity used for resume.
/// Only `validate` builds one, so holding it means the filesystem checks already ran (off the
/// UI thread) and `Player::open_validated` need not repeat them.
#[derive(Clone, Debug)]
pub struct Validated {
    path: PathBuf,
    identity: Option<(u64, u64)>,
}
impl Validated {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn into_path(self) -> PathBuf {
        self.path
    }
    /// File size and modification time at the moment of the check; the key resume history uses.
    pub fn identity(&self) -> Option<(u64, u64)> {
        self.identity
    }
}

/// Every filesystem check an open needs, in one call that may block on a slow or busy disk.
pub fn validate(path: &Path) -> Result<Validated, String> {
    let path = local_file(path)?;
    let identity = identity(&path);
    Ok(Validated { path, identity })
}

pub fn candidate(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "mp4"
            | "m4v"
            | "mov"
            | "mkv"
            | "avi"
            | "webm"
            | "mpg"
            | "mpeg"
            | "ts"
            | "m2ts"
            | "wmv"
            | "flv"
            | "ogv"
            | "3gp"
    )
}

/// Extensions that name a container already proven by a fixture under another extension.
pub const EXTENSION_ALIASES: &[(&str, &str)] = &[("m4v", "mp4"), ("mpeg", "mpg")];

pub fn subtitle_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "srt" | "ass" | "ssa" | "vtt"
    )
}

/// What a drop of files and folders means. Built on a worker thread: every dropped path is
/// checked against the local-path rules, which touch the filesystem.
#[derive(Debug, Default)]
pub struct DropPlan {
    pub videos: Vec<PathBuf>,
    pub subtitles: Vec<PathBuf>,
    /// The first dropped folder; its videos are listed by `folder`, one level only.
    pub folder: Option<PathBuf>,
    pub errors: Vec<String>,
}

pub const DROPPED_SUBTITLE_LIMIT: usize = 16;

pub fn classify_drops(paths: impl IntoIterator<Item = PathBuf>) -> DropPlan {
    let mut plan = DropPlan::default();
    for path in paths.into_iter().take(QUEUE_LIMIT) {
        match local_input(&path) {
            Ok(path) if path.is_dir() => {
                plan.folder.get_or_insert(path);
            }
            Ok(path) if subtitle_extension(&path) => {
                if plan.subtitles.len() < DROPPED_SUBTITLE_LIMIT {
                    plan.subtitles.push(path);
                }
            }
            Ok(path) => plan.videos.push(path),
            Err(e) => plan.errors.push(e),
        }
    }
    plan
}

/// One explicitly chosen folder, no recursion, symlink traversal or disk scan.
pub fn folder(path: &Path) -> Result<Vec<PathBuf>, String> {
    direct_local_path(path)?;
    let resolved = path.canonicalize().map_err(|e| e.to_string())?;
    local_namespace(&resolved)?;
    let mut files = Vec::new();
    // Bound inspected entries as well as accepted files.
    for entry in std::fs::read_dir(resolved)
        .map_err(|e| e.to_string())?
        .take(10_000)
    {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map_err(|e| e.to_string())?.is_file() && candidate(&entry.path()) {
            files.push(entry.path());
            if files.len() == QUEUE_LIMIT {
                break;
            }
        }
    }
    files.sort_by_cached_key(|p| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
    });
    Ok(files)
}

pub fn seek(position: f64, offset: f64, duration: f64) -> f64 {
    if !position.is_finite() || !offset.is_finite() || !duration.is_finite() {
        return 0.0;
    }
    (position + offset).clamp(0.0, duration.max(0.0))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resume {
    pub path: PathBuf,
    pub seconds: f64,
    pub size: u64,
    pub modified: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct History {
    pub entries: VecDeque<Resume>,
}

fn identity(path: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    let t = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos()
        .try_into()
        .ok()?;
    Some((m.len(), t))
}
impl History {
    pub fn load(path: &Path) -> Self {
        if std::fs::metadata(path)
            .map(|m| m.len() > HISTORY_BYTES)
            .unwrap_or(true)
        {
            return Self::default();
        }
        let mut h: Self = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        h.entries.truncate(HISTORY_LIMIT);
        h.entries
            .retain(|r| r.seconds.is_finite() && r.seconds >= 0.0);
        h
    }
    pub fn position(&self, path: &Path) -> Option<f64> {
        self.position_for(path, identity(path)?)
    }
    /// Resume position for a file whose identity was already read (no filesystem access).
    pub fn position_for(&self, path: &Path, id: (u64, u64)) -> Option<f64> {
        self.entries
            .iter()
            .find(|r| r.path == path && (r.size, r.modified) == id)
            .map(|r| r.seconds)
    }
    pub fn remember(&mut self, path: &Path, seconds: f64, duration: f64) {
        if !seconds.is_finite() || !duration.is_finite() {
            return;
        }
        self.entries.retain(|r| r.path != path);
        if seconds < 5.0 || duration - seconds < 5.0 {
            return;
        }
        if let Some((size, modified)) = identity(path) {
            self.entries.push_front(Resume {
                path: path.into(),
                seconds,
                size,
                modified,
            });
            self.entries.truncate(HISTORY_LIMIT);
        }
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let data = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, data).map_err(|e| e.to_string())?;
        std::fs::rename(&temp, path).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_size_change_in_same_second_does_not_resume() {
        let p = std::env::temp_dir().join(format!("vesper-ns-{}.mp4", std::process::id()));
        std::fs::write(&p, b"fixture").unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        let base = std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000);
        f.set_times(
            std::fs::FileTimes::new().set_modified(base + std::time::Duration::from_millis(100)),
        )
        .unwrap();
        let mut h = History::default();
        h.remember(&p, 30.0, 90.0);
        assert_eq!(h.position(&p), Some(30.0));
        f.set_times(
            std::fs::FileTimes::new().set_modified(base + std::time::Duration::from_millis(200)),
        )
        .unwrap();
        assert_eq!(h.position(&p), None);
        drop(f);
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn rejects_extended_devices_and_unc() {
        for path in [
            "\\\\?\\GLOBALROOT\\Device\\HarddiskVolume1\\a.mp4",
            "\\\\.\\PhysicalDrive0",
            "\\\\?\\UNC\\server\\movie.mp4",
            "//server/movie.mp4",
        ] {
            assert!(local_namespace(Path::new(path)).is_err());
        }
        assert!(local_namespace(Path::new("\\\\?\\B:\\files\\movie.mp4")).is_ok());
    }
    #[test]
    fn drops_are_sorted_into_videos_subtitles_one_folder_and_errors() {
        let root = std::env::temp_dir().join(format!("vesper-drop-{}", std::process::id()));
        let folder = root.join("episodes");
        std::fs::create_dir_all(&folder).unwrap();
        let video = root.join("movie.mkv");
        let second = root.join("clip.mp4");
        let subtitle = root.join("movie.hr.srt");
        for f in [&video, &second, &subtitle] {
            std::fs::write(f, b"x").unwrap();
        }
        let plan = classify_drops([
            video.clone(),
            subtitle.clone(),
            folder.clone(),
            second.clone(),
            PathBuf::from(r"\\server\share\remote.mkv"),
            root.join("missing.mp4"),
        ]);
        let canon = |p: &PathBuf| p.canonicalize().unwrap();
        assert_eq!(plan.videos, vec![canon(&video), canon(&second)]);
        assert_eq!(plan.subtitles, vec![canon(&subtitle)]);
        assert_eq!(plan.folder, Some(canon(&folder)));
        assert_eq!(plan.errors.len(), 2, "{:?}", plan.errors);
        std::fs::remove_file(video).unwrap();
        std::fs::remove_file(second).unwrap();
        std::fs::remove_file(subtitle).unwrap();
        std::fs::remove_dir(folder).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn dropped_subtitles_and_queue_are_bounded() {
        let root = std::env::temp_dir().join(format!("vesper-dropb-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut paths = Vec::new();
        for i in 0..40 {
            let f = root.join(format!("s{i}.srt"));
            std::fs::write(&f, b"1").unwrap();
            paths.push(f);
        }
        let plan = classify_drops(paths.clone());
        assert_eq!(plan.subtitles.len(), DROPPED_SUBTITLE_LIMIT);
        assert!(plan.videos.is_empty());
        let many = classify_drops(
            (0..QUEUE_LIMIT + 500).map(|i| PathBuf::from(format!(r"\\server\share\{i}.mp4"))),
        );
        assert_eq!(
            many.errors.len(),
            QUEUE_LIMIT,
            "the plan examines at most QUEUE_LIMIT dropped paths"
        );
        for f in paths {
            std::fs::remove_file(f).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn every_listed_extension_has_a_fixture_or_is_an_alias_of_one() {
        // A picker or queue extension is not format evidence: each one needs a real file that
        // decodes, or must be another name for a container that has one.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let have: Vec<String> = std::fs::read_dir(&dir)
            .map(|d| {
                d.flatten()
                    .filter_map(|e| {
                        e.path()
                            .extension()
                            .map(|x| x.to_string_lossy().to_lowercase())
                    })
                    .collect()
            })
            .unwrap_or_default();
        if have.len() < 10 {
            return; // a source bundle carries only the three small regression fixtures
        }
        for ext in [
            "mp4", "m4v", "mov", "mkv", "avi", "webm", "mpg", "mpeg", "ts", "m2ts", "wmv", "flv",
            "ogv", "3gp",
        ] {
            assert!(
                candidate(Path::new(&format!("x.{ext}"))),
                "{ext} is not listed"
            );
            let target = EXTENSION_ALIASES
                .iter()
                .find(|(alias, _)| *alias == ext)
                .map_or(ext, |(_, real)| *real);
            assert!(
                have.iter().any(|h| h == target),
                "{ext} is listed but no {target} fixture exists"
            );
        }
        assert!(!candidate(Path::new("notes.txt")) && !candidate(Path::new("movie.iso")));
    }

    #[test]
    fn clamps_seek_and_nonfinite() {
        assert_eq!(seek(3.0, -5.0, 10.0), 0.0);
        assert_eq!(seek(8.0, 5.0, 10.0), 10.0);
        assert_eq!(seek(f64::NAN, 1.0, 10.0), 0.0);
    }
    #[test]
    fn rejects_network_and_urls() {
        assert!(local_file(Path::new("\\\\server\\clip.mp4")).is_err());
        assert!(local_file(Path::new("https://example.test/movie.mp4")).is_err());
    }
    #[test]
    fn malformed_history_is_empty() {
        let p = std::env::temp_dir().join(format!("vesper-history-{}.json", std::process::id()));
        std::fs::write(&p, b"{broken").unwrap();
        assert!(History::load(&p).entries.is_empty());
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn changed_file_does_not_resume() {
        let p = std::env::temp_dir().join(format!("vesper-resume-{}.mp4", std::process::id()));
        std::fs::write(&p, b"fixture").unwrap();
        let mut h = History::default();
        h.remember(&p, 30.0, 90.0);
        assert_eq!(h.position(&p), Some(30.0));
        std::fs::write(&p, b"changed media").unwrap();
        assert_eq!(h.position(&p), None);
        std::fs::remove_file(p).unwrap();
    }
    #[test]
    fn completed_file_clears_resume() {
        let p = std::env::temp_dir().join(format!("vesper-done-{}.mp4", std::process::id()));
        std::fs::write(&p, b"fixture").unwrap();
        let mut h = History::default();
        h.remember(&p, 30.0, 90.0);
        h.remember(&p, 89.0, 90.0);
        assert!(h.entries.is_empty());
        std::fs::remove_file(p).unwrap();
    }
}
