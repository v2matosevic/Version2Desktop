//! A bounded ISO-BMFF (HEIF) reader. Luma only needs three things from a HEIC file that the
//! Windows codec does not hand over: the primary image's stored size and container rotation (to
//! check the codec's output orientation), and the EXIF item. Everything is range-checked and
//! size-capped; the file is never read whole and never written.
use std::io::{Read, Seek, SeekFrom};

const MAX_META: u64 = 16 * 1024 * 1024;
const MAX_EXIF: u64 = 1024 * 1024;
const MAX_TOP_LEVEL_BOXES: usize = 64;
const MAX_ITEMS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// Anticlockwise quarter turns (`irot`), 1 to 3.
    Rotate(u8),
    /// `imir`, as stored: 0 exchanges top and bottom, 1 exchanges left and right (ISO/IEC
    /// 23008-12:2022 as quoted in libavif's `avifImageMirror`, and what the Windows HEIF decoder
    /// does to the pixels; see tests/windows_codecs.rs).
    Mirror(u8),
}

#[derive(Debug, Default, Clone)]
pub struct HeifInfo {
    pub major_brand: [u8; 4],
    /// Stored size of the primary item, before any transform.
    pub stored_size: Option<(u32, u32)>,
    /// Transform properties of the primary item, in the order the file applies them.
    pub transforms: Vec<Transform>,
    /// The raw EXIF TIFF structure, with the HEIF header offset already skipped.
    pub exif: Option<Vec<u8>>,
}

