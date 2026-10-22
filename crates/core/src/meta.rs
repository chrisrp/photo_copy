//! Per-file details shown in the list view: date taken, kind label and name ordering.

use std::cmp::Ordering;
use std::fs::File;
use std::io::BufReader;
use std::iter::Peekable;
use std::path::Path;
use std::str::Chars;

use chrono::{DateTime, Local, NaiveDateTime};
use rayon::prelude::*;

use crate::scan::FileEntry;
use crate::thumb::is_video;

/// When a photo was taken, as a local wall-clock time without time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhotoDate {
    pub when: NaiveDateTime,
    /// False when the file has no usable EXIF date and its modified time is used.
    pub from_exif: bool,
}

/// A file's modification time (seconds since the Unix epoch) as local wall-clock time.
pub fn fallback_date(mtime_secs: i64) -> NaiveDateTime {
    DateTime::from_timestamp(mtime_secs, 0)
        .unwrap_or_default()
        .with_timezone(&Local)
        .naive_local()
}

/// The EXIF "date taken" of an image (original capture time, else the EXIF date).
/// Cameras that did not know the time write zeros, which count as missing.
pub fn exif_date_taken(path: &Path) -> Option<NaiveDateTime> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    let exif = exif::Reader::new().read_from_container(&mut reader).ok()?;
    [exif::Tag::DateTimeOriginal, exif::Tag::DateTime]
        .into_iter()
        .find_map(|tag| {
            let field = exif.get_field(tag, exif::In::PRIMARY)?;
            let exif::Value::Ascii(values) = &field.value else {
                return None;
            };
            let text = std::str::from_utf8(values.first()?).ok()?;
            NaiveDateTime::parse_from_str(text.trim_matches(['\0', ' ']), "%Y:%m:%d %H:%M:%S").ok()
        })
}

/// Date taken from EXIF, falling back to the modified time (`mtime_secs`).
pub fn date_taken(path: &Path, mtime_secs: i64) -> PhotoDate {
    let exif_date = if is_video(path) {
        None
    } else {
        exif_date_taken(path)
    };
    match exif_date {
        Some(when) => PhotoDate {
            when,
            from_exif: true,
        },
        None => PhotoDate {
            when: fallback_date(mtime_secs),
            from_exif: false,
        },
    }
}

/// `date_taken` for many files, read in parallel; the result has the same order as `files`.
pub fn read_dates(files: &[FileEntry]) -> Vec<PhotoDate> {
    files
        .par_iter()
        .map(|f| date_taken(&f.path, f.mtime))
        .collect()
}

/// Every EXIF field of an image (JPEG, PNG, HEIF, TIFF, WebP) as `(name, value)`,
/// sorted by name. Fields of the embedded thumbnail are named `Thumbnail <tag>`.
/// Long values (binary data such as a MakerNote) are shortened. Empty when the file
/// has no EXIF data or cannot be read.
pub fn metadata_fields(path: &Path) -> Vec<(String, String)> {
    const MAX_VALUE: usize = 80;
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(exif) = exif::Reader::new().read_from_container(&mut BufReader::new(file)) else {
        return Vec::new();
    };
    let mut fields: Vec<(String, String)> = exif
        .fields()
        .map(|field| {
            let name = match field.ifd_num {
                exif::In::PRIMARY => field.tag.to_string(),
                exif::In::THUMBNAIL => format!("Thumbnail {}", field.tag),
                exif::In(n) => format!("Image {n} {}", field.tag),
            };
            let mut value = field.display_value().with_unit(&exif).to_string();
            if value.chars().count() > MAX_VALUE {
                let digest = blake3::hash(value.as_bytes()).to_hex();
                value = format!(
                    "{}... ({} chars, #{})",
                    value.chars().take(40).collect::<String>(),
                    value.chars().count(),
                    &digest[..8]
                );
            }
            (name, value)
        })
        .collect();
    fields.sort();
    fields
}

pub const FOLDER_KIND: &str = "Folder";

/// Finder-style description of a media file, from its extension.
pub fn kind_label(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "JPEG image",
        "png" => "PNG image",
        "gif" => "GIF image",
        "webp" => "WebP image",
        "bmp" => "BMP image",
        "tif" | "tiff" => "TIFF image",
        "heic" => "HEIC image",
        "heif" => "HEIF image",
        "mp4" | "m4v" => "MPEG-4 movie",
        "mov" => "QuickTime movie",
        "avi" => "AVI movie",
        _ => "File",
    }
}

fn take_number(chars: &mut Peekable<Chars<'_>>) -> String {
    let mut digits = String::new();
    while let Some(&c) = chars.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        digits.push(c);
        chars.next();
    }
    digits.trim_start_matches('0').to_string()
}

