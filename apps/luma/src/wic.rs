//! Windows Imaging Component (WIC) bridge for formats Luma does not decode itself: HEIC/HEIF,
//! camera RAW, JPEG XR and JPEG XL. These are decoded by whatever codec Windows has installed.
//! Luma bundles no codec, downloads nothing, and never installs anything: a missing codec is
//! reported as such, with the exact Windows error when a codec exists but cannot read the file.
//!
//! Safety contract, in order: the compressed file is size-checked by the caller; the decoder's
//! reported dimensions are checked against Luma's pixel budget before any pixel is copied; the
//! output buffer is allocated fallibly; pixels are copied in bands so a newer navigation request
//! can cancel between them. Codecs run in this process, so a crashing third-party codec would end
//! Luma; that limit is documented rather than hidden.
#![allow(unsafe_code)]
use crate::{
    bmff,
    decode::{Cancel, Frame, Photo, apply_icc, checked_dimensions},
    exif,
};
use std::{fs::File, io::Read, os::windows::io::AsRawHandle, path::Path};
use windows::{
    Win32::{
        Foundation::{WINCODEC_ERR_COMPONENTNOTFOUND, WINCODEC_ERR_UNKNOWNIMAGEFORMAT},
        Graphics::Imaging::{
            CLSID_WICImagingFactory, GUID_ContainerFormatAdng, GUID_ContainerFormatHeif,
            GUID_ContainerFormatRaw, GUID_ContainerFormatWmp, GUID_WICPixelFormat32bppBGRA,
            IWICBitmapDecoder, IWICBitmapDecoderInfo, IWICBitmapFrameDecode, IWICImagingFactory,
            IWICPixelFormatInfo, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom,
            WICColorContextProfile, WICComponentEnumerateDefault, WICDecodeMetadataCacheOnDemand,
            WICDecoder, WICRect,
        },
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
    },
    core::{GUID, Interface},
};

/// The codec families Luma hands to Windows, by what a person calls them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Family {
    Heif,
    /// Adobe DNG. Judged on its own because a DNG sample proves nothing about CR2, NEF or ARW.
    Dng,
    /// Camera RAW other than DNG (CR2, NEF, ARW, ...): the Raw Image Extension or a vendor codec.
    Raw,
    JpegXr,
    JpegXl,
}

/// RAW extensions other than ".dng", which is its own family.
const RAW_EXTENSIONS: &[&str] = &[
    "raw", "cr2", "cr3", "crw", "nef", "nrw", "arw", "sr2", "srf", "orf", "rw2", "rwl", "raf",
    "pef", "srw", "3fr", "iiq", "mrw", "erf", "kdc", "mef", "x3f",
];

impl Family {
    pub const ALL: [Family; 5] = [
        Family::Heif,
        Family::Dng,
        Family::Raw,
        Family::JpegXr,
        Family::JpegXl,
    ];

    pub fn from_extension(ext: &str) -> Option<Family> {
        let ext = ext.to_ascii_lowercase();
        match ext.as_str() {
            "heic" | "heif" | "hif" => Some(Family::Heif),
            "dng" => Some(Family::Dng),
            "jxr" | "wdp" | "hdp" => Some(Family::JpegXr),
            "jxl" => Some(Family::JpegXl),
            e if RAW_EXTENSIONS.contains(&e) => Some(Family::Raw),
            _ => None,
        }
    }
    pub fn from_path(path: &Path) -> Option<Family> {
        Self::from_extension(path.extension()?.to_str()?)
    }
    pub fn label(self) -> &'static str {
        match self {
            Family::Heif => "HEIC / HEIF",
            Family::Dng => "DNG",
            Family::Raw => "Camera RAW",
            Family::JpegXr => "JPEG XR",
            Family::JpegXl => "JPEG XL",
        }
    }
    pub fn extensions(self) -> &'static str {
        match self {
            Family::Heif => ".heic .heif .hif",
            Family::Dng => ".dng",
            Family::Raw => ".cr2 .cr3 .nef .arw .orf .rw2 .raf and more, not .dng",
            Family::JpegXr => ".jxr .wdp .hdp",
            Family::JpegXl => ".jxl",
        }
    }
    /// What a person can do about a missing codec. Luma itself never installs one.
    pub fn how_to_get(self) -> &'static str {
        match self {
            Family::Heif => {
                "Install \"HEIF Image Extensions\" from Microsoft Store. HEVC-compressed HEIC (iPhone photos) also needs \"HEVC Video Extensions\"."
            }
            Family::Dng => {
                "Install \"Raw Image Extension\" from Microsoft Store. Windows' built-in DNG decoder may read some DNG files without it."
            }
            Family::Raw => {
                "Install \"Raw Image Extension\" from Microsoft Store, or your camera maker's Windows RAW codec."
            }
            Family::JpegXr => "JPEG XR is part of Windows; no extra codec is normally needed.",
            Family::JpegXl => "Install a Windows JPEG XL codec (for example from Microsoft Store).",
        }
    }
}

