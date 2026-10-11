//! Headless UI evidence: rasterize actual egui meshes on CPU. This proves layout,
//! not the Windows/OpenGL presentation path. Never creates a desktop window.
use eframe::egui::{self, Pos2, Rect, TextureId, Vec2};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// What a capture shows. The first five are the original ten layouts' scenes, unchanged.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scene {
    Empty,
    Photo,
    Error,
    Details,
    Loading,
    /// Camera facts, the decoder and an applied orientation in the details card.
    DetailsExif,
    /// A copy succeeded, shown near the dock.
    NoticeOk,
    NoticeFailed,
    /// The next photo is still decoding while the current one stays up.
    Busy,
    /// A three-frame animation, so the play/pause control is in the dock.
    Animated,
    /// A real failed decode through the Windows-codec path, message as the app shows it.
    ErrorCodecDamaged,
    /// The message when a codec family is not installed.
    ErrorCodecMissing,
    /// The Format support sheet with what this machine's Windows reports right now.
    FormatsHere,
    /// The same sheet with one codec missing, one failing and one found but unsampled.
    FormatsMixed,
    /// The photo's right-click menu.
    ContextMenu,
    /// The keyboard shortcut sheet (F1).
    Shortcuts,
}

pub fn run(output: &Path) -> Result<(), String> {
    use Scene::*;
    std::fs::create_dir_all(output).map_err(|e| e.to_string())?;
    for (name, light, scale, scene, w, h) in [
        ("empty-dark", false, 1.0, Empty, 1100.0, 740.0),
        ("empty-light", true, 1.0, Empty, 1100.0, 740.0),
        ("photo-dark", false, 1.0, Photo, 1100.0, 740.0),
        ("photo-light", true, 1.0, Photo, 1100.0, 740.0),
        ("details-dark", false, 1.0, Details, 1100.0, 740.0),
        ("details-light", true, 1.0, Details, 1100.0, 740.0),
        ("error-dark", false, 1.0, Error, 1100.0, 740.0),
        ("loading-dark", false, 1.0, Loading, 1100.0, 740.0),
        ("compact-dark", false, 1.0, Photo, 880.0, 560.0),
        ("photo-dpi200", false, 2.0, Photo, 900.0, 600.0),
        ("details-exif-dark", false, 1.0, DetailsExif, 1100.0, 740.0),
        ("details-exif-light", true, 1.0, DetailsExif, 1100.0, 740.0),
        ("notice-copied-dark", false, 1.0, NoticeOk, 1100.0, 740.0),
        (
            "notice-failed-light",
            true,
            1.0,
            NoticeFailed,
            1100.0,
            740.0,
        ),
        ("busy-dark", false, 1.0, Busy, 1100.0, 740.0),
        ("animated-dark", false, 1.0, Animated, 1100.0, 740.0),
        (
            "error-codec-damaged-dark",
            false,
            1.0,
            ErrorCodecDamaged,
            1100.0,
            740.0,
        ),
        (
            "error-codec-missing-light",
            true,
            1.0,
            ErrorCodecMissing,
            1100.0,
            740.0,
        ),
        ("formats-here-dark", false, 1.0, FormatsHere, 1100.0, 740.0),
        (
            "formats-mixed-light",
            true,
            1.0,
            FormatsMixed,
            1100.0,
            740.0,
        ),
        ("context-menu-dark", false, 1.0, ContextMenu, 1100.0, 740.0),
        ("shortcuts-dark", false, 1.0, Shortcuts, 1100.0, 740.0),
        ("narrow-shortcuts-light", true, 1.0, Shortcuts, 640.0, 440.0),
        ("narrow-dark", false, 1.0, Photo, 640.0, 440.0),
        ("narrow-details-dark", false, 1.0, Details, 640.0, 440.0),
        ("narrow-animated-light", true, 1.0, Animated, 640.0, 440.0),
        ("narrow-empty-dark", false, 1.0, Empty, 640.0, 440.0),
        (
            "narrow-formats-light",
            true,
            1.0,
            FormatsMixed,
            640.0,
            440.0,
        ),
    ] {
        let ctx = egui::Context::default();
        crate::ui::install(&ctx);
        ctx.set_theme(if light {
            egui::Theme::Light
        } else {
            egui::Theme::Dark
        });
        ctx.set_pixels_per_point(scale);
        let mut app = crate::ui::Luma::headless();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        if matches!(
            scene,
            Photo
                | Details
                | DetailsExif
                | NoticeOk
                | NoticeFailed
                | Busy
                | FormatsHere
                | FormatsMixed
                | ContextMenu
                | Shortcuts
        ) {
            let path = root.join("visual-scene.png");
            let mut photo = crate::decode::decode(&path, &crate::decode::Cancel::never())?;
            if scene == DetailsExif {
                photo.decoder = "Windows codec: Microsoft HEIF Image Decoder".into();
                photo.format = "HEIC / Windows codec".into();
                photo.applied_orientation = Some(6);
                photo.exif = Some(sample_exif());
            }
            app.show_photo(photo, path.clone());
            // A folder of photos, so position and the edge arrows have something to say.
            app.album
                .files
                .extend(["harbour.jpg", "old-town.jpg"].map(|n| path.with_file_name(n)));
        }
        match scene {
            Error => {
                app.error = Some(
                    "HEIC / HEIF needs a Windows codec, and none is installed on this PC. Luma does not install or download codecs. Install \"HEIF Image Extensions\" from Microsoft Store. HEVC-compressed HEIC (iPhone photos) also needs \"HEVC Video Extensions\". (Windows reported 0x88982F50.)".into(),
                );
            }
            ErrorCodecMissing => {
                app.error = Some(crate::wic::missing_codec_message(
                    crate::wic::Family::Raw,
                    "0x88982F50",
                ));
            }
            ErrorCodecDamaged => {
                // The real thing: decode a damaged file through the Windows path.
                let damaged = root.join("windows-codecs/truncated.heic");
                app.error = crate::decode::decode(&damaged, &crate::decode::Cancel::never()).err();
                if app.error.is_none() {
                    return Err("truncated.heic unexpectedly decoded".into());
                }
            }
            Details | DetailsExif => app.visual_details(),
            Shortcuts => app.visual_shortcuts(),
            Loading => app.loading = true,
            Busy => app.visual_busy(),
            NoticeOk => app.visual_notice("Image copied", true),
            NoticeFailed => app.visual_notice(
                "The clipboard is in use by another program. Try again.",
                false,
            ),
            Animated => {
                let path = root.join("animation.gif");
                app.show_photo(
                    crate::decode::decode(&path, &crate::decode::Cancel::never())?,
                    path.clone(),
                );
            }
            _ => {}
        }
        match scene {
            FormatsHere => app.visual_formats(crate::wic::status()),
            FormatsMixed => app.visual_formats(mixed_status()),
            _ => {}
        }
        let mut textures = HashMap::new();
        let mut image = None;
        // The menu fades in over about five frames, so its capture runs well past the click.
        let frames = if scene == ContextMenu { 12 } else { 3 };
        for n in 0..frames {
            let centre = Pos2::new(w * 0.5, h * 0.45);
            let events = match (scene, n) {
                (Photo | Animated | Busy | NoticeOk | NoticeFailed, _) => {
                    // The pointer rests on the photo, as it would while someone is looking at it.
                    vec![egui::Event::PointerMoved(centre)]
                }
                (ContextMenu, 0) => vec![egui::Event::PointerMoved(centre)],
                // A right click as a real mouse delivers it, in the same order the unit test
                // `a_right_click_on_the_photo_opens_its_menu` proves paints the menu: hover, a
                // settling frame so the photo's hit area is known, press, release, then empty
                // frames for the menu to be measured, drawn and faded in.
                (ContextMenu, 2 | 3) => vec![egui::Event::PointerButton {
                    pos: centre,
                    button: egui::PointerButton::Secondary,
                    pressed: n == 2,
                    modifiers: Default::default(),
                }],
                _ => vec![],
            };
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))),
                time: Some(n as f64 / 60.0),
                events,
                ..Default::default()
            };
            let output_frame = ctx.run_ui(raw, |ui| app.render(ui));
            for (id, delta) in output_frame.textures_delta.set {
                let egui::ImageData::Color(source) = delta.image;
                if let Some(pos) = delta.pos {
                    let target = textures
                        .get_mut(&id)
                        .ok_or("Missing texture patch target")?;
                    patch(target, &source, pos);
                } else {
                    textures.insert(id, (*source).clone());
                }
            }
            let primitives = ctx.tessellate(output_frame.shapes, output_frame.pixels_per_point);
            image = Some(raster(
                &primitives,
                &textures,
                (w * scale) as u32,
                (h * scale) as u32,
                scale,
            ));
            for id in output_frame.textures_delta.free {
                textures.remove(&id);
            }
        }
        image
            .unwrap()
            .save(output.join(format!("{name}.png")))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
