use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
pub const MAX_TRACKS: usize = 5000;
pub const EXTENSIONS: &[&str] = &[
    "mp3", "wav", "flac", "aac", "m4a", "ogg", "opus", "aif", "aiff",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub path: PathBuf,
    pub title: String,
}
impl Track {
    pub fn new(path: PathBuf) -> Self {
        let title = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        Self { path, title }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}
#[derive(Default)]
pub struct Queue {
    pub tracks: Vec<Track>,
    pub current: Option<usize>,
    pub shuffle: bool,
    pub repeat: Repeat,
    history: Vec<usize>,
    shuffle_remaining: Vec<usize>,
    shuffle_active: bool,
}
impl Queue {
    pub fn add(&mut self, paths: Vec<PathBuf>) -> usize {
        let mut known: HashSet<PathBuf> = self.tracks.iter().map(|t| t.path.clone()).collect();
        let before = self.tracks.len();
        for path in paths {
            if self.tracks.len() == MAX_TRACKS {
                break;
            }
            if known.insert(path.clone()) {
                self.tracks.push(Track::new(path));
            }
        }
        if self.tracks.len() != before {
            self.shuffle_active = false;
        }
        self.tracks.len() - before
    }
    pub fn next_index(&mut self, automatic: bool) -> Option<usize> {
        let n = self.tracks.len();
        if n == 0 {
            return None;
        }
        if automatic && self.repeat == Repeat::One {
            return self.current;
        }
        if let Some(i) = self.current {
            self.history.push(i);
            if self.history.len() > MAX_TRACKS {
                self.history.remove(0);
            }
        }
        let next = if self.shuffle {
            if !self.shuffle_active
                || (self.shuffle_remaining.is_empty() && (self.repeat == Repeat::All || !automatic))
            {
                self.build_shuffle_plan();
            }
            self.shuffle_remaining.pop()
        } else {
            self.shuffle_active = false;
            match self.current {
                None => Some(0),
                Some(i) if i + 1 < n => Some(i + 1),
                _ if self.repeat == Repeat::All || !automatic => Some(0),
                _ => None,
            }
        };
        self.current = next;
        next
    }
    fn build_shuffle_plan(&mut self) {
        let n = self.tracks.len();
        self.shuffle_remaining = (0..n)
            .filter(|i| Some(*i) != self.current || n == 1)
            .collect();
        self.shuffle_remaining.shuffle(&mut rand::rng());
        self.shuffle_active = true;
    }

    /// The track that follows the current one when it ends by itself, skipping `skip` (files that
    /// failed), without moving. This is what the player opens ahead of time for a gapless or
    /// crossfaded change, so it must agree with what [`Queue::next_index`] then chooses: a
    /// shuffle order is fixed here (the only random step) and consumed later.
    pub fn peek_next(&mut self, skip: &HashSet<PathBuf>) -> Option<usize> {
        let n = self.tracks.len();
        if n == 0 {
            return None;
        }
        let usable = |i: usize, q: &Queue| !skip.contains(&q.tracks[i].path);
        if self.repeat == Repeat::One {
            return self.current.filter(|i| usable(*i, self));
        }
        if self.shuffle {
            if !self.shuffle_active {
                self.build_shuffle_plan();
            }
            let pick = |q: &Queue| {
                q.shuffle_remaining
                    .iter()
                    .rev()
                    .copied()
                    .find(|i| usable(*i, q))
            };
            let found = pick(self);
            if found.is_none() && self.repeat == Repeat::All {
                // Every track left in the order is unplayable: the next pass starts a new order.
                self.build_shuffle_plan();
                return pick(self);
            }
            return found;
        }
        let after = match self.current {
            None => 0..n,
            Some(c) => c + 1..n,
        };
        let wrap = match (self.current, self.repeat) {
            (Some(c), Repeat::All) => 0..c + 1,
            _ => 0..0,
        };
        after.chain(wrap).find(|i| usable(*i, self))
    }

    /// Makes `path` the current track after the player moved on to it by itself, keeping the
    /// shuffle order and history as if [`Queue::next_index`] had chosen it. Falls back to a
    /// lookup when the queue was edited meanwhile.
    pub fn jump_to_path(&mut self, path: &Path) -> Option<usize> {
        let i = self.tracks.iter().position(|t| t.path == path)?;
        if self.current != Some(i) {
            if let Some(c) = self.current {
                self.history.push(c);
                if self.history.len() > MAX_TRACKS {
                    self.history.remove(0);
                }
            }
            self.current = Some(i);
        }
        if self.shuffle_active {
            self.shuffle_remaining.retain(|r| *r != i);
        }
        Some(i)
    }

