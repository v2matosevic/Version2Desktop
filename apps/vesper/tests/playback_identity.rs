#![cfg(windows)]

use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use vesper::mpv::{Api, Player};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}
fn player() -> std::sync::Arc<Player> {
    let dll = std::env::var_os("VESPER_TEST_DLL")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("dist/current-portable/Vesper-0.1.0-x64/libmpv-2.dll")
        });
    Player::new(
        Api::load(&dll).expect("verified bundled decoder"),
        &[("vo", "null"), ("ao", "null"), ("keep-open", "yes")],
    )
    .expect("headless local player")
}
fn observe(p: &Player) -> Vec<(i32, Option<String>)> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut events = Vec::new();
    let mut loaded = None;
    while Instant::now() < deadline {
        for event in p.poll() {
            if event.0 == 8 {
                loaded.get_or_insert_with(Instant::now);
            }
            events.push(event);
        }
        if loaded.is_some_and(|at| at.elapsed() >= Duration::from_millis(200)) {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    events
}

#[test]
fn superseded_malformed_file_cannot_signal_error_on_valid_selection() {
    for delay_ms in [10, 50, 100] {
        let p = player();
        p.open(&fixture("malformed.mp4"), 0.0).unwrap();
        thread::sleep(Duration::from_millis(delay_ms));
        p.open(&fixture("h264-aac.mp4"), 0.0).unwrap();
        let events = observe(&p);
        assert!(
            events.iter().any(|(id, _)| *id == 8),
            "new file must load: {events:?}"
        );
        assert!(
            events.iter().all(|(_, error)| error.is_none()),
            "old file's error reached the selected valid file after {delay_ms}ms: {events:?}"
        );
    }
}

#[test]
fn current_malformed_file_still_reports_its_error() {
    let p = player();
    p.open(&fixture("malformed.mp4"), 0.0).unwrap();
    let events = observe(&p);
    assert!(
        events.iter().any(|(id, error)| *id == 7 && error.is_some()),
        "current malformed media error must remain visible: {events:?}"
    );
}

#[test]
fn superseded_valid_file_cannot_finish_new_files_loading() {
    let p = player();
    p.open(&fixture("h264-aac.mp4"), 0.0).unwrap();
    thread::sleep(Duration::from_millis(50));
    p.open(&fixture("hevc-aac.mkv"), 0.0).unwrap();
    let events = observe(&p);
    assert_eq!(
        events.iter().filter(|(id, _)| *id == 8).count(),
        1,
        "only the selected file may signal loaded: {events:?}"
    );
    let first_loaded = events.iter().position(|(id, _)| *id == 8).unwrap();
    assert!(
        events[..first_loaded].iter().all(|(id, _)| *id != 7),
        "old END_FILE must not clear new loading before FILE_LOADED: {events:?}"
    );
}

#[test]
fn stale_control_failure_is_ignored_but_current_failure_is_visible() {
    let p = player();
    p.open(&fixture("h264-aac.mp4"), 0.0).unwrap();
    p.command_async(&["set", "v2-nonexistent-property", "yes"])
        .unwrap();
    p.open(&fixture("hevc-aac.mkv"), 0.0).unwrap();
    let events = observe(&p);
    assert!(
        events.iter().all(|(_, error)| error.is_none()),
        "stale control reply: {events:?}"
    );
    p.command_async(&["set", "v2-nonexistent-property", "yes"])
        .unwrap();
    let events = observe(&p);
    assert!(
        events.iter().any(|(id, error)| *id == 5 && error.is_some()),
        "a current control error must be visible: {events:?}"
    );
}

#[test]
fn rejected_open_preserves_previous_file_attribution() {
    let p = player();
    p.open(&fixture("h264-aac.mp4"), 0.0).unwrap();
    assert!(p.open(&fixture("v2-nonexistent-fixture.mp4"), 0.0).is_err());
    let events = observe(&p);
    assert!(
        events.iter().any(|(id, error)| *id == 8 && error.is_none()),
        "rejected replacement must leave the previous file able to load: {events:?}"
    );
}

// The window checks files on a worker and then calls `open_validated`. These cases run that exact
// two-step path against the real decoder.
#[test]
fn validated_open_keeps_the_replacement_guarantees_for_malformed_then_valid() {
    for delay_ms in [0, 10, 100] {
        let p = player();
        let bad = vesper::media::validate(&fixture("malformed.mp4")).unwrap();
        p.open_validated(&bad, 0.0).unwrap();
        thread::sleep(Duration::from_millis(delay_ms));
        let good = vesper::media::validate(&fixture("h264-aac.mp4")).unwrap();
        p.open_validated(&good, 0.0).unwrap();
        let events = observe(&p);
        assert!(
            events.iter().any(|(id, _)| *id == 8),
            "valid file must load after {delay_ms} ms: {events:?}"
        );
        assert!(
            events.iter().all(|(_, error)| error.is_none()),
            "the malformed file's error leaked after {delay_ms} ms: {events:?}"
        );
    }
}

#[test]
fn twenty_rapid_validated_replacements_load_only_the_last_file() {
    let p = player();
    let files = ["malformed.mp4", "h264-aac.mp4", "hevc-aac.mkv"];
    for i in 0..20 {
        let file = vesper::media::validate(&fixture(files[i % files.len()])).unwrap();
        p.open_validated(&file, 0.0).unwrap();
    }
    // End on one known valid Matroska file so the expectation is explicit.
    let last = vesper::media::validate(&fixture("hevc-aac.mkv")).unwrap();
    p.open_validated(&last, 0.0).unwrap();
    let events = observe(&p);
    assert_eq!(
        events.iter().filter(|(id, _)| *id == 8).count(),
        1,
        "exactly one file may signal loaded: {events:?}"
    );
    assert!(
        events.iter().all(|(_, error)| error.is_none()),
        "no superseded error may surface: {events:?}"
    );
    // The decoder reports the selected file's container: Matroska, not the MP4 files above.
    assert_eq!(p.string("file-format").as_deref(), Some("mkv"));
}

#[test]
fn validation_refuses_what_opening_always_refused() {
    let dir = std::env::temp_dir().join(format!("vesper-validate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let empty = dir.join("empty.mp4");
    std::fs::write(&empty, b"").unwrap();
    let playlist = dir.join("list.m3u8");
    std::fs::write(&playlist, b"#EXTM3U\nhttp://example.test/a.mp4\n").unwrap();
    for bad in [
        PathBuf::from(r"\\server\share\movie.mp4"),
        PathBuf::from("https://example.test/movie.mp4"),
        empty.clone(),
        playlist.clone(),
        dir.clone(),
        dir.join("missing.mkv"),
    ] {
        assert!(vesper::media::validate(&bad).is_err(), "{bad:?}");
    }
    let _ = std::fs::remove_file(empty);
    let _ = std::fs::remove_file(playlist);
    let _ = std::fs::remove_dir(dir);
}
