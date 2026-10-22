//! Hash of a photo's or video's image data without its metadata, so that copies
//! of the same photo whose metadata differs (a new export ID, a removed GPS
//! position, another embedded thumbnail, ...) can be recognised. Only the encoded
//! image itself is hashed, byte for byte: a re-encoded copy (converted, resized,
//! edited) does not match. Supported: JPEG, PNG, HEIF/HEIC and MP4/MOV; the format
//! is detected from the file content, not the extension.
//!
//! What counts as image data:
//! * JPEG: every segment needed to decode the main image (tables, frame and scan
//!   headers, compressed data), the ICC colour profile and the Adobe colour transform,
//!   plus the EXIF orientation. Skipped: EXIF (with its thumbnail and Apple
//!   MakerNote), XMP, JFIF, comments, other APPn segments, and anything after the
//!   end of the main image (such as an HDR gain map).
//! * PNG: critical chunks, transparency, colour (gAMA, cHRM, sRGB, iCCP, sBIT) and
//!   animation chunks, plus the EXIF orientation. Skipped: text, EXIF, time,
//!   physical size and private chunks.
//! * HEIF: the primary item (and, for a grid or other derived image, the items it is
//!   made of, in order) with each item's type, data and properties (decoder
//!   configuration, size, rotation, mirroring, colour, ...), plus the EXIF
//!   orientation (viewers differ on whether they follow it or `irot`, so both count).
//!   Skipped: the rest of the EXIF and XMP items, thumbnails and auxiliary images
//!   (depth map, HDR gain map).
//! * MP4/MOV: the video and audio tracks, each with its handler, display matrix and
//!   size (the video's rotation), edit list (its durations as exact fractions of a
//!   second, so a remux that picks another movie time scale does not matter), time
//!   scale, sample descriptions (codec configuration), sample timing and the sample
//!   data itself, located through the sample tables. Skipped: every other track (iPhone
//!   timed metadata such as the Live Photo ID or location, timecode), `udta` and
//!   `meta` atoms, creation and modification times, and where the samples sit in
//!   the file (moving the movie header to the front does not change the hash).
//!   Fragmented MP4 files are not supported.
//!
//! The hash input starts with a per-format tag holding that format's version:
//! change it whenever what is hashed for the format changes, and bump [`VERSION`]
//! (which follows the set of supported formats) so stored hashes are recomputed.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// Version of the image hash definition as a whole: 1 = photos (JPEG, PNG, HEIF),
/// 2 = videos added. Indexes record it to know which files must be hashed again.
pub const VERSION: u32 = 2;

/// Photos larger than this are not read into memory for an image hash (no photo
/// is that big). Videos are streamed instead.
const MAX_IMAGE_SIZE: u64 = 512 * 1024 * 1024;

/// A movie header larger than this is not read (real ones are a few MB at most).
const MAX_MOVIE_HEADER: u64 = 256 * 1024 * 1024;

/// Both hashes of a file, from one read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHashes {
    /// BLAKE3 of the whole file, as a hex string.
    pub full: String,
    /// Image data without metadata (see the module docs), or `None` if the format is
    /// not supported or the file could not be parsed.
    pub image: Option<String>,
}

/// Hash the whole file and, for a supported photo or video, its image data. The
/// file is read once (videos are streamed, never loaded whole).
pub fn hash_file_and_image(path: &Path) -> io::Result<FileHashes> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut head = [0u8; 12];
    let head_len = read_up_to(&mut file, &mut head)?;
    let in_memory = match detect(&head[..head_len]) {
        Some(Format::IsoBmff) => {
            let top = top_level_boxes(&mut file, size)?.unwrap_or_default();
            let has = |kind: &[u8; 4]| top.iter().any(|b| &b.kind == kind);
            // An image sequence has both; its primary image is what is shown.
            if has(b"moov") && !has(b"meta") {
                return match video_plan(&mut file, size, &top)? {
                    Some(plan) => video::hash(&mut file, &plan),
                    None => full_hash_only(&mut file),
                };
            }
            has(b"meta")
        }
        Some(_) => true,
        None => false,
    };
    if !in_memory || size > MAX_IMAGE_SIZE {
        return full_hash_only(&mut file);
    }
    file.seek(SeekFrom::Start(0))?;
    let mut data = Vec::with_capacity(size as usize);
    file.read_to_end(&mut data)?;
    Ok(FileHashes {
        full: blake3::hash(&data).to_hex().to_string(),
        image: image_hash(&data),
    })
}

fn full_hash_only(file: &mut File) -> io::Result<FileHashes> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = blake3::Hasher::new();
    io::copy(file, &mut hasher)?;
    Ok(FileHashes {
        full: hasher.finalize().to_hex().to_string(),
        image: None,
    })
}

/// A top-level box of an ISO base media file, located in the file.
struct TopBox {
    kind: [u8; 4],
    /// Where its content (after the header) starts, and its length.
    content: u64,
    length: u64,
}

/// The top-level boxes of the file, read by seeking from header to header (the
/// media data in between is not read). `None` if the structure is malformed.
fn top_level_boxes(file: &mut File, size: u64) -> io::Result<Option<Vec<TopBox>>> {
    let mut list = Vec::new();
    let mut pos = 0;
    while pos < size {
        file.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 16];
        let read = read_up_to(file, &mut header)?;
        let Some(size32) = be32(&header[..read], 0) else {
            return Ok(None);
        };
        let kind: [u8; 4] = header[4..8].try_into().expect("8 bytes were read");
        let (box_size, header_len) = match size32 {
            0 => (size - pos, 8),
            1 => match be64(&header[..read], 8) {
                Some(n) => (n, 16),
                None => return Ok(None),
            },
            n => (u64::from(n), 8),
        };
        let Some(end) = pos.checked_add(box_size) else {
            return Ok(None);
        };
        if box_size < header_len || end > size {
            return Ok(None);
        }
        list.push(TopBox {
            kind,
            content: pos + header_len,
            length: box_size - header_len,
        });
        pos = end;
    }
    Ok(Some(list))
}

/// Read the movie header and work out what to hash, if it is a supported movie.
fn video_plan(file: &mut File, size: u64, top: &[TopBox]) -> io::Result<Option<video::Plan>> {
    let Some(moov) = top.iter().find(|b| &b.kind == b"moov") else {
        return Ok(None);
    };
    if moov.length > MAX_MOVIE_HEADER {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(moov.content))?;
    let mut body = vec![0u8; moov.length as usize];
    file.read_exact(&mut body)?;
    Ok(video::plan(&body, size))
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Jpeg,
    Png,
    /// ISO base media file (HEIF, but also MP4/MOV until the boxes are read).
    IsoBmff,
}

fn detect(head: &[u8]) -> Option<Format> {
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Format::Jpeg)
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Format::Png)
    } else if head.len() >= 8 && &head[4..8] == b"ftyp" {
        Some(Format::IsoBmff)
    } else {
        None
    }
}

