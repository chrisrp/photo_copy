use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::image_hash::{self, hash_file_and_image};
use crate::scan::FileEntry;

pub const INDEX_FILE: &str = ".photo_index.json";
/// Follows the image hash version: 1 had no image hashes, 2 had them for photos,
/// 3 also for videos. See [`Index::reusable`] for what an older index keeps.
const INDEX_VERSION: u32 = 1 + image_hash::VERSION;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRecord {
    pub size: u64,
    pub mtime: i64,
    /// BLAKE3 of the whole file.
    pub hash: String,
    /// Hash of the image data without metadata (see `image_hash`); `None` for
    /// videos and other unsupported or unparsable files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateStats {
    pub hashed: usize,
    pub reused: usize,
    pub removed: usize,
    /// Files that could not be read (so they are not in the index), and anything the
    /// caller added about unreadable folders. Each entry says which file and why.
    pub skipped: Vec<String>,
}

/// Hash database for one root folder. Keys are paths relative to the root,
/// always using `/` as separator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub version: u32,
    pub files: BTreeMap<String, IndexRecord>,
}

impl Default for Index {
    fn default() -> Self {
        Index {
            version: INDEX_VERSION,
            files: BTreeMap::new(),
        }
    }
}

fn rel_key(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

impl Index {
    /// Whether the index was built by this version (an older one lacks image hashes
    /// until the folder is scanned again).
    pub fn is_current(&self) -> bool {
        self.version == INDEX_VERSION
    }

    /// Whether the stored hashes of an unchanged file are still valid. A version 2
    /// index already holds today's photo hashes: only files without an image hash
    /// (videos, then) are read again. Anything older is read again entirely.
    fn reusable(&self, record: &IndexRecord) -> bool {
        match self.version {
            INDEX_VERSION => true,
            2 => record.image.is_some(),
            _ => false,
        }
    }

    /// Load the index stored in `root`; a missing index file yields an empty index.
    pub fn load(root: &Path) -> io::Result<Index> {
        match fs::read(root.join(INDEX_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Index::default()),
            Err(e) => Err(e),
        }
    }

    /// Write the index atomically (temp file, then rename).
    pub fn save(&self, root: &Path) -> io::Result<()> {
        let tmp = root.join(format!("{INDEX_FILE}.tmp"));
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, json)?;
        fs::rename(&tmp, root.join(INDEX_FILE))
    }

    /// Bring the index in line with the scanned `entries`. Files whose size and
    /// mtime are unchanged keep their stored hashes; new or changed files are hashed
    /// in parallel; records for files no longer present are dropped. A file that
    /// cannot be read is reported in `skipped` and left out of the index. Records of
    /// an older index are kept only where still valid (see [`Index::reusable`]).
    pub fn update(&mut self, root: &Path, entries: &[FileEntry]) -> io::Result<UpdateStats> {
        let mut stats = UpdateStats::default();
        let mut present = HashSet::new();
        let mut to_hash = Vec::new();

        for entry in entries {
            let key = rel_key(root, &entry.path);
            present.insert(key.clone());
            let unchanged = self.files.get(&key).is_some_and(|r| {
                r.size == entry.size && r.mtime == entry.mtime && self.reusable(r)
            });
            if unchanged {
                stats.reused += 1;
            } else {
                to_hash.push((key, entry));
            }
        }

        let results: Vec<_> = to_hash
            .par_iter()
            .map(|(key, entry)| (key.clone(), *entry, hash_file_and_image(&entry.path)))
            .collect();
        for (key, entry, result) in results {
            match result {
                Ok(hashes) => {
                    self.files.insert(
                        key,
                        IndexRecord {
                            size: entry.size,
                            mtime: entry.mtime,
                            hash: hashes.full,
                            image: hashes.image,
                        },
                    );
                    stats.hashed += 1;
                }
                Err(e) => {
                    // An unreadable file must not keep a stale hash from an earlier run.
                    self.files.remove(&key);
                    stats.skipped.push(format!("{}: {e}", entry.path.display()));
                }
            }
        }

        let before = self.files.len();
        self.files.retain(|key, _| present.contains(key));
        stats.removed = before - self.files.len();
        self.version = INDEX_VERSION;

        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::scan;

    fn build(root: &Path, index: &mut Index) -> UpdateStats {
        let entries = scan(root).unwrap().entries;
        index.update(root, &entries).unwrap()
    }

    #[test]
    fn first_build_hashes_everything() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("2024")).unwrap();
        fs::write(dir.path().join("2024/a.jpg"), b"aaa").unwrap();
        fs::write(dir.path().join("b.jpg"), b"bbb").unwrap();

        let mut index = Index::default();
        let stats = build(dir.path(), &mut index);

        assert_eq!(
            stats,
            UpdateStats {
                hashed: 2,
                reused: 0,
                removed: 0,
                skipped: vec![],
            }
        );
        assert!(index.files.contains_key("2024/a.jpg"));
        assert!(index.files.contains_key("b.jpg"));
    }

    #[test]
    fn second_build_reuses_unchanged_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.jpg"), b"aaa").unwrap();

        let mut index = Index::default();
        build(dir.path(), &mut index);
        let stats = build(dir.path(), &mut index);

        assert_eq!(
            stats,
            UpdateStats {
                hashed: 0,
                reused: 1,
                removed: 0,
                skipped: vec![],
            }
        );
    }

    #[test]
    fn changed_file_is_rehashed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.jpg");
        fs::write(&file, b"aaa").unwrap();

        let mut index = Index::default();
        build(dir.path(), &mut index);
        let old_hash = index.files["a.jpg"].hash.clone();

        // Different size, so the change is detected even if mtime granularity is coarse.
        fs::write(&file, b"aaaa-changed").unwrap();
        let stats = build(dir.path(), &mut index);

        assert_eq!(stats.hashed, 1);
        assert_ne!(index.files["a.jpg"].hash, old_hash);
    }

    #[test]
    fn deleted_file_is_dropped_from_index() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.jpg"), b"aaa").unwrap();
        fs::write(dir.path().join("b.jpg"), b"bbb").unwrap();

        let mut index = Index::default();
        build(dir.path(), &mut index);
        fs::remove_file(dir.path().join("b.jpg")).unwrap();
        let stats = build(dir.path(), &mut index);

        assert_eq!(stats.removed, 1);
        assert!(!index.files.contains_key("b.jpg"));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.jpg"), b"aaa").unwrap();

        let mut index = Index::default();
        build(dir.path(), &mut index);
        index.save(dir.path()).unwrap();

        assert_eq!(Index::load(dir.path()).unwrap(), index);
        assert!(!dir.path().join(format!("{INDEX_FILE}.tmp")).exists());
    }

    #[test]
    fn images_get_an_image_hash_and_other_files_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/staging/IMG_0007.heic");
        fs::copy(src, dir.path().join("a.heic")).unwrap();
        fs::write(dir.path().join("clip.mov"), b"video").unwrap();

        let mut index = Index::default();
        build(dir.path(), &mut index);

        assert!(index.files["a.heic"].image.is_some());
        assert_eq!(index.files["clip.mov"].image, None);
    }

    #[test]
    fn an_old_index_is_hashed_again_and_upgraded() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.jpg"), b"aaa").unwrap();
        // A version 1 index: no image hashes.
        let old = r#"{"version":1,"files":{"a.jpg":{"size":3,"mtime":0,"hash":"x"}}}"#;
        fs::write(dir.path().join(INDEX_FILE), old).unwrap();

        let mut index = Index::load(dir.path()).unwrap();
        assert!(!index.is_current());
        let stats = build(dir.path(), &mut index);

        assert_eq!((stats.hashed, stats.reused), (1, 0));
        assert!(index.is_current());
        assert_ne!(index.files["a.jpg"].hash, "x");
    }

    #[test]
    fn a_photo_only_index_keeps_photo_hashes_and_reads_the_rest_again() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.jpg"), b"aaa").unwrap();
        fs::write(dir.path().join("clip.mov"), b"video").unwrap();
        let scanned = scan(dir.path()).unwrap().entries;
        let mtime = |name: &str| {
            scanned
                .iter()
                .find(|e| e.path.ends_with(name))
                .unwrap()
                .mtime
        };
        // Version 2: the photo has an image hash, the video does not.
        let old = format!(
            r#"{{"version":2,"files":{{
                "a.jpg":{{"size":3,"mtime":{},"hash":"kept","image":"photo"}},
                "clip.mov":{{"size":5,"mtime":{},"hash":"old"}}}}}}"#,
            mtime("a.jpg"),
            mtime("clip.mov")
        );
        fs::write(dir.path().join(INDEX_FILE), old).unwrap();

        let mut index = Index::load(dir.path()).unwrap();
        assert!(!index.is_current());
        let stats = build(dir.path(), &mut index);

        assert_eq!((stats.hashed, stats.reused), (1, 1));
        assert_eq!(index.files["a.jpg"].hash, "kept");
        assert_ne!(index.files["clip.mov"].hash, "old");
        assert!(index.is_current());
    }

    #[test]
    fn load_missing_index_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Index::load(dir.path()).unwrap(), Index::default());
    }

    #[test]
    fn index_file_is_not_indexed_itself() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.jpg"), b"aaa").unwrap();

        let mut index = Index::default();
        build(dir.path(), &mut index);
        index.save(dir.path()).unwrap();
        build(dir.path(), &mut index);

        assert_eq!(index.files.len(), 1);
    }

    /// Make `path` unreadable. Returns false when permissions are not enforced
    /// (running as root, or a platform without Unix permissions): nothing to test then.
    #[cfg(unix)]
    fn make_unreadable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
        fs::File::open(path).is_err()
    }

    #[cfg(unix)]
    fn make_readable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_skipped_and_the_rest_is_still_indexed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ok.jpg"), b"ok").unwrap();
        let locked = dir.path().join("locked.jpg");
        fs::write(&locked, b"secret").unwrap();
        let enforced = make_unreadable(&locked);

        let mut index = Index::default();
        let stats = build(dir.path(), &mut index);

        make_readable(&locked);
        if !enforced {
            return;
        }
        assert_eq!(stats.hashed, 1);
        assert_eq!(stats.skipped.len(), 1);
        assert!(
            stats.skipped[0].contains("locked.jpg"),
            "{:?}",
            stats.skipped
        );
        assert!(index.files.contains_key("ok.jpg"));
        assert!(!index.files.contains_key("locked.jpg"));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_becomes_unreadable_loses_its_old_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        fs::write(&path, b"first").unwrap();
        let mut index = Index::default();
        build(dir.path(), &mut index);
        assert!(index.files.contains_key("a.jpg"));

        fs::write(&path, b"second, different size").unwrap();
        let enforced = make_unreadable(&path);
        let stats = build(dir.path(), &mut index);

        make_readable(&path);
        if !enforced {
            return;
        }
        assert_eq!(stats.skipped.len(), 1);
        assert!(!index.files.contains_key("a.jpg"), "stale hash kept");
    }
}
