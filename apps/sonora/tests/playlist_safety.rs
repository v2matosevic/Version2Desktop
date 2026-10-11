use sonora::queue::{load_playlist, save_playlist, Track};
use std::{fs, path::PathBuf, time::SystemTime};

fn fixture_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "sonora-playlist-safety-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&dir).unwrap();
    dir
}

#[test]
fn saving_through_a_hard_link_preserves_queued_audio_bytes() {
    let dir = fixture_dir("hard-link");
    let original = dir.join("original.wav");
    let playlist = dir.join("alias.m3u");
    fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/audio/tone.wav"),
        &original,
    )
    .unwrap();
    let before = fs::read(&original).unwrap();
    fs::hard_link(&original, &playlist).unwrap();
    assert_ne!(
        fs::canonicalize(&original).unwrap(),
        fs::canonicalize(&playlist).unwrap(),
        "Different hard-link names bypass a pathname-only comparison"
    );

    // Either reject the alias or replace its directory entry atomically. Never
    // truncate the shared file that contains the queued audio original.
    let _ = save_playlist(&playlist, &[Track::new(original.clone())]);
    assert!(
        fs::read(&original).unwrap() == before,
        "Saving a playlist changed a queued audio original through a hard link"
    );
}

#[test]
fn saving_replaces_an_ordinary_existing_playlist() {
    let dir = fixture_dir("replace");
    let playlist = dir.join("queue.m3u");
    let first = dir.join("first.wav");
    let second = dir.join("second.wav");
    save_playlist(&playlist, &[Track::new(first)]).unwrap();
    save_playlist(&playlist, &[Track::new(second.clone())]).unwrap();
    assert_eq!(load_playlist(&playlist).unwrap(), vec![second]);
}