/// Image hash of a whole photo file's content, or `None` if unsupported or
/// malformed (videos are only hashed from a file, see [`hash_file_and_image`]).
pub fn image_hash(data: &[u8]) -> Option<String> {
    let mut hasher = blake3::Hasher::new();
    match detect(data)? {
        Format::Jpeg => jpeg::hash(data, &mut Parts::new(&mut hasher, "jpeg", 1))?,
        Format::Png => png::hash(data, &mut Parts::new(&mut hasher, "png", 1))?,
        Format::IsoBmff => heif::hash(data, &mut Parts::new(&mut hasher, "heif", 1))?,
    }
    Some(hasher.finalize().to_hex().to_string())
}

/// Feeds length-prefixed parts to the hasher, so that different splits of the same
/// bytes never produce the same hash input.
struct Parts<'a> {
    hasher: &'a mut blake3::Hasher,
}

impl<'a> Parts<'a> {
    /// Start a hash for `format`, whose definition has `version`.
    fn new(hasher: &'a mut blake3::Hasher, format: &str, version: u32) -> Self {
        let mut parts = Parts { hasher };
        parts.add(format!("photo_copy image hash v{version} {format}").as_bytes());
        parts
    }

    fn add(&mut self, bytes: &[u8]) {
        self.hasher.update(&(bytes.len() as u64).to_le_bytes());
        self.hasher.update(bytes);
    }
}

/// EXIF orientation (1 to 8) from a TIFF structure, if present.
fn exif_orientation(tiff: &[u8]) -> Option<u32> {
    let exif = exif::Reader::new().read_raw(tiff.to_vec()).ok()?;
    exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?
        .value
        .get_uint(0)
}

fn be16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

/// Big-endian unsigned number of `size` bytes (0, 4 or 8, as used by HEIF `iloc`).
fn be_sized(data: &[u8], at: usize, size: usize) -> Option<u64> {
    match size {
        0 => Some(0),
        2 => be16(data, at).map(u64::from),
        4 => be32(data, at).map(u64::from),
        8 => be64(data, at),
        _ => None,
    }
}

/// A box of an ISO base media file (HEIF, MP4, MOV).
struct BoxRef<'a> {
    kind: [u8; 4],
    /// The whole box, header included.
    raw: &'a [u8],
    /// The content after the header.
    body: &'a [u8],
}

/// The boxes in `data`, in order. `None` if one is malformed.
fn boxes(data: &[u8]) -> Option<Vec<BoxRef<'_>>> {
    let mut list = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let size32 = be32(data, pos)? as u64;
        let kind: [u8; 4] = data.get(pos + 4..pos + 8)?.try_into().ok()?;
        let (size, header) = match size32 {
            0 => ((data.len() - pos) as u64, 8),
            1 => (be64(data, pos + 8)?, 16),
            n => (n, 8),
        };
        let end = pos.checked_add(usize::try_from(size).ok()?)?;
        if size < header as u64 || end > data.len() {
            return None;
        }
        list.push(BoxRef {
            kind,
            raw: &data[pos..end],
            body: &data[pos + header..end],
        });
        pos = end;
    }
    Some(list)
}

/// Content of the first box of this kind.
fn find<'d>(list: &[BoxRef<'d>], kind: &[u8; 4]) -> Option<&'d [u8]> {
    list.iter().find(|b| &b.kind == kind).map(|b| b.body)
}

/// Version and the content after the version/flags header of a "full box".
fn full(body: &[u8]) -> Option<(u8, u32, &[u8])> {
    let header = be32(body, 0)?;
    Some(((header >> 24) as u8, header & 0x00FF_FFFF, body.get(4..)?))
}

mod jpeg {
    use super::{be16, exif_orientation, Parts};

    const SOI: u8 = 0xD8;
    const EOI: u8 = 0xD9;
    const SOS: u8 = 0xDA;
    const APP1: u8 = 0xE1;
    const APP2: u8 = 0xE2;
    const APP14: u8 = 0xEE;
    const COM: u8 = 0xFE;

    pub(super) fn hash(data: &[u8], parts: &mut Parts) -> Option<()> {
        let mut orientation = None;
        let mut pos = 2; // after SOI
        let mut saw_scan = false;
        loop {
            // A marker is 0xFF (possibly repeated as fill) followed by its code.
            if *data.get(pos)? != 0xFF {
                return None;
            }
            while *data.get(pos)? == 0xFF {
                pos += 1;
            }
            let marker = data[pos];
            pos += 1;
            match marker {
                EOI => break,
                SOI | 0x01 | 0xD0..=0xD7 => return None, // not expected between segments
                _ => {}
            }
            let length = be16(data, pos)? as usize;
            if length < 2 {
                return None;
            }
            let segment = data.get(pos + 2..pos + length)?;
            pos += length;
            match marker {
                APP1 if segment.starts_with(b"Exif\0\0") => {
                    // The first EXIF segment holds the main image's orientation.
                    if orientation.is_none() {
                        orientation = exif_orientation(&segment[6..]);
                    }
                }
                APP2 if segment.starts_with(b"ICC_PROFILE\0") => {
                    parts.add(&[&[marker], segment].concat())
                }
                APP14 if segment.starts_with(b"Adobe") => parts.add(&[&[marker], segment].concat()),
                0xE0..=0xEF | COM => {}
                SOS => {
                    parts.add(&[&[marker], segment].concat());
                    let end = scan_end(data, pos)?;
                    parts.add(&data[pos..end]);
                    pos = end;
                    saw_scan = true;
                }
                _ => parts.add(&[&[marker], segment].concat()),
            }
        }
        if !saw_scan {
            return None;
        }
        parts.add(format!("orientation {}", orientation.unwrap_or(1)).as_bytes());
        Some(())
    }

    /// End of the entropy-coded data starting at `from`: the next marker that is
    /// neither a stuffed 0xFF00 nor a restart marker.
    fn scan_end(data: &[u8], from: usize) -> Option<usize> {
        let mut pos = from;
        loop {
            let at = pos + data.get(pos..)?.iter().position(|&b| b == 0xFF)?;
            match *data.get(at + 1)? {
                0x00 | 0xD0..=0xD7 | 0xFF => pos = at + 1,
                _ => return Some(at),
            }
        }
    }
}

mod png {
    use super::{be32, exif_orientation, Parts};

    /// Ancillary chunks that change how the image looks, so they are image data.
    const VISUAL: [&[u8; 4]; 9] = [
        b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"iCCP", b"sBIT", b"acTL", b"fcTL", b"fdAT",
    ];

    pub(super) fn hash(data: &[u8], parts: &mut Parts) -> Option<()> {
        let mut orientation = None;
        let mut pos = 8;
        let mut saw_data = false;
        loop {
            let length = be32(data, pos)? as usize;
            let kind: &[u8; 4] = data.get(pos + 4..pos + 8)?.try_into().ok()?;
            let body = data.get(pos + 8..pos + 8 + length)?;
            pos += 12 + length; // length, type, data, CRC
            let critical = kind[0].is_ascii_uppercase();
            if critical || VISUAL.contains(&kind) {
                parts.add(&[kind.as_slice(), body].concat());
            } else if kind == b"eXIf" {
                orientation = exif_orientation(body);
            }
            saw_data |= kind == b"IDAT";
            if kind == b"IEND" {
                break;
            }
        }
        if !saw_data {
            return None;
        }
        parts.add(format!("orientation {}", orientation.unwrap_or(1)).as_bytes());
        Some(())
    }
}

