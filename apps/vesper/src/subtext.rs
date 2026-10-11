//! Text subtitles in the encoding the owner's file really uses.
//!
//! The bundled decoder reads text subtitles as UTF-8 only. Checked directly against libmpv on
//! 2026-10-11, a Windows-1250 Croatian file shows broken characters whatever `sub-codepage` is set
//! to (`auto`, `cp1250`, `windows-1250`, `+cp1250`, `ISO-8859-2`, `utf8:cp1250`, `enca:hr:cp1250`):
//! this build has no character-set conversion. Subtitle sites still serve many files in the
//! legacy code page of their language, so Vesper converts them itself before the decoder sees
//! them: it reads the file, decides how it is encoded, and, when that is not plain UTF-8, writes a
//! UTF-8 copy into Vesper's own cache folder and gives the decoder that copy. The owner's file is
//! never opened for writing. The code pages come from Windows itself (`MultiByteToWideChar`), so
//! there is no extra library or licence.
use crate::{media, subsearch};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::UNIX_EPOCH,
};

pub const MAX_BYTES: u64 = 16 * 1024 * 1024;
/// One converted copy: UTF-8 can be up to three times the size of single-byte text.
pub const MAX_CONVERTED_BYTES: usize = 32 * 1024 * 1024;
/// Converted copies kept; the oldest beyond this are removed from Vesper's own cache folder.
pub const CACHE_LIMIT: usize = 32;
/// Total size of the cache folder before the oldest copies are removed.
pub const CACHE_BYTES: u64 = 128 * 1024 * 1024;
const PREFIX: &str = "sub-";
const TEMP_MARK: &str = ".tmp-";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct CodePage {
    pub id: u32,
    pub name: &'static str,
}

/// What the owner can choose when "Automatic" guesses wrong.
pub const CODE_PAGES: &[CodePage] = &[
    CodePage {
        id: 1250,
        name: "Central European (Windows-1250)",
    },
    CodePage {
        id: 1251,
        name: "Cyrillic (Windows-1251)",
    },
    CodePage {
        id: 1252,
        name: "Western European (Windows-1252)",
    },
    CodePage {
        id: 1253,
        name: "Greek (Windows-1253)",
    },
    CodePage {
        id: 1254,
        name: "Turkish (Windows-1254)",
    },
    CodePage {
        id: 1255,
        name: "Hebrew (Windows-1255)",
    },
    CodePage {
        id: 1256,
        name: "Arabic (Windows-1256)",
    },
    CodePage {
        id: 1257,
        name: "Baltic (Windows-1257)",
    },
    CodePage {
        id: 1258,
        name: "Vietnamese (Windows-1258)",
    },
    CodePage {
        id: 874,
        name: "Thai (Windows-874)",
    },
    CodePage {
        id: 932,
        name: "Japanese (Shift JIS)",
    },
    CodePage {
        id: 936,
        name: "Chinese, simplified (GBK)",
    },
    CodePage {
        id: 949,
        name: "Korean",
    },
    CodePage {
        id: 950,
        name: "Chinese, traditional (Big5)",
    },
];

pub fn code_page_name(id: u32) -> String {
    CODE_PAGES
        .iter()
        .find(|c| c.id == id)
        .map_or_else(|| format!("code page {id}"), |c| c.name.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    /// Not valid UTF-8 and no byte-order mark: an older single or double byte code page.
    Legacy,
}

pub fn classify(bytes: &[u8]) -> Source {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Source::Utf8Bom
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        Source::Utf16Le
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        Source::Utf16Be
    } else if std::str::from_utf8(bytes).is_ok() {
        Source::Utf8
    } else {
        Source::Legacy
    }
}

pub fn decode_utf16(bytes: &[u8], big_endian: bool) -> Result<String, String> {
    let body = &bytes[2.min(bytes.len())..];
    if !body.len().is_multiple_of(2) {
        return Err("This UTF-16 subtitle file ends in the middle of a character.".into());
    }
    let units: Vec<u16> = body
        .chunks_exact(2)
        .map(|b| {
            if big_endian {
                u16::from_be_bytes([b[0], b[1]])
            } else {
                u16::from_le_bytes([b[0], b[1]])
            }
        })
        .collect();
    String::from_utf16(&units)
        .map_err(|_| "This UTF-16 subtitle file has invalid text.".to_string())
}

