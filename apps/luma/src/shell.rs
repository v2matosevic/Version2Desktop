//! Explicit native actions: show a photo in Explorer, "Open with…", and copy to the clipboard.
//!
//! Every action runs only when the person chooses it, on its own short-lived thread so the
//! window never waits on Explorer or a busy clipboard, and reports success or the failure in
//! words. Nothing here writes to, moves, renames or deletes the photo: the clipboard "copy file"
//! carries the path with a copy intent, so pasting makes a new file and never moves the original.
//!
//! The clipboard is reached through the small `Sink` trait. The real sink talks to Windows; the
//! tests use a fake that records what would be written, so byte layouts, ordering, memory bounds
//! and partial failures are all checked without touching anyone's real clipboard.
#![allow(unsafe_code)]
use crate::decode::{MAX_PIXELS, Photo};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND},
        System::{
            Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            DataExchange::{
                CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW,
                SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            Ole::{CF_DIBV5, CF_HDROP, CF_UNICODETEXT},
        },
        UI::Shell::{
            ILCreateFromPathW, ILFree, OAIF_EXEC, OAIF_HIDE_REGISTRATION, OPENASINFO,
            SHOpenFolderAndSelectItems, SHOpenWithDialog,
        },
    },
    core::{PCWSTR, w},
};

/// What a native action should do.
pub enum Action {
    /// Open the folder in Explorer with the photo selected.
    Reveal(PathBuf),
    /// Windows' own "Open with" chooser for this one file.
    OpenWith(PathBuf),
    CopyPath(PathBuf),
    /// Put the file on the clipboard as a copy, for pasting in Explorer or an e-mail.
    CopyFile(PathBuf),
    /// The picture as shown: this frame, turned by the view rotation. Never changes the file.
    CopyImage {
        photo: Arc<Photo>,
        frame: usize,
        clockwise_quarter_turns: u8,
    },
}
impl Action {
    /// A short name for "still working on ..." messages.
    pub fn label(&self) -> &'static str {
        match self {
            Action::Reveal(_) => "Opening Explorer",
            Action::OpenWith(_) => "Choosing an app",
            Action::CopyPath(_) | Action::CopyFile(_) | Action::CopyImage { .. } => "Copying",
        }
    }
}

/// The outcome of one action, ready to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    pub ok: bool,
    /// Empty when there is nothing to say (the Explorer window itself is the confirmation).
    pub message: String,
}
impl Done {
    fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }
    fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }
}

/// Run `action` on its own thread. `owner` is the window handle clipboard and dialogs belong to;
/// `wake` repaints the window when the result is ready.
pub fn spawn(
    action: Action,
    owner: isize,
    wake: impl FnOnce() + Send + 'static,
) -> mpsc::Receiver<Done> {
    let (tx, rx) = mpsc::channel();
    let started = std::thread::Builder::new()
        .name("luma-native-action".into())
        .spawn({
            let tx = tx.clone();
            move || {
                let done = run(action, owner);
                let _ = tx.send(done);
                wake();
            }
        });
    if started.is_err() {
        let _ = tx.send(Done::err("Could not start the action."));
    }
    rx
}

fn run(action: Action, owner: isize) -> Done {
    // Shell calls expect a single-threaded COM apartment on the calling thread.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let done = match action {
        Action::Reveal(path) => reveal(&path),
        Action::OpenWith(path) => open_with(&path, owner),
        Action::CopyPath(path) => write_text(
            &mut NativeSink::new(owner),
            &crate::model::user_path(&path).display().to_string(),
        ),
        Action::CopyFile(path) => write_file(&mut NativeSink::new(owner), &path),
        Action::CopyImage {
            photo,
            frame,
            clockwise_quarter_turns,
        } => write_image(
            &mut NativeSink::new(owner),
            &photo,
            frame,
            clockwise_quarter_turns,
        ),
    };
    if com {
        unsafe { CoUninitialize() };
    }
    done
}

