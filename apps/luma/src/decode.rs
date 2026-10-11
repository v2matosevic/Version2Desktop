use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use serde::Serialize;
use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_PIXELS: u64 = 24_000_000;
pub const MAX_SIDE: u32 = 16_384;
pub const MAX_FRAME_BYTES: usize = 96 * 1024 * 1024;
pub const MAX_FRAMES: usize = 200;

#[derive(Debug)]
pub struct Frame {
    pub rgba: Vec<u8>,
    pub delay_ms: u32,
}
#[derive(Debug, Default)]
pub struct Photo {
    pub width: u32,
    pub height: u32,
    pub preview_source: Option<[u32; 2]>,
    pub format: String,
    pub file_bytes: u64,
    pub frames: Vec<Frame>,
    pub warnings: Vec<String>,
    /// Camera and exposure facts read from the file's own metadata, when present.
    pub exif: Option<crate::exif::Exif>,
    /// Who decoded it: Luma's built-in decoders or an installed Windows codec.
    pub decoder: String,
    /// The EXIF orientation (2 to 8) Luma applied to show the photo upright.
    pub applied_orientation: Option<u16>,
}
impl Photo {
    pub fn byte_len(&self) -> usize {
        self.frames.iter().map(|f| f.rgba.len()).sum()
    }
}

#[derive(Clone)]
pub struct Cancel {
    generation: Arc<AtomicU64>,
    expected: u64,
}
impl Cancel {
    pub fn new(generation: Arc<AtomicU64>, expected: u64) -> Self {
        Self {
            generation,
            expected,
        }
    }
    pub fn never() -> Self {
        Self::new(Arc::new(AtomicU64::new(0)), 0)
    }
    pub fn check(&self) -> Result<(), String> {
        if self.generation.load(Ordering::Acquire) != self.expected {
            Err("Cancelled by newer navigation".into())
        } else {
            Ok(())
        }
    }
}
struct CancelReader {
    file: File,
    cancel: Cancel,
    remaining: u64,
}
impl Read for CancelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.cancel.check().map_err(std::io::Error::other)?;
        let count = buf.len().min(self.remaining as usize);
        if count == 0 {
            return Ok(0);
        }
        let n = self.file.read(&mut buf[..count])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}