#[cfg(windows)]
pub fn decode_code_page(bytes: &[u8], code_page: u32) -> Result<String, String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            source: *const u8,
            source_len: i32,
            destination: *mut u16,
            destination_len: i32,
        ) -> i32;
    }
    if bytes.is_empty() {
        return Ok(String::new());
    }
    let length = i32::try_from(bytes.len()).map_err(|_| "The subtitle file is too large.")?;
    let needed = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            length,
            std::ptr::null_mut(),
            0,
        )
    };
    if needed <= 0 {
        return Err(format!(
            "This copy of Windows cannot read {}.",
            code_page_name(code_page)
        ));
    }
    let mut wide = vec![0_u16; needed as usize];
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            length,
            wide.as_mut_ptr(),
            needed,
        )
    };
    if written <= 0 {
        return Err(format!(
            "Could not read the file as {}.",
            code_page_name(code_page)
        ));
    }
    String::from_utf16(&wide[..written as usize])
        .map_err(|_| format!("Could not read the file as {}.", code_page_name(code_page)))
}

#[cfg(not(windows))]
pub fn decode_code_page(_bytes: &[u8], _code_page: u32) -> Result<String, String> {
    Err("Legacy subtitle code pages are only converted on Windows.".into())
}

/// Windows' own default code page for non-Unicode text, used when no language is chosen.
#[cfg(windows)]
fn system_code_page() -> u32 {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetACP() -> u32;
    }
    unsafe { GetACP() }
}
#[cfg(not(windows))]
fn system_code_page() -> u32 {
    1252
}

/// The code page a legacy file most likely uses: the chosen subtitle language's, else Windows'
/// own default, else Western European.
pub fn default_code_page(language_id: Option<&str>) -> u32 {
    if let Some(page) = language_id
        .and_then(subsearch::language)
        .and_then(|l| l.codepage)
    {
        return page;
    }
    let system = system_code_page();
    if CODE_PAGES.iter().any(|c| c.id == system) {
        system
    } else {
        1252
    }
}

#[derive(Clone, Debug)]
pub struct Prepared {
    /// The file to give the decoder: the original when it is already UTF-8, else the converted copy.
    pub path: PathBuf,
    /// What the file was read as when it had to be converted, for the confirmation message.
    pub read_as: Option<String>,
}

fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf29ce484222325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}

/// Cache file name: depends on the file, its size and time, and how it was read, so a different
/// encoding choice gets a different file and an edited subtitle is converted again.
pub fn cache_name(original: &Path, len: u64, modified: u128, code_page: u32) -> String {
    let key = format!(
        "{}|{len}|{modified}|{code_page}",
        original.to_string_lossy().to_lowercase()
    );
    let extension = original
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| matches!(e.as_str(), "srt" | "ass" | "ssa" | "vtt"))
        .unwrap_or_else(|| "srt".into());
    format!("{PREFIX}{:016x}.{extension}", fnv(key.as_bytes()))
}

fn is_cache_file(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(PREFIX) else {
        return false;
    };
    let Some((stem, extension)) = rest.split_once('.') else {
        return false;
    };
    stem.len() == 16
        && stem.bytes().all(|b| b.is_ascii_hexdigit())
        && matches!(extension, "srt" | "ass" | "ssa" | "vtt")
}

fn is_stale_temp(name: &str) -> bool {
    name.strip_prefix(PREFIX)
        .is_some_and(|rest| rest.contains(TEMP_MARK))
}