/// A target the shell may be asked about: absolute, present and a regular file.
pub fn validate_target(path: &Path) -> Result<PathBuf, String> {
    let clean = crate::model::user_path(path);
    if !clean.is_absolute() {
        return Err("This photo has no full path, so Windows cannot find it.".into());
    }
    let meta = std::fs::metadata(&clean).map_err(|_| {
        "Windows could not find this file. It may have been moved, renamed or deleted.".to_string()
    })?;
    if !meta.is_file() {
        return Err("This is not a regular file.".into());
    }
    Ok(clean)
}

/// UTF-16 with a terminating NUL, refusing paths that already contain one (it would silently
/// shorten the name the shell sees).
pub fn wide_z(path: &Path) -> Result<Vec<u16>, String> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err("The file name contains an invalid character.".into());
    }
    wide.push(0);
    Ok(wide)
}

fn reveal(path: &Path) -> Done {
    let result = (|| {
        let clean = validate_target(path)?;
        let wide = wide_z(&clean)?;
        unsafe {
            let item = ILCreateFromPathW(PCWSTR(wide.as_ptr()));
            if item.is_null() {
                return Err("Explorer could not locate this file.".to_string());
            }
            let result = SHOpenFolderAndSelectItems(item, None, 0);
            ILFree(Some(item));
            result.map_err(|e| format!("Explorer could not show this file: {e}"))
        }
    })();
    match result {
        Ok(()) => Done::ok(""),
        Err(e) => Done::err(e),
    }
}

/// ERROR_CANCELLED as an HRESULT: the person closed the chooser.
const CANCELLED: i32 = 0x800704C7u32 as i32;

fn open_with(path: &Path, owner: isize) -> Done {
    let result = (|| {
        let clean = validate_target(path)?;
        let wide = wide_z(&clean)?;
        // OAIF_EXEC starts the chosen app on this file. No "always use" flag is passed, and
        // Windows 10 and later ignore those flags anyway: the chooser is for this one open.
        let info = OPENASINFO {
            pcszFile: PCWSTR(wide.as_ptr()),
            pcszClass: PCWSTR::null(),
            oaifInFlags: OAIF_EXEC | OAIF_HIDE_REGISTRATION,
        };
        let parent = (owner != 0).then_some(HWND(owner as *mut _));
        match unsafe { SHOpenWithDialog(parent, &info) } {
            Ok(()) => Ok(()),
            Err(e) if e.code().0 == CANCELLED => Ok(()),
            Err(e) => Err(format!("Windows could not show the Open with list: {e}")),
        }
    })();
    match result {
        Ok(()) => Done::ok(""),
        Err(e) => Done::err(e),
    }
}

// ---- clipboard orchestration over a sink ----------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    FileList,
    /// CF_DIBV5: a bitmap with alpha, understood by almost every app.
    Dib,
    /// The widely used "PNG" registered format.
    Png,
    /// "Preferred DropEffect": copy, so a paste never moves the original.
    DropEffect,
}

/// Where clipboard data goes. `fill` writes exactly `len` bytes straight into the destination,
/// so a large bitmap is built once, in the clipboard's own memory, never as an extra copy.
pub trait Sink {
    /// Take the clipboard and clear it. Called once, after everything that can fail cheaply
    /// has been checked, so a failure earlier leaves the person's clipboard alone.
    fn begin(&mut self) -> Result<(), String>;
    fn set(
        &mut self,
        format: Format,
        len: usize,
        fill: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), String>;
}

fn set_bytes(sink: &mut dyn Sink, format: Format, bytes: &[u8]) -> Result<(), String> {
    sink.set(format, bytes.len(), &mut |dst| dst.copy_from_slice(bytes))
}

pub fn write_text(sink: &mut dyn Sink, text: &str) -> Done {
    let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    bytes.extend([0, 0]);
    if let Err(e) = sink.begin() {
        return Done::err(e);
    }
    match set_bytes(sink, Format::Text, &bytes) {
        Ok(()) => Done::ok("Path copied"),
        Err(e) => Done::err(e),
    }
}

