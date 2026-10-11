use sonora::decoder::{self, AudioDecoder};
use std::{fs, path::PathBuf};
fn fixture(ext: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../fixtures/audio/tone.{ext}"))
}
#[test]
fn seven_major_formats_decode_real_audio() {
    for ext in ["mp3", "wav", "flac", "m4a", "ogg", "opus", "aiff"] {
        let r = decoder::smoke(&fixture(ext)).unwrap_or_else(|e| panic!("{ext}: {e}"));
        assert!(r.frames > 40_000, "{ext}: too little audio");
        assert!(r.peak > 0.01, "{ext}: silent decode");
        assert!(
            (0.9..1.2).contains(&r.duration_seconds),
            "{ext}: wrong duration {}",
            r.duration_seconds
        );
    }
}
#[test]
fn seeking_returns_bounded_remaining_audio() {
    for ext in ["wav", "flac", "mp3", "m4a", "ogg", "opus", "aiff"] {
        let mut d = AudioDecoder::open(&fixture(ext), false).unwrap();
        d.seek(0.5).unwrap_or_else(|e| panic!("{ext}: {e}"));
        let mut frames = 0;
        while let Some(p) = d.next_packet().unwrap() {
            frames += p.len() / d.channels as usize;
        }
        let seconds = frames as f64 / d.rate as f64;
        assert!(
            (0.35..0.7).contains(&seconds),
            "{ext}: seek left {seconds}s"
        );
    }
}
#[test]
fn malformed_and_empty_files_fail_without_panics() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-fixtures");
    fs::create_dir_all(&dir).unwrap();
    for (name, bytes) in [
        ("empty.wav", Vec::new()),
        ("broken.mp3", vec![0xff; 1024]),
        ("broken.flac", b"fLaCbad".to_vec()),
        ("broken.opus", b"OggSbad".to_vec()),
    ] {
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        let r = std::panic::catch_unwind(|| decoder::smoke(&p));
        assert!(r.is_ok(), "panic {name}");
        assert!(r.unwrap().is_err(), "accepted {name}");
    }
}
#[test]
fn unicode_and_long_paths_decode() {
    let mut dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-fixtures/Glazba ž ć 音楽");
    for i in 0..9 {
        dir = dir.join(format!("long-folder-{i}-abcdefghijklmno"));
    }
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("Pjesma č 音.wav");
    fs::copy(fixture("wav"), &p).unwrap();
    assert!(decoder::smoke(&p).unwrap().frames > 0);
}
#[test]
fn extra_codecs_metadata_and_artwork() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in [
        "tone.aac",
        "tone-alac.m4a",
        "tone-24bit.wav",
        "tone-float.wav",
        "tone-mono.opus",
        "tone-192k-8ch.wav",
        "tagged-cover.mp3",
    ] {
        assert!(decoder::smoke(&root.join(name)).unwrap().peak > 0.01);
    }
    let d = AudioDecoder::open(&root.join("tagged-cover.mp3"), true).unwrap();
    assert_eq!(d.metadata.title, "Sonora fixture");
    assert_eq!(d.metadata.artist, "Version2");
    assert_eq!(d.metadata.album, "Offline tests");
    let (w, h, p) = d.metadata.artwork.unwrap();
    assert_eq!((w, h, p.len()), (32, 32, 4096));
    let r = decoder::smoke(&root.join("tone-192k-8ch.wav")).unwrap();
    assert_eq!((r.sample_rate, r.channels, r.frames), (192000, 8, 192000));
}
#[test]
fn playlist_roundtrip_preserves_unicode_order_and_rejects_network() {
    use sonora::queue::{load_playlist, save_playlist, Track};
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-fixtures");
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("list.sonora");
    let tracks = vec![Track::new(fixture("opus")), Track::new(fixture("wav"))];
    save_playlist(&p, &tracks).unwrap();
    assert_eq!(
        load_playlist(&p).unwrap(),
        tracks.iter().map(|t| t.path.clone()).collect::<Vec<_>>()
    );
    let m = dir.join("network.m3u8");
    fs::write(&m, "#EXTM3U\nhttps://example.test/music.mp3\n").unwrap();
    assert!(load_playlist(&m).is_err());
    let original = dir.join("original.wav");
    fs::copy(fixture("wav"), &original).unwrap();
    let before = fs::read(&original).unwrap();
    assert!(save_playlist(&original, &tracks).is_err());
    assert_eq!(fs::read(&original).unwrap(), before);
    let queued = dir.join("queued-original.sonora");
    fs::write(&queued, b"preserve original bytes").unwrap();
    assert!(save_playlist(&queued, &[Track::new(queued.clone())]).is_err());
    assert_eq!(fs::read(&queued).unwrap(), b"preserve original bytes");
}
