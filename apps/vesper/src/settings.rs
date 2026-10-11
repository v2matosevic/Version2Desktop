//! The few choices Vesper remembers between runs, in one small local file
//! (`%LOCALAPPDATA%/Version2/Vesper/settings.json`). Nothing here is sent anywhere.
use crate::subsearch::{self, Provider};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const SETTINGS_BYTES: u64 = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Preferred subtitle language (a [`subsearch::Language::id`]); `None` means no preference.
    pub subtitle_language: Option<String>,
    pub subtitle_provider: Provider,
    /// Whether the owner has chosen a language, so Windows' language is only a first-run default.
    pub language_chosen: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            subtitle_language: None,
            subtitle_provider: Provider::OpenSubtitlesOrg,
            language_chosen: false,
        }
    }
}

impl Settings {
    /// Read the file, ignoring anything missing, oversized, malformed or naming an unknown
    /// language. The first run starts from Windows' display language when Vesper knows it.
    pub fn load(path: &Path) -> Self {
        let mut settings = std::fs::metadata(path)
            .ok()
            .filter(|m| m.len() <= SETTINGS_BYTES)
            .and_then(|_| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .unwrap_or_default();
        if settings
            .subtitle_language
            .as_deref()
            .is_some_and(|id| subsearch::language(id).is_none())
        {
            settings.subtitle_language = None;
        }
        if !settings.language_chosen && settings.subtitle_language.is_none() {
            settings.subtitle_language = system_language().map(str::to_string);
        }
        settings
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let data = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, data).map_err(|e| e.to_string())?;
        std::fs::rename(&temp, path).map_err(|e| e.to_string())
    }
}

/// Map a BCP-47 name such as `hr-HR` or `pt-BR` to a subtitle language id.
pub fn language_for_locale(locale: &str) -> Option<&'static str> {
    let lower = locale.to_ascii_lowercase();
    if lower.starts_with("pt-br") {
        return Some("pob");
    }
    if lower.starts_with("zh") {
        return Some("chi");
    }
    let primary = lower.split(['-', '_']).next()?;
    subsearch::LANGUAGES
        .iter()
        .find(|l| l.tags.split(',').next() == Some(primary))
        .map(|l| l.id)
}

#[cfg(windows)]
fn system_language() -> Option<&'static str> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultLocaleName(buffer: *mut u16, size: i32) -> i32;
    }
    let mut buffer = [0_u16; 85];
    let n = unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) };
    if n <= 1 {
        return None;
    }
    language_for_locale(&String::from_utf16_lossy(&buffer[..n as usize - 1]))
}
#[cfg(not(windows))]
fn system_language() -> Option<&'static str> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "vesper-settings-{}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn locales_map_to_subtitle_languages() {
        assert_eq!(language_for_locale("hr-HR"), Some("hrv"));
        assert_eq!(language_for_locale("en-US"), Some("eng"));
        assert_eq!(language_for_locale("pt-BR"), Some("pob"));
        assert_eq!(language_for_locale("pt-PT"), Some("por"));
        assert_eq!(language_for_locale("zh-CN"), Some("chi"));
        assert_eq!(language_for_locale("de-AT"), Some("ger"));
        assert_eq!(language_for_locale("xx-YY"), None);
        assert_eq!(language_for_locale(""), None);
    }

    #[test]
    fn a_chosen_language_round_trips_and_wins_over_windows() {
        let p = temp("roundtrip");
        let saved = Settings {
            subtitle_language: Some("slv".into()),
            subtitle_provider: Provider::Titlovi,
            language_chosen: true,
        };
        saved.save(&p).unwrap();
        assert_eq!(Settings::load(&p), saved);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn choosing_no_language_stays_none_after_a_reload() {
        let p = temp("none");
        let saved = Settings {
            subtitle_language: None,
            subtitle_provider: Provider::OpenSubtitles,
            language_chosen: true,
        };
        saved.save(&p).unwrap();
        assert_eq!(Settings::load(&p).subtitle_language, None);
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn malformed_oversized_and_unknown_values_fall_back_safely() {
        let p = temp("bad");
        std::fs::write(&p, b"{not json").unwrap();
        assert_eq!(
            Settings::load(&p).subtitle_provider,
            Provider::OpenSubtitlesOrg
        );
        std::fs::write(&p, vec![b' '; SETTINGS_BYTES as usize + 1]).unwrap();
        assert!(!Settings::load(&p).language_chosen);
        std::fs::write(
            &p,
            br#"{"subtitle_language":"zzz","subtitle_provider":"Titlovi","language_chosen":true}"#,
        )
        .unwrap();
        let loaded = Settings::load(&p);
        assert_eq!(loaded.subtitle_language, None);
        assert_eq!(loaded.subtitle_provider, Provider::Titlovi);
        std::fs::remove_file(&p).unwrap();
        assert_eq!(
            Settings::load(&temp("absent")).subtitle_provider,
            Provider::OpenSubtitlesOrg
        );
    }
}
