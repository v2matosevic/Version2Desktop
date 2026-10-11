//! A small, bounded reader for the EXIF fields Luma shows. It never writes anything and never
//! trusts offsets: every read is range-checked, IFD walks are depth- and count-limited, and text
//! is length-capped and stripped of control characters.
use std::fmt::Write as _;

const MAX_BLOB: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 512;
const MAX_TEXT: usize = 128;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gps {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Exif {
    pub make: Option<String>,
    pub model: Option<String>,
    pub lens: Option<String>,
    pub software: Option<String>,
    /// "2026-10-09 14:03:22", exactly as the camera recorded it (no time zone is implied).
    pub taken: Option<String>,
    pub exposure_time: Option<(u32, u32)>,
    pub aperture: Option<f64>,
    pub iso: Option<u32>,
    pub focal_length: Option<f64>,
    pub focal_length_35mm: Option<u32>,
    pub exposure_bias: Option<f64>,
    pub flash_fired: Option<bool>,
    pub gps: Option<Gps>,
    /// The raw EXIF orientation value, 1 to 8.
    pub orientation: Option<u16>,
}

impl Exif {
    pub fn is_empty(&self) -> bool {
        *self == Exif::default()
    }

    /// Label and value pairs for the details card, in reading order. Absent fields are left out.
    pub fn rows(&self) -> Vec<(&'static str, String)> {
        let mut rows = Vec::new();
        let camera = match (&self.make, &self.model) {
            (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => {
                Some(model.clone())
            }
            (Some(make), Some(model)) => Some(format!("{make} {model}")),
            (Some(make), None) => Some(make.clone()),
            (None, Some(model)) => Some(model.clone()),
            (None, None) => None,
        };
        if let Some(camera) = camera {
            rows.push(("Camera", camera));
        }
        if let Some(lens) = &self.lens {
            rows.push(("Lens", lens.clone()));
        }
        if let Some(taken) = &self.taken {
            rows.push(("Taken", taken.clone()));
        }
        let mut exposure = Vec::new();
        if let Some((n, d)) = self.exposure_time {
            exposure.push(format_exposure(n, d));
        }
        if let Some(a) = self.aperture {
            exposure.push(format!("f/{}", trim_float(a, 1)));
        }
        if let Some(iso) = self.iso {
            exposure.push(format!("ISO {iso}"));
        }
        if !exposure.is_empty() {
            rows.push(("Exposure", exposure.join(" · ")));
        }
        if let Some(f) = self.focal_length {
            let mut text = format!("{} mm", trim_float(f, 1));
            if let Some(eq) = self.focal_length_35mm.filter(|eq| *eq > 0) {
                let _ = write!(text, " ({eq} mm equivalent)");
            }
            rows.push(("Focal length", text));
        }
        if let Some(bias) = self.exposure_bias.filter(|b| b.abs() > 0.04) {
            rows.push(("Exposure bias", format!("{bias:+.1} EV")));
        }
        if let Some(fired) = self.flash_fired {
            rows.push((
                "Flash",
                if fired { "Fired" } else { "Did not fire" }.to_owned(),
            ));
        }
        if let Some(gps) = &self.gps {
            let mut text = format!(
                "{:.5}° {}, {:.5}° {}",
                gps.latitude.abs(),
                if gps.latitude >= 0.0 { "N" } else { "S" },
                gps.longitude.abs(),
                if gps.longitude >= 0.0 { "E" } else { "W" }
            );
            if let Some(alt) = gps.altitude {
                let _ = write!(text, " · {} m", trim_float(alt, 0));
            }
            rows.push(("Location", text));
        }
        if let Some(software) = &self.software {
            rows.push(("Software", software.clone()));
        }
        rows
    }
}

fn trim_float(value: f64, places: usize) -> String {
    let text = format!("{value:.places$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

fn format_exposure(n: u32, d: u32) -> String {
    if d == 0 || n == 0 {
        return "—".into();
    }
    let seconds = f64::from(n) / f64::from(d);
    if seconds < 0.5 {
        format!("1/{} s", (1.0 / seconds).round() as u64)
    } else {
        format!("{} s", trim_float(seconds, 1))
    }
}

struct Reader<'a> {
    data: &'a [u8],
    little: bool,
}

#[derive(Clone, Copy)]
struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    /// Absolute offset of the value bytes inside the blob.
    at: usize,
}

impl Reader<'_> {
    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.data.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }
    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.data.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }
    fn entry(&self, at: usize) -> Option<Entry> {
        let tag = self.u16(at)?;
        let kind = self.u16(at + 2)?;
        let count = self.u32(at + 4)?;
        let unit = match kind {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 => 4,
            5 | 10 | 12 => 8,
            _ => return None,
        };
        let total = (count as usize).checked_mul(unit)?;
        let value_at = if total <= 4 {
            at + 8
        } else {
            self.u32(at + 8)? as usize
        };
        // The whole value must lie inside the blob.
        self.data.get(value_at..value_at.checked_add(total)?)?;
        Some(Entry {
            tag,
            kind,
            count,
            at: value_at,
        })
    }
    fn text(&self, e: Entry) -> Option<String> {
        if e.kind != 2 && e.kind != 7 {
            return None;
        }
        let len = (e.count as usize).min(MAX_TEXT * 4);
        let raw = self.data.get(e.at..e.at + len)?;
        let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
        let text: String = String::from_utf8_lossy(&raw[..end])
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_TEXT)
            .collect();
        let text = text.trim().to_owned();
        (!text.is_empty()).then_some(text)
    }
    fn short(&self, e: Entry) -> Option<u16> {
        (e.kind == 3 && e.count >= 1)
            .then(|| self.u16(e.at))
            .flatten()
    }
    fn long(&self, e: Entry) -> Option<u32> {
        match e.kind {
            3 => self.short(e).map(u32::from),
            4 if e.count >= 1 => self.u32(e.at),
            _ => None,
        }
    }
    fn rational(&self, e: Entry, index: usize) -> Option<(u32, u32)> {
        if e.kind != 5 || index >= e.count as usize {
            return None;
        }
        let at = e.at + index * 8;
        Some((self.u32(at)?, self.u32(at + 4)?))
    }
    fn srational(&self, e: Entry) -> Option<f64> {
        if e.kind != 10 || e.count < 1 {
            return None;
        }
        let n = self.u32(e.at)? as i32;
        let d = self.u32(e.at + 4)? as i32;
        (d != 0).then(|| f64::from(n) / f64::from(d))
    }
    fn real(&self, e: Entry, index: usize) -> Option<f64> {
        let (n, d) = self.rational(e, index)?;
        (d != 0).then(|| f64::from(n) / f64::from(d))
    }
    fn entries(&self, ifd: usize) -> Option<Vec<Entry>> {
        let n = self.u16(ifd)? as usize;
        if n > MAX_ENTRIES {
            return None;
        }
        // A short or damaged table yields what is readable rather than nothing.
        Some(
            (0..n)
                .filter_map(|i| self.entry(ifd + 2 + i * 12))
                .collect(),
        )
    }
}