impl Seek for CancelReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.cancel.check().map_err(std::io::Error::other)?;
        let position = self.file.seek(pos)?;
        self.remaining = MAX_FILE_BYTES.saturating_sub(position);
        Ok(position)
    }
}
pub fn checked_dimensions(width: u32, height: u32) -> Result<usize, String> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || pixels > MAX_PIXELS {
        return Err(format!(
            "Image {width} × {height} exceeds Luma's safe decode limit (24 megapixels, 16384 per side)."
        ));
    }
    Ok(pixels as usize * 4)
}
fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(192 * 1024 * 1024);
    limits
}
pub fn decode(path: &Path, cancel: &Cancel) -> Result<Photo, String> {
    cancel.check()?;
    let file = File::open(path).map_err(|e| format!("Cannot read image: {e}"))?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("Choose a regular image file.".into());
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err("Image exceeds the 64 MiB compressed-file limit.".into());
    }
    // HEIC, camera RAW, JPEG XR and JPEG XL are decoded by whatever Windows codec is installed.
    if crate::wic::Family::from_path(path).is_some() {
        return match crate::wic::decode(path, &file, metadata.len(), cancel) {
            crate::wic::Outcome::Photo(photo) => Ok(*photo),
            crate::wic::Outcome::Unrecognised(message) | crate::wic::Outcome::Failed(message) => {
                Err(message)
            }
        };
    }
    let mut reader = BufReader::new(CancelReader {
        file,
        cancel: cancel.clone(),
        remaining: MAX_FILE_BYTES,
    });
    let mut signature = [0u8; 32];
    let count = reader.read(&mut signature).map_err(|e| e.to_string())?;
    reader.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let avif = count >= 16
        && &signature[4..8] == b"ftyp"
        && (signature[8..count]
            .windows(4)
            .any(|s| s == b"avif" || s == b"avis"));
    if avif {
        return decode_avif(reader, metadata.len(), cancel);
    }
    let mut image_reader = ImageReader::new(reader)
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    image_reader.limits(limits());
    let Some(format) = image_reader.format() else {
        // Not something Luma decodes itself. A file with the wrong extension (a HEIC saved as
        // .jpg, say) may still be an image an installed Windows codec can read.
        let second = File::open(path).map_err(|e| format!("Cannot read image: {e}"))?;
        return match crate::wic::decode(path, &second, metadata.len(), cancel) {
            crate::wic::Outcome::Photo(photo) => Ok(*photo),
            crate::wic::Outcome::Unrecognised(_) => {
                Err("This file is not a recognized image, or its codec is unavailable.".into())
            }
            crate::wic::Outcome::Failed(message) => Err(message),
        };
    };
    let mut decoder = image_reader
        .into_decoder()
        .map_err(|e| format!("Cannot decode image: {e}"))?;
    let (width, height) = decoder.dimensions();
    let exceeds_budget = checked_dimensions(width, height).is_err();
    if exceeds_budget && format != ImageFormat::Jpeg {
        checked_dimensions(width, height)?;
    }
    let mut warnings = Vec::new();
    let features = inspect_features(path, format, cancel)?;
    warnings.extend(features.warnings);
    if matches!(
        decoder.color_type(),
        image::ColorType::L16
            | image::ColorType::La16
            | image::ColorType::Rgb16
            | image::ColorType::Rgba16
            | image::ColorType::Rgb32F
            | image::ColorType::Rgba32F
    ) {
        warnings.push(
            "High bit depth is reduced to 8-bit SDR display; HDR tone mapping is unavailable."
                .into(),
        );
    }
    let orientation = decoder
        .orientation()
        .map_err(|e| format!("Invalid orientation metadata: {e}"))?;
    let exif = decoder
        .exif_metadata()
        .ok()
        .flatten()
        .as_deref()
        .and_then(crate::exif::parse);
    let applied_orientation = Some(u16::from(orientation.to_exif())).filter(|o| *o != 1);
    let icc = decoder
        .icc_profile()
        .map_err(|e| format!("Invalid color profile metadata: {e}"))?;
    if icc.as_ref().is_some_and(|p| p.len() > 4 * 1024 * 1024) {
        return Err("Embedded color profile exceeds 4 MiB.".into());
    }
    if exceeds_budget && format == ImageFormat::Jpeg {
        drop(decoder);
        let mut photo = decode_scaled_jpeg(
            path,
            metadata.len(),
            [width, height],
            orientation,
            icc.as_deref(),
            cancel,
        )?;
        photo.exif = exif;
        photo.applied_orientation = applied_orientation;
        return Ok(photo);
    }
    if format == ImageFormat::Gif {
        // Reopen after metadata inspection. image's GIF iterator composites disposal/transparency.
        drop(decoder);
        let mut gif = image::codecs::gif::GifDecoder::new(BufReader::new(CancelReader {
            file: File::open(path).map_err(|e| e.to_string())?,
            cancel: cancel.clone(),
            remaining: MAX_FILE_BYTES,
        }))
        .map_err(|e| e.to_string())?;
        gif.set_limits(limits()).map_err(|e| e.to_string())?;
        let frames = collect_frames(
            gif.into_frames(),
            "GIF",
            icc.as_deref(),
            &mut warnings,
            cancel,
        )?;
        warnings.push("GIF frame delays below 20 ms are clamped to 20 ms. Loop count is presented as continuous playback.".into());
        return Ok(Photo {
            width,
            height,
            format: "GIF".into(),
            file_bytes: metadata.len(),
            frames,
            warnings,
            decoder: BUILT_IN.into(),
            ..Photo::default()
        });
    }
    if features.animated && matches!(format, ImageFormat::Png | ImageFormat::WebP) {
        match decode_animation(path, format, icc.as_deref(), &mut warnings, cancel) {
            Ok(frames) => {
                warnings.push("Frame delays below 20 ms are clamped to 20 ms. Loop count is presented as continuous playback.".into());
                return Ok(Photo {
                    width,
                    height,
                    format: if format == ImageFormat::Png {
                        "APNG".into()
                    } else {
                        "WebP / animated".into()
                    },
                    file_bytes: metadata.len(),
                    frames,
                    warnings,
                    exif,
                    decoder: BUILT_IN.into(),
                    ..Photo::default()
                });
            }
            Err(e) => {
                // A newer navigation request ends this decode; any other failure degrades to
                // the still first frame the file always has, and says why.
                cancel.check()?;
                warnings.push(format!(
                    "The animation could not be played ({e}); its first frame is shown."
                ));
            }
        }
    }
    cancel.check()?;
    let mut image =
        DynamicImage::from_decoder(decoder).map_err(|e| format!("Invalid image data: {e}"))?;
    cancel.check()?;
    image.apply_orientation(orientation);
    let mut rgba = image.into_rgba8();
    apply_icc(&mut rgba, icc.as_deref(), &mut warnings)?;
    cancel.check()?;
    Ok(Photo {
        width: rgba.width(),
        height: rgba.height(),
        format: format!("{format:?}"),
        file_bytes: metadata.len(),
        frames: vec![Frame {
            rgba: rgba.into_raw(),
            delay_ms: 0,
        }],
        warnings,
        exif,
        decoder: BUILT_IN.into(),
        applied_orientation,
        ..Photo::default()
    })
}
const BUILT_IN: &str = "Luma built-in decoder";

