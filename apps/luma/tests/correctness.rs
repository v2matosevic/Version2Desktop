use luma::{
    decode::{self, Cancel, Frame, Photo},
    model::{Album, PhotoCache, View},
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
    decode::decode(&fixture(name), &Cancel::never()).unwrap()
}
fn pixel(p: &Photo, x: usize, y: usize) -> [u8; 4] {
    p.frames[0].rgba[(y * p.width as usize + x) * 4..(y * p.width as usize + x) * 4 + 4]
        .try_into()
        .unwrap()
}
#[test]
fn representative_major_formats_decode_real_files() {
    for name in [
        "pattern.jpg",
        "pattern.png",
        "pattern.webp",
        "pattern.bmp",
        "pattern.tiff",
        "pattern.avif",
    ] {
        let p = photo(name);
        assert_eq!((p.width, p.height), (64, 48), "{name}");
        assert_eq!(p.byte_len(), 64 * 48 * 4);
        let a = pixel(&p, 8, 8);
        assert!(a[0] > 200 && a[1] < 50 && a[2] < 60, "{name}: {a:?}");
        let b = pixel(&p, 56, 8);
        assert!(b[1] > 180 && b[0] < 60, "{name}: {b:?}");
    }
}
#[test]
fn large_baseline_jpeg_uses_oriented_bounded_preview_but_progressive_rejects() {
    let p = photo("large-baseline.jpg");
    assert_eq!(p.preview_source, Some([8000, 6000]));
    assert_eq!((p.width, p.height), (3000, 4000));
    assert!(p.byte_len() < decode::MAX_FRAME_BYTES);
    assert!(p.warnings.iter().any(|w| w.contains("Scaled JPEG preview")));
    let corner = pixel(&p, 8, 8);
    assert!(corner[2] > 200 && corner[0] < 60);
    let error = decode::decode(&fixture("large-progressive.jpg"), &Cancel::never()).unwrap_err();
    assert!(error.contains("progressive"));
}
#[test]
fn png_alpha_and_gif_disposal_are_preserved() {
    let p = photo("alpha.png");
    assert_eq!(pixel(&p, 5, 5)[3], 128);
    assert_eq!(pixel(&p, 25, 20)[3], 0);
    let g = photo("disposal.gif");
    assert_eq!(g.frames.len(), 3);
    assert_eq!(g.frames[0].rgba[0], 235);
    assert_eq!(g.frames[1].rgba[3], 0);
    let green = (4 * 64 + 20) * 4;
    assert!(g.frames[1].rgba[green + 1] > 180);
    assert_eq!(g.frames[2].rgba[green + 3], 0);
}
#[test]
fn static_png_and_webp_do_not_claim_the_file_is_animated() {
    for name in [
        "pattern.png",
        "pattern.webp",
        "lossy.webp",
        "progressive.jpg",
    ] {
        let p = photo(name);
        assert!(!p.warnings.iter().any(|w| w.contains("Animated")));
    }
}
#[test]
fn all_eight_exif_orientations_match_expected_quadrants() {
    // Independent corner mapping, labels TL red, TR green, BR yellow, BL blue.
    let expected = [
        [0, 1, 2, 3],
        [1, 0, 3, 2],
        [2, 3, 0, 1],
        [3, 2, 1, 0],
        [0, 3, 2, 1],
        [3, 0, 1, 2],
        [2, 1, 0, 3],
        [1, 2, 3, 0],
    ];
    let reference = photo("orientation-1.jpg");
    let colors = [
        pixel(&reference, 8, 8),
        pixel(&reference, 56, 8),
        pixel(&reference, 56, 40),
        pixel(&reference, 8, 40),
    ];
    for n in 1..=8 {
        let p = photo(&format!("orientation-{n}.jpg"));
        let swapped = n >= 5;
        assert_eq!(
            (p.width, p.height),
            if swapped { (48, 64) } else { (64, 48) }
        );
        let w = p.width as usize;
        let h = p.height as usize;
        let corners = [
            pixel(&p, 8, 8),
            pixel(&p, w - 9, 8),
            pixel(&p, w - 9, h - 9),
            pixel(&p, 8, h - 9),
        ];
        for i in 0..4 {
            let expected = colors[expected[n - 1][i]];
            for c in 0..3 {
                assert!(
                    corners[i][c].abs_diff(expected[c]) < 8,
                    "orientation {n}, corner {i}"
                );
            }
        }
    }
}
#[test]
fn rgb_icc_srgb_preserves_colors() {
    let p = photo("icc-srgb.png");
    let reference = photo("pattern.png");
    for (a, b) in p.frames[0].rgba.iter().zip(&reference.frames[0].rgba) {
        assert!(a.abs_diff(*b) <= 2);
    }
}
#[test]
fn gif_animation_has_real_frames_timing_and_pixels() {
    let p = photo("animation.gif");
    assert_eq!(p.frames.len(), 3);
    assert_eq!(
        p.frames.iter().map(|f| f.delay_ms).collect::<Vec<_>>(),
        [40, 80, 120]
    );
    assert!(p.frames[0].rgba[0] > 200);
    assert!(p.frames[1].rgba[1] > 180);
    assert!(p.frames[2].rgba[2] > 200);
}
#[test]
fn malformed_huge_and_overlong_animation_return_errors() {
    for name in ["malformed.png", "huge.png", "too-many-frames.gif"] {
        assert!(
            decode::decode(&fixture(name), &Cancel::never()).is_err(),
            "{name}"
        );
    }
    assert!(decode::checked_dimensions(u32::MAX, u32::MAX).is_err());
    assert!(decode::checked_dimensions(0, 16).is_err());
}
#[test]
fn windows_codec_files_that_are_not_what_they_claim_fail_with_a_clear_message() {
    // Whatever codecs this machine has, a text file named .heic/.dng must fail usefully: either
    // "needs a Windows codec" or "no installed codec recognised this file", never a crash and
    // never a bare error code.
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "photo.heic",
        "photo.heif",
        "photo.nef",
        "photo.dng",
        "photo.jxr",
        "photo.jxl",
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, b"this is not an image, only text").unwrap();
        let error = decode::decode(&path, &Cancel::never()).unwrap_err();
        assert!(
            error.contains("Windows codec")
                || error.contains("recognised")
                || error.contains("recognized"),
            "{name}: {error}"
        );
        assert!(
            error.contains("0x") || error.contains("needs a Windows codec"),
            "{name}: {error}"
        );
    }
    // A file that is not there is reported as unreadable, not as a codec problem.
    let error = decode::decode(&dir.path().join("gone.heic"), &Cancel::never()).unwrap_err();
    assert!(error.contains("Cannot read image"));
}
#[test]
fn compressed_file_limit_rejects_sparse_file_without_reading_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.jpg");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(decode::MAX_FILE_BYTES + 1)
        .unwrap();
    let error = decode::decode(&path, &Cancel::never()).unwrap_err();
    assert!(error.contains("compressed-file"));
}
#[test]
fn cancellation_blocks_old_request() {
    let gen_id = Arc::new(AtomicU64::new(4));
    let cancel = Cancel::new(gen_id.clone(), 4);
    assert!(cancel.check().is_ok());
    gen_id.store(5, Ordering::Release);
    assert!(
        decode::decode(&fixture("pattern.png"), &cancel)
            .unwrap_err()
            .contains("Cancelled")
    );
}
#[test]
fn album_navigation_wraps_and_ignores_nested_folders() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.jpg", "b.png", "c.avif", "z.txt"] {
        std::fs::write(dir.path().join(name), b"fixture").unwrap();
    }
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/hidden.png"), b"fixture").unwrap();
    let mut album = Album::open(&dir.path().join("b.png")).unwrap();
    assert_eq!(album.files.len(), 3);
    assert_eq!(album.index, 1);
    assert!(album.step(1));
    assert_eq!(album.index, 2);
    album.step(1);
    assert_eq!(album.index, 0);
    album.step(-1);
    assert_eq!(album.index, 2);
}
#[test]
fn empty_folder_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let mut album = Album::open(dir.path()).unwrap();
    assert!(album.current().is_none());
    assert!(!album.step(1));
}
#[test]
fn unicode_and_long_paths_decode_and_navigate() {
    let dir = tempfile::tempdir().unwrap();
    let mut path = dir.path().to_path_buf();
    for _ in 0..5 {
        path = path.join("long-folder-0123456789012345678901234567890123456789");
    }
    std::fs::create_dir_all(&path).unwrap();
    let file = path.join("žuta-写真-😀.png");
    std::fs::copy(fixture("pattern.png"), &file).unwrap();
    let p = decode::decode(&file, &Cancel::never()).unwrap();
    assert_eq!(p.width, 64);
    let album = Album::open(&file).unwrap();
    assert_eq!(album.files.len(), 1);
    assert!(album.current().unwrap().to_string_lossy().contains("写真"));
}
#[test]
fn zoom_keeps_cursor_image_coordinate_and_rotation_cycles() {
    let mut v = View::default();
    let image = [4000.0, 3000.0];
    let viewport = [800.0, 600.0];
    let cursor = [140.0, -50.0];
    let before = v.scale(image, viewport);
    let point = [
        (cursor[0] - v.pan[0]) / before,
        (cursor[1] - v.pan[1]) / before,
    ];
    v.zoom_at(2.0, cursor, image, viewport);
    for i in 0..2 {
        assert!(((cursor[i] - v.pan[i]) / v.zoom - point[i]).abs() < 0.001);
    }
    let uv = v.uv();
    for _ in 0..4 {
        v.rotate(true);
    }
    assert_eq!(v.uv(), uv);
    v.rotate(true);
    assert_eq!(v.dimensions(image), [3000.0, 4000.0]);
    assert_eq!(v.uv()[0], [0.0, 1.0]);
    v.rotate(false);
    assert_eq!(v.rotation, 0);
}
#[test]
fn cache_is_bounded_by_total_animation_bytes_and_entry_count() {
    let p = Arc::new(Photo {
        width: 2,
        height: 2,
        format: "test".into(),
        frames: vec![
            Frame {
                rgba: vec![0; 16],
                delay_ms: 40,
            },
            Frame {
                rgba: vec![0; 16],
                delay_ms: 40,
            },
        ],
        ..Photo::default()
    });
    let mut cache = PhotoCache::new(64);
    for i in 0..10 {
        cache.insert(PathBuf::from(format!("{i}.gif")), p.clone());
        assert!(cache.bytes <= 64);
    }
    assert!(cache.get(Path::new("0.gif")).is_none());
    assert!(cache.get(Path::new("9.gif")).is_some());
    cache.clear();
    assert_eq!(cache.bytes, 0);
    let mut tiny = PhotoCache::new(8);
    tiny.insert(PathBuf::from("large.gif"), p);
    assert_eq!(tiny.bytes, 0);
}
#[test]
fn latest_worker_request_is_the_only_result() {
    let ctx = eframe::egui::Context::default();
    let worker = luma::worker::Worker::new(ctx);
    for _ in 0..100 {
        worker.open(fixture("malformed.png"));
    }
    let generation = worker.open(fixture("pattern.png"));
    let start = std::time::Instant::now();
    loop {
        if let Some(reply) = worker.take() {
            assert_eq!(reply.generation, generation);
            assert_eq!(reply.photo.unwrap().unwrap().width, 64);
            break;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