mod heif {
    use std::collections::HashMap;

    use super::{be16, be32, be_sized, boxes, exif_orientation, find, full, Parts};

    /// Derived images are made of other items; this bounds the nesting (and stops
    /// reference loops in broken files).
    const MAX_DEPTH: usize = 4;

    struct Extent {
        /// Construction method 1: the offset is inside the `idat` box.
        in_idat: bool,
        offset: u64,
        length: u64,
    }

    struct Meta<'a> {
        primary: u32,
        types: HashMap<u32, [u8; 4]>,
        locations: HashMap<u32, Vec<Extent>>,
        properties: Vec<&'a [u8]>,
        /// item -> 1-based indexes into `properties`, in association order.
        associations: HashMap<u32, Vec<usize>>,
        /// item -> items of its `dimg` (derived image) reference, in order.
        derived_from: HashMap<u32, Vec<u32>>,
        idat: &'a [u8],
    }

    pub(super) fn hash(data: &[u8], parts: &mut Parts) -> Option<()> {
        let top = boxes(data)?;
        // MP4/MOV files have `ftyp` too, but no top-level `meta` with a primary item.
        let (_, _, meta_body) = full(find(&top, b"meta")?)?;
        let meta = parse_meta(meta_body)?;
        add_item(data, &meta, meta.primary, parts, 0)?;
        // An Exif item starts with the offset of the TIFF header after that field.
        let orientation = meta
            .types
            .iter()
            .filter(|(_, kind)| *kind == b"Exif")
            .min_by_key(|(id, _)| **id)
            .and_then(|(id, _)| item_data(data, &meta, *id))
            .and_then(|exif| {
                let skip = usize::try_from(be32(&exif, 0)?).ok()?;
                exif_orientation(exif.get(4 + skip..)?)
            });
        parts.add(format!("orientation {}", orientation.unwrap_or(1)).as_bytes());
        Some(())
    }

    /// The item's data: its extents, joined.
    fn item_data(data: &[u8], meta: &Meta, id: u32) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        for extent in meta.locations.get(&id)? {
            out.extend_from_slice(extent_bytes(data, meta, extent)?);
        }
        Some(out)
    }

    fn extent_bytes<'d>(data: &'d [u8], meta: &Meta<'d>, extent: &Extent) -> Option<&'d [u8]> {
        let source = if extent.in_idat { meta.idat } else { data };
        let start = usize::try_from(extent.offset).ok()?;
        // Length 0 means "to the end of the source".
        let end = match extent.length {
            0 => source.len(),
            n => start.checked_add(usize::try_from(n).ok()?)?,
        };
        source.get(start..end)
    }

    fn parse_meta(body: &[u8]) -> Option<Meta<'_>> {
        let children = boxes(body)?;
        let (version, _, pitm) = full(find(&children, b"pitm")?)?;
        let primary = if version == 0 {
            u32::from(be16(pitm, 0)?)
        } else {
            be32(pitm, 0)?
        };
        let iprp = boxes(find(&children, b"iprp")?)?;
        let properties = boxes(find(&iprp, b"ipco")?)?
            .into_iter()
            .map(|b| b.raw)
            .collect();
        let mut associations = HashMap::new();
        for ipma in iprp.iter().filter(|b| &b.kind == b"ipma") {
            parse_ipma(ipma.body, &mut associations)?;
        }
        Some(Meta {
            primary,
            types: parse_iinf(find(&children, b"iinf")?)?,
            locations: parse_iloc(find(&children, b"iloc")?)?,
            properties,
            associations,
            derived_from: match find(&children, b"iref") {
                Some(iref) => parse_dimg(iref)?,
                None => HashMap::new(),
            },
            idat: find(&children, b"idat").unwrap_or(&[]),
        })
    }

    fn parse_iinf(body: &[u8]) -> Option<HashMap<u32, [u8; 4]>> {
        let (version, _, rest) = full(body)?;
        let skip = if version == 0 { 2 } else { 4 }; // entry count
        let mut types = HashMap::new();
        for infe in boxes(rest.get(skip..)?)? {
            if &infe.kind != b"infe" {
                continue;
            }
            let (version, _, entry) = full(infe.body)?;
            let (id, kind_at) = match version {
                2 => (u32::from(be16(entry, 0)?), 4),
                3 => (be32(entry, 0)?, 6),
                _ => continue, // versions 0 and 1 carry no item type
            };
            types.insert(id, entry.get(kind_at..kind_at + 4)?.try_into().ok()?);
        }
        Some(types)
    }

    fn parse_iloc(body: &[u8]) -> Option<HashMap<u32, Vec<Extent>>> {
        let (version, _, rest) = full(body)?;
        let sizes = be16(rest, 0)?;
        let offset_size = (sizes >> 12) as usize;
        let length_size = ((sizes >> 8) & 0xF) as usize;
        let base_size = ((sizes >> 4) & 0xF) as usize;
        let index_size = if version >= 1 {
            (sizes & 0xF) as usize
        } else {
            0
        };
        let mut pos = 2;
        let count = if version < 2 {
            pos += 2;
            u32::from(be16(rest, 2)?)
        } else {
            pos += 4;
            be32(rest, 2)?
        };
        let mut locations = HashMap::new();
        for _ in 0..count {
            let id = if version < 2 {
                pos += 2;
                u32::from(be16(rest, pos - 2)?)
            } else {
                pos += 4;
                be32(rest, pos - 4)?
            };
            let method = if version >= 1 {
                pos += 2;
                be16(rest, pos - 2)? & 0xF
            } else {
                0
            };
            pos += 2; // data reference index
            let base = be_sized(rest, pos, base_size)?;
            pos += base_size;
            let extent_count = be16(rest, pos)?;
            pos += 2;
            let mut extents = Vec::new();
            for _ in 0..extent_count {
                pos += index_size;
                let offset = be_sized(rest, pos, offset_size)?;
                pos += offset_size;
                let length = be_sized(rest, pos, length_size)?;
                pos += length_size;
                extents.push(Extent {
                    in_idat: method == 1,
                    offset: base.checked_add(offset)?,
                    length,
                });
            }
            // Method 2 (data in another item) is not used for photos.
            if method <= 1 {
                locations.insert(id, extents);
            }
        }
        Some(locations)
    }

    fn parse_ipma(body: &[u8], associations: &mut HashMap<u32, Vec<usize>>) -> Option<()> {
        let (version, flags, rest) = full(body)?;
        let count = be32(rest, 0)?;
        let mut pos = 4;
        for _ in 0..count {
            let id = if version < 1 {
                pos += 2;
                u32::from(be16(rest, pos - 2)?)
            } else {
                pos += 4;
                be32(rest, pos - 4)?
            };
            let n = *rest.get(pos)?;
            pos += 1;
            let list = associations.entry(id).or_default();
            for _ in 0..n {
                let index = if flags & 1 == 1 {
                    pos += 2;
                    usize::from(be16(rest, pos - 2)? & 0x7FFF)
                } else {
                    pos += 1;
                    usize::from(rest[pos - 1] & 0x7F)
                };
                if index > 0 {
                    list.push(index);
                }
            }
        }
        Some(())
    }

    fn parse_dimg(body: &[u8]) -> Option<HashMap<u32, Vec<u32>>> {
        let (version, _, rest) = full(body)?;
        let id_size = if version == 0 { 2 } else { 4 };
        let mut derived = HashMap::new();
        for reference in boxes(rest)? {
            if &reference.kind != b"dimg" {
                continue;
            }
            let body = reference.body;
            let read_id = |at| match id_size {
                2 => be16(body, at).map(u32::from),
                _ => be32(body, at),
            };
            let from = read_id(0)?;
            let count = be16(body, id_size)?;
            let to = (0..usize::from(count))
                .map(|i| read_id(id_size + 2 + i * id_size))
                .collect::<Option<Vec<_>>>()?;
            derived.insert(from, to);
        }
        Some(derived)
    }

    /// Hash one item: its type, its data, its properties, then (for a derived image
    /// such as a grid) the items it is made of.
    fn add_item(data: &[u8], meta: &Meta, id: u32, parts: &mut Parts, depth: usize) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        parts.add(meta.types.get(&id)?);
        for extent in meta.locations.get(&id)? {
            parts.add(extent_bytes(data, meta, extent)?);
        }
        for &index in meta.associations.get(&id).into_iter().flatten() {
            parts.add(meta.properties.get(index - 1)?);
        }
        for &source in meta.derived_from.get(&id).into_iter().flatten() {
            add_item(data, meta, source, parts, depth + 1)?;
        }
        Some(())
    }
}