    /// Plays `i` straight after the current track: moves it there in order, or to the front of
    /// the remaining shuffle order. Returns the track's new index, or `None` when there is
    /// nothing to do.
    pub fn play_next(&mut self, i: usize) -> Option<usize> {
        let n = self.tracks.len();
        let current = self.current?;
        if i >= n || i == current {
            return None;
        }
        if self.shuffle && self.shuffle_active {
            self.shuffle_remaining.retain(|r| *r != i);
            self.shuffle_remaining.push(i);
            return Some(i);
        }
        let track = self.tracks.remove(i);
        let current = if i < current { current - 1 } else { current };
        self.tracks.insert(current + 1, track);
        self.current = Some(current);
        self.history.clear();
        self.shuffle_active = false;
        Some(current + 1)
    }

    pub fn previous(&mut self) -> Option<usize> {
        let n = self.tracks.len();
        if n == 0 {
            return None;
        }
        let i = if self.shuffle {
            self.history.pop().unwrap_or(0)
        } else {
            self.current.unwrap_or(0).checked_sub(1).unwrap_or(n - 1)
        };
        self.current = Some(i.min(n - 1));
        self.current
    }
    pub fn reorder(&mut self, i: usize, j: usize) {
        if i >= self.tracks.len() || j >= self.tracks.len() {
            return;
        }
        self.tracks.swap(i, j);
        self.current = self.current.map(|c| {
            if c == i {
                j
            } else if c == j {
                i
            } else {
                c
            }
        });
        self.history.clear();
        self.shuffle_active = false;
    }
    pub fn remove(&mut self, i: usize) {
        if i >= self.tracks.len() {
            return;
        }
        self.tracks.remove(i);
        self.history.clear();
        self.shuffle_active = false;
        self.current = match self.current {
            Some(c) if c == i => None,
            Some(c) if c > i => Some(c - 1),
            other => other,
        };
    }
}

pub fn collect_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::new();
    for path in paths {
        crate::paths::local(path)?;
        if path.is_dir() {
            let folder_start = result.len();
            for item in fs::read_dir(path)
                .map_err(|e| e.to_string())?
                .take(MAX_TRACKS * 4)
            {
                let item = item.map_err(|e| e.to_string())?;
                if item.file_type().map_err(|e| e.to_string())?.is_file() && supported(&item.path())
                {
                    result.push(item.path());
                }
                if result.len() >= MAX_TRACKS {
                    break;
                }
            }
            result[folder_start..].sort();
        } else if path.is_file() && supported(path) {
            result.push(path.clone());
        } else if path.is_file() && extension_is(path, &["m3u", "m3u8", "sonora"]) {
            result.extend(load_playlist(path)?);
        } else {
            return Err(format!(
                "Not a supported local audio file: {}",
                path.display()
            ));
        }
        if result.len() >= MAX_TRACKS {
            result.truncate(MAX_TRACKS);
            break;
        }
    }
    let mut seen = HashSet::new();
    result.retain(|p| seen.insert(p.clone()));
    Ok(result)
}
fn supported(path: &Path) -> bool {
    extension_is(path, EXTENSIONS)
}
fn extension_is(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| extensions.contains(&e.to_ascii_lowercase().as_str()))
}
pub fn save_playlist(path: &Path, tracks: &[Track]) -> Result<(), String> {
    crate::paths::local(path)?;
    if !extension_is(path, &["sonora", "m3u", "m3u8"]) {
        return Err(
            "Save playlists as .sonora, .m3u or .m3u8; audio originals are never overwritten"
                .into(),
        );
    }
    let existing_target = fs::canonicalize(path).ok();
    if tracks.iter().any(|t| {
        t.path == path
            || existing_target
                .as_ref()
                .is_some_and(|target| fs::canonicalize(&t.path).ok().as_ref() == Some(target))
    }) {
        return Err("A playlist cannot overwrite a queued original".into());
    }
    if tracks.len() > MAX_TRACKS {
        return Err("Playlist is too large".into());
    }
    if extension_is(path, &["m3u8", "m3u"]) {
        let mut text = String::from("#EXTM3U\n");
        for track in tracks {
            let p = track.path.to_str().ok_or("M3U requires Unicode paths")?;
            if p.contains(['\r', '\n']) {
                return Err("M3U cannot store paths containing newlines".into());
            }
            text.push_str(p);
            text.push('\n');
        }
        replace_playlist(path, text.as_bytes())
    } else {
        replace_playlist(
            path,
            &serde_json::to_vec_pretty(tracks).map_err(|e| e.to_string())?,
        )
    }
}
fn replace_playlist(path: &Path, bytes: &[u8]) -> Result<(), String> {
    // Never truncate the destination inode: another name can be a hard link to
    // an audio original even when canonical paths differ. Write a new sibling,
    // then replace only the chosen directory entry on the same filesystem.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut file = None;
    let mut temporary = PathBuf::new();
    for _ in 0..32 {
        temporary = parent.join(format!(
            ".sonora-playlist-{:016x}.tmp",
            rand::random::<u64>()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(created) => {
                file = Some(created);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    }
    let mut file = file.ok_or("Could not create a unique playlist file")?;
    let written = file.write_all(bytes).and_then(|_| file.sync_all());
    drop(file);
    let result = written.and_then(|_| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| error.to_string())
}
pub fn load_playlist(path: &Path) -> Result<Vec<PathBuf>, String> {
    crate::paths::local(path)?;
    if fs::metadata(path).map_err(|e| e.to_string())?.len() > 4 * 1024 * 1024 {
        return Err("Playlist exceeds 4 MiB".into());
    }
    let mut text = String::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(4 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() > 4 * 1024 * 1024 {
        return Err("Playlist exceeds 4 MiB".into());
    }
    let paths: Vec<PathBuf> = if extension_is(path, &["sonora"]) {
        let tracks: Vec<Track> = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        tracks.into_iter().map(|t| t.path).collect()
    } else {
        text.lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .map(PathBuf::from)
            .collect()
    };
    if paths.len() > MAX_TRACKS {
        return Err("Playlist exceeds 5000 tracks".into());
    }
    paths
        .into_iter()
        .map(|p| {
            let p = if p.is_absolute() {
                p
            } else {
                path.parent().unwrap_or(Path::new(".")).join(p)
            };
            crate::paths::local(&p)?;
            Ok(p)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shuffle_without_repeat_visits_remaining_once_then_stops() {
        let mut q = queue();
        q.current = Some(0);
        q.shuffle = true;
        let a = q.next_index(true).unwrap();
        let b = q.next_index(true).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert_eq!(q.next_index(true), None);
        q.repeat = Repeat::All;
        assert!(q.next_index(true).is_some());
    }
    fn queue() -> Queue {
        let mut q = Queue::default();
        q.add((0..3).map(|i| PathBuf::from(format!("{i}.wav"))).collect());
        q
    }
    #[test]
    fn eos_stops_and_repeat_cycles() {
        let mut q = queue();
        assert_eq!(q.next_index(true), Some(0));
        q.current = Some(2);
        assert_eq!(q.next_index(true), None);
        q.current = Some(2);
        q.repeat = Repeat::All;
        assert_eq!(q.next_index(true), Some(0));
        q.repeat = Repeat::One;
        assert_eq!(q.next_index(true), Some(0));
    }
    #[test]
    fn shuffle_does_not_repeat_and_remove_adjusts() {
        let mut q = queue();
        q.current = Some(1);
        q.shuffle = true;
        for _ in 0..50 {
            let old = q.current;
            assert_ne!(q.next_index(false), old);
        }
        q.current = Some(2);
        q.remove(0);
        assert_eq!(q.current, Some(1));
        q.remove(1);
        assert_eq!(q.current, None);
    }
    fn queue_of(n: usize) -> Queue {
        let mut q = Queue::default();
        q.add((0..n).map(|i| PathBuf::from(format!("{i}.flac"))).collect());
        q
    }
    fn bad(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn peek_matches_what_the_player_then_advances_to_in_order() {
        let mut q = queue_of(4);
        q.current = Some(1);
        let none = HashSet::new();
        assert_eq!(q.peek_next(&none), Some(2));
        assert_eq!(q.peek_next(&none), Some(2), "peeking moves nothing");
        assert_eq!(q.next_index(true), Some(2));
        q.current = Some(3);
        assert_eq!(
            q.peek_next(&none),
            None,
            "the end of the queue has no successor"
        );
        q.repeat = Repeat::All;
        assert_eq!(q.peek_next(&none), Some(0));
        assert_eq!(q.next_index(true), Some(0));
        q.repeat = Repeat::One;
        assert_eq!(
            q.peek_next(&none),
            Some(0),
            "repeat one loops the same track"
        );
        assert_eq!(q.next_index(true), Some(0));
    }

    #[test]
    fn peek_skips_failed_tracks_like_the_ui_does() {
        let mut q = queue_of(5);
        q.current = Some(0);
        assert_eq!(q.peek_next(&bad(&["1.flac", "2.flac"])), Some(3));
        assert_eq!(
            q.peek_next(&bad(&["1.flac", "2.flac", "3.flac", "4.flac"])),
            None
        );
        q.repeat = Repeat::All;
        assert_eq!(
            q.peek_next(&bad(&["1.flac", "2.flac", "3.flac", "4.flac"])),
            Some(0)
        );
        q.current = Some(4);
        assert_eq!(
            q.peek_next(&bad(&["0.flac"])),
            Some(1),
            "wraps past an unplayable first track"
        );
        q.repeat = Repeat::One;
        assert_eq!(
            q.peek_next(&bad(&["4.flac"])),
            None,
            "a failed track is not repeated"
        );
    }

    #[test]
    fn peek_in_shuffle_fixes_the_order_and_next_index_follows_it() {
        for _ in 0..20 {
            let mut q = queue_of(8);
            q.current = Some(3);
            q.shuffle = true;
            let none = HashSet::new();
            let mut seen = vec![3];
            for _ in 0..7 {
                let peeked = q.peek_next(&none).expect("a successor");
                assert_eq!(q.peek_next(&none), Some(peeked), "stable until consumed");
                assert_eq!(q.next_index(true), Some(peeked));
                assert!(!seen.contains(&peeked), "no repeats within one pass");
                seen.push(peeked);
            }
            assert_eq!(
                q.peek_next(&none),
                None,
                "an exhausted pass without repeat ends"
            );
        }
    }

    #[test]
    fn peek_survives_edits_that_reshuffle_the_order() {
        let mut q = queue_of(6);
        q.current = Some(0);
        q.shuffle = true;
        let none = HashSet::new();
        let before = q.peek_next(&none).unwrap();
        // Adding a track rebuilds the order: the hint may change, and must stay valid.
        q.add(vec![PathBuf::from("new.flac")]);
        let after = q.peek_next(&none).unwrap();
        assert!(after < 7 && after != 0);
        assert_eq!(q.next_index(true), Some(after));
        let _ = before;
    }

    #[test]
    fn jump_to_path_follows_the_player_and_keeps_the_shuffle_order_consistent() {
        let mut q = queue_of(5);
        q.current = Some(0);
        q.shuffle = true;
        let none = HashSet::new();
        let next = q.peek_next(&none).unwrap();
        let path = q.tracks[next].path.clone();
        assert_eq!(q.jump_to_path(&path), Some(next));
        assert_eq!(q.current, Some(next));
        assert!(
            !q.shuffle_remaining.contains(&next),
            "consumed from the order exactly once"
        );
        assert_eq!(q.previous(), Some(0), "history knows where it came from");
        assert_eq!(q.jump_to_path(Path::new("missing.flac")), None);
        let mut q = queue_of(3);
        q.current = Some(1);
        assert_eq!(
            q.jump_to_path(Path::new("1.flac")),
            Some(1),
            "repeat one lands on itself"
        );
        assert_eq!(q.current, Some(1));
    }

    #[test]
    fn play_next_moves_a_track_behind_the_current_one() {
        let mut q = queue_of(5);
        q.current = Some(1);
        assert_eq!(q.play_next(4), Some(2));
        let order: Vec<_> = q
            .tracks
            .iter()
            .map(|t| t.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(order, ["0.flac", "1.flac", "4.flac", "2.flac", "3.flac"]);
        assert_eq!(q.current, Some(1));
        assert_eq!(q.play_next(0), Some(1));
        assert_eq!(
            q.current,
            Some(0),
            "the current track keeps its identity when an earlier one moves"
        );
        assert_eq!(q.tracks[q.current.unwrap()].path, PathBuf::from("1.flac"));
        assert_eq!(q.peek_next(&HashSet::new()), Some(1));
        assert_eq!(q.tracks[1].path, PathBuf::from("0.flac"));
        assert_eq!(q.play_next(0), None, "already the current track");
        q.shuffle = true;
        let _ = q.peek_next(&HashSet::new());
        assert!(q.play_next(4).is_some());
        assert_eq!(q.peek_next(&HashSet::new()), Some(4));
    }

    #[test]
    fn bounded_and_deduplicated() {
        let mut q = Queue::default();
        q.add((0..6000).map(|i| PathBuf::from(format!("{i}"))).collect());
        assert_eq!(q.tracks.len(), MAX_TRACKS);
        assert_eq!(q.add(vec![PathBuf::from("0")]), 0);
    }
}
