//! Check a folder's `.trash` (staging's, in the app): which trashed files still
//! have a copy in the library, and which do not (removed on purpose, or by mistake). Every trashed
//! file is read and hashed; a match found through an index is confirmed by reading
//! the indexed file again, since the index may be out of date.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use walkdir::WalkDir;

use crate::image_hash::hash_file_and_image;
use crate::index::Index;
use crate::ops::TRASH_DIR;
use crate::scan::{is_ignored_name, is_media};

/// An indexed folder to look for copies in.
#[derive(Clone, Copy)]
pub struct Folder<'a> {
    pub root: &'a Path,
    pub index: &'a Index,
}

/// Where a copy of a trashed file was found (paths relative to that folder's root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Copy {
    /// An identical file in the library.
    Identical(String),
    /// The same photo or video in the library, with other metadata.
    SameImage(String),
    /// No copy in the library.
    None,
}

impl Copy {
    pub fn in_library(&self) -> bool {
        matches!(self, Copy::Identical(_) | Copy::SameImage(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashedFile {
    pub path: PathBuf,
    /// Path inside the `.trash` folder, `/`-separated (its original place).
    pub rel: String,
    pub size: u64,
    pub copy: Copy,
}

/// The media files in `root/.trash`, sorted by path. Hidden entries inside it
/// (`.DS_Store`, `._` files) and non-media files (trash logs) are skipped.
pub fn trashed_files(root: &Path) -> Vec<PathBuf> {
    let trash = root.join(TRASH_DIR);
    let mut files: Vec<PathBuf> = WalkDir::new(&trash)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !is_ignored_name(&e.file_name().to_string_lossy()))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && is_media(e.path()))
        .map(|e| e.into_path())
        .collect();
    files.sort();
    files
}

/// Check every media file in `root/.trash` against `library`. `done` counts
/// checked files. A file that cannot be read is reported with `Copy::None`.
pub fn check_trash(root: &Path, library: Folder, done: &AtomicUsize) -> Vec<TrashedFile> {
    let trash = root.join(TRASH_DIR);
    trashed_files(root)
        .into_par_iter()
        .map(|path| {
            let (size, copy) = match hash_file_and_image(&path) {
                Ok(hashes) => {
                    let size = std::fs::metadata(&path).map_or(0, |m| m.len());
                    let copy = find_copy(&hashes.full, hashes.image.as_deref(), library);
                    (size, copy)
                }
                Err(_) => (0, Copy::None),
            };
            done.fetch_add(1, Ordering::Relaxed);
            let rel = path
                .strip_prefix(&trash)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            TrashedFile {
                path,
                rel,
                size,
                copy,
            }
        })
        .collect()
}

fn find_copy(full: &str, image: Option<&str>, library: Folder) -> Copy {
    if let Some(rel) = confirmed(library, |r| r.hash == full, |h| h.full == full) {
        return Copy::Identical(rel);
    }
    if let Some(image) = image {
        let same_image = |h: &crate::image_hash::FileHashes| h.image.as_deref() == Some(image);
        if let Some(rel) = confirmed(library, |r| r.image.as_deref() == Some(image), same_image) {
            return Copy::SameImage(rel);
        }
    }
    Copy::None
}

/// The first indexed file of `folder` that the index says matches (`indexed`) and
/// that still matches when read from disk (`on_disk`).
fn confirmed(
    folder: Folder,
    indexed: impl Fn(&crate::index::IndexRecord) -> bool,
    on_disk: impl Fn(&crate::image_hash::FileHashes) -> bool,
) -> Option<String> {
    folder
        .index
        .files
        .iter()
        .filter(|(_, record)| indexed(record))
        .find(|(rel, _)| {
            hash_file_and_image(&folder.root.join(rel.as_str())).is_ok_and(|h| on_disk(&h))
        })
        .map(|(rel, _)| rel.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn indexed(root: &Path) -> Index {
        let mut index = Index::default();
        index
            .update(root, &crate::scan::scan(root).unwrap().entries)
            .unwrap();
        index
    }

    fn copy_of<'a>(files: &'a [TrashedFile], rel: &str) -> &'a Copy {
        &files.iter().find(|f| f.rel == rel).unwrap().copy
    }

    #[test]
    fn classifies_trashed_files() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "2021/beach.jpg", b"beach");
        // Its twin is still in staging, but only the destination counts.
        write(stg.path(), "later/IMG_9.jpg", b"not moved yet");
        write(stg.path(), ".trash/IMG_1.jpg", b"beach");
        write(stg.path(), ".trash/phone/IMG_9 2.jpg", b"not moved yet");
        write(stg.path(), ".trash/unwanted.jpg", b"blurry");
        write(stg.path(), ".trash/trash-log-2026.csv", b"status,...");
        write(stg.path(), ".trash/.DS_Store", b"x");
        let lib_index = indexed(lib.path());
        let library = Folder {
            root: lib.path(),
            index: &lib_index,
        };
        let done = AtomicUsize::new(0);

        let files = check_trash(stg.path(), library, &done);

        assert_eq!(files.len(), 3);
        assert_eq!(done.load(Ordering::Relaxed), 3);
        assert_eq!(
            copy_of(&files, "IMG_1.jpg"),
            &Copy::Identical("2021/beach.jpg".into())
        );
        assert_eq!(copy_of(&files, "phone/IMG_9 2.jpg"), &Copy::None);
        assert_eq!(copy_of(&files, "unwanted.jpg"), &Copy::None);
        assert_eq!(
            files.iter().find(|f| f.rel == "unwanted.jpg").unwrap().size,
            6
        );
    }

    #[test]
    fn same_photo_with_other_metadata_counts_as_in_the_library() {
        let lib = tempfile::tempdir().unwrap();
        let heic = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/staging/IMG_0007.heic"),
        )
        .unwrap();
        let mut other = heic.clone();
        other.extend_from_slice(b"\0\0\0\x10freeexport-i");
        write(lib.path(), "2021/IMG_7.heic", &heic);
        write(lib.path(), ".trash/extra/IMG_7 2.heic", &other);
        let index = indexed(lib.path());

        let files = check_trash(
            lib.path(),
            Folder {
                root: lib.path(),
                index: &index,
            },
            &AtomicUsize::new(0),
        );

        assert_eq!(files[0].copy, Copy::SameImage("2021/IMG_7.heic".into()));
        assert!(files[0].copy.in_library());
    }

    #[test]
    fn an_outdated_index_is_not_trusted() {
        let lib = tempfile::tempdir().unwrap();
        write(lib.path(), "2021/beach.jpg", b"beach");
        write(lib.path(), ".trash/IMG_1.jpg", b"beach");
        let index = indexed(lib.path());
        // Changed after indexing: the index still claims the same hash.
        write(lib.path(), "2021/beach.jpg", b"edited");

        let files = check_trash(
            lib.path(),
            Folder {
                root: lib.path(),
                index: &index,
            },
            &AtomicUsize::new(0),
        );

        assert_eq!(files[0].copy, Copy::None);
    }

    #[test]
    fn no_trash_folder_means_no_files() {
        let lib = tempfile::tempdir().unwrap();
        assert!(trashed_files(lib.path()).is_empty());
    }
}