pub fn write_file(sink: &mut dyn Sink, path: &Path) -> Done {
    let list = match validate_target(path).and_then(|p| dropfiles_bytes(&p)) {
        Ok(l) => l,
        Err(e) => return Done::err(e),
    };
    if let Err(e) = sink.begin() {
        return Done::err(e);
    }
    if let Err(e) = set_bytes(sink, Format::FileList, &list) {
        return Done::err(e);
    }
    // The file list alone already pastes as a copy; the explicit flag makes that unmistakable.
    // Its failure leaves a working copy, so it is reported, not treated as a failed copy.
    match set_bytes(sink, Format::DropEffect, &copy_effect_bytes()) {
        Ok(()) => Done::ok("File copied. Paste it in Explorer or an e-mail."),
        Err(e) => Done::ok(format!(
            "File copied. Paste it in Explorer or an e-mail. (The copy-only flag could not be added: {e})"
        )),
    }
}

/// The largest bitmap copied: Luma's own decode ceiling, so a copy is never bigger than a photo.
pub const MAX_COPY_BYTES: usize = MAX_PIXELS as usize * 4;
/// Above this many pixels only the bitmap form is written; a PNG would double peak memory.
pub const MAX_PNG_PIXELS: usize = 8_000_000;

pub fn write_image(sink: &mut dyn Sink, photo: &Photo, frame: usize, turns: u8) -> Done {
    let Some(frame) = photo.frames.get(frame) else {
        return Done::err("There is no picture to copy.");
    };
    let (w, h) = (photo.width as usize, photo.height as usize);
    let bytes = w.checked_mul(h).and_then(|p| p.checked_mul(4));
    if bytes.is_none_or(|b| b == 0 || b > MAX_COPY_BYTES || frame.rgba.len() != b) {
        return Done::err("This image is too large to copy.");
    }
    let turns = turns % 4;
    let (nw, nh) = if turns % 2 == 1 { (h, w) } else { (w, h) };
    let pixels = nw * nh;
    // Everything that can fail without side effects happens before the clipboard is touched.
    let png = (pixels <= MAX_PNG_PIXELS)
        .then(|| {
            let (rotated, rw, rh) = rotate_rgba(&frame.rgba, w as u32, h as u32, turns);
            png_bytes(&rotated, rw, rh)
        })
        .transpose();
    let png = match png {
        Ok(p) => p,
        Err(e) => return Done::err(e),
    };
    if let Err(e) = sink.begin() {
        return Done::err(e);
    }
    let primary = sink.set(Format::Dib, 124 + pixels * 4, &mut |dst| {
        fill_dibv5(dst, &frame.rgba, w, h, turns)
    });
    if let Err(e) = primary {
        return Done::err(e);
    }
    // The bitmap is already on the clipboard. The PNG is a bonus for apps that keep transparency
    // from it; if it cannot be added the copy still succeeded and says so.
    match png {
        Some(png) => match set_bytes(sink, Format::Png, &png) {
            Ok(()) => Done::ok("Image copied"),
            Err(e) => Done::ok(format!(
                "Image copied. (The PNG form could not be added: {e})"
            )),
        },
        None => Done::ok("Image copied as a bitmap"),
    }
}

// ---- byte layouts (pure) --------------------------------------------------------------------

/// A CF_HDROP payload: a DROPFILES header followed by the wide path list.
pub fn dropfiles_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let wide = wide_z(path)?;
    let mut out = Vec::with_capacity(20 + wide.len() * 2 + 2);
    out.extend(20u32.to_le_bytes()); // pFiles: offset of the list
    out.extend(0i32.to_le_bytes()); // pt.x
    out.extend(0i32.to_le_bytes()); // pt.y
    out.extend(0u32.to_le_bytes()); // fNC
    out.extend(1u32.to_le_bytes()); // fWide
    for unit in wide {
        out.extend(unit.to_le_bytes());
    }
    out.extend(0u16.to_le_bytes()); // the list ends with an empty string
    Ok(out)
}

/// DROPEFFECT_COPY, for the "Preferred DropEffect" format Explorer reads on paste.
pub fn copy_effect_bytes() -> [u8; 4] {
    1u32.to_le_bytes()
}

