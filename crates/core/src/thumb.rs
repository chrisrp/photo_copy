//! Decode photos into small RGBA thumbnails (or larger previews) for the UI.
//! JPEG, PNG, GIF, WebP, BMP and TIFF are decoded in-process and rotated according
//! to their EXIF orientation. HEIC/HEIF is converted by an external tool (`sips` on
//! macOS, `heif-convert` elsewhere). Videos have no thumbnail yet.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, non-premultiplied RGBA.
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbError {
    /// No thumbnail can be made for this kind of file (e.g. videos).
    Unsupported,
    Decode(String),
}

impl fmt::Display for ThumbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThumbError::Unsupported => write!(f, "no preview for this file type"),
            ThumbError::Decode(msg) => write!(f, "{msg}"),
        }
    }
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn is_video(path: &Path) -> bool {
    matches!(extension(path).as_str(), "mp4" | "mov" | "m4v" | "avi")
}

pub fn is_heif(path: &Path) -> bool {
    matches!(extension(path).as_str(), "heic" | "heif")
}

/// Decode `path` and shrink it to fit within `max_dim` x `max_dim` (never enlarging).
pub fn load_thumbnail(path: &Path, max_dim: u32) -> Result<Thumbnail, ThumbError> {
    if is_video(path) {
        return Err(ThumbError::Unsupported);
    }
    let img = if is_heif(path) {
        decode_heif(path, max_dim)?
    } else {
        decode_image(path)?
    };
    let img = if img.width() > max_dim || img.height() > max_dim {
        img.thumbnail(max_dim, max_dim)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Ok(Thumbnail {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    })
}

fn decode_error(e: impl fmt::Display) -> ThumbError {
    ThumbError::Decode(e.to_string())
}

fn decode_image(path: &Path) -> Result<DynamicImage, ThumbError> {
    let reader = ImageReader::open(path)
        .map_err(decode_error)?
        .with_guessed_format()
        .map_err(decode_error)?;
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    img.apply_orientation(orientation);
    Ok(img)
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_jpeg_path() -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("photo_copy_{}_{n}.jpg", std::process::id()))
}

fn decode_heif(path: &Path, max_dim: u32) -> Result<DynamicImage, ThumbError> {
    let tmp = temp_jpeg_path();
    let result = convert_heif_to_jpeg(path, &tmp, max_dim).and_then(|()| decode_image(&tmp));
    let _ = fs::remove_file(&tmp);
    result
}

fn run_quietly(command: &mut Command) -> bool {
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Longest side in pixels according to `sips -g`, if it can be determined.
fn sips_longest_side(path: &Path) -> Option<u32> {
    let out = Command::new("sips")
        .args(["-g", "pixelWidth", "-g", "pixelHeight"])
        .arg(path)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            l.trim()
                .strip_prefix("pixelWidth:")
                .or(l.trim().strip_prefix("pixelHeight:"))
        })
        .filter_map(|v| v.trim().parse::<u32>().ok())
        .max()
}

