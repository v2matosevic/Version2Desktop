use crate::mpv::{Api, Player, SoftwareRenderer};
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// Exercises real decoder control state without opening a native window.
pub fn check(file: &Path, dll: &Path, srt: &Path, ass: &Path) -> Result<serde_json::Value, String> {
    let p = Player::new(
        Api::load(dll)?,
        &[
            ("ao", "null"),
            ("pause", "yes"),
            ("keep-open", "yes"),
            ("hwdec", "no"),
            ("sub-auto", "no"),
        ],
    )?;
    let mut render = SoftwareRenderer::new(p.clone(), 320, 180)?;
    p.open(file, 0.0)?;
    // The first load after a rebuild reads a freshly written executable and the 127 MiB decoder:
    // allow the same 15 seconds the playback smoke allows. Every later wait stays at 5.
    let started = Instant::now();
    let mut loaded = false;
    while started.elapsed() < Duration::from_secs(15) && !loaded {
        for (id, error) in p.poll() {
            if let Some(e) = error {
                return Err(format!("Loading fixture: {e}"));
            }
            if id == 8 {
                loaded = true;
            }
        }
        render.draw()?;
        std::thread::sleep(Duration::from_millis(5));
    }
    if !loaded {
        return Err("Fixture did not reach file-loaded state.".into());
    }
    p.command_async(&["set", "pause", "yes"])?;
    until(&p, &mut render, |p| {
        p.string("pause").as_deref() == Some("yes")
    })?;
    p.command_async(&["seek", "0.7", "absolute+exact"])?;
    until(&p, &mut render, |p| {
        (p.number("time-pos").unwrap_or(-1.0) - 0.7).abs() < 0.15
    })?;
    p.set_number("speed", 1.5)?;
    p.set_number("volume", 30.0)?;
    p.command_async(&["set", "mute", "yes"])?;
    until(&p, &mut render, |p| {
        p.string("mute").as_deref() == Some("yes")
            && p.number("speed") == Some(1.5)
            && p.number("volume") == Some(30.0)
    })?;
    if p.number("speed") != Some(1.5) || p.number("volume") != Some(30.0) {
        return Err("Speed or volume control state did not change.".into());
    }
    for sub in [srt, ass] {
        let sub = crate::media::local_file(sub)?;
        p.command_async(&[
            "sub-add",
            sub.to_str().ok_or("Invalid subtitle path")?,
            "select",
        ])?;
    }
    until(&p, &mut render, |p| {
        let count = p.number("track-list/count").unwrap_or(0.0) as usize;
        count >= 5
            && (0..count.min(256)).all(|i| {
                p.string(&format!("track-list/{i}/type")).is_some()
                    && p.number(&format!("track-list/{i}/id")).is_some()
            })
    })?;
    let count = p.number("track-list/count").unwrap_or(0.0) as usize;
    let mut audio = Vec::new();
    let mut subs = Vec::new();
    for i in 0..count.min(256) {
        let base = format!("track-list/{i}/");
        let id = p.number(&(base.clone() + "id")).unwrap_or(0.0) as i64;
        match p.string(&(base.clone() + "type")).as_deref() {
            Some("audio") => audio.push(id),
            Some("sub") => subs.push(id),
            _ => {}
        }
    }
    if audio.len() < 2 || subs.len() < 2 {
        return Err("Fixture did not expose independent audio and subtitle tracks.".into());
    }
    p.command_async(&["set", "aid", &audio[1].to_string()])?;
    until(&p, &mut render, |p| {
        p.number("aid") == Some(audio[1] as f64)
    })?;
    p.command_async(&["set", "sid", &subs[0].to_string()])?;
    until(&p, &mut render, |p| p.number("sid") == Some(subs[0] as f64))?;
    p.command_async(&["set", "sid", "no"])?;
    until(&p, &mut render, |p| {
        p.string("sid").as_deref() == Some("no")
    })?;
    // Subtitle text as the decoder shows it after Vesper's own reading (`subtext::prepare`), one
    // fresh decoder per file so no earlier track can supply the text, and the text must hold steady
    // for 400 ms before it is compared. The cases cover plain UTF-8, UTF-16 with a byte-order mark
    // and four legacy code pages, each with its language, plus ASS.
    let folder = file.parent().unwrap_or(Path::new("."));
    let cache = std::env::temp_dir().join(format!("vesper-subtitle-check-{}", std::process::id()));
    let croatian = "Dobro došli! Čuješ li me, đače?";
    let mut shown = Vec::new();
    for (label, name, language, expected, converted) in [
        (
            "utf8 srt",
            "caption-croatian-utf8.srt",
            "hrv",
            croatian,
            false,
        ),
        (
            "windows-1250 srt (Croatian)",
            "caption-croatian-1250.srt",
            "hrv",
            croatian,
            true,
        ),
        (
            "utf-16 srt with byte-order mark",
            "caption-croatian-utf16le.srt",
            "hrv",
            croatian,
            true,
        ),
        (
            "windows-1251 srt (Russian)",
            "caption-russian-1251.srt",
            "rus",
            "Привет, как дела? Ёжик.",
            true,
        ),
        (
            "windows-1253 srt (Greek)",
            "caption-greek-1253.srt",
            "ell",
            "Καλημέρα, πώς είσαι;",
            true,
        ),
        ("ass", "caption.ass", "hrv", "Vesper ASS subtitle", false),
    ] {
        let prepared = crate::subtext::prepare(&folder.join(name), Some(language), None, &cache)
            .map_err(|e| format!("{label}: {e}"))?;
        if prepared.read_as.is_some() != converted {
            return Err(format!(
                "{label}: expected converted={converted}, got {:?}",
                prepared.read_as
            ));
        }
        let observed =
            subtitle_text(dll, file, &prepared.path).map_err(|e| format!("{label}: {e}"))?;
        if observed.trim() != expected {
            return Err(format!(
                "{label}: the decoder shows {observed:?}, expected {expected:?}"
            ));
        }
        shown.push(format!("{label}: {expected}"));
    }
    // Delay and size controls reach the decoder and read back.
    p.set_number("sub-delay", 0.5)?;
    p.set_number("sub-scale", 1.5)?;
    until(&p, &mut render, |p| {
        p.number("sub-delay") == Some(0.5) && p.number("sub-scale") == Some(1.5)
    })?;
    p.command_async(&["set", "pause", "no"])?;
    until(&p, &mut render, |p| {
        p.string("pause").as_deref() == Some("no")
    })?;
    // A subtitle saved next to the video under the video's name, with or without a language
    // suffix, is found by `subtext::siblings`, read in its own encoding and shown; an unrelated
    // subtitle in the same folder is not picked up.
    let autoload = sibling_autoload(
        dll,
        &folder.join("h264-aac.mp4"),
        &folder.join("caption-croatian-1250.srt"),
        &cache,
    )
    .map_err(|e| format!("sibling autoload: {e}"))?;
    remove_own_folder(&cache);
    Ok(
        serde_json::json!({"ok":true,"sibling_autoload":autoload,"seek_seconds":0.7,"speed":1.5,"volume":30,"mute":true,"audio_tracks":audio.len(),"subtitle_tracks":subs.len(),"subtitle_text":shown,"subtitle_delay":0.5,"subtitle_scale":1.5,"checks":["pause","absolute seek","speed","volume","mute","SRT/ASS loading","audio selection","subtitle selection/off","subtitle text through Vesper's own encoding step (UTF-8, UTF-16, Windows-1250/1251/1253, ASS)","subtitle delay and size","resume playback"]}),
    )
}
/// Put `clip.mp4` next to `clip.hr.srt` (Windows-1250) and `another movie.srt` in a private temp
/// folder; Vesper's sibling search must find exactly the first subtitle, and the decoder must show
/// its text correctly.
fn sibling_autoload(
    dll: &Path,
    video: &Path,
    subtitle: &Path,
    cache: &Path,
) -> Result<serde_json::Value, String> {
    let dir = std::env::temp_dir().join(format!("vesper-autoload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let clip = dir.join("clip.mp4");
    std::fs::copy(video, &clip).map_err(|e| e.to_string())?;
    std::fs::copy(subtitle, dir.join("clip.hr.srt")).map_err(|e| e.to_string())?;
    std::fs::copy(subtitle, dir.join("another movie.srt")).map_err(|e| e.to_string())?;
    let result = (|| {
        let found = crate::subtext::siblings(&clip);
        if found.len() != 1 || found[0].language.as_deref() != Some("hr") {
            return Err(format!("expected exactly clip.hr.srt, found {found:?}"));
        }
        let prepared = crate::subtext::prepare(&found[0].path, Some("hrv"), None, cache)?;
        let text = subtitle_text(dll, &clip, &prepared.path)?;
        Ok(serde_json::json!({
            "found": found.len(),
            "language_tag": found[0].language,
            "read_as": prepared.read_as,
            "shown": text.trim(),
        }))
    })();
    remove_own_folder(&dir);
    result
}
/// The text a freshly started decoder shows for `subtitle` half a second into `video`.
fn subtitle_text(dll: &Path, video: &Path, subtitle: &Path) -> Result<String, String> {
    let p = Player::new(
        Api::load(dll)?,
        &[
            ("ao", "null"),
            ("pause", "yes"),
            ("keep-open", "yes"),
            ("hwdec", "no"),
            ("sub-auto", "no"),
        ],
    )?;
    let mut render = SoftwareRenderer::new(p.clone(), 320, 180)?;
    p.open(video, 0.0)?;
    // The file must be loaded before a subtitle is added to it.
    let started = Instant::now();
    let mut loaded = false;
    while !loaded && started.elapsed() < Duration::from_secs(6) {
        for (id, error) in p.poll() {
            if let Some(e) = error {
                return Err(format!("Loading the video: {e}"));
            }
            loaded |= id == 8;
        }
        render.draw()?;
        std::thread::sleep(Duration::from_millis(5));
    }
    if !loaded {
        return Err("The video did not finish loading.".into());
    }
    let subtitle = crate::media::local_file(subtitle)?;
    p.command_async(&[
        "sub-add",
        subtitle.to_str().ok_or("Invalid subtitle path")?,
        "select",
    ])?;
    // The new track is a subtitle track that is selected.
    until(&p, &mut render, |p| {
        let count = p.number("track-list/count").unwrap_or(0.0) as usize;
        (0..count.min(256)).any(|i| {
            p.string(&format!("track-list/{i}/type")).as_deref() == Some("sub")
                && p.string(&format!("track-list/{i}/selected")).as_deref() == Some("yes")
        })
    })?;
    p.command_async(&["seek", "0.5", "absolute+exact"])?;
    // Wait for any text, then until it has not changed for 400 ms.
    let started = Instant::now();
    let mut last: Option<(String, Instant)> = None;
    while started.elapsed() < Duration::from_secs(6) {
        for (_, error) in p.poll() {
            if let Some(e) = error {
                return Err(e);
            }
        }
        render.draw()?;
        if let Some(text) = p.string("sub-text").filter(|t| !t.trim().is_empty()) {
            match &last {
                Some((previous, since)) if *previous == text => {
                    if since.elapsed() >= Duration::from_millis(400) {
                        return Ok(text);
                    }
                }
                _ => last = Some((text, Instant::now())),
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err("no subtitle text appeared".into())
}
/// Remove the plain files this check created in its own temp folder, then the folder.
fn remove_own_folder(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_file()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}
fn until(
    p: &Player,
    render: &mut SoftwareRenderer,
    predicate: impl Fn(&Player) -> bool,
) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        for (_, e) in p.poll() {
            if let Some(e) = e {
                return Err(e);
            }
        }
        render.draw()?;
        if predicate(p) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let count = p.number("track-list/count");
    Err(format!(
        "Controller check timed out (pause {:?}, sid {:?}, tracks {count:?}, time {:?}; decoder said {:?}).",
        p.string("pause"),
        p.number("sid"),
        p.number("time-pos"),
        p.messages().iter().rev().take(3).collect::<Vec<_>>()
    ))
}