/// Check the TIFF header of a blob (with or without the leading "Exif\0\0") and open a reader.
fn open(blob: &[u8]) -> Option<Reader<'_>> {
    if blob.len() > MAX_BLOB {
        return None;
    }
    let data = blob.strip_prefix(b"Exif\0\0").unwrap_or(blob);
    let little = match data.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let r = Reader { data, little };
    (r.u16(2)? == 42).then_some(r)
}

/// True when the first IFD of a TIFF-structured file carries a DNGVersion tag (0xC612), which is
/// what makes a TIFF a DNG: a file's name proves nothing. Bounded like the rest of this reader.
pub fn is_dng(blob: &[u8]) -> bool {
    let Some(r) = open(blob) else {
        return false;
    };
    let Some(entries) = r.u32(4).and_then(|at| r.entries(at as usize)) else {
        return false;
    };
    entries.iter().any(|e| e.tag == 0xC612)
}

/// Parse the TIFF structure of an EXIF blob (with or without the leading "Exif\0\0").
pub fn parse(blob: &[u8]) -> Option<Exif> {
    let r = open(blob)?;
    let mut out = Exif::default();
    let mut exif_ifd = None;
    let mut gps_ifd = None;
    let mut datetime = None;
    for e in r.entries(r.u32(4)? as usize)? {
        match e.tag {
            0x010F => out.make = r.text(e),
            0x0110 => out.model = r.text(e),
            0x0112 => out.orientation = r.short(e).filter(|o| (1..=8).contains(o)),
            0x0131 => out.software = r.text(e),
            0x0132 => datetime = r.text(e),
            0x8769 => exif_ifd = r.long(e),
            0x8825 => gps_ifd = r.long(e),
            _ => {}
        }
    }
    let mut taken = None;
    if let Some(ifd) = exif_ifd.and_then(|o| r.entries(o as usize)) {
        for e in ifd {
            match e.tag {
                0x829A => out.exposure_time = r.rational(e, 0).filter(|(n, d)| *n > 0 && *d > 0),
                0x829D => out.aperture = r.real(e, 0).filter(|a| a.is_finite() && *a > 0.0),
                0x8827 => out.iso = r.long(e).filter(|i| *i > 0),
                0x9003 => taken = r.text(e),
                0x9204 => out.exposure_bias = r.srational(e).filter(|b| b.is_finite()),
                0x9209 => out.flash_fired = r.short(e).map(|f| f & 1 == 1),
                0x920A => out.focal_length = r.real(e, 0).filter(|f| f.is_finite() && *f > 0.0),
                0xA405 => out.focal_length_35mm = r.long(e).filter(|f| *f > 0),
                0xA434 => out.lens = r.text(e),
                _ => {}
            }
        }
    }
    out.taken = taken.or(datetime).map(normalise_datetime);
    out.gps = gps_ifd.and_then(|o| read_gps(&r, o as usize));
    (!out.is_empty()).then_some(out)
}

