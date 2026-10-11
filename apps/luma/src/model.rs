use crate::decode::Photo;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
};

pub const MAX_FOLDER_ENTRIES: usize = 20_000;
pub const CACHE_BUDGET: usize = 128 * 1024 * 1024;

/// Extensions Luma decodes itself. Everything else it will try (HEIC, RAW, JPEG XR, JPEG XL) goes
/// through an installed Windows codec; see `wic::Family`.
pub const BUILT_IN_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "png", "webp", "gif", "bmp", "tif", "tiff", "avif",
];

/// Whether a file name looks like a photo worth listing in the folder. This only decides what is
/// offered; whether it decodes is a separate question answered by actually decoding it.
pub fn candidate(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    BUILT_IN_EXTENSIONS.contains(&ext.as_str()) || crate::wic::is_windows_codec_extension(&ext)
}

/// A path as a person reads and pastes it: without the `\\?\` extended-length prefix that
/// `canonicalize` adds. A path too long for the short form keeps the prefix, since stripping it
/// would make it unusable.
pub fn user_path(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_owned();
    };
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        let short = format!(r"\\{rest}");
        return if short.len() < 260 {
            short.into()
        } else {
            path.to_owned()
        };
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        let drive = rest.as_bytes();
        let looks_like_drive = drive.len() >= 3
            && drive[0].is_ascii_alphabetic()
            && drive[1] == b':'
            && drive[2] == b'\\';
        if looks_like_drive && rest.len() < 260 {
            return rest.into();
        }
    }
    path.to_owned()
}

/// Windows Explorer's own file-name order: case-insensitive and numeric-aware, so IMG_2 comes
/// before IMG_10. Falls back to a plain comparison on ties so the order is always total.
pub fn explorer_order(a: &std::ffi::OsStr, b: &std::ffi::OsStr) -> std::cmp::Ordering {
    use std::os::windows::ffi::OsStrExt;
    use windows::{Win32::UI::Shell::StrCmpLogicalW, core::PCWSTR};
    let wide = |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().chain([0]).collect() };
    let (wa, wb) = (wide(a), wide(b));
    // An interior NUL would end the string early; compare such names plainly.
    if wa[..wa.len() - 1].contains(&0) || wb[..wb.len() - 1].contains(&0) {
        return a.cmp(b);
    }
    let natural = unsafe { StrCmpLogicalW(PCWSTR(wa.as_ptr()), PCWSTR(wb.as_ptr())) };
    natural.cmp(&0).then_with(|| a.cmp(b))
}

fn sort_in_explorer_order(files: &mut Vec<PathBuf>) {
    let mut sorted = files.clone();
    // The ordering comes from the OS, so guard against a panic from an inconsistent comparison
    // on unusual names rather than losing the worker thread; fall back to a plain sort.
    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sorted.sort_by(|a, b| {
            explorer_order(
                a.file_name().unwrap_or_default(),
                b.file_name().unwrap_or_default(),
            )
        });
        sorted
    }));
    match attempt {
        Ok(sorted) => *files = sorted,
        Err(_) => files.sort(),
    }
}

#[derive(Default, Debug)]
pub struct Album {
    pub files: Vec<PathBuf>,
    pub index: usize,
    pub truncated: bool,
}
impl Album {
    /// Only enumerate the explicitly chosen folder; no recursion or symlink traversal.
    pub fn open(path: &Path) -> Result<Self, String> {
        let path = user_path(
            &std::fs::canonicalize(path).map_err(|e| format!("Cannot open this path: {e}"))?,
        );
        let folder = if path.is_dir() {
            path.clone()
        } else {
            path.parent().ok_or("Path has no parent folder")?.to_owned()
        };
        let mut files = Vec::new();
        let mut truncated = false;
        for (n, entry) in std::fs::read_dir(&folder)
            .map_err(|e| format!("Cannot read folder: {e}"))?
            .enumerate()
        {
            if n >= MAX_FOLDER_ENTRIES {
                truncated = true;
                break;
            }
            let entry = entry.map_err(|e| format!("Cannot read folder entry: {e}"))?;
            if entry.file_type().map_err(|e| e.to_string())?.is_file() && candidate(&entry.path()) {
                files.push(entry.path());
            }
        }
        if path.is_file() && !files.contains(&path) {
            // An explicitly chosen file is always shown, even if its extension is unusual.
            files.push(path.clone());
        }
        sort_in_explorer_order(&mut files);
        let index = files.iter().position(|f| f == &path).unwrap_or(0);
        Ok(Self {
            files,
            index,
            truncated,
        })
    }
    pub fn current(&self) -> Option<&PathBuf> {
        self.files.get(self.index)
    }
    pub fn step(&mut self, delta: isize) -> bool {
        if self.files.is_empty() {
            return false;
        }
        let previous = self.index;
        self.index = (self.index as isize + delta).rem_euclid(self.files.len() as isize) as usize;
        self.index != previous
    }
    pub fn next_path(&self) -> Option<PathBuf> {
        (self.files.len() > 1).then(|| self.files[(self.index + 1) % self.files.len()].clone())
    }
    /// The photo before the current one, wrapping, so reverse navigation can be prefetched too.
    pub fn previous_path(&self) -> Option<PathBuf> {
        (self.files.len() > 1)
            .then(|| self.files[(self.index + self.files.len() - 1) % self.files.len()].clone())
    }
    /// The neighbour in the direction of travel: the photo the person is most likely to ask for.
    pub fn neighbour_path(&self, direction: isize) -> Option<PathBuf> {
        if direction < 0 {
            self.previous_path()
        } else {
            self.next_path()
        }
    }
}