/// What a decoder actually is, judged from the decoder that handled the file (its container
/// format and its name), never from the file's extension: a HEIC saved as ".jxr" is HEIC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Heif,
    /// Any camera RAW or DNG decoder: Microsoft's Raw Image Extension, Windows' built-in DNG
    /// decoder, or a vendor's.
    Raw,
    JpegXr,
    JpegXl,
    Other,
}

fn kind_of(container: &GUID, decoder_name: &str) -> Kind {
    let name = decoder_name.to_ascii_lowercase();
    if *container == GUID_ContainerFormatHeif || name.contains("heif") || name.contains("heic") {
        Kind::Heif
    } else if *container == GUID_ContainerFormatRaw
        || *container == GUID_ContainerFormatAdng
        || name.contains("raw")
        || name.contains("dng")
    {
        Kind::Raw
    } else if *container == GUID_ContainerFormatWmp
        || name.contains("hd photo")
        || name.contains("jpeg xr")
        || name.contains("jpegxr")
    {
        Kind::JpegXr
    } else if name.contains("jpeg xl") || name.contains("jxl") {
        Kind::JpegXl
    } else {
        Kind::Other
    }
}

/// The format name shown to people. The extension only adds the camera brand to a RAW label
/// ("RAW (NEF)") and only when it is itself a RAW extension; it never decides the format.
fn format_name(kind: Kind, decoder_name: &str, ext: &str) -> String {
    match kind {
        Kind::Heif => "HEIC".into(),
        Kind::JpegXr => "JPEG XR".into(),
        Kind::JpegXl => "JPEG XL".into(),
        Kind::Raw if matches!(Family::from_extension(ext), Some(Family::Raw | Family::Dng)) => {
            format!("RAW ({})", ext.to_ascii_uppercase())
        }
        Kind::Raw => "RAW".into(),
        Kind::Other => {
            let cleaned = decoder_name
                .trim_start_matches("Microsoft ")
                .trim_end_matches(" Decoder")
                .trim();
            if cleaned.is_empty() {
                "Image".into()
            } else {
                cleaned.to_owned()
            }
        }
    }
}

/// True for any extension Luma will try through Windows.
pub fn is_windows_codec_extension(ext: &str) -> bool {
    Family::from_extension(ext).is_some()
}

struct Com(bool);
impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}
thread_local! {
    // Multithreaded apartment per thread that ever decodes; a thread already in another
    // apartment (the UI thread) keeps working with it, which is why the result is not an error.
    static COM: Com = Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok());
}
fn factory() -> Result<IWICImagingFactory, String> {
    COM.with(|_| {});
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| format!("Windows Imaging Component is not available: {e}"))
}

/// One installed WIC decoder.
#[derive(Clone, Debug)]
pub struct DecoderEntry {
    pub name: String,
    /// Lower-case, without the dot.
    pub extensions: Vec<String>,
    pub container: GUID,
}

fn wide(read: impl Fn(&mut [u16], *mut u32) -> windows::core::Result<()>) -> String {
    // WIC's string getters report the length they need only alongside a real buffer, so start
    // with a generous one and grow once if a (long) extension list needs more.
    let mut buffer = vec![0u16; 512];
    let mut needed = 0u32;
    if read(&mut buffer, &mut needed).is_err() {
        return String::new();
    }
    if needed as usize > buffer.len() {
        if needed > 16 * 1024 {
            return String::new();
        }
        buffer = vec![0u16; needed as usize];
        if read(&mut buffer, &mut needed).is_err() {
            return String::new();
        }
    }
    let end = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// Every decoder WIC knows right now, built in or from an installed extension. This is the
/// authoritative "is a codec registered" answer; it is not proof a particular file will decode.
pub fn decoders() -> Result<Vec<DecoderEntry>, String> {
    let factory = factory()?;
    let list = unsafe {
        factory
            .CreateComponentEnumerator(WICDecoder.0 as u32, WICComponentEnumerateDefault.0 as u32)
    }
    .map_err(|e| format!("Cannot list Windows image codecs: {e}"))?;
    let mut out = Vec::new();
    for _ in 0..256 {
        let mut item = [None];
        let mut fetched = 0u32;
        let result = unsafe { list.Next(&mut item, Some(&mut fetched)) };
        if result.is_err() || fetched == 0 {
            break;
        }
        let Some(unknown) = item[0].take() else { break };
        let Ok(info) = unknown.cast::<IWICBitmapDecoderInfo>() else {
            continue;
        };
        let Ok(container) = (unsafe { info.GetContainerFormat() }) else {
            continue;
        };
        let name = wide(|buf, n| unsafe { info.GetFriendlyName(buf, n) });
        let extensions = wide(|buf, n| unsafe { info.GetFileExtensions(buf, n) })
            .split(',')
            .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        out.push(DecoderEntry {
            name,
            extensions,
            container,
        });
    }
    Ok(out)
}

pub fn decoder_for(family: Family, all: &[DecoderEntry]) -> Option<&DecoderEntry> {
    if family == Family::Dng {
        // Only a listing: Windows' built-in DNG decoder if registered, else any codec that lists
        // ".dng". Which of them really decodes a file is decided by Windows and reported from
        // the decode itself, never from this list.
        return all
            .iter()
            .find(|d| d.container == GUID_ContainerFormatAdng)
            .or_else(|| all.iter().find(|d| d.extensions.iter().any(|e| e == "dng")));
    }
    all.iter().find(|d| match family {
        Family::Heif => {
            d.container == GUID_ContainerFormatHeif || d.extensions.iter().any(|e| e == "heic")
        }
        // Handled above: the DNG decoder and the Raw Image Extension have different evidence
        // behind them, so a working DNG decoder never makes "camera RAW" look supported.
        Family::Dng => false,
        Family::Raw => {
            d.container == GUID_ContainerFormatRaw
                || d.extensions
                    .iter()
                    .any(|e| e == "cr2" || e == "nef" || e == "arw")
        }
        Family::JpegXr => {
            d.container == GUID_ContainerFormatWmp
                || d.extensions.iter().any(|e| e == "jxr" || e == "wdp")
        }
        Family::JpegXl => {
            d.extensions.iter().any(|e| e == "jxl")
                || d.name.to_ascii_lowercase().contains("jpeg xl")
        }
    })
}

fn hr(e: &windows::core::Error) -> String {
    format!("0x{:08X}", e.code().0 as u32)
}

/// The message for a family with no installed decoder. `reported` is the Windows error code.
pub fn missing_codec_message(family: Family, reported: &str) -> String {
    format!(
        "{} needs a Windows codec, and none is installed on this PC. Luma does not install or download codecs. {} (Windows reported {reported}.)",
        family.label(),
        family.how_to_get(),
    )
}

/// Where in decoding a Windows error arose. The same code means different things: "no codec
/// recognised this file" when opening, "the codec cannot handle its contents" while decoding.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Open,
    Decode,
}