/// Where the pixel at (nx, ny) of the turned image comes from in the original, `w` x `h`.
/// `turns` are clockwise quarter turns. This is the one definition both the bitmap writer and
/// `rotate_rgba` use.
fn source_xy(turns: u8, w: usize, h: usize, nx: usize, ny: usize) -> (usize, usize) {
    match turns % 4 {
        0 => (nx, ny),
        1 => (ny, h - 1 - nx),
        2 => (w - 1 - nx, h - 1 - ny),
        _ => (w - 1 - ny, nx),
    }
}

/// Write a 32-bit CF_DIBV5 into `dst` (exactly 124 + 4 x pixels bytes): a header, then
/// bottom-up BGRA rows with straight alpha, with the view rotation applied on the way.
pub fn fill_dibv5(dst: &mut [u8], rgba: &[u8], w: usize, h: usize, turns: u8) {
    let (nw, nh) = if turns % 2 == 1 { (h, w) } else { (w, h) };
    let pixel_bytes = nw * nh * 4;
    assert_eq!(dst.len(), 124 + pixel_bytes);
    assert_eq!(rgba.len(), w * h * 4);
    let mut header = Vec::with_capacity(124);
    header.extend(124u32.to_le_bytes()); // bV5Size
    header.extend((nw as i32).to_le_bytes());
    header.extend((nh as i32).to_le_bytes()); // positive: bottom-up, which every app reads
    header.extend(1u16.to_le_bytes()); // planes
    header.extend(32u16.to_le_bytes()); // bit count
    header.extend(3u32.to_le_bytes()); // BI_BITFIELDS
    header.extend((pixel_bytes as u32).to_le_bytes());
    header.extend([0u8; 16]); // resolution and palette fields
    header.extend(0x00FF_0000u32.to_le_bytes()); // red mask
    header.extend(0x0000_FF00u32.to_le_bytes()); // green mask
    header.extend(0x0000_00FFu32.to_le_bytes()); // blue mask
    header.extend(0xFF00_0000u32.to_le_bytes()); // alpha mask
    header.extend(0x7352_4742u32.to_le_bytes()); // LCS_sRGB
    header.extend([0u8; 36]); // endpoints
    header.extend([0u8; 12]); // gamma
    header.extend(4u32.to_le_bytes()); // LCS_GM_IMAGES
    header.extend([0u8; 12]); // profile data, size, reserved
    assert_eq!(header.len(), 124);
    dst[..124].copy_from_slice(&header);
    let mut at = 124;
    for ny in (0..nh).rev() {
        for nx in 0..nw {
            let (x, y) = source_xy(turns, w, h, nx, ny);
            let p = (y * w + x) * 4;
            dst[at..at + 4].copy_from_slice(&[rgba[p + 2], rgba[p + 1], rgba[p], rgba[p + 3]]);
            at += 4;
        }
    }
}

/// The same bitmap as an owned buffer; used by tests and anything that wants plain bytes.
pub fn dibv5_bytes(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let (w, h) = (width as usize, height as usize);
    let pixels = w
        .checked_mul(h)
        .and_then(|p| p.checked_mul(4))
        .ok_or("Image is too large to copy.")?;
    if w == 0 || h == 0 || rgba.len() != pixels {
        return Err("Image data does not match its size.".into());
    }
    let mut out = Vec::new();
    out.try_reserve_exact(124 + pixels)
        .map_err(|_| "Not enough memory to copy this image.".to_string())?;
    out.resize(124 + pixels, 0);
    fill_dibv5(&mut out, rgba, w, h, 0);
    Ok(out)
}

/// The picture as a PNG, for apps that prefer it (and keep transparency).
pub fn png_bytes(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    use image::{ExtendedColorType, ImageEncoder, codecs::png};
    let mut out = Vec::new();
    png::PngEncoder::new_with_quality(&mut out, png::CompressionType::Fast, png::FilterType::Sub)
        .write_image(rgba, width, height, ExtendedColorType::Rgba8)
        .map_err(|e| format!("Could not encode the PNG copy: {e}"))?;
    Ok(out)
}