/// Case-insensitive ordering that compares digit runs by value, so `IMG_2` sorts
/// before `IMG_10` (like Finder).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let (na, nb) = (take_number(&mut a), take_number(&mut b));
                let order = na.len().cmp(&nb.len()).then_with(|| na.cmp(&nb));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                let order = x.to_lowercase().cmp(y.to_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                a.next();
                b.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, NaiveDate};
    use image::{ImageFormat, Rgb, RgbImage};
    use std::fs;
    use std::io::Cursor;

    /// Big-endian TIFF with `DateTime` in IFD0 and `DateTimeOriginal` in the Exif IFD.
    fn tiff_with_dates(date_time: &str, original: &str) -> Vec<u8> {
        assert!(date_time.len() == 19 && original.len() == 19);
        let mut t = vec![b'M', b'M', 0, 42, 0, 0, 0, 8];
        t.extend([0, 2]); // IFD0: two entries
        t.extend([0x01, 0x32, 0, 2, 0, 0, 0, 20, 0, 0, 0, 38]); // DateTime -> offset 38
        t.extend([0x87, 0x69, 0, 4, 0, 0, 0, 1, 0, 0, 0, 58]); // Exif IFD pointer -> 58
        t.extend([0, 0, 0, 0]);
        assert_eq!(t.len(), 38);
        t.extend(date_time.as_bytes());
        t.push(0);
        t.extend([0, 1]); // Exif IFD: one entry
        t.extend([0x90, 0x03, 0, 2, 0, 0, 0, 20, 0, 0, 0, 76]); // DateTimeOriginal -> 76
        t.extend([0, 0, 0, 0]);
        assert_eq!(t.len(), 76);
        t.extend(original.as_bytes());
        t.push(0);
        t
    }

    fn jpeg_with_dates(date_time: &str, original: &str) -> Vec<u8> {
        let mut jpeg = Vec::new();
        RgbImage::from_pixel(8, 8, Rgb([1, 2, 3]))
            .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .unwrap();
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend(tiff_with_dates(date_time, original));
        let mut app1 = vec![0xFF, 0xE1];
        app1.extend(((payload.len() + 2) as u16).to_be_bytes());
        app1.extend(payload);
        jpeg.splice(2..2, app1);
        jpeg
    }

    fn date(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, s)
            .unwrap()
    }

    #[test]
    fn metadata_fields_lists_exif_fields_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        fs::write(
            &path,
            jpeg_with_dates("2020:01:02 03:04:05", "2019:12:31 23:59:58"),
        )
        .unwrap();

        let fields = metadata_fields(&path);

        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"DateTime"), "{names:?}");
        assert!(names.contains(&"DateTimeOriginal"), "{names:?}");
        let original = fields
            .iter()
            .find(|(n, _)| n == "DateTimeOriginal")
            .unwrap();
        assert_eq!(original.1, "2019-12-31 23:59:58");
        assert!(metadata_fields(&dir.path().join("missing.jpg")).is_empty());
    }

    #[test]
    fn prefers_exif_original_date() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        fs::write(
            &path,
            jpeg_with_dates("2020:01:01 00:00:00", "2024:07:15 18:30:05"),
        )
        .unwrap();

        let taken = date_taken(&path, 0);

        assert_eq!(taken.when, date(2024, 7, 15, 18, 30, 5));
        assert!(taken.from_exif);
    }

    #[test]
    fn uses_exif_date_when_original_is_zeroed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        fs::write(
            &path,
            jpeg_with_dates("2020:01:01 10:20:30", "0000:00:00 00:00:00"),
        )
        .unwrap();

        let taken = date_taken(&path, 0);

        assert_eq!(taken.when, date(2020, 1, 1, 10, 20, 30));
        assert!(taken.from_exif);
    }

    #[test]
    fn falls_back_to_modified_time_without_exif() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        RgbImage::new(4, 4).save(&png).unwrap();
        let mov = dir.path().join("clip.mov");
        fs::write(&mov, b"not a real video").unwrap();

        for path in [png, mov, dir.path().join("missing.jpg")] {
            let taken = date_taken(&path, 1_700_000_000);
            assert!(!taken.from_exif);
            assert_eq!(taken.when, fallback_date(1_700_000_000));
            assert_eq!(taken.when.year(), 2023); // the same year in every time zone
        }
    }

    #[test]
    fn read_dates_keeps_the_order_of_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let exif = dir.path().join("a.jpg");
        let plain = dir.path().join("b.png");
        fs::write(
            &exif,
            jpeg_with_dates("2020:01:01 00:00:00", "2019:05:06 07:08:09"),
        )
        .unwrap();
        RgbImage::new(4, 4).save(&plain).unwrap();
        let files = [
            FileEntry {
                path: plain,
                size: 1,
                mtime: 1_700_000_000,
            },
            FileEntry {
                path: exif,
                size: 1,
                mtime: 0,
            },
        ];

        let dates = read_dates(&files);

        assert!(!dates[0].from_exif);
        assert_eq!(dates[1].when, date(2019, 5, 6, 7, 8, 9));
    }

    #[test]
    fn kind_labels_are_finder_like() {
        assert_eq!(kind_label(Path::new("a.JPG")), "JPEG image");
        assert_eq!(kind_label(Path::new("a.heic")), "HEIC image");
        assert_eq!(kind_label(Path::new("a.mov")), "QuickTime movie");
        assert_eq!(kind_label(Path::new("a.xyz")), "File");
        assert_eq!(kind_label(Path::new("noext")), "File");
    }

    #[test]
    fn natural_order_compares_numbers_by_value_and_ignores_case() {
        assert_eq!(natural_cmp("IMG_2.jpg", "IMG_10.jpg"), Ordering::Less);
        assert_eq!(natural_cmp("img_10.jpg", "IMG_2.jpg"), Ordering::Greater);
        assert_eq!(natural_cmp("a.jpg", "B.jpg"), Ordering::Less);
        assert_eq!(natural_cmp("IMG_007.jpg", "IMG_7.jpg"), Ordering::Equal);
        assert_eq!(natural_cmp("a", "a1"), Ordering::Less);
        assert_eq!(natural_cmp("", ""), Ordering::Equal);
        let mut names = ["IMG_10", "img_2", "IMG_1", "Beach"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, ["Beach", "IMG_1", "img_2", "IMG_10"]);
    }
}