/// Composite animation frames under the same bounds as every other decode: at most 200 frames,
/// 96 MiB of RGBA in total, each frame inside the pixel budget, cancellable between frames.
fn collect_frames(
    frames: image::Frames<'_>,
    label: &str,
    icc: Option<&[u8]>,
    warnings: &mut Vec<String>,
    cancel: &Cancel,
) -> Result<Vec<Frame>, String> {
    let mut out = Vec::new();
    let mut bytes = 0usize;
    for frame in frames {
        cancel.check()?;
        if out.len() >= MAX_FRAMES {
            return Err("Animation exceeds the 200-frame limit. No frames were displayed.".into());
        }
        let frame = frame.map_err(|e| format!("Invalid {label} frame: {e}"))?;
        let (num, den) = frame.delay().numer_denom_ms();
        let mut rgba = frame.into_buffer();
        checked_dimensions(rgba.width(), rgba.height())?;
        bytes = bytes
            .checked_add(rgba.len())
            .ok_or("Animation size overflow")?;
        if bytes > MAX_FRAME_BYTES {
            return Err("Animation exceeds the 96 MiB total decoded-frame budget.".into());
        }
        apply_icc(&mut rgba, icc, warnings)?;
        out.push(Frame {
            rgba: rgba.into_raw(),
            delay_ms: ((num as u64 / u64::from(den.max(1))) as u32).clamp(20, 60_000),
        });
    }
    if out.is_empty() {
        return Err(format!("{label} contains no decodable frames."));
    }
    Ok(out)
}