fn normalise_datetime(text: String) -> String {
    // "2026:10:09 14:03:22" is the EXIF form; show it as a plain ISO-style date.
    let b = text.as_bytes();
    if b.len() >= 19 && b[4] == b':' && b[7] == b':' {
        format!(
            "{}-{}-{}{}",
            &text[..4],
            &text[5..7],
            &text[8..10],
            &text[10..]
        )
    } else {
        text
    }
}

fn read_gps(r: &Reader, ifd: usize) -> Option<Gps> {
    let mut lat = None;
    let mut lon = None;
    let (mut lat_ref, mut lon_ref) = (None, None);
    let (mut alt, mut alt_ref) = (None, 0u8);
    for e in r.entries(ifd)? {
        match e.tag {
            1 => lat_ref = r.text(e).and_then(|t| t.chars().next()),
            2 => lat = dms(r, e),
            3 => lon_ref = r.text(e).and_then(|t| t.chars().next()),
            4 => lon = dms(r, e),
            5 => alt_ref = r.data.get(e.at).copied().unwrap_or(0),
            6 => alt = r.real(e, 0).filter(|a| a.is_finite()),
            // GPSStatus: "V" means the receiver had no valid fix; the coordinates are not a position.
            9 => {
                if matches!(r.text(e).and_then(|t| t.chars().next()), Some('V' | 'v')) {
                    return None;
                }
            }
            _ => {}
        }
    }
    let mut latitude = lat?;
    let mut longitude = lon?;
    if matches!(lat_ref, Some('S' | 's')) {
        latitude = -latitude;
    }
    if matches!(lon_ref, Some('W' | 'w')) {
        longitude = -longitude;
    }
    if !(-90.0..=90.0).contains(&latitude) || !(-180.0..=180.0).contains(&longitude) {
        return None;
    }
    Some(Gps {
        latitude,
        longitude,
        altitude: alt.map(|a| if alt_ref == 1 { -a } else { a }),
    })
}