/// A plain-language message for a failure, naming the Windows error code.
fn explain(family: Option<Family>, stage: Stage, e: &windows::core::Error) -> String {
    let code = e.code();
    let not_found =
        code == WINCODEC_ERR_COMPONENTNOTFOUND || code == WINCODEC_ERR_UNKNOWNIMAGEFORMAT;
    let installed = family.and_then(|f| {
        let all = decoders().ok()?;
        decoder_for(f, &all).map(|d| d.name.clone())
    });
    match (family, installed) {
        (Some(f), None) => missing_codec_message(f, &hr(e)),
        (Some(f), Some(name)) if stage == Stage::Open && not_found => format!(
            "No installed Windows codec recognised this file as {}. It may be damaged, or not really a {} file. (The \"{name}\" codec is installed; Windows reported {}.)",
            f.label(),
            f.label(),
            hr(e)
        ),
        (Some(Family::Heif), Some(name)) => format!(
            "Windows has a HEIF decoder (\"{name}\") but could not decode this image. Two usual causes: the file is damaged, or it uses HEVC compression (iPhone-style HEIC), which needs \"HEVC Video Extensions\" from Microsoft Store; Luma does not install it. (Windows reported {}.)",
            hr(e)
        ),
        (Some(f), Some(name)) => format!(
            "The Windows {} codec (\"{name}\") could not decode this file. It may be damaged, or newer than the installed codec understands. Open a JPEG or PNG export instead. (Windows reported {}.)",
            f.label(),
            hr(e)
        ),
        (None, _) => format!(
            "This file is not a recognized image, and no installed Windows codec could read it. (Windows reported {}.)",
            hr(e)
        ),
    }
}

/// What came out of a decoder before it becomes a `Photo`.
struct Decoded {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    decoder: String,
    container: GUID,
    /// The decoder implementation's class id (zero if Windows would not say).
    clsid: GUID,
    kind: Kind,
    stored_bits_exceed_8: bool,
    profile: Option<Vec<u8>>,
}

fn pixel_depth_exceeds_8(factory: &IWICImagingFactory, format: &GUID) -> bool {
    let Ok(info) = (unsafe { factory.CreateComponentInfo(format) }) else {
        return false;
    };
    let Ok(info) = info.cast::<IWICPixelFormatInfo>() else {
        return false;
    };
    let (Ok(bits), Ok(channels)) = (unsafe { info.GetBitsPerPixel() }, unsafe {
        info.GetChannelCount()
    }) else {
        return false;
    };
    channels > 0 && bits / channels > 8
}

fn embedded_profile(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
) -> Option<Vec<u8>> {
    let mut count = 0u32;
    unsafe { frame.GetColorContexts(&mut [], &mut count) }.ok()?;
    if count == 0 || count > 4 {
        return None;
    }
    let mut contexts: Vec<_> = (0..count)
        .map(|_| unsafe { factory.CreateColorContext() }.ok())
        .collect();
    let mut actual = 0u32;
    unsafe { frame.GetColorContexts(&mut contexts, &mut actual) }.ok()?;
    for context in contexts.iter().flatten() {
        if unsafe { context.GetType() }.ok()? != WICColorContextProfile {
            continue;
        }
        let mut length = 0u32;
        unsafe { context.GetProfileBytes(&mut [], &mut length) }.ok()?;
        if length == 0 || length > 4 * 1024 * 1024 {
            return None;
        }
        let mut bytes = vec![0u8; length as usize];
        unsafe { context.GetProfileBytes(&mut bytes, &mut length) }.ok()?;
        return Some(bytes);
    }
    None
}