fn decode_animation(
    path: &Path,
    format: ImageFormat,
    icc: Option<&[u8]>,
    warnings: &mut Vec<String>,
    cancel: &Cancel,
) -> Result<Vec<Frame>, String> {
    let reader = BufReader::new(CancelReader {
        file: File::open(path).map_err(|e| e.to_string())?,
        cancel: cancel.clone(),
        remaining: MAX_FILE_BYTES,
    });
    match format {
        ImageFormat::Png => {
            let mut decoder =
                image::codecs::png::PngDecoder::new(reader).map_err(|e| e.to_string())?;
            decoder.set_limits(limits()).map_err(|e| e.to_string())?;
            if !decoder.is_apng().map_err(|e| e.to_string())? {
                return Err("the file has no animation data".into());
            }
            let apng = decoder.apng().map_err(|e| e.to_string())?;
            collect_frames(apng.into_frames(), "APNG", icc, warnings, cancel)
        }
        ImageFormat::WebP => {
            let mut decoder =
                image::codecs::webp::WebPDecoder::new(reader).map_err(|e| e.to_string())?;
            decoder.set_limits(limits()).map_err(|e| e.to_string())?;
            collect_frames(decoder.into_frames(), "WebP", icc, warnings, cancel)
        }
        _ => Err("this format has no animation support".into()),
    }
}
fn decode_scaled_jpeg(
    path: &Path,
    file_bytes: u64,
    source: [u32; 2],
    orientation: image::metadata::Orientation,
    icc: Option<&[u8]>,
    cancel: &Cancel,
) -> Result<Photo, String> {
    if source[0] > MAX_SIDE
        || source[1] > MAX_SIDE
        || u64::from(source[0]) * u64::from(source[1]) > 144_000_000
    {
        return Err(
            "Large JPEG exceeds the 144 MP / 16384-pixel scaled-preview source limit.".into(),
        );
    }
    let mut decoder = jpeg_decoder::Decoder::new(BufReader::new(CancelReader {
        file: File::open(path).map_err(|e| e.to_string())?,
        cancel: cancel.clone(),
        remaining: MAX_FILE_BYTES,
    }));
    decoder.set_max_decoding_buffer_size(72_000_000);
    decoder
        .read_info()
        .map_err(|e| format!("Cannot inspect large JPEG: {e}"))?;
    let info = decoder.info().ok_or("JPEG has no image metadata")?;
    if info.coding_process != jpeg_decoder::CodingProcess::DctSequential {
        return Err("JPEG exceeds 24 MP and uses progressive or lossless encoding. A bounded scaled preview is only available for baseline JPEG.".into());
    }
    if !matches!(
        info.pixel_format,
        jpeg_decoder::PixelFormat::RGB24 | jpeg_decoder::PixelFormat::L8
    ) {
        return Err(
            "JPEG exceeds 24 MP and uses CMYK/high bit depth. Its scaled preview is unavailable."
                .into(),
        );
    }
    let mut requested = 4096u16;
    let (width, height) = loop {
        let size = decoder
            .scale(requested, requested)
            .map_err(|e| e.to_string())?;
        if checked_dimensions(u32::from(size.0), u32::from(size.1)).is_ok() {
            break size;
        }
        requested /= 2;
        if requested < 256 {
            return Err("JPEG preview dimensions exceed the safe limit.".into());
        }
    };
    cancel.check()?;
    let pixels = decoder
        .decode()
        .map_err(|e| format!("Cannot decode JPEG preview: {e}"))?;
    cancel.check()?;
    let image = if info.pixel_format == jpeg_decoder::PixelFormat::RGB24 {
        DynamicImage::ImageRgb8(
            image::RgbImage::from_raw(u32::from(width), u32::from(height), pixels)
                .ok_or("Invalid preview buffer size")?,
        )
    } else {
        DynamicImage::ImageLuma8(
            image::GrayImage::from_raw(u32::from(width), u32::from(height), pixels)
                .ok_or("Invalid preview buffer size")?,
        )
    };
    let mut image = image;
    image.apply_orientation(orientation);
    let mut rgba = image.into_rgba8();
    let mut warnings = vec![format!(
        "Scaled JPEG preview of {} × {} source pixels. Actual-size mode shows preview pixels; full-resolution viewing is unavailable for this file.",
        source[0], source[1]
    )];
    apply_icc(&mut rgba, icc, &mut warnings)?;
    Ok(Photo {
        width: rgba.width(),
        height: rgba.height(),
        preview_source: Some(source),
        format: "JPEG / scaled preview".into(),
        file_bytes,
        frames: vec![Frame {
            rgba: rgba.into_raw(),
            delay_ms: 0,
        }],
        warnings,
        decoder: BUILT_IN.into(),
        ..Photo::default()
    })
}
pub(crate) fn apply_icc(
    rgba: &mut image::RgbaImage,
    icc: Option<&[u8]>,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    if let Some(icc) = icc {
        let profile = moxcms::ColorProfile::new_from_slice(icc)
            .map_err(|e| format!("Cannot parse embedded ICC profile: {e}"))?;
        if profile.color_space != moxcms::DataColorSpace::Rgb {
            warnings.push(
                "Non-RGB ICC profiles are not applied after the decoder's RGB conversion.".into(),
            );
            return Ok(());
        }
        let transform = profile
            .create_transform_8bit(
                moxcms::Layout::Rgba,
                &moxcms::ColorProfile::new_srgb(),
                moxcms::Layout::Rgba,
                Default::default(),
            )
            .map_err(|e| format!("Unsupported ICC transform: {e}"))?;
        // Transform one scanline at a time; no second full-size bitmap.
        let stride = rgba.width() as usize * 4;
        let mut row = vec![0u8; stride];
        for source in rgba.as_mut().chunks_exact_mut(stride) {
            transform
                .transform(source, &mut row)
                .map_err(|e| e.to_string())?;
            source.copy_from_slice(&row);
        }
    }
    Ok(())
}

/// What a PNG or WebP container says about itself beyond what the pixel decoder reports.
struct Features {
    /// An animation chunk is present (APNG `acTL`, WebP `VP8X` animation flag).
    animated: bool,
    warnings: Vec<String>,
}