/// Turn RGBA pixels clockwise by whole quarter turns. Pure index arithmetic, never touches
/// the source.
pub fn rotate_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
    clockwise_quarter_turns: u8,
) -> (Vec<u8>, u32, u32) {
    let (w, h) = (width as usize, height as usize);
    let turns = clockwise_quarter_turns % 4;
    if turns == 0 {
        return (rgba.to_vec(), width, height);
    }
    let (nw, nh) = if turns % 2 == 1 { (h, w) } else { (w, h) };
    let mut out = vec![0u8; rgba.len()];
    for ny in 0..nh {
        for nx in 0..nw {
            let (x, y) = source_xy(turns, w, h, nx, ny);
            let from = (y * w + x) * 4;
            let to = (ny * nw + nx) * 4;
            out[to..to + 4].copy_from_slice(&rgba[from..from + 4]);
        }
    }
    (out, nw as u32, nh as u32)
}

// ---- the real clipboard ---------------------------------------------------------------------

struct NativeSink {
    owner: isize,
    open: bool,
}
impl NativeSink {
    fn new(owner: isize) -> Self {
        Self { owner, open: false }
    }
    fn native_format(format: Format) -> u32 {
        match format {
            Format::Text => u32::from(CF_UNICODETEXT.0),
            Format::FileList => u32::from(CF_HDROP.0),
            Format::Dib => u32::from(CF_DIBV5.0),
            Format::Png => unsafe { RegisterClipboardFormatW(w!("PNG")) },
            Format::DropEffect => unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) },
        }
    }
}
impl Sink for NativeSink {
    fn begin(&mut self) -> Result<(), String> {
        // A real owner window is required: with none, Windows clears the clipboard and then
        // refuses the data. Refuse up front, before anything is cleared.
        if self.owner == 0 {
            return Err("Luma's window is not available to own the clipboard.".into());
        }
        let owner = Some(HWND(self.owner as *mut _));
        // The clipboard is shared and can be busy for a moment; try for about a second.
        for _ in 0..25 {
            if unsafe { OpenClipboard(owner) }.is_ok() {
                self.open = true;
                return unsafe { EmptyClipboard() }
                    .map_err(|e| format!("Could not prepare the clipboard: {e}"));
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        Err("The clipboard is in use by another program. Try again.".into())
    }
    fn set(
        &mut self,
        format: Format,
        len: usize,
        fill: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), String> {
        if !self.open {
            return Err("The clipboard is not open.".into());
        }
        let id = Self::native_format(format);
        if id == 0 {
            return Err("Windows would not register this clipboard format.".into());
        }
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE, len.max(1))
                .map_err(|_| "Not enough memory to copy this.".to_string())?;
            let target = GlobalLock(handle);
            if target.is_null() {
                let _ = GlobalFree(Some(handle));
                return Err("Could not lock clipboard memory.".into());
            }
            fill(std::slice::from_raw_parts_mut(target.cast::<u8>(), len));
            let _ = GlobalUnlock(handle);
            // On success the clipboard owns the memory; on failure it is still ours to free.
            match SetClipboardData(id, Some(HANDLE(handle.0))) {
                Ok(_) => Ok(()),
                Err(e) => {
                    let _ = GlobalFree(Some(HGLOBAL(handle.0)));
                    Err(format!("The clipboard refused the data: {e}"))
                }
            }
        }
    }
}
impl Drop for NativeSink {
    fn drop(&mut self) {
        if self.open {
            let _ = unsafe { CloseClipboard() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what would be written, and can be told to fail a format.
    #[derive(Default)]
    struct Fake {
        began: usize,
        fail_begin: bool,
        fail: Vec<Format>,
        written: Vec<(Format, Vec<u8>)>,
        order: Vec<&'static str>,
    }
    impl Sink for Fake {
        fn begin(&mut self) -> Result<(), String> {
            self.order.push("begin");
            if self.fail_begin {
                return Err("clipboard busy".into());
            }
            self.began += 1;
            Ok(())
        }
        fn set(
            &mut self,
            format: Format,
            len: usize,
            fill: &mut dyn FnMut(&mut [u8]),
        ) -> Result<(), String> {
            self.order.push("set");
            if self.fail.contains(&format) {
                return Err(format!("{format:?} refused"));
            }
            let mut bytes = vec![0u8; len];
            fill(&mut bytes);
            self.written.push((format, bytes));
            Ok(())
        }
    }
    fn tiny_photo() -> Photo {
        // 3 x 2, one distinct red value per pixel.
        let rgba: Vec<u8> = [1u8, 2, 3, 4, 5, 6]
            .into_iter()
            .flat_map(|v| [v, 0, 0, 255])
            .collect();
        Photo {
            width: 3,
            height: 2,
            frames: vec![crate::decode::Frame { rgba, delay_ms: 0 }],
            ..Photo::default()
        }
    }

    #[test]
    fn dropfiles_layout_is_a_wide_list_that_ends_with_an_empty_string() {
        let bytes = dropfiles_bytes(Path::new(r"C:\Photos\žuta 写真.png")).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 20);
        assert_eq!(
            u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            1,
            "fWide"
        );
        let units: Vec<u16> = bytes[20..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(
            &units[units.len() - 2..],
            &[0, 0],
            "name NUL, then list NUL"
        );
        assert_eq!(
            String::from_utf16(&units[..units.len() - 2]).unwrap(),
            r"C:\Photos\žuta 写真.png"
        );
    }
    #[test]
    fn copy_intent_is_dropeffect_copy_not_move() {
        assert_eq!(u32::from_le_bytes(copy_effect_bytes()), 1);
    }
    #[test]
    fn paths_with_embedded_nul_are_refused() {
        let bad = std::ffi::OsString::from("a\u{0}b.png");
        assert!(wide_z(Path::new(&bad)).is_err());
        assert!(dropfiles_bytes(Path::new(&bad)).is_err());
    }
    #[test]
    fn dib_header_and_rows_match_what_apps_read() {
        // 2 x 2: red, green / blue, translucent white.
        let rgba = [
            255, 0, 0, 255, 0, 255, 0, 255, // top row
            0, 0, 255, 255, 255, 255, 255, 128, // bottom row
        ];
        let dib = dibv5_bytes(&rgba, 2, 2).unwrap();
        assert_eq!(dib.len(), 124 + 16);
        let u32at = |o: usize| u32::from_le_bytes(dib[o..o + 4].try_into().unwrap());
        assert_eq!(u32at(0), 124);
        assert_eq!(u32at(4), 2);
        assert_eq!(u32at(8), 2, "positive height means bottom-up");
        assert_eq!(u16::from_le_bytes([dib[14], dib[15]]), 32);
        assert_eq!(u32at(16), 3, "BI_BITFIELDS");
        assert_eq!(u32at(20), 16);
        assert_eq!(u32at(40), 0x00FF_0000);
        assert_eq!(u32at(52), 0xFF00_0000);
        assert_eq!(u32at(56), 0x7352_4742, "LCS_sRGB");
        assert_eq!(
            &dib[124..132],
            &[255, 0, 0, 255, 255, 255, 255, 128],
            "bottom row first, BGRA"
        );
        assert_eq!(
            &dib[132..140],
            &[0, 0, 255, 255, 0, 255, 0, 255],
            "top row last"
        );
    }
    #[test]
    fn dib_refuses_mismatched_or_empty_input() {
        assert!(dibv5_bytes(&[0; 8], 2, 2).is_err());
        assert!(dibv5_bytes(&[], 0, 0).is_err());
        assert!(dibv5_bytes(&[0; 4], u32::MAX, u32::MAX).is_err());
    }
    #[test]
    fn png_copy_round_trips_pixels_and_alpha() {
        let rgba: Vec<u8> = (0..16u8)
            .flat_map(|i| [i * 15, 255 - i * 15, i, 255 - i * 8])
            .collect();
        let png = png_bytes(&rgba, 4, 4).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let back = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .into_rgba8();
        assert_eq!(back.as_raw(), &rgba);
    }
    #[test]
    fn rotation_matches_clockwise_quarter_turns_and_cycles() {
        let photo = tiny_photo();
        let src = &photo.frames[0].rgba;
        let reds = |v: &[u8]| v.chunks_exact(4).map(|p| p[0]).collect::<Vec<_>>();
        // 3 x 2:  1 2 3 / 4 5 6.
        let (cw, w, h) = rotate_rgba(src, 3, 2, 1);
        assert_eq!((w, h), (2, 3));
        assert_eq!(
            reds(&cw),
            vec![4, 1, 5, 2, 6, 3],
            "left column bottom-to-top is the new top row"
        );
        let (half, w, h) = rotate_rgba(src, 3, 2, 2);
        assert_eq!((w, h), (3, 2));
        assert_eq!(reds(&half), vec![6, 5, 4, 3, 2, 1]);
        let (ccw, _, _) = rotate_rgba(src, 3, 2, 3);
        assert_eq!(reds(&ccw), vec![3, 6, 2, 5, 1, 4]);
        let (mut cycle, mut w, mut h) = (src.clone(), 3, 2);
        for _ in 0..4 {
            (cycle, w, h) = rotate_rgba(&cycle, w, h, 1);
        }
        assert_eq!((&cycle, w, h), (src, 3, 2));
    }
    #[test]
    fn the_bitmap_writer_rotates_while_writing_and_matches_rotate_rgba() {
        let photo = tiny_photo();
        let src = &photo.frames[0].rgba;
        for turns in 0..4u8 {
            let (rotated, rw, rh) = rotate_rgba(src, 3, 2, turns);
            let mut dib = vec![0u8; 124 + rotated.len()];
            fill_dibv5(&mut dib, src, 3, 2, turns);
            assert_eq!(i32::from_le_bytes(dib[4..8].try_into().unwrap()), rw as i32);
            assert_eq!(
                i32::from_le_bytes(dib[8..12].try_into().unwrap()),
                rh as i32
            );
            // Un-flip the bottom-up rows and un-swizzle BGRA back to RGBA, then compare.
            let stride = rw as usize * 4;
            let mut back = Vec::new();
            for row in dib[124..].chunks_exact(stride).rev() {
                for px in row.chunks_exact(4) {
                    back.extend([px[2], px[1], px[0], px[3]]);
                }
            }
            assert_eq!(back, rotated, "turns {turns}");
        }
    }
    #[test]
    fn copy_image_writes_the_bitmap_first_then_png_and_rotates() {
        let photo = tiny_photo();
        let mut sink = Fake::default();
        let done = write_image(&mut sink, &photo, 0, 1);
        assert_eq!(done, Done::ok("Image copied"));
        assert_eq!(sink.began, 1, "the clipboard is taken exactly once");
        assert_eq!(sink.order, ["begin", "set", "set"]);
        let formats: Vec<_> = sink.written.iter().map(|(f, _)| *f).collect();
        assert_eq!(formats, [Format::Dib, Format::Png]);
        let dib = &sink.written[0].1;
        assert_eq!(
            i32::from_le_bytes(dib[4..8].try_into().unwrap()),
            2,
            "turned: now 2 wide"
        );
        let png = image::load_from_memory(&sink.written[1].1)
            .unwrap()
            .into_rgba8();
        assert_eq!((png.width(), png.height()), (2, 3));
        assert_eq!(
            png.get_pixel(0, 0)[0],
            4,
            "the PNG carries the same rotation"
        );
    }
    #[test]
    fn a_failed_png_still_reports_a_successful_image_copy() {
        let photo = tiny_photo();
        let mut sink = Fake {
            fail: vec![Format::Png],
            ..Fake::default()
        };
        let done = write_image(&mut sink, &photo, 0, 0);
        assert!(done.ok, "the bitmap is on the clipboard");
        assert!(done.message.starts_with("Image copied."));
        assert!(done.message.contains("PNG form could not be added"));
        assert_eq!(sink.written.len(), 1);
    }
    #[test]
    fn a_failed_bitmap_is_a_failed_copy_and_a_busy_clipboard_is_never_cleared() {
        let photo = tiny_photo();
        let mut sink = Fake {
            fail: vec![Format::Dib],
            ..Fake::default()
        };
        let done = write_image(&mut sink, &photo, 0, 0);
        assert!(!done.ok);
        assert!(sink.written.is_empty());
        let mut busy = Fake {
            fail_begin: true,
            ..Fake::default()
        };
        let done = write_image(&mut busy, &photo, 0, 0);
        assert_eq!(done, Done::err("clipboard busy"));
        assert!(busy.written.is_empty());
    }
    #[test]
    fn oversized_or_inconsistent_images_are_refused_before_the_clipboard_is_touched() {
        let mut sink = Fake::default();
        let mut huge = tiny_photo();
        huge.width = 20_000;
        huge.height = 20_000;
        assert!(!write_image(&mut sink, &huge, 0, 0).ok);
        let mut mismatched = tiny_photo();
        mismatched.width = 9;
        assert!(!write_image(&mut sink, &mismatched, 0, 0).ok);
        assert!(
            !write_image(&mut sink, &tiny_photo(), 7, 0).ok,
            "no such frame"
        );
        assert!(sink.order.is_empty(), "nothing began");
    }
    #[test]
    fn copy_file_sets_the_list_then_the_copy_flag_and_reports_a_partial_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.png");
        std::fs::write(&file, b"x").unwrap();
        let mut sink = Fake::default();
        let done = write_file(&mut sink, &file);
        assert!(done.ok && done.message.starts_with("File copied."));
        let formats: Vec<_> = sink.written.iter().map(|(f, _)| *f).collect();
        assert_eq!(formats, [Format::FileList, Format::DropEffect]);
        assert_eq!(sink.written[1].1, 1u32.to_le_bytes());
        let mut flaky = Fake {
            fail: vec![Format::DropEffect],
            ..Fake::default()
        };
        let done = write_file(&mut flaky, &file);
        assert!(done.ok, "the file list is on the clipboard");
        assert!(done.message.contains("copy-only flag could not be added"));
        let mut broken = Fake {
            fail: vec![Format::FileList],
            ..Fake::default()
        };
        assert!(!write_file(&mut broken, &file).ok);
        // A file that has gone away is refused before the clipboard is cleared.
        let mut untouched = Fake::default();
        assert!(!write_file(&mut untouched, &dir.path().join("gone.png")).ok);
        assert!(untouched.order.is_empty());
    }
    #[test]
    fn copy_path_writes_utf16_text() {
        let mut sink = Fake::default();
        let done = write_text(&mut sink, "C:\\Fotografije\\žuta.png");
        assert!(done.ok);
        let bytes = &sink.written[0].1;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(*units.last().unwrap(), 0);
        assert_eq!(
            String::from_utf16(&units[..units.len() - 1]).unwrap(),
            "C:\\Fotografije\\žuta.png"
        );
    }
    #[test]
    fn the_real_sink_refuses_to_clear_the_clipboard_without_an_owner_window() {
        // No window handle: it must fail before any Windows clipboard call, so nothing of the
        // person's current clipboard is touched by this test.
        let mut sink = NativeSink::new(0);
        assert!(sink.begin().is_err());
        assert!(
            sink.set(Format::Text, 2, &mut |_| {}).is_err(),
            "and nothing can be set on it"
        );
    }
    #[test]
    fn shell_targets_must_exist_and_be_files() {
        assert!(validate_target(Path::new("relative.png")).is_err());
        assert!(validate_target(Path::new(r"C:\definitely\not\here\x.png")).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(
            validate_target(dir.path()).is_err(),
            "folders are not photos"
        );
        let file = dir.path().join("a.png");
        std::fs::write(&file, b"x").unwrap();
        assert!(validate_target(&file).is_ok());
    }
}