fn decode_frame(
    factory: &IWICImagingFactory,
    decoder: &IWICBitmapDecoder,
    family: Option<Family>,
    cancel: &Cancel,
) -> Result<Decoded, String> {
    cancel.check()?;
    let info = unsafe { decoder.GetDecoderInfo() }.ok();
    let name = info
        .as_ref()
        .map(|i| wide(|buf, n| unsafe { i.GetFriendlyName(buf, n) }))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Windows image codec".into());
    let container = unsafe { decoder.GetContainerFormat() }.unwrap_or_default();
    // The decoder's own class id: a container format is shared by whoever implements it, so only
    // this says which decoder actually read the file.
    let clsid = info
        .as_ref()
        .and_then(|i| unsafe { i.GetCLSID() }.ok())
        .unwrap_or_default();
    let frame = unsafe { decoder.GetFrame(0) }.map_err(|e| explain(family, Stage::Decode, &e))?;
    let (mut width, mut height) = (0u32, 0u32);
    unsafe { frame.GetSize(&mut width, &mut height) }
        .map_err(|e| explain(family, Stage::Decode, &e))?;
    // The pixel budget is enforced on the size the codec reports, before anything is allocated
    // or decoded. A hostile header cannot ask for gigabytes.
    let expected = checked_dimensions(width, height)?;
    let source_format =
        unsafe { frame.GetPixelFormat() }.map_err(|e| explain(family, Stage::Decode, &e))?;
    let stored_bits_exceed_8 = pixel_depth_exceeds_8(factory, &source_format);
    let converter = unsafe { factory.CreateFormatConverter() }
        .map_err(|e| format!("Cannot prepare the Windows pixel converter: {e}"))?;
    let can_convert =
        unsafe { converter.CanConvert(&source_format, &GUID_WICPixelFormat32bppBGRA) }
            .map(|b| b.as_bool())
            .unwrap_or(false);
    if !can_convert {
        return Err("Windows cannot convert this image's pixel format for display.".into());
    }
    unsafe {
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
    }
    .map_err(|e| explain(family, Stage::Decode, &e))?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(expected)
        .map_err(|_| "Not enough memory to decode this image.".to_string())?;
    rgba.resize(expected, 0);
    let stride = width * 4;
    // About 2 MiB per band: a cancellation point every few milliseconds on a large photo.
    let rows_per_band = ((2 * 1024 * 1024) / stride.max(1)).clamp(1, height);
    let mut y = 0u32;
    while y < height {
        cancel.check()?;
        let rows = rows_per_band.min(height - y);
        let rect = WICRect {
            X: 0,
            Y: y as i32,
            Width: width as i32,
            Height: rows as i32,
        };
        let start = y as usize * stride as usize;
        let end = start + rows as usize * stride as usize;
        unsafe { converter.CopyPixels(&rect, stride, &mut rgba[start..end]) }
            .map_err(|e| explain(family, Stage::Decode, &e))?;
        y += rows;
    }
    // WIC hands back BGRA; the rest of Luma works in RGBA.
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Ok(Decoded {
        width,
        height,
        rgba,
        kind: kind_of(&container, &name),
        decoder: name,
        container,
        clsid,
        stored_bits_exceed_8,
        profile: embedded_profile(factory, &frame),
    })
}

/// Result of asking Windows to decode a file.
pub enum Outcome {
    /// No installed codec recognised the file at all.
    Unrecognised(String),
    Photo(Box<Photo>),
    Failed(String),
}

/// Decode `path` through Windows. `family` comes from the extension and only shapes messages:
/// Windows chooses the decoder by file content.
pub fn decode(path: &Path, file: &File, file_bytes: u64, cancel: &Cancel) -> Outcome {
    let family = Family::from_path(path);
    let factory = match factory() {
        Ok(f) => f,
        Err(e) => return Outcome::Failed(e),
    };
    // The handle stays open (and owned by `file`) for as long as the decoder exists.
    let handle = file.as_raw_handle() as usize;
    let decoder = match unsafe {
        factory.CreateDecoderFromFileHandle(
            handle,
            std::ptr::null(),
            WICDecodeMetadataCacheOnDemand,
        )
    } {
        Ok(d) => d,
        Err(e) => {
            let message = explain(family, Stage::Open, &e);
            return if e.code() == WINCODEC_ERR_COMPONENTNOTFOUND
                || e.code() == WINCODEC_ERR_UNKNOWNIMAGEFORMAT
            {
                Outcome::Unrecognised(message)
            } else {
                Outcome::Failed(message)
            };
        }
    };
    let decoded = match decode_frame(&factory, &decoder, family, cancel) {
        Ok(d) => d,
        Err(e) => return Outcome::Failed(e),
    };
    match finish(path, decoded, file_bytes, cancel) {
        Ok(p) => Outcome::Photo(Box::new(p)),
        Err(e) => Outcome::Failed(e),
    }
}