fn inspect_features(path: &Path, format: ImageFormat, cancel: &Cancel) -> Result<Features, String> {
    let mut animated = false;
    if format != ImageFormat::Png && format != ImageFormat::WebP {
        return Ok(Features {
            animated,
            warnings: vec![],
        });
    }
    let file = File::open(path).map_err(|e| e.to_string())?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    let mut reader = CancelReader {
        file,
        cancel: cancel.clone(),
        remaining: MAX_FILE_BYTES,
    };
    let mut warnings = Vec::new();
    if format == ImageFormat::Png {
        reader.seek(SeekFrom::Start(8)).map_err(|e| e.to_string())?;
        let mut offset = 8u64;
        while offset + 12 <= size {
            let mut header = [0u8; 8];
            reader.read_exact(&mut header).map_err(|e| e.to_string())?;
            let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as u64;
            if length > size - offset - 12 {
                break;
            }
            if &header[4..] == b"acTL" {
                animated = true;
            }
            if &header[4..] == b"cICP" && length == 4 {
                let mut cicp = [0u8; 4];
                reader.read_exact(&mut cicp).map_err(|e| e.to_string())?;
                if cicp[1] == 16 || cicp[1] == 18 {
                    warnings.push("HDR PNG transfer metadata is not tone-mapped.".into());
                }
            }
            offset += length + 12;
            reader
                .seek(SeekFrom::Start(offset))
                .map_err(|e| e.to_string())?;
        }
    } else {
        reader
            .seek(SeekFrom::Start(12))
            .map_err(|e| e.to_string())?;
        let mut offset = 12u64;
        while offset + 8 <= size {
            let mut header = [0u8; 8];
            reader.read_exact(&mut header).map_err(|e| e.to_string())?;
            let length = u32::from_le_bytes(header[4..].try_into().unwrap()) as u64;
            if length > size - offset - 8 {
                break;
            }
            if &header[..4] == b"VP8X" && length >= 1 {
                let mut flags = [0u8; 1];
                reader.read_exact(&mut flags).map_err(|e| e.to_string())?;
                if flags[0] & 2 != 0 {
                    animated = true;
                }
            }
            offset += 8 + length + (length % 2);
            reader
                .seek(SeekFrom::Start(offset))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(Features { animated, warnings })
}

/// Inspect actual BMFF boxes, never arbitrary byte matches. Every ispe must fit the limits.
fn inspect_avif(
    data: &[u8],
    depth: u8,
    boxes: &mut usize,
    dimensions: &mut Vec<(u32, u32)>,
) -> Result<(), String> {
    if depth > 12 {
        return Err("AVIF nesting exceeds the safe limit.".into());
    }
    let mut offset = 0usize;
    while offset < data.len() {
        *boxes += 1;
        if *boxes > 10_000 {
            return Err("AVIF has too many container boxes.".into());
        }
        if data.len() - offset < 8 {
            return Err("Truncated AVIF box.".into());
        }
        let mut size = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as u64;
        let kind = &data[offset + 4..offset + 8];
        let mut header = 8usize;
        if size == 1 {
            if data.len() - offset < 16 {
                return Err("Truncated extended AVIF box".into());
            }
            size = u64::from_be_bytes(data[offset + 8..offset + 16].try_into().unwrap());
            header = 16;
        }
        if size == 0 {
            size = (data.len() - offset) as u64;
        }
        if size < header as u64 || size > (data.len() - offset) as u64 {
            return Err("Invalid AVIF box size.".into());
        }
        let payload = &data[offset + header..offset + size as usize];
        if kind == b"ispe" {
            if payload.len() != 12 {
                return Err("Invalid AVIF image dimensions box.".into());
            }
            let w = u32::from_be_bytes(payload[4..8].try_into().unwrap());
            let h = u32::from_be_bytes(payload[8..12].try_into().unwrap());
            checked_dimensions(w, h)?;
            if w > 8192 || h > 8192 {
                return Err("AVIF exceeds its 8192-pixel codec limit.".into());
            }
            dimensions.push((w, h));
        } else if kind == b"irot" || kind == b"imir" || kind == b"clap" {
            return Err("This AVIF uses container rotation, mirroring or cropping, which this build cannot display correctly. Export it to PNG or JPEG.".into());
        } else if kind == b"meta" {
            if payload.len() < 4 {
                return Err("Truncated AVIF metadata".into());
            }
            inspect_avif(&payload[4..], depth + 1, boxes, dimensions)?;
        } else if kind == b"iprp" || kind == b"ipco" {
            inspect_avif(payload, depth + 1, boxes, dimensions)?;
        } else if kind == b"ftyp" && payload.windows(4).any(|b| b == b"avis") {
            return Err("Animated AVIF is unavailable in this build.".into());
        }
        offset += size as usize;
    }
    Ok(())
}
fn decode_avif<R: Read>(mut reader: R, file_bytes: u64, cancel: &Cancel) -> Result<Photo, String> {
    let mut data = Vec::new();
    reader
        .by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() as u64 > MAX_FILE_BYTES {
        return Err("AVIF exceeds the compressed-file limit.".into());
    }
    let mut dimensions = Vec::new();
    inspect_avif(&data, 0, &mut 0, &mut dimensions)?;
    if dimensions.is_empty() {
        return Err("AVIF has no safe dimension metadata.".into());
    }
    cancel.check()?;
    let decoded = avif_decode::Decoder::from_avif(&data)
        .map_err(|e| format!("Invalid AVIF: {e}"))?
        .to_image()
        .map_err(|e| format!("Cannot decode AVIF: {e}"))?;
    cancel.check()?;
    let (width, height, rgba) = match decoded {
        avif_decode::Image::Rgb8(img) => (
            img.width(),
            img.height(),
            img.pixels()
                .flat_map(|p| [p.r, p.g, p.b, 255])
                .collect::<Vec<u8>>(),
        ),
        avif_decode::Image::Rgba8(img) => (
            img.width(),
            img.height(),
            img.pixels().flat_map(|p| [p.r, p.g, p.b, p.a]).collect(),
        ),
        avif_decode::Image::Rgb16(img) => (
            img.width(),
            img.height(),
            img.pixels()
                .flat_map(|p| [(p.r >> 8) as u8, (p.g >> 8) as u8, (p.b >> 8) as u8, 255])
                .collect(),
        ),
        avif_decode::Image::Rgba16(img) => (
            img.width(),
            img.height(),
            img.pixels()
                .flat_map(|p| {
                    [
                        (p.r >> 8) as u8,
                        (p.g >> 8) as u8,
                        (p.b >> 8) as u8,
                        (p.a >> 8) as u8,
                    ]
                })
                .collect(),
        ),
        avif_decode::Image::Gray8(img) => (
            img.width(),
            img.height(),
            img.pixels()
                .flat_map(|p| [p.value(), p.value(), p.value(), 255])
                .collect(),
        ),
        avif_decode::Image::Gray16(img) => (
            img.width(),
            img.height(),
            img.pixels()
                .flat_map(|p| {
                    [
                        (p.value() >> 8) as u8,
                        (p.value() >> 8) as u8,
                        (p.value() >> 8) as u8,
                        255,
                    ]
                })
                .collect(),
        ),
    };
    let expected = checked_dimensions(width as u32, height as u32)?;
    if rgba.len() != expected {
        return Err("AVIF color/alpha dimensions are inconsistent.".into());
    }
    Ok(Photo {
        width: width as u32,
        height: height as u32,
        format: "AVIF / libaom".into(),
        file_bytes,
        frames: vec![Frame { rgba, delay_ms: 0 }],
        warnings: vec!["AVIF is shown in 8-bit SDR. Embedded ICC, HDR tone mapping, grids and container rotation/mirroring are unavailable.".into()],
        decoder: "libaom (built in)".into(),
        ..Photo::default()
    })
}

#[derive(Serialize)]
pub struct SmokeResult {
    pub path: String,
    /// Who actually decoded the file (a Windows codec is named as such); on failure, the
    /// built-in decoders that were tried.
    pub decoder: String,
    pub format: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub preview_source: Option<[u32; 2]>,
    pub frames: usize,
    pub rgba_bytes: usize,
    pub decode_ms: f64,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}
const SMOKE_BUILT_IN: &str = "image 0.25.10 / avif-decode 1.0.2 / libaom";

pub fn smoke(path: &Path) -> SmokeResult {
    let start = std::time::Instant::now();
    let result = decode(path, &Cancel::never());
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    match result {
        Ok(p) => SmokeResult {
            path: path.display().to_string(),
            decoder: if p.decoder.is_empty() {
                SMOKE_BUILT_IN.into()
            } else {
                p.decoder
            },
            format: Some(p.format),
            width: Some(p.width),
            height: Some(p.height),
            preview_source: p.preview_source,
            frames: p.frames.len(),
            rgba_bytes: p.frames.iter().map(|f| f.rgba.len()).sum(),
            decode_ms: ms,
            warnings: p.warnings,
            error: None,
        },
        Err(e) => SmokeResult {
            path: path.display().to_string(),
            decoder: SMOKE_BUILT_IN.into(),
            format: None,
            width: None,
            height: None,
            preview_source: None,
            frames: 0,
            rgba_bytes: 0,
            decode_ms: ms,
            warnings: vec![],
            error: Some(e),
        },
    }
}
