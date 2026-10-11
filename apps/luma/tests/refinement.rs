//! 2026-10-11 refinement: built-in animation, EXIF, folder order and view safeguards.
use luma::{
    decode::{self, Cancel, Frame, Photo},
    model::{Album, View, user_path},
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}
fn photo(name: &str) -> Photo {
    decode::decode(&fixture(name), &Cancel::never()).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn animated_webp_and_apng_play_all_frames_in_order() {
    for name in ["animated.webp", "animated.png"] {
        let p = photo(name);
        assert_eq!(p.frames.len(), 3, "{name}");
        assert_eq!((p.width, p.height), (64, 48), "{name}");
        // Each frame is dominated by one colour: red, green, blue, so order is checkable.
        let dominant = |f: &Frame| {
            let at = (24 * 64 + 40) * 4;
            let (r, g, b) = (f.rgba[at], f.rgba[at + 1], f.rgba[at + 2]);
            if r > g && r > b {
                'R'
            } else if g > r && g > b {
                'G'
            } else {
                'B'
            }
        };
        let order: String = p.frames.iter().map(dominant).collect();
        assert_eq!(order, "RGB", "{name}");
        for f in &p.frames {
            assert!(
                (20..=60_000).contains(&f.delay_ms),
                "{name}: {}",
                f.delay_ms
            );
        }
        // The old "shown as its first frame" limitation no longer applies to these.
        assert!(
            !p.warnings.iter().any(|w| w.contains("first frame")),
            "{name}: {:?}",
            p.warnings
        );
        assert!(p.byte_len() <= decode::MAX_FRAME_BYTES);
    }
    assert_eq!(photo("animated.png").format, "APNG");
    assert_eq!(photo("animated.webp").format, "WebP / animated");
}

#[test]
fn a_still_webp_is_not_mistaken_for_an_animation() {
    let p = photo("animated-still.webp");
    assert_eq!(p.frames.len(), 1);
    assert_eq!(p.frames[0].delay_ms, 0);
}

#[test]
fn animation_is_cancellable_and_still_bounded() {
    let generation = Arc::new(AtomicU64::new(1));
    let cancel = Cancel::new(generation.clone(), 1);
    generation.store(2, Ordering::Release);
    for name in ["animated.webp", "animated.png"] {
        let error = decode::decode(&fixture(name), &cancel).unwrap_err();
        assert!(error.contains("Cancelled"), "{name}: {error}");
    }
    // The existing frame-count limit still holds.
    assert!(decode::decode(&fixture("too-many-frames.gif"), &Cancel::never()).is_err());
}

#[test]
fn camera_exif_is_read_from_a_jpeg_and_listed_in_reading_order() {
    let p = photo("exif-camera.jpg");
    let exif = p.exif.as_ref().expect("EXIF block");
    assert_eq!(exif.make.as_deref(), Some("Version2"));
    assert_eq!(exif.model.as_deref(), Some("Version2 Cam X"));
    assert_eq!(exif.lens.as_deref(), Some("Prime 35mm f/1.8"));
    assert_eq!(exif.taken.as_deref(), Some("2026-10-09 14:03:22"));
    assert_eq!(exif.iso, Some(400));
    assert_eq!(exif.flash_fired, Some(false));
    let gps = exif.gps.as_ref().expect("gps");
    assert!((gps.latitude - (44.0 + 6.0 / 60.0 + 30.0 / 3600.0)).abs() < 1e-6);
    assert!((gps.longitude - (15.0 + 13.0 / 60.0 + 12.0 / 3600.0)).abs() < 1e-6);
    assert!((gps.altitude.unwrap() - 12.34).abs() < 1e-6);
    let labels: Vec<_> = exif.rows().iter().map(|(k, _)| *k).collect();
    assert_eq!(
        labels,
        [
            "Camera",
            "Lens",
            "Taken",
            "Exposure",
            "Focal length",
            "Exposure bias",
            "Flash",
            "Location",
            "Software"
        ]
    );
    assert_eq!(p.decoder, "Luma built-in decoder");
    assert_eq!(p.applied_orientation, None);
    // A photo with no EXIF block has none, rather than an empty one.
    assert!(photo("pattern.jpg").exif.is_none());
}

#[test]
fn exif_orientation_is_still_applied_and_reported() {
    let p = photo("orientation-6.jpg");
    assert_eq!(p.applied_orientation, Some(6));
    assert_eq!((p.width, p.height), (48, 64));
    assert_eq!(photo("orientation-1.jpg").applied_orientation, None);
}

#[test]
fn folders_list_in_explorer_order_not_byte_order() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "IMG_10.png",
        "img_2.png",
        "IMG_1.png",
        "b.png",
        "A.png",
        "img_20.jpg",
    ] {
        std::fs::write(dir.path().join(name), b"x").unwrap();
    }
    let album = Album::open(dir.path()).unwrap();
    let names: Vec<_> = album
        .files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "A.png",
            "b.png",
            "IMG_1.png",
            "img_2.png",
            "IMG_10.png",
            "img_20.jpg"
        ]
    );
    // A file the person picked explicitly is listed even with an unusual extension.
    let odd = dir.path().join("zz.xyz");
    std::fs::write(&odd, b"x").unwrap();
    let opened = Album::open(&odd).unwrap();
    let wanted = user_path(&odd.canonicalize().unwrap());
    assert!(opened.files.contains(&wanted), "{:?}", opened.files);
    assert_eq!(opened.current(), Some(&wanted));
}