fn dms(r: &Reader, e: Entry) -> Option<f64> {
    if e.kind != 5 || e.count < 3 {
        return None;
    }
    // Zero denominators fail inside `real`; minutes and seconds must be proper sexagesimal parts,
    // otherwise damaged metadata would be normalised into a plausible-looking place.
    let (d, m, s) = (r.real(e, 0)?, r.real(e, 1)?, r.real(e, 2)?);
    let proper = [d, m, s].iter().all(|v| v.is_finite())
        && (0.0..=180.0).contains(&d)
        && (0.0..60.0).contains(&m)
        && (0.0..60.0).contains(&s);
    proper.then(|| d + m / 60.0 + s / 3600.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a TIFF/EXIF blob so the parser is checked against bytes written independently of it.
    struct Builder {
        little: bool,
        bytes: Vec<u8>,
    }
    impl Builder {
        fn u16(&self, v: u16) -> [u8; 2] {
            if self.little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        }
        fn u32(&self, v: u32) -> [u8; 4] {
            if self.little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        }
        fn ifd(&mut self, entries: &[(u16, u16, u32, Vec<u8>)]) -> u32 {
            // Entries first, then out-of-line values after the table.
            let start = self.bytes.len() as u32;
            let table = 2 + entries.len() * 12 + 4;
            let mut tail = start as usize + table;
            let mut out: Vec<u8> = Vec::new();
            out.extend(self.u16(entries.len() as u16));
            let mut extra: Vec<u8> = Vec::new();
            for (tag, kind, count, value) in entries {
                out.extend(self.u16(*tag));
                out.extend(self.u16(*kind));
                out.extend(self.u32(*count));
                if value.len() <= 4 {
                    let mut v = value.clone();
                    v.resize(4, 0);
                    out.extend(v);
                } else {
                    out.extend(self.u32(tail as u32));
                    tail += value.len();
                    extra.extend(value);
                }
            }
            out.extend([0u8; 4]);
            out.extend(extra);
            self.bytes.extend(out);
            start
        }
    }
    fn rational(b: &Builder, n: u32, d: u32) -> Vec<u8> {
        [b.u32(n), b.u32(d)].concat()
    }
    /// GPS fields a test can corrupt; the default is a valid position.
    struct GpsSpec {
        lat: [(u32, u32); 3],
        lon: [(u32, u32); 3],
        lat_ref: &'static [u8],
        lon_ref: &'static [u8],
        status: Option<&'static [u8]>,
    }
    impl Default for GpsSpec {
        fn default() -> Self {
            Self {
                lat: [(44, 1), (6, 1), (3000, 100)],
                lon: [(15, 1), (13, 1), (1200, 100)],
                lat_ref: b"N\0",
                lon_ref: b"W\0",
                status: None,
            }
        }
    }
    fn sample(little: bool) -> Vec<u8> {
        sample_with(little, GpsSpec::default())
    }
    fn sample_with(little: bool, spec: GpsSpec) -> Vec<u8> {
        let mut b = Builder {
            little,
            bytes: if little {
                b"II".to_vec()
            } else {
                b"MM".to_vec()
            },
        };
        let magic = b.u16(42);
        b.bytes.extend(magic);
        let first = b.u32(8);
        b.bytes.extend(first);
        // Layout: IFD0 at 8, then ExifIFD, then GPS IFD; pointers patched below.
        let ifd0_at = b.bytes.len();
        let placeholder = |b: &Builder, v: u32| b.u32(v).to_vec();
        let make = b"Version2\0".to_vec();
        let model = b"Version2 Cam X\0".to_vec();
        let ifd0 = vec![
            (0x010F, 2, make.len() as u32, make),
            (0x0110, 2, model.len() as u32, model),
            (0x0112, 3, 1, b.u16(6).to_vec()),
            (0x0131, 2, 9, b"Luma 0.2\0".to_vec()),
            (0x8769, 4, 1, placeholder(&b, 0)),
            (0x8825, 4, 1, placeholder(&b, 0)),
        ];
        b.ifd(&ifd0);
        let exif_at = b.bytes.len() as u32;
        let lens = b"Prime 35mm f/1.8\0".to_vec();
        let taken = b"2026:10:09 14:03:22\0".to_vec();
        let exif = vec![
            (0x829A, 5, 1, rational(&b, 1, 250)),
            (0x829D, 5, 1, rational(&b, 18, 10)),
            (0x8827, 3, 1, b.u16(400).to_vec()),
            (0x9003, 2, taken.len() as u32, taken),
            (0x9204, 10, 1, [b.u32(-7i32 as u32), b.u32(10)].concat()),
            (0x9209, 3, 1, b.u16(1).to_vec()),
            (0x920A, 5, 1, rational(&b, 35, 1)),
            (0xA405, 3, 1, b.u16(52).to_vec()),
            (0xA434, 2, lens.len() as u32, lens),
        ];
        b.ifd(&exif);
        let gps_at = b.bytes.len() as u32;
        let triple = |b: &Builder, v: [(u32, u32); 3]| -> Vec<u8> {
            v.iter().flat_map(|(n, d)| rational(b, *n, *d)).collect()
        };
        let mut gps = vec![
            (1, 2, 2, spec.lat_ref.to_vec()),
            (2, 5, 3, triple(&b, spec.lat)),
            (3, 2, 2, spec.lon_ref.to_vec()),
            (4, 5, 3, triple(&b, spec.lon)),
            (5, 1, 1, vec![1]),
            (6, 5, 1, rational(&b, 1234, 100)),
        ];
        if let Some(status) = spec.status {
            gps.push((9, 2, 2, status.to_vec()));
        }
        b.ifd(&gps);
        // Patch the two IFD pointers inside IFD0 (entries 4 and 5).
        for (i, value) in [(4usize, exif_at), (5, gps_at)] {
            let at = ifd0_at + 2 + i * 12 + 8;
            let bytes = b.u32(value);
            b.bytes[at..at + 4].copy_from_slice(&bytes);
        }
        b.bytes
    }
    #[test]
    fn reads_camera_exposure_and_location_in_both_byte_orders() {
        for little in [true, false] {
            let exif = parse(&sample(little)).expect("blob parses");
            assert_eq!(exif.make.as_deref(), Some("Version2"));
            assert_eq!(exif.model.as_deref(), Some("Version2 Cam X"));
            assert_eq!(exif.orientation, Some(6));
            assert_eq!(exif.exposure_time, Some((1, 250)));
            assert_eq!(exif.iso, Some(400));
            assert_eq!(exif.taken.as_deref(), Some("2026-10-09 14:03:22"));
            assert_eq!(exif.lens.as_deref(), Some("Prime 35mm f/1.8"));
            assert_eq!(exif.flash_fired, Some(true));
            assert!((exif.exposure_bias.unwrap() + 0.7).abs() < 1e-9);
            let gps = exif.gps.clone().expect("gps");
            assert!((gps.latitude - (44.0 + 6.0 / 60.0 + 30.0 / 3600.0)).abs() < 1e-9);
            assert!((gps.longitude + (15.0 + 13.0 / 60.0 + 12.0 / 3600.0)).abs() < 1e-9);
            assert_eq!(gps.altitude, Some(-12.34));
            let rows = exif.rows();
            let get = |k: &str| rows.iter().find(|(l, _)| *l == k).map(|(_, v)| v.as_str());
            assert_eq!(get("Camera"), Some("Version2 Cam X"));
            assert_eq!(get("Exposure"), Some("1/250 s · f/1.8 · ISO 400"));
            assert_eq!(get("Focal length"), Some("35 mm (52 mm equivalent)"));
            assert_eq!(get("Exposure bias"), Some("-0.7 EV"));
            assert!(get("Location").unwrap().contains("W"));
        }
    }
    /// A TIFF whose first IFD holds exactly `entries`, in either byte order.
    fn tiff_with(little: bool, entries: &[(u16, u16, u32, Vec<u8>)]) -> Vec<u8> {
        let mut b = Builder {
            little,
            bytes: if little {
                b"II".to_vec()
            } else {
                b"MM".to_vec()
            },
        };
        let magic = b.u16(42);
        b.bytes.extend(magic);
        let first = b.u32(8);
        b.bytes.extend(first);
        b.ifd(entries);
        b.bytes
    }
    #[test]
    fn a_dng_is_recognised_by_its_dngversion_tag_not_by_its_name() {
        for little in [true, false] {
            let b = Builder {
                little,
                bytes: vec![],
            };
            let orientation = (0x0112, 3, 1, b.u16(6).to_vec());
            let dng = tiff_with(
                little,
                &[orientation.clone(), (0xC612, 1, 4, vec![1, 4, 0, 0])],
            );
            assert!(is_dng(&dng), "little={little}");
            // The orientation is still read from a DNG, and nothing about it is DNG-specific.
            assert_eq!(parse(&dng).and_then(|e| e.orientation), Some(6));
            // A plain TIFF/EXIF structure (like a CR2 or NEF header) is not called a DNG.
            let plain = tiff_with(little, &[orientation]);
            assert!(!is_dng(&plain), "little={little}");
        }
        // Hostile or empty input is simply "not a DNG".
        assert!(!is_dng(b""));
        assert!(!is_dng(b"II*\0\xff\xff\xff\xff"));
        assert!(!is_dng(&[0xFF; 64]));
        assert!(!is_dng(&sample(true)));
    }
    #[test]
    fn accepts_the_jpeg_style_exif_prefix() {
        let mut blob = b"Exif\0\0".to_vec();
        blob.extend(sample(true));
        assert!(parse(&blob).is_some());
    }
    #[test]
    fn rejects_garbage_truncation_and_hostile_offsets_without_panicking() {
        assert!(parse(b"").is_none());
        assert!(parse(b"not exif at all").is_none());
        assert!(parse(&[0xFF; 64]).is_none());
        let good = sample(true);
        // Every truncation of a valid blob must return None or a partial result, never panic.
        for len in 0..good.len() {
            let _ = parse(&good[..len]);
        }
        // Offset pointing far outside the blob.
        let mut bad = good.clone();
        bad[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse(&bad).is_none());
        // An IFD with an enormous entry count is refused, not looped over.
        let mut loopy = b"II*\0\x08\0\0\0".to_vec();
        loopy.extend(0xFFFFu16.to_le_bytes());
        assert!(parse(&loopy).is_none());
        // A pointer cycle (ExifIFD pointing back at IFD0) cannot recurse: the walk is two-level.
        let mut cyc = sample(true);
        let at = 8 + 2 + 4 * 12 + 8;
        cyc[at..at + 4].copy_from_slice(&8u32.to_le_bytes());
        let _ = parse(&cyc);
    }
    #[test]
    fn gps_zero_zero_is_a_position_but_a_void_fix_is_not() {
        let at_origin = GpsSpec {
            lat: [(0, 1), (0, 1), (0, 1)],
            lon: [(0, 1), (0, 1), (0, 1)],
            lat_ref: b"N\0",
            lon_ref: b"E\0",
            status: Some(b"A\0"),
        };
        let gps = parse(&sample_with(true, at_origin)).unwrap().gps.unwrap();
        assert_eq!((gps.latitude, gps.longitude), (0.0, 0.0));
        let void = GpsSpec {
            status: Some(b"V\0"),
            ..GpsSpec::default()
        };
        assert!(parse(&sample_with(true, void)).unwrap().gps.is_none());
    }
    #[test]
    fn malformed_gps_is_rejected_rather_than_normalised() {
        let cases: [(&str, GpsSpec); 6] = [
            (
                "minutes of 75",
                GpsSpec {
                    lat: [(44, 1), (75, 1), (0, 1)],
                    ..GpsSpec::default()
                },
            ),
            (
                "seconds of 60",
                GpsSpec {
                    lon: [(15, 1), (13, 1), (60, 1)],
                    ..GpsSpec::default()
                },
            ),
            (
                "zero denominator",
                GpsSpec {
                    lat: [(44, 0), (6, 1), (0, 1)],
                    ..GpsSpec::default()
                },
            ),
            (
                "latitude above 90",
                GpsSpec {
                    lat: [(91, 1), (0, 1), (0, 1)],
                    ..GpsSpec::default()
                },
            ),
            (
                "longitude above 180",
                GpsSpec {
                    lon: [(181, 1), (0, 1), (0, 1)],
                    ..GpsSpec::default()
                },
            ),
            (
                "degrees overflow",
                GpsSpec {
                    lat: [(u32::MAX, 1), (0, 1), (0, 1)],
                    ..GpsSpec::default()
                },
            ),
        ];
        for (name, spec) in cases {
            let exif = parse(&sample_with(true, spec)).expect("the rest of the blob still parses");
            assert!(exif.gps.is_none(), "{name} must not produce a location");
            assert_eq!(exif.make.as_deref(), Some("Version2"), "{name}");
        }
        // Boundary: 90 degrees exactly and 59'59.99" are valid.
        let edge = GpsSpec {
            lat: [(89, 1), (59, 1), (5999, 100)],
            lon: [(179, 1), (59, 1), (5999, 100)],
            ..GpsSpec::default()
        };
        assert!(parse(&sample_with(true, edge)).unwrap().gps.is_some());
    }
    #[test]
    fn exposure_text_reads_like_a_camera() {
        assert_eq!(format_exposure(1, 250), "1/250 s");
        assert_eq!(format_exposure(10, 10), "1 s");
        assert_eq!(format_exposure(5, 2), "2.5 s");
        assert_eq!(format_exposure(1, 3), "1/3 s");
        assert_eq!(format_exposure(0, 1), "—");
    }
}