fn finish(
    path: &Path,
    decoded: Decoded,
    file_bytes: u64,
    cancel: &Cancel,
) -> Result<Photo, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_owned();
    let mut warnings = Vec::new();
    let Decoded {
        mut width,
        mut height,
        mut rgba,
        decoder,
        container,
        clsid,
        kind,
        stored_bits_exceed_8,
        profile,
    } = decoded;
    if stored_bits_exceed_8 {
        warnings.push(
            "High bit depth or HDR data is reduced to 8-bit SDR by Windows; Luma applies no HDR tone mapping."
                .into(),
        );
    }
    let mut exif_data: Option<exif::Exif> = None;
    let mut applied_orientation = None;
    let mut is_dng = false;
    if kind == Kind::Heif {
        // HEIF orientation lives in container transforms (irot, imir). The Windows HEIF decoder
        // applies them (tests/windows_codecs.rs proves it per transform with pixel fixtures), so
        // EXIF orientation is not applied on top of them. As a guard against a decoder that does
        // not, a quarter-turn file whose output still has the stored, un-turned size gets the
        // transforms applied here. A half turn, a mirror or a square picture leaves the output
        // size unchanged, so that guard cannot see those: they rely on the decoder alone.
        if let Ok(mut file) = File::open(path)
            && let Ok(info) = bmff::read(&mut file)
        {
            exif_data = info.exif.as_deref().and_then(exif::parse);
            if let Some((sw, sh)) = info.stored_size
                && info.swaps_axes()
                && sw != sh
                && (width, height) == (sw, sh)
            {
                let image = image::RgbaImage::from_raw(width, height, rgba)
                    .ok_or("Decoded pixel buffer has an unexpected size.")?;
                let image = apply_container_transforms(image, &info.transforms);
                (width, height) = (image.width(), image.height());
                rgba = image.into_raw();
                warnings.push(
                    "The Windows decoder returned this HEIC unrotated; Luma applied its container rotation."
                        .into(),
                );
            }
        }
    } else if kind == Kind::Raw {
        let (found, dng) = read_tiff(path);
        exif_data = found;
        is_dng = dng;
        if let Some(o) = exif_data
            .as_ref()
            .and_then(|e| e.orientation)
            .filter(|o| *o != 1)
        {
            match image::metadata::Orientation::from_exif(o as u8) {
                Some(orientation) if raw_orientation_is_proven(&clsid, &container, is_dng) => {
                    let image = image::RgbaImage::from_raw(width, height, rgba)
                        .ok_or("Decoded pixel buffer has an unexpected size.")?;
                    let mut image = image::DynamicImage::ImageRgba8(image);
                    image.apply_orientation(orientation);
                    let image = image.into_rgba8();
                    (width, height) = (image.width(), image.height());
                    rgba = image.into_raw();
                    applied_orientation = Some(o);
                }
                _ => warnings.push(format!(
                    "This RAW file carries orientation tag {o}. Luma has only seen Microsoft's Raw Image Decoder return a DNG un-turned, so for this decoder and file the picture is shown as the codec returned it."
                )),
            }
        }
    }
    cancel.check()?;
    if let Some(icc) = profile {
        let mut image = image::RgbaImage::from_raw(width, height, rgba)
            .ok_or("Decoded pixel buffer has an unexpected size.")?;
        // A profile Luma cannot use must not hide a photo Windows already decoded.
        let mut notes = Vec::new();
        match apply_icc(&mut image, Some(&icc), &mut notes) {
            Ok(()) => warnings.extend(notes),
            Err(e) => warnings.push(format!("Embedded colour profile was not applied: {e}")),
        }
        rgba = image.into_raw();
    }
    // A DNG is named from its own contents (a DNGVersion tag), not from its file name.
    let name = if is_dng {
        "RAW (DNG)".to_owned()
    } else {
        format_name(kind, &decoder, &ext)
    };
    Ok(Photo {
        width,
        height,
        preview_source: None,
        format: format!("{name} / Windows codec"),
        file_bytes,
        frames: vec![Frame { rgba, delay_ms: 0 }],
        warnings,
        exif: exif_data,
        decoder: format!("Windows codec: {decoder}"),
        applied_orientation,
    })
}

fn apply_container_transforms(
    mut image: image::RgbaImage,
    transforms: &[bmff::Transform],
) -> image::RgbaImage {
    for t in transforms {
        image = match t {
            // irot counts anticlockwise turns; imageops rotates clockwise.
            bmff::Transform::Rotate(1) => image::imageops::rotate270(&image),
            bmff::Transform::Rotate(2) => image::imageops::rotate180(&image),
            bmff::Transform::Rotate(3) => image::imageops::rotate90(&image),
            bmff::Transform::Rotate(_) => image,
            // imir: 0 exchanges top and bottom, 1 left and right (ISO/IEC 23008-12:2022, as in
            // libavif's avifImageMirror), which is also what the Windows decoder's pixels show.
            bmff::Transform::Mirror(0) => image::imageops::flip_vertical(&image),
            bmff::Transform::Mirror(_) => image::imageops::flip_horizontal(&image),
        };
    }
    image
}

/// How much of a RAW file is read for its optional EXIF. A RAW file is mostly sensor data, so
/// only a prefix is read; fields stored beyond it are left out rather than costing a whole-file read.
pub const EXIF_PREFIX_BYTES: u64 = 2 * 1024 * 1024;

