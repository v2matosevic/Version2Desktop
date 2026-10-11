//! The few choices Sonora remembers between runs, in one small JSON file under
//! `%LOCALAPPDATA%\Version2\Sonora\settings.json`. Nothing here is sent anywhere, no audio file is
//! ever touched, and a missing or damaged file simply means the defaults.

use crate::engine::MAX_CROSSFADE_SECONDS;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
};

const MAX_BYTES: u64 = 4 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Crossfade length in seconds. 0 is off, which is how a fresh install starts.
    pub crossfade_seconds: f32,
    pub volume: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            crossfade_seconds: 0.0,
            volume: 0.7,
        }
    }
}

impl Settings {
    /// Clamps every value into its valid range; NaN and infinity fall back to the default.
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let fix = |v: f32, lo: f32, hi: f32, default: f32| {
            if v.is_finite() {
                v.clamp(lo, hi)
            } else {
                default
            }
        };
        Self {
            // Half-second steps are what the control offers; anything finer is rounded.
            crossfade_seconds: (fix(
                self.crossfade_seconds,
                0.0,
                MAX_CROSSFADE_SECONDS,
                d.crossfade_seconds,
            ) * 2.0)
                .round()
                / 2.0,
            volume: fix(self.volume, 0.0, 1.0, d.volume),
        }
    }
}

/// `%LOCALAPPDATA%\Version2\Sonora`, or `SONORA_SETTINGS_DIR` when set (used by tests).
pub fn directory() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SONORA_SETTINGS_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("Version2").join("Sonora"))
}

/// Reads at most [`MAX_BYTES`]: a file that grows after it was opened is still cut off, not read.
pub fn load_from(dir: &std::path::Path) -> Settings {
    let Ok(file) = fs::File::open(dir.join("settings.json")) else {
        return Settings::default();
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Settings::default();
    }
    let mut bytes = Vec::new();
    if file.take(MAX_BYTES + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_BYTES {
        return Settings::default();
    }
    serde_json::from_slice::<Settings>(&bytes)
        .map(Settings::sanitized)
        .unwrap_or_default()
}

pub fn load() -> Settings {
    directory().map(|d| load_from(&d)).unwrap_or_default()
}

/// Writes a brand-new sibling (never an existing file, so a hard link is never truncated),
/// syncs it, then renames it over the settings file. A failure leaves the previous settings in
/// place and removes the temporary file.
pub fn save_to(dir: &std::path::Path, settings: &Settings) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("Cannot create the settings folder: {e}"))?;
    let bytes = serde_json::to_vec_pretty(&settings.sanitized()).map_err(|e| e.to_string())?;
    let mut created = None;
    for _ in 0..16 {
        let temp = dir.join(format!(".settings-{:016x}.tmp", rand::random::<u64>()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => {
                created = Some((temp, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Cannot save settings: {e}")),
        }
    }
    let (temp, mut file) = created.ok_or("Cannot save settings: no unused temporary name")?;
    let written = file.write_all(&bytes).and_then(|_| file.sync_all());
    drop(file);
    let result = written.and_then(|_| fs::rename(&temp, dir.join("settings.json")));
    if let Err(e) = result {
        let _ = fs::remove_file(&temp);
        return Err(format!("Cannot save settings: {e}"));
    }
    Ok(())
}

pub fn save(settings: &Settings) -> Result<(), String> {
    match directory() {
        Some(dir) => save_to(&dir, settings),
        None => Err("No settings folder is available".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sonora-settings-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_fresh_install_has_crossfade_off() {
        assert_eq!(Settings::default().crossfade_seconds, 0.0);
        assert_eq!(load_from(&scratch("missing")), Settings::default());
    }

    #[test]
    fn choices_survive_a_restart_and_leave_no_temporary_files() {
        let dir = scratch("roundtrip");
        let chosen = Settings {
            crossfade_seconds: 6.5,
            volume: 0.35,
        };
        save_to(&dir, &chosen).unwrap();
        assert_eq!(load_from(&dir), chosen);
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("settings.json")]);
        save_to(
            &dir,
            &Settings {
                crossfade_seconds: 0.0,
                ..chosen
            },
        )
        .unwrap();
        assert_eq!(
            load_from(&dir).crossfade_seconds,
            0.0,
            "turning it off persists too"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn damaged_oversized_or_out_of_range_files_fall_back_safely() {
        let dir = scratch("damaged");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.json");
        for junk in [&b"{ not json"[..], b"", b"[1,2,3]", b"\xff\xfe\x00"] {
            fs::write(&file, junk).unwrap();
            assert_eq!(load_from(&dir), Settings::default());
        }
        fs::write(&file, vec![b' '; 10_000]).unwrap();
        assert_eq!(
            load_from(&dir),
            Settings::default(),
            "an oversized file is ignored unread"
        );
        fs::write(&file, br#"{"crossfade_seconds": 99, "volume": -4}"#).unwrap();
        assert_eq!(
            load_from(&dir),
            Settings {
                crossfade_seconds: 12.0,
                volume: 0.0
            }
        );
        fs::write(&file, br#"{"crossfade_seconds": 4.26}"#).unwrap();
        assert_eq!(
            load_from(&dir),
            Settings {
                crossfade_seconds: 4.5,
                volume: 0.7
            },
            "missing fields keep defaults"
        );
        assert_eq!(
            Settings {
                crossfade_seconds: f32::NAN,
                volume: f32::INFINITY
            }
            .sanitized(),
            Settings::default()
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_into_an_unwritable_place_reports_instead_of_panicking() {
        let dir = scratch("blocked");
        fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-folder");
        fs::write(&blocker, b"file").unwrap();
        assert!(save_to(&blocker, &Settings::default()).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_save_cleans_up_after_itself_and_never_touches_unrelated_files() {
        let dir = scratch("failed-save");
        fs::create_dir_all(dir.join("settings.json")).unwrap(); // a folder where the file should go
        let bystander = dir.join("keep.txt");
        fs::write(&bystander, b"user data").unwrap();
        let error = save_to(
            &dir,
            &Settings {
                crossfade_seconds: 3.0,
                volume: 0.2,
            },
        )
        .unwrap_err();
        assert!(error.starts_with("Cannot save settings"), "{error}");
        let mut names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["keep.txt", "settings.json"],
            "no temporary file is left behind"
        );
        assert_eq!(fs::read(&bystander).unwrap(), b"user data");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hard_linked_settings_file_is_replaced_not_truncated() {
        let dir = scratch("hardlink");
        fs::create_dir_all(&dir).unwrap();
        save_to(
            &dir,
            &Settings {
                crossfade_seconds: 2.0,
                volume: 0.5,
            },
        )
        .unwrap();
        let alias = dir.join("alias.json");
        fs::hard_link(dir.join("settings.json"), &alias).unwrap();
        let before = fs::read(&alias).unwrap();
        save_to(
            &dir,
            &Settings {
                crossfade_seconds: 8.0,
                volume: 0.9,
            },
        )
        .unwrap();
        assert_eq!(
            fs::read(&alias).unwrap(),
            before,
            "the other name still holds the original bytes"
        );
        assert_eq!(load_from(&dir).crossfade_seconds, 8.0);
        fs::remove_dir_all(&dir).unwrap();
    }
}