/// Camera facts as a typical phone photo would carry them. Values are invented for the capture.
fn sample_exif() -> crate::exif::Exif {
    crate::exif::Exif {
        make: Some("Version2".into()),
        model: Some("Version2 Cam X".into()),
        lens: Some("Prime 35mm f/1.8".into()),
        software: Some("Luma capture fixture".into()),
        taken: Some("2026-10-09 14:03:22".into()),
        exposure_time: Some((1, 250)),
        aperture: Some(1.8),
        iso: Some(400),
        focal_length: Some(35.0),
        focal_length_35mm: Some(52),
        exposure_bias: Some(-0.7),
        flash_fired: Some(false),
        gps: Some(crate::exif::Gps {
            latitude: 44.11,
            longitude: 15.23,
            altitude: Some(12.0),
        }),
        orientation: Some(6),
    }
}

/// A mix of outcomes, so the sheet's wording for each can be inspected.
fn mixed_status() -> Vec<crate::wic::FamilyStatus> {
    use crate::wic::{Availability, Family, FamilyStatus};
    vec![
        FamilyStatus {
            family: Family::Heif,
            availability: Availability::Working {
                decoder: "Microsoft HEIF Image Decoder".into(),
            },
        },
        FamilyStatus {
            family: Family::Dng,
            availability: Availability::Working {
                decoder: "Microsoft Raw Image Decoder".into(),
            },
        },
        FamilyStatus {
            family: Family::Raw,
            availability: Availability::Missing,
        },
        FamilyStatus {
            family: Family::JpegXr,
            availability: Availability::Failing {
                decoder: "Microsoft HD Photo Decoder".into(),
                error: "The Windows JPEG XR codec could not decode this file (0x88982F60).".into(),
            },
        },
        FamilyStatus {
            family: Family::JpegXl,
            availability: Availability::Registered {
                decoder: "Microsoft JPEG XL Decoder".into(),
            },
        },
    ]
}