#[test]
fn windows_codec_extensions_are_listed_in_folders() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "a.heic", "b.dng", "c.jxr", "d.jxl", "e.png", "f.txt", "g.cr3",
    ] {
        std::fs::write(dir.path().join(name), b"x").unwrap();
    }
    let album = Album::open(dir.path()).unwrap();
    assert_eq!(album.files.len(), 6, "{:?}", album.files);
}

#[test]
fn neighbour_prefetch_follows_the_direction_of_travel_and_wraps() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.png", "b.png", "c.png"] {
        std::fs::write(dir.path().join(name), b"x").unwrap();
    }
    let mut album = Album::open(&dir.path().join("a.png")).unwrap();
    let name =
        |p: Option<PathBuf>| p.map(|p| p.file_name().unwrap().to_string_lossy().into_owned());
    assert_eq!(name(album.neighbour_path(1)), Some("b.png".into()));
    assert_eq!(
        name(album.neighbour_path(-1)),
        Some("c.png".into()),
        "wraps backwards"
    );
    album.step(-1);
    assert_eq!(name(album.neighbour_path(-1)), Some("b.png".into()));
    // A single photo has no neighbour to prefetch.
    let single = tempfile::tempdir().unwrap();
    std::fs::write(single.path().join("only.png"), b"x").unwrap();
    let album = Album::open(single.path()).unwrap();
    assert!(album.neighbour_path(1).is_none() && album.neighbour_path(-1).is_none());
}

#[test]
fn extended_length_prefixes_are_hidden_from_people_but_kept_when_needed() {
    assert_eq!(
        user_path(Path::new(r"\\?\C:\Photos\a.png")),
        PathBuf::from(r"C:\Photos\a.png")
    );
    assert_eq!(
        user_path(Path::new(r"\\?\UNC\nas\photos\a.png")),
        PathBuf::from(r"\\nas\photos\a.png")
    );
    assert_eq!(
        user_path(Path::new(r"D:\plain\a.png")),
        PathBuf::from(r"D:\plain\a.png")
    );
    // Too long for the short form: the prefix stays, otherwise the path would stop working.
    let long = format!(r"\\?\C:\{}\a.png", "x".repeat(300));
    assert_eq!(user_path(Path::new(&long)), PathBuf::from(&long));
    // Something that only looks like a prefix is left alone.
    assert_eq!(
        user_path(Path::new(r"\\?\Volume{1234}\a.png")),
        PathBuf::from(r"\\?\Volume{1234}\a.png")
    );
}

#[test]
fn actual_pixels_stay_one_to_one_when_the_window_moves_between_displays() {
    let image = [4000.0, 3000.0];
    let viewport = [800.0, 600.0];
    let mut v = View::default();
    v.actual_physical(1.5);
    // 1:1 physical at 150% scaling is 1/1.5 of a logical point per image pixel.
    assert!((v.scale(image, viewport) - 1.0 / 1.5).abs() < 1e-6);
    // The window is dragged to a 200% display: still exactly one image pixel per screen pixel.
    v.sync_display_scale(2.0);
    assert!((v.scale(image, viewport) - 0.5).abs() < 1e-6);
    v.sync_display_scale(1.0);
    assert!((v.scale(image, viewport) - 1.0).abs() < 1e-6);
    // Any deliberate zoom ends the 1:1 promise, so the next display change cannot undo it.
    v.zoom_at(1.25, [0.0, 0.0], image, viewport);
    let zoomed = v.scale(image, viewport);
    v.sync_display_scale(2.0);
    assert_eq!(v.scale(image, viewport), zoomed);
    v.actual_physical(1.0);
    v.fit();
    v.sync_display_scale(3.0);
    assert!(v.fit && !v.physical);
}

#[test]
fn panning_cannot_lose_the_picture() {
    let viewport = [800.0, 600.0];
    let mut v = View::default();
    v.actual();
    v.pan = [100_000.0, -100_000.0];
    v.clamp_pan([4000.0, 3000.0], viewport);
    // 4000 wide at 100% in an 800 view: the edge may meet the edge, never pass it.
    assert_eq!(v.pan, [1600.0, -1200.0]);
    // A picture smaller than the view stays centred however hard it is dragged.
    let mut small = View::default();
    small.actual();
    small.pan = [300.0, -50.0];
    small.clamp_pan([200.0, 100.0], viewport);
    assert_eq!(small.pan, [0.0, 0.0]);
    // Rotation swaps which side overflows: 1000 x 300 turned is 300 wide, 1000 tall.
    let mut turned = View::default();
    turned.actual();
    turned.rotate(true);
    turned.pan = [100_000.0, 100_000.0];
    turned.clamp_pan([1000.0, 300.0], viewport);
    assert_eq!(turned.pan, [0.0, 200.0]);
}
