use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use walkdir::WalkDir;

const MEDIA_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif", "mp4", "mov", "m4v",
    "avi",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    /// Modification time in seconds since the Unix epoch (0 if unavailable).
    pub mtime: i64,
}

pub fn is_media(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| MEDIA_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Modification time in whole seconds since the Unix epoch (0 if unavailable).
pub fn mtime_secs(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Names that are never part of a photo library: hidden entries (which covers
/// `.trash` and the index file) and operating system folders found at the top of
/// drives.
pub fn is_ignored_name(name: &str) -> bool {
    const SYSTEM_FOLDERS: [&str; 3] = ["System Volume Information", "$RECYCLE.BIN", "lost+found"];
    name.starts_with('.') || SYSTEM_FOLDERS.iter().any(|f| name.eq_ignore_ascii_case(f))
}

/// Result of scanning a folder.
#[derive(Debug, Default)]
pub struct Scan {
    /// Media files found, sorted by path.
    pub entries: Vec<FileEntry>,
    /// One message for every folder or file that could not be read (permissions,
    /// locked files, ...). Such entries are left out; the scan carries on.
    pub warnings: Vec<String>,
}

/// Recursively list media files under `root`, skipping ignored names (see
/// [`is_ignored_name`]). Only an unreadable `root` is an error; anything unreadable
/// below it becomes a warning.
pub fn scan(root: &Path) -> io::Result<Scan> {
    let mut scan = Scan::default();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_ignored_name(&e.file_name().to_string_lossy()));

    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) if e.depth() == 0 => return Err(e.into()),
            Err(e) => {
                scan.warnings.push(e.to_string());
                continue;
            }
        };
        if !entry.file_type().is_file() || !is_media(entry.path()) {
            continue;
        }
        match entry.metadata() {
            Ok(meta) => scan.entries.push(FileEntry {
                size: meta.len(),
                mtime: mtime_secs(&meta),
                path: entry.into_path(),
            }),
            Err(e) => scan
                .warnings
                .push(format!("{}: {e}", entry.path().display())),
        }
    }
    scan.entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Paths relative to `root`, always joined with `/` (also on Windows).
    fn names(root: &Path, entries: &[FileEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| {
                e.path
                    .strip_prefix(root)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect()
    }

    #[test]
    fn finds_media_recursively_with_size() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("2024/trip")).unwrap();
        fs::write(dir.path().join("2024/a.JPG"), b"12345").unwrap();
        fs::write(dir.path().join("2024/trip/b.heic"), b"1").unwrap();

        let entries = scan(dir.path()).unwrap().entries;

        assert_eq!(
            names(dir.path(), &entries),
            ["2024/a.JPG", "2024/trip/b.heic"]
        );
        assert_eq!(entries[0].size, 5);
    }

    #[test]
    fn ignores_non_media_and_hidden() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".trash")).unwrap();
        fs::write(dir.path().join(".trash/old.jpg"), b"x").unwrap();
        fs::write(dir.path().join(".hidden.jpg"), b"x").unwrap();
        fs::write(dir.path().join("notes.txt"), b"x").unwrap();
        fs::write(dir.path().join("keep.png"), b"x").unwrap();

        let entries = scan(dir.path()).unwrap().entries;

        assert_eq!(names(dir.path(), &entries), ["keep.png"]);
    }

    #[test]
    fn ignores_operating_system_folders() {
        let dir = tempfile::tempdir().unwrap();
        for d in [
            "lost+found",
            "$RECYCLE.BIN",
            "System Volume Information",
            "2024",
        ] {
            fs::create_dir(dir.path().join(d)).unwrap();
            fs::write(dir.path().join(d).join("a.jpg"), b"x").unwrap();
        }

        let scan = scan(dir.path()).unwrap();

        assert_eq!(names(dir.path(), &scan.entries), ["2024/a.jpg"]);
        assert!(scan.warnings.is_empty());
        assert!(is_ignored_name(".trash") && is_ignored_name("LOST+FOUND"));
        assert!(!is_ignored_name("Holiday"));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_folder_becomes_a_warning_and_the_scan_continues() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ok.jpg"), b"x").unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("hidden_from_us.jpg"), b"x").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // Permissions are not enforced when running as root: nothing to test then.
        let enforced = fs::read_dir(&locked).is_err();

        let result = scan(dir.path());

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if !enforced {
            return;
        }
        let scan = result.unwrap();
        assert_eq!(names(dir.path(), &scan.entries), ["ok.jpg"]);
        assert_eq!(scan.warnings.len(), 1);
        assert!(scan.warnings[0].contains("locked"), "{:?}", scan.warnings);
    }

    #[test]
    fn missing_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan(&dir.path().join("nope")).is_err());
    }
}
