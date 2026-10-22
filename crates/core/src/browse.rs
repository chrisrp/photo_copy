use std::fs;
use std::io;
use std::path::Path;

use crate::ops::plain_relative;
use crate::scan::{is_ignored_name, is_media, mtime_secs, FileEntry};

/// Contents of one folder (not recursive), for the photo browser.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listing {
    /// Names of the visible subfolders, sorted case-insensitively.
    pub dirs: Vec<String>,
    /// Media files in the folder, sorted case-insensitively by name.
    pub files: Vec<FileEntry>,
}

/// List the subfolders and media files of `root/rel`. Hidden entries (including
/// `.trash`) are skipped. `rel` must be a plain relative path (empty = the root).
pub fn list_dir(root: &Path, rel: &str) -> io::Result<Listing> {
    let dir = root.join(plain_relative(rel)?);
    let mut listing = Listing::default();

    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_ignored_name(&name) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            listing.dirs.push(name);
        } else if file_type.is_file() && is_media(&entry.path()) {
            let meta = entry.metadata()?;
            listing.files.push(FileEntry {
                path: entry.path(),
                size: meta.len(),
                mtime: mtime_secs(&meta),
            });
        }
    }

    listing.dirs.sort_by_key(|n| n.to_lowercase());
    listing.files.sort_by_key(|f| {
        f.path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
    });
    Ok(listing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_names(listing: &Listing) -> Vec<String> {
        listing
            .files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn lists_subfolders_and_media_sorted_ignoring_case() {
        let dir = tempfile::tempdir().unwrap();
        for d in ["beta", "Alpha", ".trash", "lost+found"] {
            fs::create_dir(dir.path().join(d)).unwrap();
        }
        for f in [
            "b.jpg",
            "A.png",
            "notes.txt",
            ".hidden.jpg",
            ".photo_index.json",
        ] {
            fs::write(dir.path().join(f), b"x").unwrap();
        }

        let listing = list_dir(dir.path(), "").unwrap();

        assert_eq!(listing.dirs, ["Alpha", "beta"]);
        assert_eq!(file_names(&listing), ["A.png", "b.jpg"]);
    }

    #[test]
    fn is_not_recursive_and_lists_a_subfolder() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("2024/trip")).unwrap();
        fs::write(dir.path().join("root.jpg"), b"x").unwrap();
        fs::write(dir.path().join("2024/a.jpg"), b"12345").unwrap();
        fs::write(dir.path().join("2024/trip/deep.jpg"), b"x").unwrap();

        let top = list_dir(dir.path(), "").unwrap();
        assert_eq!(top.dirs, ["2024"]);
        assert_eq!(file_names(&top), ["root.jpg"]);

        let year = list_dir(dir.path(), "2024").unwrap();
        assert_eq!(year.dirs, ["trip"]);
        assert_eq!(file_names(&year), ["a.jpg"]);
        assert_eq!(year.files[0].size, 5);
    }

    #[test]
    fn rejects_paths_outside_the_root_and_missing_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();

        assert_eq!(
            list_dir(&root, "..").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(list_dir(&root, "/etc").is_err());
        assert_eq!(
            list_dir(&root, "nope").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