impl HeifInfo {
    /// Whether the container rotation turns the picture a quarter or three quarters.
    pub fn swaps_axes(&self) -> bool {
        self.transforms
            .iter()
            .filter(|t| matches!(t, Transform::Rotate(n) if n % 2 == 1))
            .count()
            % 2
            == 1
    }
    pub fn is_identity(&self) -> bool {
        self.transforms.is_empty()
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let out = self.data.get(self.at..end)?;
        self.at = end;
        Some(out)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// A big-endian unsigned integer of 0, 4 or 8 bytes (iloc field sizes).
    fn sized(&mut self, size: u8) -> Option<u64> {
        match size {
            0 => Some(0),
            4 => self.u32().map(u64::from),
            8 => {
                let b = self.take(8)?;
                Some(u64::from_be_bytes(b.try_into().ok()?))
            }
            _ => None,
        }
    }
}

/// Iterate the boxes in `data`: (type, payload). Stops at the first malformed box.
fn boxes(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut c = Cursor { data, at: 0 };
    while c.at + 8 <= data.len() && out.len() < MAX_ITEMS {
        let Some(size) = c.u32() else { break };
        let Some(kind) = c.take(4) else { break };
        let kind: [u8; 4] = kind.try_into().unwrap();
        let mut header = 8usize;
        let mut size = u64::from(size);
        if size == 1 {
            let Some(b) = c.take(8) else { break };
            size = u64::from_be_bytes(b.try_into().unwrap());
            header = 16;
        } else if size == 0 {
            size = (data.len() - c.at + header) as u64;
        }
        let Some(payload_len) = size.checked_sub(header as u64) else {
            break;
        };
        let Some(payload) = c.take(payload_len as usize) else {
            break;
        };
        out.push((kind, payload));
    }
    out
}

struct Extent {
    offset: u64,
    length: u64,
}

/// Read one top-level box header at `pos`: (box size, kind, header length). Uses exact reads, so
/// a reader that returns short counts cannot be mistaken for a truncated file.
fn top_level_header<R: Read + Seek>(
    file: &mut R,
    pos: u64,
    end: u64,
) -> Result<Option<(u64, [u8; 4], u64)>, String> {
    if end.saturating_sub(pos) < 8 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
    let mut first = [0u8; 8];
    file.read_exact(&mut first).map_err(|e| e.to_string())?;
    let mut size = u64::from(u32::from_be_bytes(first[0..4].try_into().unwrap()));
    let kind: [u8; 4] = first[4..8].try_into().unwrap();
    let mut head = 8u64;
    if size == 1 {
        if end - pos < 16 {
            return Err("HEIF extended box header is truncated.".into());
        }
        let mut extended = [0u8; 8];
        file.read_exact(&mut extended).map_err(|e| e.to_string())?;
        size = u64::from_be_bytes(extended);
        head = 16;
    } else if size == 0 {
        size = end - pos;
    }
    // `pos + size` can overflow for a hostile extended size; check the sum once, here.
    let box_end = pos
        .checked_add(size)
        .ok_or("HEIF box size overflows the file.")?;
    if size < head || box_end > end {
        return Err("HEIF box sizes are inconsistent.".into());
    }
    Ok(Some((size, kind, head)))
}

pub fn read<R: Read + Seek>(file: &mut R) -> Result<HeifInfo, String> {
    let end = file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    let mut pos = 0u64;
    let mut info = HeifInfo::default();
    for _ in 0..MAX_TOP_LEVEL_BOXES {
        let Some((size, kind, head)) = top_level_header(file, pos, end)? else {
            break;
        };
        if &kind == b"ftyp" && size >= head + 4 {
            // The major brand follows the header, which is 16 bytes long for an extended box.
            file.seek(SeekFrom::Start(pos + head))
                .map_err(|e| e.to_string())?;
            file.read_exact(&mut info.major_brand)
                .map_err(|e| e.to_string())?;
        }
        if &kind == b"meta" {
            let len = size - head;
            if len > MAX_META {
                return Err("HEIF metadata box is unreasonably large.".into());
            }
            file.seek(SeekFrom::Start(pos + head))
                .map_err(|e| e.to_string())?;
            let mut meta = vec![0u8; len as usize];
            file.read_exact(&mut meta).map_err(|e| e.to_string())?;
            let exif_extent = parse_meta(&meta, &mut info);
            if let Some(extent) = exif_extent
                && extent.length >= 8
                && extent.length <= MAX_EXIF
                && extent
                    .offset
                    .checked_add(extent.length)
                    .is_some_and(|e| e <= end)
            {
                file.seek(SeekFrom::Start(extent.offset))
                    .map_err(|e| e.to_string())?;
                let mut item = vec![0u8; extent.length as usize];
                file.read_exact(&mut item).map_err(|e| e.to_string())?;
                // exif_tiff_header_offset counts bytes after the 4-byte field itself.
                let skip = u32::from_be_bytes(item[..4].try_into().unwrap()) as usize;
                if let Some(tiff) = item.get(4usize.saturating_add(skip)..)
                    && !tiff.is_empty()
                {
                    info.exif = Some(tiff.to_vec());
                }
            }
            break;
        }
        pos += size;
    }
    Ok(info)
}

fn parse_meta(meta: &[u8], info: &mut HeifInfo) -> Option<Extent> {
    // `meta` is a FullBox: four bytes of version and flags, then child boxes.
    let children = boxes(meta.get(4..)?);
    let mut primary: Option<u32> = None;
    let mut exif_item: Option<u32> = None;
    let mut first_item: Option<u32> = None;
    let mut properties: Vec<([u8; 4], &[u8])> = Vec::new();
    let mut associations: Vec<(u32, Vec<u16>)> = Vec::new();
    let mut locations: Vec<(u32, u16, Option<Extent>)> = Vec::new();
    for (kind, payload) in &children {
        match kind {
            b"pitm" => {
                let mut c = Cursor {
                    data: payload,
                    at: 0,
                };
                let version = c.u8()?;
                c.take(3)?;
                primary = Some(if version == 0 {
                    u32::from(c.u16()?)
                } else {
                    c.u32()?
                });
            }
            b"iinf" => {
                let mut c = Cursor {
                    data: payload,
                    at: 0,
                };
                let version = c.u8()?;
                c.take(3)?;
                if version == 0 {
                    c.u16()?;
                } else {
                    c.u32()?;
                }
                for (k, p) in boxes(payload.get(c.at..)?) {
                    if &k != b"infe" {
                        continue;
                    }
                    let mut e = Cursor { data: p, at: 0 };
                    let v = e.u8()?;
                    e.take(3)?;
                    if v < 2 {
                        continue;
                    }
                    let id = if v == 2 {
                        u32::from(e.u16()?)
                    } else {
                        e.u32()?
                    };
                    e.u16()?;
                    let item_type = e.take(4)?;
                    first_item.get_or_insert(id);
                    if item_type == b"Exif" {
                        exif_item = Some(id);
                    }
                }
            }
            b"iloc" => locations = parse_iloc(payload)?,
            b"iprp" => {
                for (k, p) in boxes(payload) {
                    if &k == b"ipco" {
                        properties = boxes(p);
                    } else if &k == b"ipma" {
                        associations = parse_ipma(p)?;
                    }
                }
            }
            _ => {}
        }
    }
    let primary = primary.or(first_item)?;
    if let Some((_, indexes)) = associations.iter().find(|(id, _)| *id == primary) {
        for index in indexes {
            // Property indexes are 1-based; zero means "no property".
            let Some((kind, payload)) = (*index as usize)
                .checked_sub(1)
                .and_then(|i| properties.get(i))
            else {
                continue;
            };
            match kind {
                b"ispe" if payload.len() >= 12 => {
                    let w = u32::from_be_bytes(payload[4..8].try_into().ok()?);
                    let h = u32::from_be_bytes(payload[8..12].try_into().ok()?);
                    info.stored_size = Some((w, h));
                }
                b"irot" if !payload.is_empty() => {
                    let turns = payload[0] & 3;
                    if turns != 0 {
                        info.transforms.push(Transform::Rotate(turns));
                    }
                }
                b"imir" if !payload.is_empty() => {
                    info.transforms.push(Transform::Mirror(payload[0] & 1));
                }
                _ => {}
            }
        }
    }
    let exif_item = exif_item?;
    locations
        .into_iter()
        .find(|(id, construction, _)| *id == exif_item && *construction == 0)
        .and_then(|(_, _, extent)| extent)
}

fn parse_ipma(payload: &[u8]) -> Option<Vec<(u32, Vec<u16>)>> {
    let mut c = Cursor {
        data: payload,
        at: 0,
    };
    let version = c.u8()?;
    let flags = c.take(3)?;
    let wide = flags[2] & 1 == 1;
    let count = c.u32()? as usize;
    let mut out = Vec::new();
    for _ in 0..count.min(MAX_ITEMS) {
        let id = if version < 1 {
            u32::from(c.u16()?)
        } else {
            c.u32()?
        };
        let n = c.u8()?;
        let mut indexes = Vec::new();
        for _ in 0..n {
            indexes.push(if wide {
                c.u16()? & 0x7FFF
            } else {
                u16::from(c.u8()? & 0x7F)
            });
        }
        out.push((id, indexes));
    }
    Some(out)
}

fn parse_iloc(payload: &[u8]) -> Option<Vec<(u32, u16, Option<Extent>)>> {
    let mut c = Cursor {
        data: payload,
        at: 0,
    };
    let version = c.u8()?;
    c.take(3)?;
    let sizes = c.u8()?;
    let (offset_size, length_size) = (sizes >> 4, sizes & 15);
    let sizes = c.u8()?;
    let (base_size, index_size) = (sizes >> 4, if version >= 1 { sizes & 15 } else { 0 });
    let count = if version < 2 {
        u32::from(c.u16()?)
    } else {
        c.u32()?
    } as usize;
    let mut out = Vec::new();
    for _ in 0..count.min(MAX_ITEMS) {
        let id = if version < 2 {
            u32::from(c.u16()?)
        } else {
            c.u32()?
        };
        let construction = if version >= 1 { c.u16()? & 15 } else { 0 };
        c.u16()?; // data_reference_index
        let base = c.sized(base_size)?;
        let extents = c.u16()?;
        let mut first = None;
        for n in 0..extents {
            if index_size > 0 {
                c.sized(index_size)?;
            }
            let offset = c.sized(offset_size)?;
            let length = c.sized(length_size)?;
            if n == 0 {
                first = Some(Extent {
                    offset: base.checked_add(offset)?,
                    length,
                });
            }
        }
        // Only single-extent items are read; a split Exif item is ignored rather than guessed at.
        out.push((id, construction, if extents == 1 { first } else { None }));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor as IoCursor;

    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend(kind);
        out.extend(payload);
        out
    }
    fn full(kind: &[u8; 4], version: u8, flags: [u8; 3], body: &[u8]) -> Vec<u8> {
        let mut p = vec![version];
        p.extend(flags);
        p.extend(body);
        boxed(kind, &p)
    }
    /// A HEIC-shaped file: ftyp, meta (pitm, iinf, iloc, iprp) and an mdat holding EXIF.
    fn sample(rotation: Option<u8>, with_exif: bool) -> Vec<u8> {
        let ftyp = boxed(b"ftyp", b"heic\0\0\0\0mif1heic");
        let pitm = full(b"pitm", 0, [0; 3], &1u16.to_be_bytes());
        let mut infe1 = vec![];
        infe1.extend(1u16.to_be_bytes());
        infe1.extend(0u16.to_be_bytes());
        infe1.extend(b"hvc1");
        infe1.push(0);
        let mut infe2 = vec![];
        infe2.extend(2u16.to_be_bytes());
        infe2.extend(0u16.to_be_bytes());
        infe2.extend(b"Exif");
        infe2.push(0);
        let mut iinf_body = vec![];
        iinf_body.extend((if with_exif { 2u16 } else { 1 }).to_be_bytes());
        iinf_body.extend(full(b"infe", 2, [0; 3], &infe1));
        if with_exif {
            iinf_body.extend(full(b"infe", 2, [0; 3], &infe2));
        }
        let iinf = full(b"iinf", 0, [0; 3], &iinf_body);
        // ispe 64 x 48 plus an optional irot.
        let mut ispe_body = vec![];
        ispe_body.extend(64u32.to_be_bytes());
        ispe_body.extend(48u32.to_be_bytes());
        let mut ipco = full(b"ispe", 0, [0; 3], &ispe_body);
        let mut assoc = vec![1u8];
        if let Some(turns) = rotation {
            ipco.extend(boxed(b"irot", &[turns]));
            assoc.push(2);
        }
        let mut ipma_body = vec![];
        ipma_body.extend(1u32.to_be_bytes());
        ipma_body.extend(1u16.to_be_bytes());
        ipma_body.push(assoc.len() as u8);
        ipma_body.extend(&assoc);
        let iprp = boxed(
            b"iprp",
            &[boxed(b"ipco", &ipco), full(b"ipma", 0, [0; 3], &ipma_body)].concat(),
        );
        // The EXIF item starts after meta; offsets are patched once sizes are known.
        let exif_payload = {
            let mut tiff = b"II*\0\x08\0\0\0\0\0\0\0\0\0".to_vec();
            tiff.truncate(14);
            let mut item = 0u32.to_be_bytes().to_vec(); // exif_tiff_header_offset
            item.extend(tiff);
            item
        };
        let build = |offset: u32| {
            let mut iloc_body = vec![0x44u8, 0x00];
            iloc_body.extend((if with_exif { 1u16 } else { 0 }).to_be_bytes());
            if with_exif {
                iloc_body.extend(2u16.to_be_bytes());
                iloc_body.extend(0u16.to_be_bytes());
                iloc_body.extend(1u16.to_be_bytes());
                iloc_body.extend(offset.to_be_bytes());
                iloc_body.extend((exif_payload.len() as u32).to_be_bytes());
            }
            let iloc = full(b"iloc", 0, [0; 3], &iloc_body);
            let meta_body = [pitm.clone(), iinf.clone(), iloc, iprp.clone()].concat();
            full(b"meta", 0, [0; 3], &meta_body)
        };
        let meta_len = build(0).len();
        let offset = (ftyp.len() + meta_len + 8) as u32; // after the mdat header
        let meta = build(offset);
        let mdat = boxed(b"mdat", &exif_payload);
        [ftyp, meta, mdat].concat()
    }

    #[test]
    fn reads_primary_size_rotation_and_exif_item() {
        let bytes = sample(Some(3), true);
        let info = read(&mut IoCursor::new(bytes)).unwrap();
        assert_eq!(&info.major_brand, b"heic");
        assert_eq!(info.stored_size, Some((64, 48)));
        assert_eq!(info.transforms, vec![Transform::Rotate(3)]);
        assert!(info.swaps_axes());
        let exif = info.exif.expect("exif item");
        assert_eq!(&exif[..2], b"II");
    }
    #[test]
    fn no_rotation_or_exif_is_an_identity() {
        let info = read(&mut IoCursor::new(sample(None, false))).unwrap();
        assert!(info.is_identity());
        assert!(!info.swaps_axes());
        assert!(info.exif.is_none());
        assert_eq!(info.stored_size, Some((64, 48)));
    }
    #[test]
    fn half_turn_does_not_swap_axes() {
        let info = read(&mut IoCursor::new(sample(Some(2), false))).unwrap();
        assert_eq!(info.transforms, vec![Transform::Rotate(2)]);
        assert!(!info.swaps_axes());
    }
    #[test]
    fn truncated_and_corrupt_files_never_panic() {
        let good = sample(Some(1), true);
        for len in 0..good.len() {
            let _ = read(&mut IoCursor::new(good[..len].to_vec()));
        }
        let mut hostile = good.clone();
        // ftyp is 24 bytes, so the meta box's size field starts at 24: claim it overruns the file.
        assert_eq!(&good[28..32], b"meta");
        hostile[24..28].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(read(&mut IoCursor::new(hostile)).is_err());
        assert!(read(&mut IoCursor::new(vec![0u8; 64])).is_ok());
    }
    /// ftyp written with a 64-bit "largesize" header: the brand sits after the 16-byte header.
    fn extended_ftyp() -> Vec<u8> {
        let mut out = 1u32.to_be_bytes().to_vec();
        out.extend(b"ftyp");
        out.extend(32u64.to_be_bytes()); // 16-byte header + 16-byte payload
        out.extend(b"heic\0\0\0\0mif1heic");
        out
    }
    #[test]
    fn extended_size_ftyp_reports_its_real_brand() {
        let ext = extended_ftyp();
        assert_eq!(ext.len(), 32);
        let mut file = ext.clone();
        let rest = sample(Some(1), false);
        file.extend(&rest[24..]); // everything after the ordinary 24-byte ftyp
        // The meta box inside `rest` carries offsets relative to the original layout, so only
        // the brand and the structure are asserted here, not the EXIF payload.
        let info = read(&mut IoCursor::new(file)).unwrap();
        assert_eq!(&info.major_brand, b"heic");
        assert_eq!(info.stored_size, Some((64, 48)));
    }
    #[test]
    fn hostile_extended_size_at_a_nonzero_offset_is_rejected_not_wrapped() {
        for hostile_size in [u64::MAX, u64::MAX - 4, u64::MAX - 20, 1u64 << 62] {
            let mut file = extended_ftyp();
            file.extend(1u32.to_be_bytes());
            file.extend(b"meta");
            file.extend(hostile_size.to_be_bytes());
            file.extend([0u8; 16]);
            assert!(
                read(&mut IoCursor::new(file)).is_err(),
                "size {hostile_size:#x} must be refused"
            );
        }
        // A declared size smaller than its own header is refused too.
        let mut tiny = 1u32.to_be_bytes().to_vec();
        tiny.extend(b"meta");
        tiny.extend(7u64.to_be_bytes());
        assert!(read(&mut IoCursor::new(tiny)).is_err());
    }
    /// A reader that never returns more than one byte per call, as some streams do.
    struct Trickle(IoCursor<Vec<u8>>);
    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(1);
            self.0.read(&mut buf[..n])
        }
    }
    impl Seek for Trickle {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.seek(pos)
        }
    }
    #[test]
    fn short_reads_do_not_look_like_a_truncated_file() {
        let bytes = sample(Some(3), true);
        let whole = read(&mut IoCursor::new(bytes.clone())).unwrap();
        let trickled = read(&mut Trickle(IoCursor::new(bytes))).unwrap();
        assert_eq!(whole.major_brand, trickled.major_brand);
        assert_eq!(whole.stored_size, trickled.stored_size);
        assert_eq!(whole.transforms, trickled.transforms);
        assert_eq!(whole.exif, trickled.exif);
        assert!(trickled.exif.is_some());
    }
}