/// EXIF from the start of a TIFF-structured RAW file (DNG, CR2, NEF, ARW, PEF, ...). Reads at
/// most `EXIF_PREFIX_BYTES`, whatever the file size.
pub fn tiff_prefix_exif<R: Read>(reader: R) -> Option<exif::Exif> {
    exif::parse(&tiff_prefix(reader)?)
}

/// The first `EXIF_PREFIX_BYTES` of a TIFF-structured file, or `None` when it is not TIFF.
fn tiff_prefix<R: Read>(reader: R) -> Option<Vec<u8>> {
    let mut data = Vec::new();
    reader.take(EXIF_PREFIX_BYTES).read_to_end(&mut data).ok()?;
    matches!(data.get(..4)?, b"II*\0" | b"MM\0*").then_some(data)
}

/// EXIF plus whether the file is a DNG (it carries a DNGVersion tag), from one bounded read.
fn read_tiff(path: &Path) -> (Option<exif::Exif>, bool) {
    let Some(data) = File::open(path).ok().and_then(tiff_prefix) else {
        return (None, false);
    };
    (exif::parse(&data), exif::is_dng(&data))
}

/// Class id of Microsoft's Raw Image Decoder as registered on the development machine
/// (HKCR\CLSID\{41945702-8302-44A6-9445-AC98E8AFA086}: friendly name "Microsoft Raw Image
/// Decoder", container {FE99CE60-F19C-433C-A3AE-00ACEFA9CA21}, server windowscodecs.dll; read
/// 2026-10-11). This is the decoder observed returning a DNG un-turned.
const MICROSOFT_RAW_DECODER: GUID = GUID::from_u128(0x41945702_8302_44A6_9445_AC98E8AFA086);

/// The one case where Luma rotates a RAW photo itself: that decoder (its class id, and the RAW
/// container it reports) reading a DNG, whose stored orientation was observed on pixels in
/// tests/windows_codecs.rs. A container format alone is not an identity, since a vendor's codec
/// can report the same one and may already turn the picture. Any other decoder, or a non-DNG RAW
/// (CR2, NEF, ARW, ...), has no observation, so it is never turned and a warning says why.
fn raw_orientation_is_proven(clsid: &GUID, container: &GUID, is_dng: bool) -> bool {
    is_dng && *clsid == MICROSOFT_RAW_DECODER && *container == GUID_ContainerFormatRaw
}

/// Decode a small in-memory file; used for the codec self-check and by tests. No path, no EXIF.
pub fn decode_memory(bytes: &[u8]) -> Result<(u32, u32, String), String> {
    let factory = factory()?;
    let stream = unsafe { factory.CreateStream() }.map_err(|e| e.to_string())?;
    unsafe { stream.InitializeFromMemory(bytes) }.map_err(|e| e.to_string())?;
    let decoder = unsafe {
        factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
    }
    .map_err(|e| explain(None, Stage::Open, &e))?;
    let decoded = decode_frame(&factory, &decoder, None, &Cancel::never())?;
    Ok((decoded.width, decoded.height, decoded.decoder))
}

/// What Luma can truthfully say about one Windows-codec family right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    /// A small embedded sample was decoded just now through the installed codec.
    Working { decoder: String },
    /// A codec is registered, but there is no self-check sample for this family.
    Registered { decoder: String },
    /// A codec is registered but failed the self-check sample.
    Failing { decoder: String, error: String },
    /// Windows was asked and the list of installed codecs was complete: there is none.
    Missing,
    /// The question could not be answered, so nothing is known: not the same as `Missing`.
    Unknown { reason: String },
}

#[derive(Clone, Debug)]
pub struct FamilyStatus {
    pub family: Family,
    pub availability: Availability,
}

/// Self-check samples: a few hundred bytes each, generated by Luma's own tools from a plain
/// colour pattern (see `examples/make_wic_fixtures.rs`). They are test inputs, not codecs.
fn sample(family: Family) -> Option<&'static [u8]> {
    let bytes: &'static [u8] = match family {
        Family::Heif => include_bytes!("../assets/probes/probe.heic"),
        Family::Dng => include_bytes!("../assets/probes/probe.dng"),
        // No camera RAW sample exists, so a Raw Image Extension is "found", never "working".
        Family::Raw => return None,
        Family::JpegXr => include_bytes!("../assets/probes/probe.jxr"),
        Family::JpegXl => return None,
    };
    (!bytes.is_empty()).then_some(bytes)
}