pub struct PhotoCache {
    entries: VecDeque<(PathBuf, Arc<Photo>)>,
    pub bytes: usize,
    budget: usize,
}
impl PhotoCache {
    pub fn new(budget: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
            budget,
        }
    }
    pub fn get(&mut self, path: &Path) -> Option<Arc<Photo>> {
        let i = self.entries.iter().position(|(p, _)| p == path)?;
        let entry = self.entries.remove(i)?;
        let result = entry.1.clone();
        self.entries.push_back(entry);
        Some(result)
    }
    pub fn insert(&mut self, path: PathBuf, photo: Arc<Photo>) {
        if let Some(i) = self.entries.iter().position(|(p, _)| p == &path)
            && let Some((_, old)) = self.entries.remove(i)
        {
            self.bytes -= old.byte_len();
        }
        let bytes = photo.byte_len();
        if bytes > self.budget {
            return;
        }
        while self.bytes + bytes > self.budget || self.entries.len() >= 3 {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= old.byte_len();
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.entries.push_back((path, photo));
    }
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

#[derive(Clone, Copy, Debug)]
pub struct View {
    pub zoom: f32,
    pub pan: [f32; 2],
    pub rotation: u8,
    pub fit: bool,
    /// "Actual pixels": one image pixel per physical screen pixel. While set, `zoom` is
    /// re-derived from the current display scale each frame, so moving the window to a monitor
    /// with a different scale factor keeps it exactly 1:1 instead of silently becoming 80% or 150%.
    pub physical: bool,
}
impl Default for View {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: [0.0; 2],
            rotation: 0,
            fit: true,
            physical: false,
        }
    }
}
impl View {
    pub fn dimensions(&self, size: [f32; 2]) -> [f32; 2] {
        if self.rotation % 2 == 1 {
            [size[1], size[0]]
        } else {
            size
        }
    }
    pub fn fitted_scale(&self, image: [f32; 2], viewport: [f32; 2]) -> f32 {
        let image = self.dimensions(image);
        (viewport[0] / image[0].max(1.0))
            .min(viewport[1] / image[1].max(1.0))
            .clamp(0.001, 1.0)
    }
    pub fn scale(&self, image: [f32; 2], viewport: [f32; 2]) -> f32 {
        if self.fit {
            self.fitted_scale(image, viewport)
        } else {
            self.zoom
        }
    }
    pub fn zoom_at(&mut self, factor: f32, cursor: [f32; 2], image: [f32; 2], viewport: [f32; 2]) {
        let previous = self.scale(image, viewport);
        let next = (previous * factor).clamp(0.01, 32.0);
        for (i, coordinate) in cursor.iter().enumerate() {
            self.pan[i] = coordinate - (coordinate - self.pan[i]) * (next / previous);
        }
        self.zoom = next;
        self.fit = false;
        self.physical = false;
    }
    /// Keep the picture from being dragged out of reach. A picture larger than the view may move
    /// until its edge meets the view's edge; one that fits stays centred.
    pub fn clamp_pan(&mut self, image: [f32; 2], viewport: [f32; 2]) {
        let scale = self.scale(image, viewport);
        let shown = self.dimensions(image);
        for i in 0..2 {
            let overflow = ((shown[i] * scale - viewport[i]) / 2.0).max(0.0);
            self.pan[i] = self.pan[i].clamp(-overflow, overflow);
        }
    }
    /// 1:1 with the display's physical pixels, at display scale `pixels_per_point`.
    pub fn actual_physical(&mut self, pixels_per_point: f32) {
        self.actual();
        self.physical = true;
        self.sync_display_scale(pixels_per_point);
    }
    /// Re-derive the zoom of a 1:1 view from the current display scale. A no-op otherwise.
    pub fn sync_display_scale(&mut self, pixels_per_point: f32) {
        if self.physical && pixels_per_point > 0.0 {
            self.zoom = 1.0 / pixels_per_point;
        }
    }
    pub fn rotate(&mut self, clockwise: bool) {
        self.rotation = (self.rotation + if clockwise { 1 } else { 3 }) % 4;
        self.pan = [0.0; 2];
    }
    pub fn fit(&mut self) {
        self.fit = true;
        self.physical = false;
        self.pan = [0.0; 2];
    }
    pub fn actual(&mut self) {
        self.fit = false;
        self.physical = false;
        self.zoom = 1.0;
        self.pan = [0.0; 2];
    }
    /// Texture corners in screen order, rotation without allocating a second bitmap.
    pub fn uv(&self) -> [[f32; 2]; 4] {
        let corners = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        std::array::from_fn(|i| corners[(i + 4 - self.rotation as usize) % 4])
    }
}