fn patch(target: &mut egui::ColorImage, source: &egui::ColorImage, pos: [usize; 2]) {
    for y in 0..source.size[1] {
        let dest = (pos[1] + y) * target.size[0] + pos[0];
        target.pixels[dest..dest + source.size[0]]
            .copy_from_slice(&source.pixels[y * source.size[0]..(y + 1) * source.size[0]]);
    }
}
fn raster(
    primitives: &[egui::ClippedPrimitive],
    textures: &HashMap<TextureId, egui::ColorImage>,
    width: u32,
    height: u32,
    scale: f32,
) -> image::RgbaImage {
    let mut image = image::RgbaImage::from_pixel(width, height, image::Rgba([12, 14, 17, 255]));
    for clipped in primitives {
        let egui::epaint::Primitive::Mesh(mesh) = &clipped.primitive else {
            panic!("Headless smoke cannot render custom GPU callbacks")
        };
        let texture = textures.get(&mesh.texture_id).expect("Mesh texture");
        for indices in mesh.indices.chunks_exact(3) {
            let v = indices.map_vertices(&mesh.vertices);
            let p = v.map(|v| v.pos * scale);
            let edge =
                |a: Pos2, b: Pos2, q: Pos2| (q.x - a.x) * (b.y - a.y) - (q.y - a.y) * (b.x - a.x);
            let area = edge(p[0], p[1], p[2]);
            if area.abs() < 1e-6 {
                continue;
            }
            // Edge i runs between the two vertices other than i, in the triangle's winding.
            let owns: [bool; 3] = std::array::from_fn(|i| {
                let (a, b) = (p[(i + 1) % 3], p[(i + 2) % 3]);
                let (dx, dy) = if area > 0.0 {
                    (b.x - a.x, b.y - a.y)
                } else {
                    (a.x - b.x, a.y - b.y)
                };
                dy > 0.0 || (dy == 0.0 && dx > 0.0)
            });
            let clip = clipped.clip_rect * scale;
            let min_x = p
                .iter()
                .map(|p| p.x)
                .fold(f32::INFINITY, f32::min)
                .floor()
                .max(clip.left())
                .max(0.0) as u32;
            let max_x = p
                .iter()
                .map(|p| p.x)
                .fold(f32::NEG_INFINITY, f32::max)
                .ceil()
                .min(clip.right())
                .min(width as f32) as u32;
            let min_y = p
                .iter()
                .map(|p| p.y)
                .fold(f32::INFINITY, f32::min)
                .floor()
                .max(clip.top())
                .max(0.0) as u32;
            let max_y = p
                .iter()
                .map(|p| p.y)
                .fold(f32::NEG_INFINITY, f32::max)
                .ceil()
                .min(clip.bottom())
                .min(height as f32) as u32;
            for y in min_y..max_y {
                for x in min_x..max_x {
                    let q = Pos2::new(x as f32 + 0.5, y as f32 + 0.5);
                    let b = [
                        edge(p[1], p[2], q) / area,
                        edge(p[2], p[0], q) / area,
                        edge(p[0], p[1], q) / area,
                    ];
                    // Top-left fill rule: a pixel on an edge two triangles share is drawn by
                    // exactly one of them, so translucent feathering never doubles into seams.
                    if (0..3).any(|i| b[i] < -1e-6 || (b[i].abs() <= 1e-6 && !owns[i])) {
                        continue;
                    }
                    let uv = Vec2::new(
                        v[0].uv.x * b[0] + v[1].uv.x * b[1] + v[2].uv.x * b[2],
                        v[0].uv.y * b[0] + v[1].uv.y * b[1] + v[2].uv.y * b[2],
                    );
                    let sample = sample(texture, uv);
                    let colors = v.map(|v| v.color.to_array());
                    let mut src = [0.0f32; 4];
                    for c in 0..4 {
                        src[c] = (colors[0][c] as f32 * b[0]
                            + colors[1][c] as f32 * b[1]
                            + colors[2][c] as f32 * b[2])
                            * sample[c]
                            / 255.0;
                    }
                    let dest = image.get_pixel_mut(x, y);
                    let alpha = src[3] / 255.0;
                    for c in 0..3 {
                        dest[c] = (src[c] + dest[c] as f32 * (1.0 - alpha)).clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    }
    image
}
trait Vertices {
    fn map_vertices(&self, vertices: &[egui::epaint::Vertex]) -> [egui::epaint::Vertex; 3];
}
impl Vertices for [u32] {
    fn map_vertices(&self, v: &[egui::epaint::Vertex]) -> [egui::epaint::Vertex; 3] {
        [
            v[self[0] as usize],
            v[self[1] as usize],
            v[self[2] as usize],
        ]
    }
}
fn sample(texture: &egui::ColorImage, uv: Vec2) -> [f32; 4] {
    let x = (uv.x * texture.size[0] as f32 - 0.5).clamp(0.0, (texture.size[0] - 1) as f32);
    let y = (uv.y * texture.size[1] as f32 - 0.5).clamp(0.0, (texture.size[1] - 1) as f32);
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(texture.size[0] - 1);
    let y1 = (y0 + 1).min(texture.size[1] - 1);
    let at = |x: usize, y: usize| texture.pixels[y * texture.size[0] + x].to_array();
    let a = at(x0, y0);
    let b = at(x1, y0);
    let c = at(x0, y1);
    let d = at(x1, y1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    std::array::from_fn(|i| {
        (a[i] as f32 * (1.0 - fx) + b[i] as f32 * fx) * (1.0 - fy)
            + (c[i] as f32 * (1.0 - fx) + d[i] as f32 * fx) * fy
    })
}