/// Keep the newest `limit` converted copies and at most `bytes` of them. Only plain files named
/// exactly like Vesper's own cache files are ever removed (links, folders and anything else in
/// the folder are left alone), plus its own abandoned temporary files older than an hour.
pub fn trim_cache(dir: &Path, limit: usize, bytes: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let hour = std::time::Duration::from_secs(3600);
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if is_cache_file(&name) {
            files.push((modified, meta.len(), entry.path()));
        } else if is_stale_temp(&name) && modified.elapsed().is_ok_and(|age| age > hour) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    let mut kept = 0_usize;
    let mut total = 0_u64;
    for (_, len, path) in files {
        if kept < limit && total + len <= bytes {
            kept += 1;
            total += len;
        } else {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Read at most `limit` bytes; a source that holds more (even if it grew after it was measured)
/// is refused instead of read in full.
fn read_limited(source: impl Read, limit: u64) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    source
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Cannot read the subtitle file: {e}"))?;
    if bytes.len() as u64 > limit {
        return Err("Subtitle files are limited to 16 MiB.".into());
    }
    Ok(bytes)
}

/// The cache folder, created if needed, and proven to be a direct local folder: no network or
/// mapped drive, and no link or junction anywhere in its path, so writes cannot be redirected.
fn owned_cache_dir(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot prepare the subtitle cache: {e}"))?;
    let resolved = media::local_input(dir)?;
    if !resolved.is_dir() {
        return Err("The subtitle cache is not a folder.".into());
    }
    Ok(resolved)
}

/// True only if `path` is a plain file (not a link or reparse point) that holds exactly `data`.
fn holds_exactly(path: &Path, data: &[u8]) -> bool {
    #[cfg(windows)]
    fn is_reparse(meta: &std::fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    fn is_reparse(_: &std::fs::Metadata) -> bool {
        false
    }
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_file() || is_reparse(&meta) || meta.len() != data.len() as u64 {
        return false;
    }
    std::fs::File::open(path)
        .map_err(|e| e.to_string())
        .and_then(|f| read_limited(f, data.len() as u64))
        .is_ok_and(|bytes| bytes == data)
}

/// Write `data` as a new file beside its final name, flush it, then rename it into place. The
/// rename replaces the directory entry instead of writing through it, so a cache name that was
/// replaced by a hard link to some other file never changes that file. Only a temporary file this
/// call created is ever removed by it; a temporary name that already exists is someone else's and
/// is skipped.
fn write_atomically(dir: &Path, name: &str, data: &[u8]) -> Result<PathBuf, String> {
    write_atomically_with(dir, name, data, &mut || {
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    })
}

fn write_atomically_with(
    dir: &Path,
    name: &str,
    data: &[u8],
    next: &mut dyn FnMut() -> u64,
) -> Result<PathBuf, String> {
    let destination = dir.join(name);
    let mut created: Option<PathBuf> = None;
    for _ in 0..8 {
        let temp = dir.join(format!(
            "{name}{TEMP_MARK}{}-{}",
            std::process::id(),
            next()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(mut file) => {
                let written = file.write_all(data).and_then(|_| file.sync_all());
                drop(file);
                if let Err(e) = written {
                    let _ = std::fs::remove_file(&temp);
                    return Err(format!("Cannot save the converted subtitle: {e}"));
                }
                created = Some(temp);
                break;
            }
            // Not ours: leave it and try the next name.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Cannot save the converted subtitle: {e}")),
        }
    }
    let Some(temp) = created else {
        return Err("Cannot save the converted subtitle: no free temporary name.".into());
    };
    match std::fs::rename(&temp, &destination) {
        Ok(()) => Ok(destination),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            // The decoder may still hold an earlier copy open. It is accepted only if it is a plain
            // file with exactly these bytes; anything else is reported.
            if holds_exactly(&destination, data) {
                Ok(destination)
            } else {
                Err(format!("Cannot save the converted subtitle: {e}"))
            }
        }
    }
}

/// Read a subtitle file and return something the decoder can show correctly.
/// `forced` is the owner's explicit code page choice and wins over everything else.
pub fn prepare(
    path: &Path,
    language_id: Option<&str>,
    forced: Option<u32>,
    cache_dir: &Path,
) -> Result<Prepared, String> {
    let file = media::local_file(path)?;
    let handle =
        std::fs::File::open(&file).map_err(|e| format!("Cannot read the subtitle file: {e}"))?;
    let meta = handle.metadata().map_err(|e| e.to_string())?;
    if meta.len() > MAX_BYTES {
        return Err("Subtitle files are limited to 16 MiB.".into());
    }
    let bytes = read_limited(handle, MAX_BYTES)?;
    let source = classify(&bytes);
    let (text, label, code_page) = match (forced, source) {
        (Some(page), _) => (decode_code_page(&bytes, page)?, code_page_name(page), page),
        (None, Source::Utf8) => {
            return Ok(Prepared {
                path: file,
                read_as: None,
            });
        }
        (None, Source::Utf8Bom) => (
            String::from_utf8_lossy(&bytes[3..]).into_owned(),
            "UTF-8".to_string(),
            65001,
        ),
        (None, Source::Utf16Le) => (decode_utf16(&bytes, false)?, "UTF-16".to_string(), 1200),
        (None, Source::Utf16Be) => (decode_utf16(&bytes, true)?, "UTF-16".to_string(), 1201),
        (None, Source::Legacy) => {
            let page = default_code_page(language_id);
            (decode_code_page(&bytes, page)?, code_page_name(page), page)
        }
    };
    if text.len() > MAX_CONVERTED_BYTES {
        return Err("This subtitle file is too large to convert.".into());
    }
    let cache_dir = owned_cache_dir(cache_dir)?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let name = cache_name(&file, meta.len(), modified, code_page);
    let converted = write_atomically(&cache_dir, &name, text.as_bytes())?;
    trim_cache(&cache_dir, CACHE_LIMIT, CACHE_BYTES);
    Ok(Prepared {
        path: converted,
        read_as: Some(label),
    })
}

pub const SIBLING_LIMIT: usize = 8;
const DIRECTORY_ENTRY_LIMIT: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sibling {
    pub path: PathBuf,
    /// The language tag in the file name (`hr` in `movie.hr.srt`), when there is one.
    pub language: Option<String>,
}

fn is_subtitle_extension(extension: &str) -> bool {
    matches!(extension, "srt" | "ass" | "ssa" | "vtt")
}

/// Subtitle files beside `video` that carry its name: `movie.srt`, or `movie.<tag>[.<tag>].srt`
/// where each tag is a short language or flag word (`hr`, `hrv`, `pt-br`, `forced`, `sdh`).
/// Only the video's own folder is listed (no subfolders), at most 10,000 entries are looked at,
/// at most 8 files are returned, and links and folders are skipped.
pub fn siblings(video: &Path) -> Vec<Sibling> {
    let (Some(folder), Some(stem)) = (video.parent(), video.file_stem().and_then(|s| s.to_str()))
    else {
        return Vec::new();
    };
    let stem = stem.to_lowercase();
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut found: Vec<Sibling> = Vec::new();
    for entry in entries.take(DIRECTORY_ENTRY_LIMIT).flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if !path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .is_some_and(|e| is_subtitle_extension(&e))
        {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_lowercase)
        else {
            continue;
        };
        if name == stem {
            found.push(Sibling {
                path,
                language: None,
            });
        } else if let Some(tags) = name
            .strip_prefix(&stem)
            .and_then(|rest| rest.strip_prefix('.'))
        {
            let tags: Vec<&str> = tags.split('.').collect();
            let tag_ok = |t: &&str| {
                (1..=8).contains(&t.len())
                    && t.bytes().all(|b| b.is_ascii_alphabetic() || b == b'-')
            };
            if (1..=3).contains(&tags.len()) && tags.iter().all(tag_ok) {
                let language = tags
                    .first()
                    .filter(|t| (2..=3).contains(&t.len()) || t.contains('-'))
                    .map(|t| t.to_string());
                found.push(Sibling { path, language });
            }
        }
    }
    // Windows file names are case-insensitive, so order them that way: a stable, predictable order.
    found.sort_by_key(|s| {
        s.path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });
    found.truncate(SIBLING_LIMIT);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_subtitles_that_carry_the_videos_name_are_siblings() {
        let d = dir("siblings");
        let video = write(&d, "Clip.mp4", b"video");
        for name in [
            "clip.srt",
            "Clip.hr.srt",
            "CLIP.HRV.FORCED.SRT",
            "clip.pt-br.ass",
            "clip.vtt",
        ] {
            write(&d, name, b"1");
        }
        for name in [
            "clip2.srt",
            "clipboard.srt",
            "another movie.srt",
            "clip.txt",
            "clip.hr.srt.bak",
            "clip.too.many.tag.words.srt",
            "clip.1080p.srt",
            "clip..srt",
        ] {
            write(&d, name, b"1");
        }
        std::fs::create_dir_all(d.join("clip.fake.srt")).unwrap();
        let names: Vec<String> = siblings(&video)
            .iter()
            .map(|s| s.path.file_name().unwrap().to_string_lossy().to_lowercase())
            .collect();
        assert_eq!(
            names,
            vec![
                "clip.hr.srt",
                "clip.hrv.forced.srt",
                "clip.pt-br.ass",
                "clip.srt",
                "clip.vtt"
            ]
        );
        let languages: Vec<Option<String>> =
            siblings(&video).into_iter().map(|s| s.language).collect();
        assert_eq!(
            languages,
            vec![
                Some("hr".into()),
                Some("hrv".into()),
                Some("pt-br".into()),
                None,
                None
            ]
        );
    }

    #[test]
    fn siblings_are_bounded_and_never_leave_the_videos_folder() {
        let d = dir("sibling-bound");
        let video = write(&d, "m.mkv", b"video");
        for i in 0..20 {
            write(&d, &format!("m.l{}.srt", (b'a' + i as u8) as char), b"1");
        }
        let sub = d.join("deeper");
        std::fs::create_dir_all(&sub).unwrap();
        write(&sub, "m.hr.srt", b"1");
        let found = siblings(&video);
        assert_eq!(found.len(), SIBLING_LIMIT);
        assert!(found.iter().all(|s| s.path.parent() == Some(d.as_path())));
        assert!(siblings(Path::new(r"Z:\no\such\folder\x.mkv")).is_empty());
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vesper-subtext-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }
    const CUE: &str = "1\r\n00:00:00,100 --> 00:00:01,400\r\n";

    #[test]
    fn utf8_files_are_given_to_the_decoder_untouched() {
        let d = dir("utf8");
        let f = write(
            &d,
            "a.srt",
            "1\r\n00:00:00,100 --> 00:00:01,400\r\nČuješ li me?\r\n".as_bytes(),
        );
        let p = prepare(&f, Some("hrv"), None, &d.join("cache")).unwrap();
        assert_eq!(p.path, f.canonicalize().unwrap());
        assert!(p.read_as.is_none());
        assert!(
            !d.join("cache").exists(),
            "nothing is written for a UTF-8 file"
        );
    }

    #[test]
    fn byte_order_marks_are_recognised_and_removed() {
        let d = dir("bom");
        let text = format!("{CUE}Čuješ li me?\r\n");
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend(text.as_bytes());
        let mut le = vec![0xFF, 0xFE];
        le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let mut be = vec![0xFE, 0xFF];
        be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        for (name, bytes, label) in [
            ("bom.srt", bom, "UTF-8"),
            ("le.srt", le, "UTF-16"),
            ("be.srt", be, "UTF-16"),
        ] {
            let f = write(&d, name, &bytes);
            let p = prepare(&f, None, None, &d.join("cache")).unwrap();
            assert_eq!(p.read_as.as_deref(), Some(label), "{name}");
            assert_eq!(std::fs::read_to_string(&p.path).unwrap(), text, "{name}");
            assert_ne!(p.path, f.canonicalize().unwrap());
        }
    }

    // Byte strings below were produced by encoding the text with each code page.
    const CASES: &[(&str, &str, &[u8], &str)] = &[
        ("hrv", "Croatian 1250", b"\x44\x6f\x62\x72\x6f\x20\x64\x6f\x9a\x6c\x69\x21\x20\xc8\x75\x6a\x65\x9a\x20\x6c\x69\x20\x6d\x65\x2c\x20\xf0\x61\xe8\x65\x3f", "Dobro došli! Čuješ li me, đače?"),
        ("rus", "Russian 1251", b"\xcf\xf0\xe8\xe2\xe5\xf2\x2c\x20\xea\xe0\xea\x20\xe4\xe5\xeb\xe0\x3f\x20\xa8\xe6\xe8\xea\x2e", "Привет, как дела? Ёжик."),
        ("ell", "Greek 1253", b"\xca\xe1\xeb\xe7\xec\xdd\xf1\xe1\x2c\x20\xf0\xfe\xf2\x20\xe5\xdf\xf3\xe1\xe9\x3b", "Καλημέρα, πώς είσαι;"),
        ("tur", "Turkish 1254", b"\xde\x69\xfe\x6c\x69\x92\x64\x65\x20\xf0\xfc\x7a\x65\x6c\x20\xf6\xf0\x6c\x65\x3a\x20\xdd\x73\x74\x61\x6e\x62\x75\x6c\x20\xfd\x73\x6c\x61\x6b\x2e", "Şişli’de ğüzel öğle: İstanbul ıslak."),
        ("jpn", "Japanese 932", b"\x82\xb1\x82\xf1\x82\xc9\x82\xbf\x82\xcd\x81\x41\x90\xa2\x8a\x45", "こんにちは、世界"),
        ("chi", "Chinese 936", b"\xc4\xe3\xba\xc3\xa3\xac\xca\xc0\xbd\xe7", "你好，世界"),
        ("kor", "Korean 949", b"\xbe\xc8\xb3\xe7\xc7\xcf\xbc\xbc\xbf\xe4\x20\xbc\xbc\xb0\xe8", "안녕하세요 세계"),
        ("heb", "Hebrew 1255", b"\xf9\xec\xe5\xed\x20\xf2\xe5\xec\xed", "שלום עולם"),
        ("ara", "Arabic 1256", b"\xe3\xd1\xcd\xc8\xc7\x20\xc8\xc7\xe1\xda\xc7\xe1\xe3", "مرحبا بالعالم"),
    ];

    #[test]
    fn legacy_files_are_converted_with_their_languages_code_page() {
        let d = dir("legacy");
        for (language, label, bytes, expected) in CASES {
            let mut file = CUE.as_bytes().to_vec();
            file.extend(*bytes);
            let f = write(&d, &format!("{language}.srt"), &file);
            assert_eq!(classify(&file), Source::Legacy, "{label}");
            let p = prepare(&f, Some(language), None, &d.join("cache")).unwrap();
            assert!(p.read_as.is_some(), "{label}");
            let shown = std::fs::read_to_string(&p.path).unwrap();
            assert_eq!(shown, format!("{CUE}{expected}"), "{label}");
        }
    }

    #[test]
    fn the_owners_choice_beats_the_languages_default() {
        let d = dir("forced");
        let mut file = CUE.as_bytes().to_vec();
        file.extend(CASES[1].2); // Windows-1251 Russian bytes
        let f = write(&d, "ru.srt", &file);
        // Croatian would read these as Windows-1250; choosing 1251 shows the Russian text.
        let wrong = prepare(&f, Some("hrv"), None, &d.join("cache")).unwrap();
        let right = prepare(&f, Some("hrv"), Some(1251), &d.join("cache")).unwrap();
        assert_ne!(
            std::fs::read(&wrong.path).unwrap(),
            std::fs::read(&right.path).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(&right.path).unwrap(),
            format!("{CUE}Привет, как дела? Ёжик.")
        );
        assert_ne!(
            wrong.path, right.path,
            "a different choice is a different cache file"
        );
    }

    #[test]
    fn the_owners_file_is_never_modified() {
        let d = dir("original");
        let mut file = CUE.as_bytes().to_vec();
        file.extend(CASES[0].2);
        let f = write(&d, "keep.srt", &file);
        let before = std::fs::metadata(&f).unwrap().modified().unwrap();
        let _ = prepare(&f, Some("hrv"), None, &d.join("cache")).unwrap();
        let _ = prepare(&f, Some("hrv"), Some(1252), &d.join("cache")).unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), file);
        assert_eq!(std::fs::metadata(&f).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn missing_oversized_network_and_unreadable_pages_are_refused_with_a_reason() {
        let d = dir("refuse");
        assert!(prepare(&d.join("absent.srt"), None, None, &d.join("cache")).is_err());
        assert!(
            prepare(
                Path::new(r"\\server\share\a.srt"),
                None,
                None,
                &d.join("cache")
            )
            .is_err()
        );
        let big = d.join("big.srt");
        let handle = std::fs::File::create(&big).unwrap();
        handle.set_len(MAX_BYTES + 1).unwrap();
        drop(handle);
        let error = prepare(&big, None, None, &d.join("cache")).unwrap_err();
        assert!(error.contains("16 MiB"), "{error}");
        let f = write(&d, "x.srt", b"\xe8\xe8\xe8");
        assert!(prepare(&f, None, Some(99999), &d.join("cache")).is_err());
    }

    #[test]
    fn the_cache_is_bounded_and_removes_only_its_own_files() {
        let d = dir("trim");
        for i in 0..40_u64 {
            write(&d, &format!("sub-{i:016x}.srt"), b"x");
        }
        write(&d, "keep-me.txt", b"x");
        write(&d, "sub-notes.srt", b"x");
        write(&d, "sub-0000000000000000zz.srt", b"x");
        trim_cache(&d, CACHE_LIMIT, CACHE_BYTES);
        let own = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_str().is_some_and(is_cache_file))
            .count();
        assert_eq!(own, CACHE_LIMIT);
        for foreign in ["keep-me.txt", "sub-notes.srt", "sub-0000000000000000zz.srt"] {
            assert!(d.join(foreign).exists(), "{foreign} must survive");
        }
    }

    #[test]
    fn the_cache_is_also_bounded_by_total_size_and_old_temporary_files_go() {
        let d = dir("trim-bytes");
        for i in 0..6_u64 {
            write(&d, &format!("sub-{i:016x}.srt"), &vec![b'x'; 1000]);
        }
        trim_cache(&d, CACHE_LIMIT, 3500);
        let remaining = std::fs::read_dir(&d).unwrap().flatten().count();
        assert_eq!(remaining, 3, "3,500 bytes holds three 1,000-byte copies");
        // A fresh temporary file of a save in progress is left alone.
        let temp = write(&d, "sub-0000000000000099.srt.tmp-1-1", b"x");
        trim_cache(&d, CACHE_LIMIT, 3500);
        assert!(temp.exists());
    }

    #[test]
    fn reading_stops_at_the_limit_even_if_the_file_grew_after_it_was_measured() {
        let big = std::io::Cursor::new(vec![b'a'; 1025]);
        assert!(read_limited(big, 1024).is_err());
        let exact = std::io::Cursor::new(vec![b'a'; 1024]);
        assert_eq!(read_limited(exact, 1024).unwrap().len(), 1024);
    }

    #[test]
    fn a_cache_name_replaced_by_a_hard_link_never_writes_through_to_the_other_file() {
        let d = dir("hardlink");
        let mut legacy = CUE.as_bytes().to_vec();
        legacy.extend(CASES[0].2);
        let original = write(&d, "movie.hr.srt", &legacy);
        let cache = d.join("cache");
        let first = prepare(&original, Some("hrv"), None, &cache).unwrap();
        // Replace the cache entry by a hard link to a file the owner cares about.
        let victim = write(&d, "victim.txt", b"precious");
        std::fs::remove_file(&first.path).unwrap();
        if std::fs::hard_link(&victim, &first.path).is_err() {
            return; // the file system cannot make hard links; nothing to prove here
        }
        let again = prepare(&original, Some("hrv"), None, &cache).unwrap();
        assert_eq!(again.path, first.path);
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        assert_eq!(
            std::fs::read_to_string(&again.path).unwrap(),
            format!("{CUE}Dobro došli! Čuješ li me, đače?")
        );
        assert_eq!(std::fs::read(&original).unwrap(), legacy);
    }

    #[test]
    fn an_existing_temporary_name_is_left_alone_and_the_next_name_is_used() {
        let d = dir("temp-owner");
        let theirs = d.join(format!(
            "sub-0000000000000042.srt{TEMP_MARK}{}-1",
            std::process::id()
        ));
        std::fs::write(&theirs, b"someone else's").unwrap();
        let mut counter = 0_u64;
        let saved = write_atomically_with(&d, "sub-0000000000000042.srt", b"mine", &mut || {
            counter += 1;
            counter
        })
        .unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), b"mine");
        assert_eq!(
            std::fs::read(&theirs).unwrap(),
            b"someone else's",
            "a temporary file this call did not create must not be touched"
        );
        // Every name taken: the save fails visibly instead of overwriting or removing anything.
        let taken = d.join(format!(
            "sub-0000000000000044.srt{TEMP_MARK}{}-7",
            std::process::id()
        ));
        std::fs::write(&taken, b"x").unwrap();
        let mut always_seven = || 7_u64;
        assert!(
            write_atomically_with(&d, "sub-0000000000000044.srt", b"y", &mut always_seven).is_err()
        );
        assert_eq!(std::fs::read(&taken).unwrap(), b"x");
    }

    #[cfg(windows)]
    #[test]
    fn a_held_cache_file_is_accepted_only_if_it_holds_exactly_the_same_bytes() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = dir("held");
        let name = "sub-0000000000000077.srt";
        let held = d.join(name);
        std::fs::write(&held, b"same size A").unwrap();
        // Hold it open with no sharing, so the rename onto it cannot succeed.
        let _hold = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&held)
            .unwrap();
        // Same length, different bytes: not trusted, reported.
        assert!(write_atomically(&d, name, b"same size B").is_err());
        // The very same bytes are fine: the copy the decoder holds is the one we would write.
        // (Reading a share_mode(0) file fails, so the comparison correctly refuses it too.)
        assert!(write_atomically(&d, name, b"same size A").is_err());
        // No temporary leftovers from either attempt.
        let leftovers = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(TEMP_MARK))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn holds_exactly_needs_a_plain_file_with_the_same_bytes() {
        let d = dir("exact");
        let f = write(&d, "a.srt", b"abc");
        assert!(holds_exactly(&f, b"abc"));
        assert!(!holds_exactly(&f, b"abd"), "same size, different bytes");
        assert!(!holds_exactly(&f, b"abcd"));
        assert!(!holds_exactly(&d, b"abc"), "a folder is not a file");
        assert!(!holds_exactly(&d.join("absent"), b"abc"));
    }

    #[test]
    fn a_cache_path_that_is_a_file_or_a_network_path_is_refused() {
        let d = dir("badcache");
        let mut legacy = CUE.as_bytes().to_vec();
        legacy.extend(CASES[0].2);
        let original = write(&d, "a.srt", &legacy);
        let file_in_the_way = write(&d, "cache", b"not a folder");
        assert!(prepare(&original, Some("hrv"), None, &file_in_the_way).is_err());
        assert!(
            prepare(
                &original,
                Some("hrv"),
                None,
                Path::new(r"\\server\share\cache")
            )
            .is_err()
        );
        // A UTF-8 file needs no cache at all, so a bad cache location does not matter.
        let utf8 = write(
            &d,
            "b.srt",
            "1\r\n00:00:00,100 --> 00:00:01,400\r\nČ\r\n".as_bytes(),
        );
        assert!(prepare(&utf8, Some("hrv"), None, &file_in_the_way).is_ok());
    }

    #[test]
    fn cache_names_follow_the_file_and_the_choice() {
        let p = Path::new(r"C:\x\Movie.HR.srt");
        let a = cache_name(p, 100, 5, 1250);
        assert_eq!(a, cache_name(p, 100, 5, 1250));
        assert_ne!(a, cache_name(p, 100, 5, 1252));
        assert_ne!(a, cache_name(p, 101, 5, 1250));
        assert_ne!(a, cache_name(p, 100, 6, 1250));
        assert!(is_cache_file(&a) && a.ends_with(".srt"));
        assert!(cache_name(Path::new("a.vtt"), 1, 1, 1250).ends_with(".vtt"));
        assert!(cache_name(Path::new("a.weird"), 1, 1, 1250).ends_with(".srt"));
    }

    #[test]
    fn defaults_follow_the_language_then_windows() {
        assert_eq!(default_code_page(Some("hrv")), 1250);
        assert_eq!(default_code_page(Some("rus")), 1251);
        assert_eq!(default_code_page(Some("jpn")), 932);
        let none = default_code_page(None);
        assert!(CODE_PAGES.iter().any(|c| c.id == none));
        assert_eq!(code_page_name(1250), "Central European (Windows-1250)");
        assert_eq!(code_page_name(7), "code page 7");
    }
}