fn convert_heif_to_jpeg(src: &Path, dest: &Path, max_dim: u32) -> Result<(), ThumbError> {
    if cfg!(target_os = "macos") {
        let mut sips = Command::new("sips");
        sips.args(["-s", "format", "jpeg"]);
        // `-Z` also enlarges small images, so only ask for it when shrinking.
        if sips_longest_side(src).is_none_or(|side| side > max_dim) {
            sips.args(["-Z", &max_dim.to_string()]);
        }
        if run_quietly(sips.arg(src).arg("--out").arg(dest)) && dest.exists() {
            return Ok(());
        }
    }
    if run_quietly(Command::new("heif-convert").arg(src).arg(dest)) {
        if dest.exists() {
            return Ok(());
        }
        // For files holding several images heif-convert writes `name-1.jpg`, ...
        let numbered = dest.with_file_name(format!(
            "{}-1.jpg",
            dest.file_stem().unwrap_or_default().to_string_lossy()
        ));
        if numbered.exists() {
            fs::rename(&numbered, dest).map_err(decode_error)?;
            return Ok(());
        }
    }
    Err(ThumbError::Decode(
        "cannot decode HEIC (needs `sips` on macOS or `heif-convert` from libheif)".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};
    use std::io::Cursor;

    fn solid(width: u32, height: u32) -> RgbImage {
        RgbImage::from_pixel(width, height, Rgb([200, 30, 30]))
    }

    fn save(img: &RgbImage, path: &Path) {
        img.save(path).unwrap();
    }

    #[test]
    fn shrinks_large_images_keeping_aspect_ratio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.png");
        save(&solid(400, 200), &path);

        let thumb = load_thumbnail(&path, 100).unwrap();

        assert_eq!((thumb.width, thumb.height), (100, 50));
        assert_eq!(thumb.rgba.len(), 100 * 50 * 4);
    }

    #[test]
    fn never_enlarges_small_images() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.png");
        save(&solid(40, 20), &path);

        let thumb = load_thumbnail(&path, 256).unwrap();

        assert_eq!((thumb.width, thumb.height), (40, 20));
    }

    #[test]
    fn decodes_by_content_not_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("actually_png.jpg");
        save(&solid(8, 8), &dir.path().join("tmp.png"));
        fs::rename(dir.path().join("tmp.png"), &path).unwrap();

        assert!(load_thumbnail(&path, 64).is_ok());
    }

    #[test]
    fn videos_are_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.MOV");
        fs::write(&path, b"not really a video").unwrap();

        assert_eq!(load_thumbnail(&path, 64), Err(ThumbError::Unsupported));
    }

    #[test]
    fn corrupt_and_missing_files_are_decode_errors() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.jpg");
        fs::write(&bad, b"definitely not a jpeg").unwrap();

        assert!(matches!(
            load_thumbnail(&bad, 64),
            Err(ThumbError::Decode(_))
        ));
        assert!(matches!(
            load_thumbnail(&dir.path().join("nope.jpg"), 64),
            Err(ThumbError::Decode(_))
        ));
    }

    /// JPEG of the given size with an EXIF APP1 segment holding `orientation`.
    fn jpeg_with_orientation(width: u32, height: u32, orientation: u8) -> Vec<u8> {
        let mut jpeg = Vec::new();
        solid(width, height)
            .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .unwrap();

        let mut tiff = vec![b'M', b'M', 0, 42, 0, 0, 0, 8]; // big-endian header, IFD at 8
        tiff.extend([0, 1]); // one entry
        tiff.extend([0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, orientation, 0, 0]); // Orientation, SHORT
        tiff.extend([0, 0, 0, 0]); // no next IFD
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend(tiff);

        let mut app1 = vec![0xFF, 0xE1];
        app1.extend(((payload.len() + 2) as u16).to_be_bytes());
        app1.extend(payload);

        // Insert right after the SOI marker.
        jpeg.splice(2..2, app1);
        jpeg
    }

    #[test]
    fn applies_exif_orientation() {
        let dir = tempfile::tempdir().unwrap();
        let rotated = dir.path().join("rotated.jpg");
        let upright = dir.path().join("upright.jpg");
        fs::write(&rotated, jpeg_with_orientation(40, 20, 6)).unwrap(); // 90 degrees
        fs::write(&upright, jpeg_with_orientation(40, 20, 1)).unwrap();

        let rotated = load_thumbnail(&rotated, 256).unwrap();
        let upright = load_thumbnail(&upright, 256).unwrap();

        assert_eq!((upright.width, upright.height), (40, 20));
        assert_eq!((rotated.width, rotated.height), (20, 40));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn decodes_heic_through_sips() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        let heic = dir.path().join("a.heic");
        save(&solid(64, 32), &png);
        let made = run_quietly(
            Command::new("sips")
                .args(["-s", "format", "heic"])
                .arg(&png)
                .arg("--out")
                .arg(&heic),
        );
        assert!(made, "sips could not create a HEIC test file");

        let thumb = load_thumbnail(&heic, 256).unwrap();

        assert_eq!((thumb.width, thumb.height), (64, 32));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn shrinks_large_heic() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("big.png");
        let heic = dir.path().join("big.heic");
        save(&solid(800, 400), &png);
        assert!(run_quietly(
            Command::new("sips")
                .args(["-s", "format", "heic"])
                .arg(&png)
                .arg("--out")
                .arg(&heic),
        ));

        let thumb = load_thumbnail(&heic, 200).unwrap();

        assert_eq!((thumb.width, thumb.height), (200, 100));
    }
}
