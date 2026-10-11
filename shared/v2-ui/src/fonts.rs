//! Typography comes from Windows itself: Segoe UI Variable (Windows 11) or Segoe UI (Windows 10).
//! Nothing is bundled, so no app pays font bytes in its package, and the text looks like the OS.
//!
//! Font files are memory-mapped read-only rather than read into the heap. The fallback chain
//! (symbols, Chinese/Japanese, Korean) is ~36 MB on disk, but only the pages a glyph lookup
//! touches are ever resident, and those pages are shared with every other process using them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily, FontTweak};

/// Family name for the semibold weight. Use [`semibold`] rather than spelling it.
pub const SEMIBOLD: &str = "v2-semibold";

pub fn semibold() -> FontFamily {
    FontFamily::Name(SEMIBOLD.into())
}

/// Which system fonts were found. Apps log this once at startup.
#[derive(Clone, Debug, Default)]
pub struct FontReport {
    pub loaded: Vec<String>,
    pub missing: Vec<String>,
}

fn fonts_dir() -> PathBuf {
    let root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("Fonts")
}

/// Build the font set. `cjk` adds Microsoft YaHei UI and Malgun Gothic as fallbacks so
/// Chinese, Japanese and Korean titles render instead of showing empty boxes.
pub fn definitions(cjk: bool) -> (FontDefinitions, FontReport) {
    let dir = fonts_dir();
    let mut defs = FontDefinitions::empty();
    let mut report = FontReport::default();

    let mut load = |key: &str, file: &str, index: u32, tweak: FontTweak| -> bool {
        match map_static(&dir.join(file)) {
            Some(bytes) => {
                let mut data = FontData::from_static(bytes).tweak(tweak);
                data.index = index;
                defs.font_data.insert(key.to_owned(), Arc::new(data));
                report.loaded.push(format!("{file}#{index} as {key}"));
                true
            }
            None => {
                report.missing.push(file.to_owned());
                false
            }
        }
    };

    let weight = |w: f32| FontTweak {
        coords: egui::epaint::text::VariationCoords::new([("wght", w)]),
        ..Default::default()
    };

    // Body and semibold: one variable file on Windows 11, two static files on Windows 10.
    let (regular, bold) = if load("segoe-400", "SegUIVar.ttf", 0, weight(400.0)) {
        load("segoe-600", "SegUIVar.ttf", 0, weight(600.0));
        ("segoe-400", "segoe-600")
    } else {
        load("segoe-400", "segoeui.ttf", 0, FontTweak::default());
        if !load("segoe-600", "seguisb.ttf", 0, FontTweak::default()) {
            load("segoe-600", "segoeuib.ttf", 0, FontTweak::default());
        }
        ("segoe-400", "segoe-600")
    };
    let mono = if load("mono", "CascadiaMono.ttf", 0, FontTweak::default())
        || load("mono", "consola.ttf", 0, FontTweak::default())
    {
        Some("mono")
    } else {
        None
    };

    let mut fallbacks = Vec::new();
    if load("symbols", "seguisym.ttf", 0, FontTweak::default()) {
        fallbacks.push("symbols");
    }
    if cjk {
        // msyh.ttc face 1 is "Microsoft YaHei UI": Han plus kana, with UI line metrics.
        if load("cjk-sc", "msyh.ttc", 1, FontTweak::default()) {
            fallbacks.push("cjk-sc");
        }
        if load("cjk-ko", "malgun.ttf", 0, FontTweak::default()) {
            fallbacks.push("cjk-ko");
        }
    }

    let chain = |first: &[&'static str]| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for k in first.iter().chain(fallbacks.iter()) {
            if defs.font_data.contains_key(*k) && !out.iter().any(|o| o == k) {
                out.push((*k).to_owned());
            }
        }
        out
    };
    let proportional = chain(&[regular]);
    let semibold = chain(&[bold, regular]);
    let monospace = chain(&[mono.unwrap_or(regular), regular]);

    defs.families.insert(FontFamily::Proportional, proportional);
    defs.families.insert(FontFamily::Name(SEMIBOLD.into()), semibold);
    defs.families.insert(FontFamily::Monospace, monospace);
    (defs, report)
}

/// Map a file read-only for the life of the process. Fonts are never unmapped: the app uses
/// them until it exits.
#[cfg(windows)]
fn map_static(path: &Path) -> Option<&'static [u8]> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileSizeEx, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, FILE_MAP_READ, PAGE_READONLY};

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: plain Win32 calls on handles we own; every handle is closed on every path, and the
    // view outlives them by design (a mapped view keeps its section alive).
    unsafe {
        let file = CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            0,
        );
        if file == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut size: i64 = 0;
        if GetFileSizeEx(file, &mut size) == 0 || size <= 0 {
            CloseHandle(file);
            return None;
        }
        let mapping = CreateFileMappingW(file, std::ptr::null(), PAGE_READONLY, 0, 0, std::ptr::null());
        CloseHandle(file);
        if mapping == 0 {
            return None;
        }
        let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
        CloseHandle(mapping);
        if view.Value.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts(view.Value as *const u8, size as usize))
    }
}

#[cfg(not(windows))]
fn map_static(path: &Path) -> Option<&'static [u8]> {
    std::fs::read(path).ok().map(|v| &*Box::leak(v.into_boxed_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_has_a_complete_set() {
        let (defs, report) = definitions(true);
        assert!(report.missing.iter().all(|m| m != "segoeui.ttf" || report.loaded.iter().any(|l| l.contains("SegUIVar"))));
        for family in [FontFamily::Proportional, FontFamily::Monospace, semibold()] {
            let chain = &defs.families[&family];
            assert!(!chain.is_empty(), "{family:?} has no font");
            assert!(chain.iter().all(|k| defs.font_data.contains_key(k)), "{family:?} names a font that did not load");
        }
    }

    #[test]
    fn a_missing_file_is_reported_not_fatal() {
        assert!(map_static(Path::new(r"Z:\definitely\not\here.ttf")).is_none());
    }
}