/// The same facts as `status()`, plus every registered decoder, as JSON-ready data for
/// `luma --codecs`.
pub fn report() -> serde_json::Value {
    let listed = decoders();
    let list_error = listed.as_ref().err().cloned();
    let installed = listed.unwrap_or_default();
    let families: Vec<_> = status()
        .into_iter()
        .map(|s| {
            let (state, decoder, error) = match s.availability {
                Availability::Working { decoder } => ("working", Some(decoder), None),
                Availability::Registered { decoder } => {
                    ("registered-not-sampled", Some(decoder), None)
                }
                Availability::Failing { decoder, error } => ("failing", Some(decoder), Some(error)),
                Availability::Missing => ("missing", None, None),
                Availability::Unknown { reason } => ("unknown", None, Some(reason)),
            };
            serde_json::json!({
                "family": s.family.label(),
                "extensions": s.family.extensions(),
                "state": state,
                "decoder": decoder,
                "error": error,
                "how_to_get": s.family.how_to_get(),
            })
        })
        .collect();
    serde_json::json!({
        "enumeration_error": list_error,
        "registered_decoders": installed
            .iter()
            .map(|d| serde_json::json!({
                "name": d.name,
                "container": format!("{:?}", d.container),
                "extensions": d.extensions,
            }))
            .collect::<Vec<_>>(),
        "families": families,
    })
}

/// Every family as "could not check", for when the question itself could not be asked.
pub fn unknown_status(reason: &str) -> Vec<FamilyStatus> {
    Family::ALL
        .into_iter()
        .map(|family| FamilyStatus {
            family,
            availability: Availability::Unknown {
                reason: reason.to_owned(),
            },
        })
        .collect()
}

/// Ask Windows, now, which families work. Cheap enough to run when a sheet opens, but it does
/// decode three tiny images, so call it off the UI thread.
pub fn status() -> Vec<FamilyStatus> {
    status_from(decoders(), &|family| {
        sample(family).map(|bytes| decode_memory(bytes).map(|(_, _, decoder)| decoder))
    })
}