mod video {
    use std::fs::File;
    use std::io::{self, Read, Seek, SeekFrom};

    use super::{be32, be64, boxes, find, full, BoxRef, FileHashes, Parts};

    /// Reads while streaming the file.
    pub(super) const BUFFER: usize = 1 << 20;

    pub(super) struct Track {
        /// Settings that affect playback, each hashed as one part.
        settings: Vec<Vec<u8>>,
        /// Where the samples are in the file, in playback order: `(offset, length)`,
        /// with adjacent ranges merged.
        extents: Vec<(u64, u64)>,
    }

    pub(super) struct Plan {
        tracks: Vec<Track>,
    }

    /// What to hash in a movie whose `moov` content is `moov`. `None` if it has no
    /// video or audio track, is fragmented, or its tables are malformed or point
    /// outside the file.
    pub(super) fn plan(moov: &[u8], file_size: u64) -> Option<Plan> {
        let children = boxes(moov)?;
        if find(&children, b"mvex").is_some() {
            return None; // fragmented: samples live in `moof` boxes
        }
        let movie_scale = time_scale(find(&children, b"mvhd")?)?;
        let mut tracks = Vec::new();
        for trak in children.iter().filter(|b| &b.kind == b"trak") {
            if let Some(track) = track(trak.body, movie_scale, file_size)? {
                tracks.push(track);
            }
        }
        if tracks.is_empty() {
            return None;
        }
        Some(Plan { tracks })
    }

    /// The edit list in a form independent of the movie time scale: per entry, the
    /// duration as a reduced fraction of a second, the media time (in the track's
    /// own time scale, which is hashed too) and the rate.
    fn edit_list(elst: &[u8], movie_scale: u32) -> Option<Vec<u8>> {
        let (version, _, rest) = full(elst)?;
        let entry_size = if version == 1 { 20 } else { 12 };
        let mut out = Vec::new();
        for i in 0..be32(rest, 0)? as usize {
            let at = 4 + i * entry_size;
            let (duration, media_time) = if version == 1 {
                (be64(rest, at)?, be64(rest, at + 8)?)
            } else {
                (u64::from(be32(rest, at)?), u64::from(be32(rest, at + 4)?))
            };
            let rate = be32(rest, at + entry_size - 4)?;
            let divisor = gcd(duration, u64::from(movie_scale)).max(1);
            out.extend((duration / divisor).to_be_bytes());
            out.extend((u64::from(movie_scale) / divisor).to_be_bytes());
            // An empty edit is -1 in either width; keep it distinguishable.
            let media_time = if version == 0 && media_time == u64::from(u32::MAX) {
                u64::MAX
            } else {
                media_time
            };
            out.extend(media_time.to_be_bytes());
            out.extend(rate.to_be_bytes());
        }
        Some(out)
    }

    fn gcd(a: u64, b: u64) -> u64 {
        if b == 0 {
            a
        } else {
            gcd(b, a % b)
        }
    }

    /// The time scale of an `mvhd` or `mdhd` box.
    fn time_scale(body: &[u8]) -> Option<u32> {
        let (version, _, rest) = full(body)?;
        be32(rest, if version == 1 { 16 } else { 8 })
    }

    /// `Some(None)` for a track that is not video or audio (not hashed).
    fn track(trak: &[u8], movie_scale: u32, file_size: u64) -> Option<Option<Track>> {
        let trak = boxes(trak)?;
        let mdia = boxes(find(&trak, b"mdia")?)?;
        let (_, _, hdlr) = full(find(&mdia, b"hdlr")?)?;
        let handler = hdlr.get(4..8)?;
        if handler != b"vide" && handler != b"soun" {
            return Some(None);
        }
        // tkhd: times, track ID and duration come first (20 or 32 bytes), then
        // 8 reserved bytes, layer, group, volume, 2 reserved, the 36-byte matrix,
        // width and height.
        let (version, _, tkhd) = full(find(&trak, b"tkhd")?)?;
        let geometry_at = if version == 1 { 32 } else { 20 } + 16;
        let geometry = tkhd.get(geometry_at..geometry_at + 44)?;
        let edit_list = match find(&trak, b"edts").and_then(|edts| find(&boxes(edts)?, b"elst")) {
            Some(elst) => edit_list(elst, movie_scale)?,
            None => Vec::new(),
        };
        let media_scale = time_scale(find(&mdia, b"mdhd")?)?;
        let stbl = boxes(find(&boxes(find(&mdia, b"minf")?)?, b"stbl")?)?;
        let settings = vec![
            handler.to_vec(),
            geometry.to_vec(),
            edit_list,
            media_scale.to_be_bytes().to_vec(),
            raw(&stbl, b"stsd")?.to_vec(),
            raw(&stbl, b"stts")?.to_vec(),
            raw(&stbl, b"ctts").unwrap_or_default().to_vec(),
        ];
        let extents = sample_extents(&stbl)?;
        let end = extents.iter().try_fold(0u64, |end, &(offset, length)| {
            Some(end.max(offset.checked_add(length)?))
        })?;
        if end > file_size {
            return None;
        }
        Some(Some(Track { settings, extents }))
    }