/// The decision, separated from Windows so each outcome can be tested with made-up input.
/// `Missing` is only ever reported from a list that was successfully and completely read;
/// when the list could not be read, nothing is known and every family says so.
/// `probe` decodes a family's self-check sample and returns the name of the decoder that really
/// decoded it (Windows may pick a different registered decoder than the first one listed for
/// the family), or `None` if the family has no sample.
pub fn status_from(
    installed: Result<Vec<DecoderEntry>, String>,
    probe: &dyn Fn(Family) -> Option<Result<String, String>>,
) -> Vec<FamilyStatus> {
    let installed = match installed {
        Ok(list) => list,
        Err(reason) => return unknown_status(&reason),
    };
    Family::ALL
        .into_iter()
        .map(|family| {
            let availability = match decoder_for(family, &installed) {
                None => Availability::Missing,
                Some(entry) => match probe(family) {
                    None => Availability::Registered {
                        decoder: entry.name.clone(),
                    },
                    Some(Ok(decoder)) => Availability::Working { decoder },
                    Some(Err(error)) => Availability::Failing {
                        decoder: entry.name.clone(),
                        error,
                    },
                },
            };
            FamilyStatus {
                family,
                availability,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, ext: &str, container: GUID) -> DecoderEntry {
        DecoderEntry {
            name: name.into(),
            extensions: vec![ext.into()],
            container,
        }
    }
    fn states(list: &[FamilyStatus]) -> Vec<&Availability> {
        list.iter().map(|s| &s.availability).collect()
    }

    #[test]
    fn a_failed_enumeration_is_unknown_never_missing() {
        let list = status_from(Err("The class is not registered.".into()), &|_| None);
        assert_eq!(list.len(), Family::ALL.len());
        for s in &list {
            assert_eq!(
                s.availability,
                Availability::Unknown {
                    reason: "The class is not registered.".into()
                },
                "{:?}",
                s.family
            );
        }
    }
    #[test]
    fn missing_is_only_reported_from_a_complete_empty_list() {
        let list = status_from(Ok(vec![]), &|_| None);
        assert!(states(&list).iter().all(|a| **a == Availability::Missing));
    }
    #[test]
    fn registered_working_and_failing_are_told_apart() {
        let installed = vec![
            entry(
                "Microsoft HEIF Image Decoder",
                "heic",
                GUID_ContainerFormatHeif,
            ),
            entry("Microsoft DNG Decoder", "dng", GUID_ContainerFormatAdng),
            entry(
                "Microsoft Raw Image Decoder",
                "cr2",
                GUID_ContainerFormatRaw,
            ),
            entry("Microsoft HD Photo Decoder", "jxr", GUID_ContainerFormatWmp),
            entry("Microsoft JPEG XL Decoder", "jxl", GUID::zeroed()),
        ];
        let list = status_from(Ok(installed.clone()), &|family| match family {
            Family::Heif => Some(Ok("Microsoft HEIF Image Decoder".into())),
            Family::Dng => Some(Err("0x88982F60".into())),
            Family::Raw => None,
            Family::JpegXr => Some(Ok("Microsoft HD Photo Decoder".into())),
            Family::JpegXl => None,
        });
        let by = |f: Family| &list.iter().find(|s| s.family == f).unwrap().availability;
        // "Working" names the decoder that really decoded the sample, which need not be the
        // first one registered for the family (here the DNG sample went to the Raw decoder).
        let actual = status_from(Ok(installed), &|family| match family {
            Family::Dng => Some(Ok("Microsoft Raw Image Decoder".into())),
            _ => None,
        });
        assert_eq!(
            actual
                .iter()
                .find(|s| s.family == Family::Dng)
                .unwrap()
                .availability,
            Availability::Working {
                decoder: "Microsoft Raw Image Decoder".into()
            }
        );
        assert_eq!(
            by(Family::Heif),
            &Availability::Working {
                decoder: "Microsoft HEIF Image Decoder".into()
            }
        );
        assert_eq!(
            by(Family::Dng),
            &Availability::Failing {
                decoder: "Microsoft DNG Decoder".into(),
                error: "0x88982F60".into()
            }
        );
        // A DNG sample says nothing about camera RAW: with no sample for it, a registered Raw
        // Image Extension can only be "registered".
        assert_eq!(
            by(Family::Raw),
            &Availability::Registered {
                decoder: "Microsoft Raw Image Decoder".into()
            }
        );
        // No sample, so it can only be "registered", never "working".
        assert_eq!(
            by(Family::JpegXl),
            &Availability::Registered {
                decoder: "Microsoft JPEG XL Decoder".into()
            }
        );
    }
    #[test]
    fn raw_orientation_is_only_applied_where_it_was_observed() {
        let raw = GUID_ContainerFormatRaw;
        // Observed: Microsoft's Raw Image Decoder returning a DNG un-turned.
        assert!(raw_orientation_is_proven(
            &MICROSOFT_RAW_DECODER,
            &raw,
            true
        ));
        // Same decoder, but a CR2/NEF/ARW-style file: never observed.
        assert!(!raw_orientation_is_proven(
            &MICROSOFT_RAW_DECODER,
            &raw,
            false
        ));
        // A vendor codec that reports the same RAW container is a different decoder: not
        // observed, however it names itself. So is a decoder Windows would not identify.
        let vendor = GUID::from_u128(0x11112222_33334444_55556666_77778888);
        assert!(!raw_orientation_is_proven(&vendor, &raw, true));
        assert!(!raw_orientation_is_proven(&GUID::zeroed(), &raw, true));
        // Right class id, wrong container: not the observation either.
        assert!(!raw_orientation_is_proven(
            &MICROSOFT_RAW_DECODER,
            &GUID_ContainerFormatAdng,
            true
        ));
        // The built-in DNG decoder was never seen reading a DNG here.
        assert!(!raw_orientation_is_proven(
            &GUID::zeroed(),
            &GUID_ContainerFormatAdng,
            true
        ));
    }
    #[test]
    fn format_names_come_from_the_decoder_not_the_extension() {
        assert_eq!(kind_of(&GUID_ContainerFormatHeif, ""), Kind::Heif);
        assert_eq!(
            kind_of(&GUID::zeroed(), "Microsoft HEIF Image Decoder"),
            Kind::Heif
        );
        assert_eq!(
            kind_of(&GUID::zeroed(), "Microsoft Raw Image Decoder"),
            Kind::Raw
        );
        assert_eq!(kind_of(&GUID_ContainerFormatRaw, ""), Kind::Raw);
        // The built-in DNG decoder is a RAW decoder too, and nothing here assumes it is the one
        // Windows picks for a DNG.
        assert_eq!(kind_of(&GUID_ContainerFormatAdng, ""), Kind::Raw);
        assert_eq!(kind_of(&GUID::zeroed(), "Microsoft DNG Decoder"), Kind::Raw);
        assert_eq!(kind_of(&GUID_ContainerFormatWmp, ""), Kind::JpegXr);
        assert_eq!(
            kind_of(&GUID::zeroed(), "Microsoft JPEG XL Decoder"),
            Kind::JpegXl
        );
        assert_eq!(
            kind_of(&GUID::zeroed(), "Something Else Decoder"),
            Kind::Other
        );
        // A RAW label only borrows a brand from an extension that is itself a RAW extension.
        assert_eq!(format_name(Kind::Raw, "", "nef"), "RAW (NEF)");
        assert_eq!(format_name(Kind::Raw, "", "heic"), "RAW");
        assert_eq!(format_name(Kind::Raw, "", "dng"), "RAW (DNG)");
        assert_eq!(format_name(Kind::Raw, "", "xyz"), "RAW");
        // The two families are listed apart: a DNG decoder is not a camera RAW decoder.
        let both = vec![
            entry(
                "Microsoft Raw Image Decoder",
                "cr2",
                GUID_ContainerFormatRaw,
            ),
            entry("Microsoft DNG Decoder", "dng", GUID_ContainerFormatAdng),
        ];
        assert_eq!(
            decoder_for(Family::Dng, &both).map(|d| d.name.as_str()),
            Some("Microsoft DNG Decoder")
        );
        assert_eq!(
            decoder_for(Family::Raw, &both).map(|d| d.name.as_str()),
            Some("Microsoft Raw Image Decoder")
        );
        assert!(decoder_for(Family::Raw, &both[1..]).is_none());
        assert!(decoder_for(Family::Dng, &both[..1]).is_none());
        assert_eq!(Family::from_extension("DNG"), Some(Family::Dng));
        assert_eq!(Family::from_extension("nef"), Some(Family::Raw));
        assert_eq!(format_name(Kind::Heif, "", "jxr"), "HEIC");
        assert_eq!(
            format_name(Kind::Other, "Microsoft Foo Decoder", "foo"),
            "Foo"
        );
    }
}