    /// The whole first box of this kind, header included.
    fn raw<'d>(list: &[BoxRef<'d>], kind: &[u8; 4]) -> Option<&'d [u8]> {
        list.iter().find(|b| &b.kind == kind).map(|b| b.raw)
    }

    /// Sample locations from the sample size, sample-to-chunk and chunk offset
    /// tables, in playback order.
    fn sample_extents(stbl: &[BoxRef]) -> Option<Vec<(u64, u64)>> {
        let (_, _, stsz) = full(find(stbl, b"stsz")?)?;
        let fixed_size = be32(stsz, 0)?;
        let sample_count = be32(stsz, 4)? as usize;
        let size_of = |i: usize| match fixed_size {
            0 => be32(stsz, 8 + 4 * i).map(u64::from),
            n => Some(u64::from(n)),
        };
        let (_, _, stsc) = full(find(stbl, b"stsc")?)?;
        let runs = (0..be32(stsc, 0)? as usize)
            .map(|i| Some((be32(stsc, 4 + 12 * i)?, be32(stsc, 8 + 12 * i)?)))
            .collect::<Option<Vec<_>>>()?;
        let offsets: Vec<u64> = if let Some(stco) = find(stbl, b"stco") {
            let (_, _, stco) = full(stco)?;
            (0..be32(stco, 0)? as usize)
                .map(|i| be32(stco, 4 + 4 * i).map(u64::from))
                .collect::<Option<_>>()?
        } else {
            let (_, _, co64) = full(find(stbl, b"co64")?)?;
            (0..be32(co64, 0)? as usize)
                .map(|i| be64(co64, 4 + 8 * i))
                .collect::<Option<_>>()?
        };

        let mut extents: Vec<(u64, u64)> = Vec::new();
        let (mut sample, mut run) = (0, 0);
        for (index, &offset) in offsets.iter().enumerate() {
            let chunk = index as u32 + 1;
            while runs.get(run + 1).is_some_and(|&(first, _)| first <= chunk) {
                run += 1;
            }
            let &(first, per_chunk) = runs.get(run)?;
            if first > chunk {
                return None;
            }
            let mut length = 0u64;
            for _ in 0..per_chunk {
                if sample >= sample_count {
                    return None;
                }
                length = length.checked_add(size_of(sample)?)?;
                sample += 1;
            }
            match extents.last_mut() {
                Some(last) if last.0.checked_add(last.1) == Some(offset) => last.1 += length,
                _ if length == 0 => {}
                _ => extents.push((offset, length)),
            }
        }
        (sample == sample_count).then_some(extents)
    }

    /// Read the file once from the start, feeding the full hash and each track's
    /// sample data. A track whose samples are not stored in playback order is read
    /// again afterwards, in order.
    pub(super) fn hash(file: &mut File, plan: &Plan) -> io::Result<FileHashes> {
        let in_order: Vec<bool> = plan
            .tracks
            .iter()
            .map(|t| t.extents.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0))
            .collect();
        let mut data_hashers: Vec<blake3::Hasher> =
            plan.tracks.iter().map(|_| blake3::Hasher::new()).collect();
        let mut next = vec![0usize; plan.tracks.len()];
        let mut full_hasher = blake3::Hasher::new();
        let mut buffer = vec![0u8; BUFFER];
        let mut pos = 0u64;
        file.seek(SeekFrom::Start(0))?;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let chunk = &buffer[..read];
            full_hasher.update(chunk);
            let end = pos + read as u64;
            for (t, track) in plan.tracks.iter().enumerate() {
                if !in_order[t] {
                    continue;
                }
                while let Some(&(offset, length)) = track.extents.get(next[t]) {
                    if offset >= end {
                        break;
                    }
                    let (from, to) = (offset.max(pos), (offset + length).min(end));
                    if from < to {
                        data_hashers[t].update(&chunk[(from - pos) as usize..(to - pos) as usize]);
                    }
                    if offset + length > end {
                        break;
                    }
                    next[t] += 1;
                }
            }
            pos = end;
        }
        for (t, track) in plan.tracks.iter().enumerate() {
            if in_order[t] {
                continue;
            }
            for &(offset, length) in &track.extents {
                file.seek(SeekFrom::Start(offset))?;
                io::copy(&mut (&mut *file).take(length), &mut data_hashers[t])?;
            }
        }

        let mut image = blake3::Hasher::new();
        let mut parts = Parts::new(&mut image, "video", 1);
        for (track, data) in plan.tracks.iter().zip(&data_hashers) {
            for setting in &track.settings {
                parts.add(setting);
            }
            parts.add(data.finalize().as_bytes());
        }
        Ok(FileHashes {
            full: full_hasher.finalize().to_hex().to_string(),
            image: Some(image.finalize().to_hex().to_string()),
        })
    }
}

#[cfg(test)]
fn video_buffer() -> usize {
    video::BUFFER
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- JPEG ----

    /// A small real JPEG with the given pixel seed.
    fn jpeg(seed: u8) -> Vec<u8> {
        let img = image::RgbImage::from_fn(16, 16, |x, y| {
            image::Rgb([x as u8 * 16, y as u8 * 16, seed])
        });
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
            .encode_image(&img)
            .unwrap();
        out
    }

    /// TIFF structure with an orientation and an extra ASCII tag (a stand-in for
    /// export IDs and other metadata that change between copies).
    fn tiff(orientation: u16, note: &str) -> Vec<u8> {
        let mut note = note.as_bytes().to_vec();
        note.push(0);
        let mut t = b"MM\0\x2a\0\0\0\x08".to_vec();
        t.extend_from_slice(&2u16.to_be_bytes()); // 2 entries
        t.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1]); // Orientation, SHORT, 1
        t.extend_from_slice(&orientation.to_be_bytes());
        t.extend_from_slice(&[0, 0]);
        t.extend_from_slice(&[0x01, 0x0E, 0, 2]); // ImageDescription, ASCII
        t.extend_from_slice(&(note.len() as u32).to_be_bytes());
        t.extend_from_slice(&38u32.to_be_bytes()); // offset of the text
        t.extend_from_slice(&[0, 0, 0, 0]); // no next IFD
        t.extend_from_slice(&note);
        t
    }

    /// `jpeg` with an EXIF segment inserted after SOI, and optionally a comment.
    fn with_exif(jpeg: &[u8], tiff: &[u8], comment: Option<&str>) -> Vec<u8> {
        let mut out = jpeg[..2].to_vec();
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(tiff);
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        if let Some(text) = comment {
            out.extend_from_slice(&[0xFF, 0xFE]);
            out.extend_from_slice(&((text.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[test]
    fn jpeg_metadata_changes_keep_the_image_hash() {
        let base = jpeg(1);
        let a = with_exif(&base, &tiff(1, "export B477172F"), None);
        let b = with_exif(&base, &tiff(1, "export 489C2A31-BE00"), Some("edited"));

        assert_ne!(blake3::hash(&a), blake3::hash(&b));
        let hash = image_hash(&a);
        assert!(hash.is_some());
        assert_eq!(hash, image_hash(&b));
        // No EXIF at all is the same as orientation 1.
        assert_eq!(hash, image_hash(&base));
    }

    #[test]
    fn jpeg_orientation_is_part_of_the_image() {
        let base = jpeg(1);
        let upright = with_exif(&base, &tiff(1, "x"), None);
        let rotated = with_exif(&base, &tiff(6, "x"), None);

        assert_ne!(image_hash(&upright), image_hash(&rotated));
    }

    #[test]
    fn jpeg_pixels_are_part_of_the_image() {
        assert_ne!(image_hash(&jpeg(1)), image_hash(&jpeg(2)));
    }

    #[test]
    fn jpeg_data_after_the_image_is_ignored() {
        let base = jpeg(1);
        let mut with_trailer = base.clone();
        with_trailer.extend_from_slice(b"\xFF\xD8 gain map image");

        assert_eq!(image_hash(&base), image_hash(&with_trailer));
    }

    #[test]
    fn truncated_jpeg_has_no_image_hash() {
        let base = jpeg(1);
        assert_eq!(image_hash(&base[..base.len() / 2]), None);
    }

    // ---- PNG ----

    fn png(seed: u8) -> Vec<u8> {
        let img = image::RgbImage::from_fn(8, 8, |x, y| image::Rgb([x as u8, y as u8, seed]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// `png` with a chunk inserted after IHDR (CRC left as zeros: not checked).
    fn with_chunk(png: &[u8], kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let ihdr_end = 8 + 12 + 13;
        let mut out = png[..ihdr_end].to_vec();
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&png[ihdr_end..]);
        out
    }

    #[test]
    fn png_text_and_time_chunks_are_metadata() {
        let base = png(1);
        let a = with_chunk(&base, b"tEXt", b"Comment\0one");
        let b = with_chunk(&with_chunk(&base, b"tIME", &[7; 7]), b"iTXt", b"x");

        assert!(image_hash(&base).is_some());
        assert_eq!(image_hash(&base), image_hash(&a));
        assert_eq!(image_hash(&base), image_hash(&b));
    }

    #[test]
    fn png_visual_chunks_and_orientation_are_image_data() {
        let base = png(1);
        assert_ne!(
            image_hash(&base),
            image_hash(&with_chunk(&base, b"gAMA", &[0, 0, 0xB1, 0x8F]))
        );
        assert_ne!(
            image_hash(&base),
            image_hash(&with_chunk(&base, b"eXIf", &tiff(8, "x")))
        );
        assert_eq!(
            image_hash(&base),
            image_hash(&with_chunk(&base, b"eXIf", &tiff(1, "x")))
        );
        assert_ne!(image_hash(&png(1)), image_hash(&png(2)));
    }

    // ---- HEIF ----

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut content = ((u32::from(version) << 24) | flags).to_be_bytes().to_vec();
        content.extend_from_slice(body);
        bx(kind, &content)
    }

    /// A minimal HEIF file. `items` are `(id, type, data)`; their data goes into
    /// `mdat` in this order. `props` are the property boxes, `assoc` maps items to
    /// 1-based property indexes, `dimg` is `(from, [to...])`.
    struct Heif<'a> {
        primary: u16,
        items: Vec<(u16, &'a [u8; 4], Vec<u8>)>,
        props: Vec<Vec<u8>>,
        assoc: Vec<(u16, Vec<u8>)>,
        dimg: Vec<(u16, Vec<u16>)>,
    }

    impl Heif<'_> {
        fn build(&self) -> Vec<u8> {
            let ftyp = bx(b"ftyp", b"heic\0\0\0\0mif1heic");
            let hdlr = full_box(b"hdlr", 0, 0, b"\0\0\0\0pict\0\0\0\0\0\0\0\0\0\0\0\0\0");
            let pitm = full_box(b"pitm", 0, 0, &self.primary.to_be_bytes());
            let mut infes = (self.items.len() as u16).to_be_bytes().to_vec();
            for (id, kind, _) in &self.items {
                let mut e = id.to_be_bytes().to_vec();
                e.extend_from_slice(&[0, 0]);
                e.extend_from_slice(*kind);
                e.push(0);
                infes.extend(full_box(b"infe", 2, 0, &e));
            }
            let iinf = full_box(b"iinf", 0, 0, &infes);
            let ipco: Vec<u8> = self.props.concat();
            let mut ipma = (self.assoc.len() as u32).to_be_bytes().to_vec();
            for (id, indexes) in &self.assoc {
                ipma.extend_from_slice(&id.to_be_bytes());
                ipma.push(indexes.len() as u8);
                ipma.extend_from_slice(indexes);
            }
            let iprp = bx(
                b"iprp",
                &[bx(b"ipco", &ipco), full_box(b"ipma", 0, 0, &ipma)].concat(),
            );
            let mut refs = Vec::new();
            for (from, to) in &self.dimg {
                let mut r = from.to_be_bytes().to_vec();
                r.extend_from_slice(&(to.len() as u16).to_be_bytes());
                for t in to {
                    r.extend_from_slice(&t.to_be_bytes());
                }
                refs.extend(bx(b"dimg", &r));
            }
            let iref = full_box(b"iref", 0, 0, &refs);

            // iloc needs absolute offsets into mdat, which follows meta: build meta
            // once with placeholder offsets to learn its size (fixed-size fields).
            let iloc_for = |mdat_data_start: u32| {
                let mut l = vec![0x44, 0x00]; // offset size 4, length size 4, base 0
                l.extend_from_slice(&(self.items.len() as u16).to_be_bytes());
                let mut offset = mdat_data_start;
                for (id, _, data) in &self.items {
                    l.extend_from_slice(&id.to_be_bytes());
                    l.extend_from_slice(&[0, 0, 0, 1]); // data ref index, 1 extent
                    l.extend_from_slice(&offset.to_be_bytes());
                    l.extend_from_slice(&(data.len() as u32).to_be_bytes());
                    offset += data.len() as u32;
                }
                full_box(b"iloc", 0, 0, &l)
            };
            let meta_for = |start| {
                full_box(
                    b"meta",
                    0,
                    0,
                    &[
                        hdlr.clone(),
                        pitm.clone(),
                        iinf.clone(),
                        iloc_for(start),
                        iprp.clone(),
                        iref.clone(),
                    ]
                    .concat(),
                )
            };
            let meta_len = meta_for(0).len();
            let data_start = (ftyp.len() + meta_len + 8) as u32;
            let mdat: Vec<u8> = self.items.iter().flat_map(|(_, _, d)| d.clone()).collect();
            [ftyp, meta_for(data_start), bx(b"mdat", &mdat)].concat()
        }
    }

    fn single(image: &[u8], exif: &[u8], rotation: u8) -> Vec<u8> {
        Heif {
            primary: 1,
            items: vec![(1, b"hvc1", image.to_vec()), (2, b"Exif", exif.to_vec())],
            props: vec![
                bx(b"hvcC", b"config"),
                bx(b"ispe", b"\0\0\0\0\0\0\0\x10\0\0\0\x10"),
                bx(b"irot", &[rotation]),
            ],
            assoc: vec![(1, vec![1, 2, 3])],
            dimg: vec![],
        }
        .build()
    }

    #[test]
    fn heif_exif_item_is_metadata() {
        let a = single(b"coded image", b"\0\0\0\0export one", 0);
        let b = single(b"coded image", b"\0\0\0\0another export id", 0);

        assert_ne!(a, b);
        assert!(image_hash(&a).is_some());
        assert_eq!(image_hash(&a), image_hash(&b));
    }

    #[test]
    fn heif_exif_orientation_counts() {
        let exif = |orientation| [&[0u8, 0, 0, 0][..], &tiff(orientation, "id")].concat();
        let upright = image_hash(&single(b"coded image", &exif(1), 0));
        assert!(upright.is_some());
        assert_ne!(upright, image_hash(&single(b"coded image", &exif(6), 0)));
        assert_eq!(
            upright,
            image_hash(&single(b"coded image", b"\0\0\0\0no tiff", 0))
        );
    }

    #[test]
    fn heif_image_data_and_rotation_count() {
        let base = image_hash(&single(b"coded image", b"x", 0));
        assert_ne!(base, image_hash(&single(b"coded IMAGE", b"x", 0)));
        assert_ne!(base, image_hash(&single(b"coded image", b"x", 1)));
    }

    fn grid(tiles: [&[u8]; 2], thumb: &[u8]) -> Vec<u8> {
        Heif {
            primary: 1,
            items: vec![
                (1, b"grid", b"grid 2x1".to_vec()),
                (2, b"hvc1", tiles[0].to_vec()),
                (3, b"hvc1", tiles[1].to_vec()),
                (4, b"hvc1", thumb.to_vec()),
            ],
            props: vec![bx(b"hvcC", b"config"), bx(b"ispe", b"size")],
            assoc: vec![(1, vec![2]), (2, vec![1]), (3, vec![1]), (4, vec![1, 2])],
            dimg: vec![(1, vec![2, 3])],
        }
        .build()
    }

    #[test]
    fn heif_grid_hashes_its_tiles_in_order_but_not_the_thumbnail() {
        let base = image_hash(&grid([b"left", b"right"], b"thumb"));
        assert!(base.is_some());
        assert_eq!(
            base,
            image_hash(&grid([b"left", b"right"], b"other thumbnail"))
        );
        assert_ne!(base, image_hash(&grid([b"right", b"left"], b"thumb")));
        assert_ne!(base, image_hash(&grid([b"left", b"RIGHT"], b"thumb")));
    }

    #[test]
    fn mp4_without_heif_meta_has_no_image_hash() {
        let mp4 = [
            bx(b"ftyp", b"isom\0\0\0\0isom"),
            bx(b"moov", b""),
            bx(b"mdat", b"video"),
        ]
        .concat();
        assert_eq!(image_hash(&mp4), None);
    }

    #[test]
    fn real_heic_file_is_parsed() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/staging/IMG_0007.heic");
        let hashes = hash_file_and_image(&path).unwrap();
        assert!(hashes.image.is_some());
        assert_eq!(hashes.full, crate::hash::hash_file(&path).unwrap());
    }

    // ---- MP4/MOV ----

    /// A track's chunks, each a list of samples.
    type Chunks = Vec<Vec<Vec<u8>>>;

    /// A movie whose tracks hold chunks of samples, laid out in `mdat` in `order`.
    #[derive(Clone)]
    struct Movie {
        /// `(handler, chunks of samples)`
        tracks: Vec<(&'static [u8; 4], Chunks)>,
        /// `(track, chunk)` in file order.
        order: Vec<(usize, usize)>,
        rotated: bool,
        created: u32,
        udta: Vec<u8>,
        moov_first: bool,
        fragmented: bool,
        /// Movie time scale, and the edit length in seconds (no edit list if 0).
        movie_scale: u32,
        edit_seconds: u32,
    }

    impl Movie {
        /// Video, audio and an iPhone-like timed metadata track, interleaved.
        fn sample() -> Self {
            let s = |text: &str| text.as_bytes().to_vec();
            Movie {
                tracks: vec![
                    (
                        b"vide",
                        vec![vec![s("frame-1"), s("frame-2")], vec![s("frame-3")]],
                    ),
                    (b"soun", vec![vec![s("audio-1")], vec![s("audio-2")]]),
                    (
                        b"meta",
                        vec![vec![s("live-photo-id=B477")], vec![s("location=+52+013")]],
                    ),
                ],
                order: vec![(0, 0), (1, 0), (2, 0), (0, 1), (1, 1), (2, 1)],
                rotated: false,
                created: 1_000,
                udta: b"2021-06-14".to_vec(),
                moov_first: false,
                fragmented: false,
                movie_scale: 600,
                edit_seconds: 0,
            }
        }

        fn build(&self) -> Vec<u8> {
            let ftyp = bx(b"ftyp", b"qt  \0\0\0\0qt  ");
            let mut payload = Vec::new();
            let mut at = std::collections::HashMap::new();
            for &(t, c) in &self.order {
                at.insert((t, c), payload.len() as u32);
                payload.extend(self.tracks[t].1[c].concat());
            }
            let moov_for = |base: u32| {
                let be = |n: u32| n.to_be_bytes().to_vec();
                let mut mvhd = vec![0; 8];
                mvhd.extend(be(self.movie_scale));
                mvhd.extend([0; 84]);
                let mut moov = full_box(b"mvhd", 0, 0, &mvhd);
                for (t, (handler, chunks)) in self.tracks.iter().enumerate() {
                    let samples: Vec<&Vec<u8>> = chunks.iter().flatten().collect();
                    let mut tkhd = be(self.created);
                    tkhd.extend(be(self.created));
                    tkhd.extend(be(t as u32 + 1));
                    tkhd.extend([0; 8 + 8 + 8]); // reserved, duration, reserved, layer, group, volume, reserved
                    let matrix: [u32; 9] = if self.rotated {
                        [0, 0x10000, 0, 0xFFFF0000, 0, 0, 0, 0, 0x40000000]
                    } else {
                        [0x10000, 0, 0, 0, 0x10000, 0, 0, 0, 0x40000000]
                    };
                    matrix.iter().for_each(|m| tkhd.extend(be(*m)));
                    tkhd.extend(be(1920 << 16));
                    tkhd.extend(be(1080 << 16));
                    let mut mdhd = vec![0; 8];
                    mdhd.extend(be(30));
                    mdhd.extend([0; 8]);
                    let mut hdlr = vec![0; 4];
                    hdlr.extend(*handler);
                    hdlr.extend([0; 13]);
                    let mut stsd = be(1);
                    stsd.extend(bx(b"avc1", b"\0\0\0\0\0\0\0\x01codec config"));
                    let mut stts = be(1);
                    stts.extend(be(samples.len() as u32));
                    stts.extend(be(1));
                    let mut stsc = be(chunks.len() as u32);
                    let mut stco = be(chunks.len() as u32);
                    for (c, chunk) in chunks.iter().enumerate() {
                        stsc.extend(be(c as u32 + 1));
                        stsc.extend(be(chunk.len() as u32));
                        stsc.extend(be(1));
                        stco.extend(be(base + at[&(t, c)]));
                    }
                    let mut stsz = be(0);
                    stsz.extend(be(samples.len() as u32));
                    samples
                        .iter()
                        .for_each(|smp| stsz.extend(be(smp.len() as u32)));
                    let stbl = bx(
                        b"stbl",
                        &[
                            full_box(b"stsd", 0, 0, &stsd),
                            full_box(b"stts", 0, 0, &stts),
                            full_box(b"stsc", 0, 0, &stsc),
                            full_box(b"stsz", 0, 0, &stsz),
                            full_box(b"stco", 0, 0, &stco),
                        ]
                        .concat(),
                    );
                    let mdia = bx(
                        b"mdia",
                        &[
                            full_box(b"mdhd", 0, 0, &mdhd),
                            full_box(b"hdlr", 0, 0, &hdlr),
                            bx(b"minf", &stbl),
                        ]
                        .concat(),
                    );
                    let mut edts = Vec::new();
                    if self.edit_seconds > 0 {
                        let mut elst = be(1);
                        elst.extend(be(self.edit_seconds * self.movie_scale));
                        elst.extend(be(0)); // media time
                        elst.extend(be(0x10000)); // rate 1.0
                        edts = bx(b"edts", &full_box(b"elst", 0, 0, &elst));
                    }
                    let tkhd = full_box(b"tkhd", 0, 0, &tkhd);
                    moov.extend(bx(b"trak", &[tkhd, edts, mdia].concat()));
                }
                moov.extend(bx(b"udta", &bx(b"\xA9day", &self.udta)));
                if self.fragmented {
                    moov.extend(bx(b"mvex", b""));
                }
                bx(b"moov", &moov)
            };
            let moov_len = moov_for(0).len();
            let mdat = bx(b"mdat", &payload);
            if self.moov_first {
                let base = (ftyp.len() + moov_len + 8) as u32;
                [ftyp, moov_for(base), mdat].concat()
            } else {
                let base = (ftyp.len() + 8) as u32;
                [ftyp.clone(), mdat, moov_for(base)].concat()
            }
        }

        fn hashes(&self) -> FileHashes {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("clip.mov");
            std::fs::write(&path, self.build()).unwrap();
            let hashes = hash_file_and_image(&path).unwrap();
            assert_eq!(hashes.full, crate::hash::hash_file(&path).unwrap());
            hashes
        }
    }

    #[test]
    fn movie_metadata_changes_keep_the_image_hash() {
        let base = Movie::sample();
        let mut other = base.clone();
        other.udta = b"2026-06-14 re-exported".to_vec();
        other.created = 2_000;
        // New Live Photo ID and location in the timed metadata track (another length,
        // so every later sample moves too).
        other.tracks[2].1 = vec![
            vec![b"live-photo-id=489C2A31".to_vec()],
            vec![b"no location".to_vec()],
        ];

        let (a, b) = (base.hashes(), other.hashes());

        assert!(a.image.is_some());
        assert_ne!(a.full, b.full);
        assert_eq!(a.image, b.image);
    }

    #[test]
    fn movie_video_audio_and_rotation_count() {
        let base = Movie::sample().hashes().image;
        let mut video = Movie::sample();
        video.tracks[0].1[1][0] = b"FRAME-3".to_vec();
        let mut audio = Movie::sample();
        audio.tracks[1].1[0][0] = b"AUDIO-1".to_vec();
        let mut rotated = Movie::sample();
        rotated.rotated = true;

        assert_ne!(base, video.hashes().image);
        assert_ne!(base, audio.hashes().image);
        assert_ne!(base, rotated.hashes().image);
    }

    #[test]
    fn where_the_samples_sit_does_not_matter() {
        let base = Movie::sample().hashes().image;
        let mut fast_start = Movie::sample();
        fast_start.moov_first = true;
        // Video chunk 2 stored before chunk 1: read again in playback order.
        let mut shuffled = Movie::sample();
        shuffled.order = vec![(0, 1), (1, 0), (2, 0), (0, 0), (1, 1), (2, 1)];

        assert_eq!(base, fast_start.hashes().image);
        assert_eq!(base, shuffled.hashes().image);
    }

    #[test]
    fn edit_lists_compare_as_durations_not_time_scale_units() {
        let mut base = Movie::sample();
        base.edit_seconds = 3;
        let mut other_scale = base.clone();
        other_scale.movie_scale = 90_000;
        let mut longer = base.clone();
        longer.edit_seconds = 4;

        let hash = base.hashes().image;
        assert_eq!(hash, other_scale.hashes().image);
        assert_ne!(hash, longer.hashes().image);
        assert_ne!(hash, Movie::sample().hashes().image);
    }

    #[test]
    fn fragmented_or_broken_movies_get_only_the_full_hash() {
        let mut fragmented = Movie::sample();
        fragmented.fragmented = true;
        assert_eq!(fragmented.hashes().image, None);

        let mut no_av = Movie::sample();
        no_av.tracks.clear();
        no_av.order.clear();
        assert_eq!(no_av.hashes().image, None);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cut.mov");
        let movie = Movie::sample().build();
        std::fs::write(&path, &movie[..movie.len() - 10]).unwrap();
        let hashes = hash_file_and_image(&path).unwrap();
        assert_eq!(hashes.image, None);
        assert_eq!(hashes.full, crate::hash::hash_file(&path).unwrap());
    }

    #[test]
    fn large_movies_are_streamed_across_buffer_boundaries() {
        let mut movie = Movie::sample();
        // Samples bigger than the read buffer, so ranges span several reads.
        let big = |byte: u8| vec![byte; super::video_buffer() + 12345];
        movie.tracks[0].1 = vec![vec![big(1), big(2)], vec![big(3)]];
        let mut shuffled = movie.clone();
        shuffled.order = vec![(0, 1), (1, 0), (2, 0), (0, 0), (1, 1), (2, 1)];

        let streamed = movie.hashes().image;
        assert!(streamed.is_some());
        assert_eq!(streamed, shuffled.hashes().image);
    }

    // ---- files ----

    #[test]
    fn unsupported_files_get_only_the_full_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mov");
        std::fs::write(&path, b"not an image at all").unwrap();

        let hashes = hash_file_and_image(&path).unwrap();

        assert_eq!(hashes.image, None);
        assert_eq!(hashes.full, crate::hash::hash_file(&path).unwrap());
    }

    #[test]
    fn full_hash_matches_hash_file_for_images() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        std::fs::write(&path, jpeg(3)).unwrap();

        let hashes = hash_file_and_image(&path).unwrap();

        assert_eq!(hashes.full, crate::hash::hash_file(&path).unwrap());
        assert_eq!(hashes.image, image_hash(&jpeg(3)));
    }

    #[test]
    fn tiny_files_do_not_panic() {
        for data in [
            &b""[..],
            b"\xFF\xD8\xFF",
            b"\x89PNG\r\n\x1a\n",
            b"\0\0\0\x08ftyp",
        ] {
            assert_eq!(image_hash(data), None);
        }
    }
}
