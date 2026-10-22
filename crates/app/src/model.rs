//! UI-independent application state: the selected folders, their indexes and the
//! derived duplicate / staging-check results. Everything here is unit tested; the
//! egui layer only renders it and forwards user actions.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use photo_copy_core::compare::{compare_staging, StagingItem, StagingStatus};
use photo_copy_core::duplicates::{
    find_duplicates, find_same_image, DuplicateGroup, SameImageGroup,
};
use photo_copy_core::image_hash::hash_file_and_image;
use photo_copy_core::index::{Index, IndexRecord, UpdateStats, INDEX_FILE};
use photo_copy_core::ops::{
    create_folder, move_file, move_to_trash, plain_name, plain_relative, MoveOutcome,
};
use photo_copy_core::scan::{mtime_secs, scan};
use photo_copy_core::verified_trash::Candidate;

use crate::i18n::tr;
use crate::paths::{file_name, join_dir};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Library,
    Staging,
}

impl Side {
    pub fn label(self) -> &'static str {
        match self {
            Side::Library => tr("Destination"),
            Side::Staging => tr("Staging"),
        }
    }
}

pub struct Folder {
    pub root: PathBuf,
    pub index: Index,
    pub duplicates: Vec<DuplicateGroup>,
    /// Copies of the same photo whose metadata differs (empty until the index is
    /// current, see `Index::is_current`).
    pub same_image: Vec<SameImageGroup>,
    /// When the index was last built: the saved index file's time on open, or the
    /// end of the last build. `None` means the folder was never indexed.
    pub indexed_at: Option<SystemTime>,
}

impl Folder {
    fn open(root: PathBuf) -> io::Result<Folder> {
        let index = Index::load(&root)?;
        let indexed_at = fs::metadata(root.join(INDEX_FILE))
            .and_then(|m| m.modified())
            .ok();
        let mut folder = Folder {
            root,
            index: Index::default(),
            duplicates: Vec::new(),
            same_image: Vec::new(),
            indexed_at,
        };
        folder.set_index(index);
        Ok(folder)
    }

    pub fn is_indexed(&self) -> bool {
        self.indexed_at.is_some()
    }

    /// Indexed by an older version: same-photo copies are only found after a rescan.
    pub fn needs_rescan(&self) -> bool {
        self.is_indexed() && !self.index.is_current()
    }

    fn set_index(&mut self, index: Index) {
        self.index = index;
        self.refresh_groups();
    }

    fn refresh_groups(&mut self) {
        self.duplicates = find_duplicates(&self.index);
        self.same_image = find_same_image(&self.index);
    }
}

/// Duplicates left per area: groups inside the destination, groups inside staging,
/// and staging files already in the destination; each counts exact copies and
/// same-photo copies with other metadata. `None` means not known yet (folder missing,
/// never indexed or indexed by an older version, or the staging check was not run),
/// which is different from `Some(0)`: nothing is shown as clean until it was
/// actually checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DuplicateStatus {
    pub library: Option<usize>,
    pub staging: Option<usize>,
    pub against_library: Option<usize>,
}

impl DuplicateStatus {
    /// Total left, only when all three areas are known.
    pub fn remaining(&self) -> Option<usize> {
        Some(self.library? + self.staging? + self.against_library?)
    }

    /// Total left among the areas that are known.
    pub fn known_remaining(&self) -> usize {
        [self.library, self.staging, self.against_library]
            .iter()
            .flatten()
            .sum()
    }
}

#[derive(Default)]
pub struct Model {
    library: Option<Folder>,
    staging: Option<Folder>,
    check: Option<Vec<StagingItem>>,
}

impl Model {
    pub fn folder(&self, side: Side) -> Option<&Folder> {
        match side {
            Side::Library => self.library.as_ref(),
            Side::Staging => self.staging.as_ref(),
        }
    }

    fn folder_mut(&mut self, side: Side) -> Option<&mut Folder> {
        match side {
            Side::Library => self.library.as_mut(),
            Side::Staging => self.staging.as_mut(),
        }
    }

    /// Select a folder, loading its saved index if there is one (nothing is scanned).
    pub fn set_root(&mut self, side: Side, root: PathBuf) -> io::Result<()> {
        let folder = Folder::open(root)?;
        match side {
            Side::Library => self.library = Some(folder),
            Side::Staging => self.staging = Some(folder),
        }
        self.check = None;
        Ok(())
    }

    /// Install a freshly built index for a folder.
    pub fn apply_index(&mut self, side: Side, index: Index) {
        if let Some(folder) = self.folder_mut(side) {
            folder.set_index(index);
            folder.indexed_at = Some(SystemTime::now());
        }
        self.refresh_check();
    }

    /// Both folders are selected and indexed.
    pub fn folders_ready(&self) -> bool {
        [&self.library, &self.staging]
            .iter()
            .all(|f| f.as_ref().is_some_and(Folder::is_indexed))
    }

    /// What is left to do in the duplicates step.
    pub fn duplicate_status(&self) -> DuplicateStatus {
        let current = |folder: &Option<Folder>| {
            folder
                .as_ref()
                .is_some_and(|f| f.is_indexed() && f.index.is_current())
        };
        let groups = |folder: &Option<Folder>| {
            folder
                .as_ref()
                .filter(|_| current(folder))
                .map(|f| f.duplicates.len() + f.same_image.len())
        };
        let against_library = self
            .check
            .as_ref()
            .filter(|_| current(&self.library) && current(&self.staging))
            .map(|items| {
                items
                    .iter()
                    .filter(|i| {
                        matches!(
                            i.status,
                            StagingStatus::DuplicateOfLibrary { .. }
                                | StagingStatus::SameImageInLibrary { .. }
                        )
                    })
                    .count()
            });
        DuplicateStatus {
            library: groups(&self.library),
            staging: groups(&self.staging),
            against_library,
        }
    }

    /// Compare staging against the destination. Returns false if a folder is missing.
    pub fn run_check(&mut self) -> bool {
        let result = match (&self.staging, &self.library) {
            (Some(staging), Some(library)) => compare_staging(&staging.index, &library.index),
            _ => return false,
        };
        self.check = Some(result);
        true
    }

    pub fn check(&self) -> Option<&[StagingItem]> {
        self.check.as_deref()
    }

    fn refresh_check(&mut self) {
        if self.check.is_some() {
            self.run_check();
        }
    }

    /// Move one indexed file to the folder's `.trash` and drop it from the index.
    pub fn trash(&mut self, side: Side, rel: &str) -> io::Result<PathBuf> {
        let folder = self
            .folder_mut(side)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "folder not selected"))?;
        let dest = move_to_trash(&folder.root, rel)?;
        let mut index = std::mem::take(&mut folder.index);
        index.files.remove(rel);
        let saved = index.save(&folder.root);
        folder.set_index(index);
        self.refresh_check();
        saved.map(|_| dest)
    }

    /// Staging files the last check found in the destination, with their copies,
    /// for trashing them all at once (each pair is verified on disk first).
    pub fn bulk_trash_candidates(&self) -> Vec<Candidate> {
        self.check
            .iter()
            .flatten()
            .filter_map(|item| match &item.status {
                StagingStatus::DuplicateOfLibrary { library_paths } => Some(Candidate {
                    staging_rel: item.path.clone(),
                    library_rels: library_paths.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// Drop staging files that were trashed outside the model from its index, save
    /// it once, and refresh the duplicates and the check.
    pub fn forget_staging_files<'a>(
        &mut self,
        rels: impl IntoIterator<Item = &'a str>,
    ) -> io::Result<()> {
        let staging = self.staging.as_mut().ok_or_else(not_selected)?;
        let mut index = std::mem::take(&mut staging.index);
        for rel in rels {
            index.files.remove(rel);
        }
        let saved = index.save(&staging.root);
        staging.set_index(index);
        self.refresh_check();
        saved
    }
}

impl Model {
    /// Destination files with the same content as the staging file `staging_rel`
    /// (according to the saved indexes).
    pub fn library_copies_of(&self, staging_rel: &str) -> Vec<String> {
        let (Some(staging), Some(library)) = (&self.staging, &self.library) else {
            return Vec::new();
        };
        let Some(record) = staging.index.files.get(staging_rel) else {
            return Vec::new();
        };
        library
            .index
            .files
            .iter()
            .filter(|(_, r)| r.hash == record.hash)
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// Create the folder `name` inside the destination folder `parent_rel`.
    /// Returns its path relative to the destination root.
    pub fn create_folder(&mut self, parent_rel: &str, name: &str) -> io::Result<String> {
        let library = self.folder(Side::Library).ok_or_else(not_selected)?;
        create_folder(&library.root, parent_rel, name)
    }

    /// Number of the staging files `staging_rels` whose content is already in the
    /// destination (according to the saved indexes).
    pub fn count_with_library_copies(&self, staging_rels: &[String]) -> usize {
        let (Some(staging), Some(library)) = (&self.staging, &self.library) else {
            return 0;
        };
        let known: HashSet<&str> = library
            .index
            .files
            .values()
            .map(|r| r.hash.as_str())
            .collect();
        staging_rels
            .iter()
            .filter(|rel| {
                staging
                    .index
                    .files
                    .get(rel.as_str())
                    .is_some_and(|r| known.contains(r.hash.as_str()))
            })
            .count()
    }

    /// Move the staging file `source_rel` into the destination folder `dest_dir_rel`
    /// under the name `name`, then update both indexes. Never overwrites: if the name
    /// is taken the call fails with `AlreadyExists` and nothing changes. If only removing
    /// the original fails, the call succeeds with `source_kept` set.
    pub fn move_to_destination(
        &mut self,
        source_rel: &str,
        dest_dir_rel: &str,
        name: &str,
    ) -> io::Result<Moved> {
        let (Some(staging), Some(library)) = (&mut self.staging, &mut self.library) else {
            return Err(not_selected());
        };
        let moved = move_one(staging, library, source_rel, dest_dir_rel, name)?;
        let saved = save_and_refresh(staging, library);
        self.refresh_check();
        saved.map(|_| moved)
    }

    /// Move several staging files into one destination folder, keeping their names.
    /// A file whose name already exists there is skipped (never overwritten); a file
    /// that fails does not stop the others. The indexes are saved once at the end.
    /// Fails as a whole only if the destination folder is unusable.
    pub fn move_many(
        &mut self,
        source_rels: &[String],
        dest_dir_rel: &str,
    ) -> io::Result<BatchReport> {
        let (Some(staging), Some(library)) = (&mut self.staging, &mut self.library) else {
            return Err(not_selected());
        };
        let dest_dir = library.root.join(plain_relative(dest_dir_rel)?);
        if !dest_dir.is_dir() {
            return Err(dest_dir_not_found(&dest_dir));
        }

        let mut report = BatchReport::default();
        for rel in source_rels {
            let outcome = match move_one(staging, library, rel, dest_dir_rel, file_name(rel)) {
                Ok(Moved {
                    dest,
                    source_kept: None,
                }) => Outcome::Moved(dest),
                Ok(Moved {
                    dest,
                    source_kept: Some(reason),
                }) => Outcome::CopiedSourceKept { dest, reason },
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Outcome::Skipped(
                    tr("a file with this name already exists in the destination folder").into(),
                ),
                Err(e) => Outcome::Failed(e.to_string()),
            };
            report.outcomes.push((rel.clone(), outcome));
        }
        if report.moved() + report.kept() > 0 {
            report.save_error = save_and_refresh(staging, library)
                .err()
                .map(|e| e.to_string());
        }
        self.refresh_check();
        Ok(report)
    }
}

/// What happened to one file of a batch move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Moved(PathBuf),
    /// Copied to the destination, but the original could not be removed (locked,
    /// read-only, ...), so both copies exist now.
    CopiedSourceKept {
        dest: PathBuf,
        reason: String,
    },
    /// Left where it was on purpose; holds the reason.
    Skipped(String),
    /// Could not be moved; holds the error.
    Failed(String),
}

/// Result of `Model::move_many`: one outcome per requested file, in request order.
#[derive(Debug, Default)]
pub struct BatchReport {
    /// `(path relative to the staging root, outcome)`
    pub outcomes: Vec<(String, Outcome)>,
    /// Set if the moves worked but an index file could not be written.
    pub save_error: Option<String>,
}

impl BatchReport {
    fn count(&self, wanted: fn(&Outcome) -> bool) -> usize {
        self.outcomes.iter().filter(|(_, o)| wanted(o)).count()
    }

    pub fn moved(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Moved(_)))
    }

    pub fn skipped(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Skipped(_)))
    }

    pub fn failed(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Failed(_)))
    }

    /// Files that were copied but whose original could not be removed.
    pub fn kept(&self) -> usize {
        self.count(|o| matches!(o, Outcome::CopiedSourceKept { .. }))
    }
}

/// A file that now exists in the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub dest: PathBuf,
    /// Set when the original could not be removed (the reason): both copies exist.
    pub source_kept: Option<String>,
}

fn dest_dir_not_found(dest_dir: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("destination folder not found: {}", dest_dir.display()),
    )
}

/// Move one staging file into the destination and update both indexes in memory
/// (nothing is saved). Fails with `AlreadyExists` if the name is taken.
fn move_one(
    staging: &mut Folder,
    library: &mut Folder,
    source_rel: &str,
    dest_dir_rel: &str,
    name: &str,
) -> io::Result<Moved> {
    let src = staging.root.join(plain_relative(source_rel)?);
    let dest_dir = library.root.join(plain_relative(dest_dir_rel)?);
    plain_name(name)?;
    if !dest_dir.is_dir() {
        return Err(dest_dir_not_found(&dest_dir));
    }
    let dest = dest_dir.join(name);
    // The copy is verified against the source hash, so the index can reuse both hashes.
    let (hash, image) = match staging.index.files.get(source_rel) {
        Some(record) => (record.hash.clone(), record.image.clone()),
        None => {
            let hashes = hash_file_and_image(&src)?;
            (hashes.full, hashes.image)
        }
    };

    let outcome = move_file(&src, &dest)?;

    let meta = fs::metadata(&dest)?;
    // If the original could not be removed it is still in staging, so it stays indexed.
    let source_kept = match outcome {
        MoveOutcome::Moved => {
            staging.index.files.remove(source_rel);
            None
        }
        MoveOutcome::SourceKept(e) => Some(e.to_string()),
    };
    library.index.files.insert(
        join_dir(dest_dir_rel, name),
        IndexRecord {
            size: meta.len(),
            mtime: mtime_secs(&meta),
            hash,
            image,
        },
    );
    Ok(Moved { dest, source_kept })
}

/// Save both indexes and recompute their duplicate groups. Returns the first save error.
fn save_and_refresh(staging: &mut Folder, library: &mut Folder) -> io::Result<()> {
    let saved_staging = staging.index.save(&staging.root);
    let saved_library = library.index.save(&library.root);
    staging.refresh_groups();
    library.refresh_groups();
    saved_staging.and(saved_library)
}

/// One metadata field that differs between copies: its name and the value in each
/// copy (`None` where the copy lacks it).
pub type Difference = (String, Vec<Option<String>>);

/// The fields of `per_file` (one `(name, value)` list per copy) whose value is not
/// the same in every copy, sorted by name.
pub fn metadata_differences(per_file: &[Vec<(String, String)>]) -> Vec<Difference> {
    let maps: Vec<BTreeMap<&str, &str>> = per_file
        .iter()
        .map(|fields| {
            fields
                .iter()
                .map(|(n, v)| (n.as_str(), v.as_str()))
                .collect()
        })
        .collect();
    let names: BTreeSet<&str> = maps.iter().flat_map(|m| m.keys().copied()).collect();
    names
        .into_iter()
        .filter_map(|name| {
            let values: Vec<Option<&str>> = maps.iter().map(|m| m.get(name).copied()).collect();
            let same = values.windows(2).all(|w| w[0] == w[1]);
            (!same).then(|| {
                let values = values.into_iter().map(|v| v.map(str::to_string)).collect();
                (name.to_string(), values)
            })
        })
        .collect()
}

fn not_selected() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        "select the destination and staging folders first",
    )
}

/// Scan `root` and bring `index` up to date, then save it. Runs on a worker thread.
pub fn build_index(root: &Path, mut index: Index) -> io::Result<(Index, UpdateStats)> {
    let scanned = scan(root)?;
    let mut stats = index.update(root, &scanned.entries)?;
    // Folders that could not be read are reported together with unreadable files.
    stats.skipped.splice(0..0, scanned.warnings);
    index.save(root)?;
    Ok((index, stats))
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

    /// Model with both folders selected and indexed.
    fn indexed(library: &Path, staging: &Path) -> Model {
        let mut model = Model::default();
        for (side, root) in [(Side::Library, library), (Side::Staging, staging)] {
            model.set_root(side, root.to_path_buf()).unwrap();
            let (index, _) = build_index(root, Index::default()).unwrap();
            model.apply_index(side, index);
        }
        model
    }

    #[test]
    fn set_root_loads_saved_index_without_scanning() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.jpg", b"a");
        let (index, _) = build_index(dir.path(), Index::default()).unwrap();
        assert_eq!(index.files.len(), 1);

        let mut model = Model::default();
        write(dir.path(), "b.jpg", b"b"); // new file, not indexed yet
        model
            .set_root(Side::Library, dir.path().to_path_buf())
            .unwrap();

        assert_eq!(model.folder(Side::Library).unwrap().index.files.len(), 1);
        assert!(model.folder(Side::Staging).is_none());
    }

    #[test]
    fn build_index_finds_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "2024/a.jpg", b"same");
        write(dir.path(), "2025/b.jpg", b"same");
        write(dir.path(), "2025/c.jpg", b"other");

        let mut model = Model::default();
        model
            .set_root(Side::Library, dir.path().to_path_buf())
            .unwrap();
        let (index, stats) = build_index(dir.path(), Index::default()).unwrap();
        model.apply_index(Side::Library, index);

        assert_eq!(stats.hashed, 3);
        let groups = &model.folder(Side::Library).unwrap().duplicates;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].paths, ["2024/a.jpg", "2025/b.jpg"]);
        assert!(dir.path().join(".photo_index.json").exists());
    }

    #[test]
    fn check_needs_both_folders() {
        let dir = tempfile::tempdir().unwrap();
        let mut model = Model::default();
        model
            .set_root(Side::Library, dir.path().to_path_buf())
            .unwrap();

        assert!(!model.run_check());
        assert!(model.check().is_none());
    }

    #[test]
    fn check_classifies_staging_files() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "2024/beach.jpg", b"beach");
        write(stg.path(), "IMG_1.jpg", b"beach");
        write(stg.path(), "IMG_2.jpg", b"new");
        let mut model = indexed(lib.path(), stg.path());

        assert!(model.run_check());

        let items = model.check().unwrap();
        assert!(matches!(
            items.iter().find(|i| i.path == "IMG_1.jpg").unwrap().status,
            StagingStatus::DuplicateOfLibrary { .. }
        ));
        assert_eq!(
            items.iter().find(|i| i.path == "IMG_2.jpg").unwrap().status,
            StagingStatus::Unique
        );
    }

    #[test]
    fn changing_a_folder_clears_the_check() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        let mut model = indexed(lib.path(), stg.path());
        model.run_check();

        model
            .set_root(Side::Staging, stg.path().to_path_buf())
            .unwrap();

        assert!(model.check().is_none());
    }

    #[test]
    fn trash_moves_file_updates_index_and_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.jpg", b"same");
        write(dir.path(), "b.jpg", b"same");
        let stg = tempfile::tempdir().unwrap();
        let mut model = indexed(dir.path(), stg.path());

        let dest = model.trash(Side::Library, "b.jpg").unwrap();

        assert_eq!(dest, dir.path().join(".trash/b.jpg"));
        assert!(!dir.path().join("b.jpg").exists());
        let folder = model.folder(Side::Library).unwrap();
        assert!(folder.duplicates.is_empty());
        assert!(!folder.index.files.contains_key("b.jpg"));
        // The saved index no longer lists the trashed file.
        assert!(!Index::load(dir.path()).unwrap().files.contains_key("b.jpg"));
    }

    #[test]
    fn trash_refreshes_an_active_check() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "a.jpg", b"same");
        write(stg.path(), "x.jpg", b"same");
        let mut model = indexed(lib.path(), stg.path());
        model.run_check();

        model.trash(Side::Staging, "x.jpg").unwrap();

        assert!(model.check().unwrap().is_empty());
    }

    fn read(root: &Path, rel: &str) -> Vec<u8> {
        fs::read(root.join(rel)).unwrap()
    }

    #[test]
    fn move_to_destination_moves_file_and_updates_both_indexes() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "2025/trip/old.jpg", b"old");
        write(stg.path(), "IMG_1.jpg", b"new photo");
        let mut model = indexed(lib.path(), stg.path());
        model.run_check();

        let dest = model
            .move_to_destination("IMG_1.jpg", "2025/trip", "mountain.jpg")
            .unwrap()
            .dest;

        assert_eq!(dest, lib.path().join("2025/trip/mountain.jpg"));
        assert_eq!(read(lib.path(), "2025/trip/mountain.jpg"), b"new photo");
        assert!(!stg.path().join("IMG_1.jpg").exists());
        let library_index = &model.folder(Side::Library).unwrap().index;
        let record = &library_index.files["2025/trip/mountain.jpg"];
        assert_eq!(record.size, 9);
        assert!(!model
            .folder(Side::Staging)
            .unwrap()
            .index
            .files
            .contains_key("IMG_1.jpg"));
        // Both saved indexes match what is on disk.
        assert!(Index::load(lib.path())
            .unwrap()
            .files
            .contains_key("2025/trip/mountain.jpg"));
        assert!(!Index::load(stg.path())
            .unwrap()
            .files
            .contains_key("IMG_1.jpg"));
        assert!(model.check().unwrap().is_empty());
    }

    #[test]
    fn moved_file_index_entry_matches_a_fresh_scan() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(stg.path(), "a.jpg", b"pixels");
        let mut model = indexed(lib.path(), stg.path());

        model.move_to_destination("a.jpg", "", "a.jpg").unwrap();

        // Re-indexing from disk must not need to rehash the moved file.
        let index = model.folder(Side::Library).unwrap().index.clone();
        let (_, stats) = build_index(lib.path(), index).unwrap();
        assert_eq!((stats.hashed, stats.reused), (0, 1));
    }

    #[test]
    fn move_to_destination_refuses_name_collision() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "a.jpg", b"existing");
        write(stg.path(), "a.jpg", b"incoming");
        let mut model = indexed(lib.path(), stg.path());

        let err = model.move_to_destination("a.jpg", "", "a.jpg").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(read(lib.path(), "a.jpg"), b"existing");
        assert_eq!(read(stg.path(), "a.jpg"), b"incoming");
        assert!(model
            .folder(Side::Staging)
            .unwrap()
            .index
            .files
            .contains_key("a.jpg"));
        assert_eq!(model.folder(Side::Library).unwrap().index.files.len(), 1);
    }

    #[test]
    fn move_to_destination_rejects_bad_input() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(stg.path(), "a.jpg", b"x");
        let mut model = indexed(lib.path(), stg.path());

        assert!(model.move_to_destination("a.jpg", "..", "a.jpg").is_err());
        assert!(model.move_to_destination("../a.jpg", "", "a.jpg").is_err());
        assert!(model.move_to_destination("a.jpg", "", "../a.jpg").is_err());
        assert!(model
            .move_to_destination("a.jpg", "", ".hidden.jpg")
            .is_err());
        assert_eq!(
            model
                .move_to_destination("a.jpg", "missing", "a.jpg")
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert!(stg.path().join("a.jpg").exists());
        assert!(Model::default()
            .move_to_destination("a.jpg", "", "a.jpg")
            .is_err());
    }

    #[test]
    fn move_to_destination_works_for_unindexed_staging_file() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        let mut model = indexed(lib.path(), stg.path());
        write(stg.path(), "late.jpg", b"added after indexing");

        model
            .move_to_destination("late.jpg", "", "late.jpg")
            .unwrap();

        let index = &model.folder(Side::Library).unwrap().index;
        assert_eq!(
            index.files["late.jpg"].hash,
            photo_copy_core::hash::hash_file(&lib.path().join("late.jpg")).unwrap()
        );
    }

    #[test]
    fn create_folder_in_destination() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        let mut model = indexed(lib.path(), stg.path());

        assert_eq!(model.create_folder("", "2026").unwrap(), "2026");
        assert_eq!(model.create_folder("2026", "party").unwrap(), "2026/party");
        assert!(lib.path().join("2026/party").is_dir());
        assert!(model.create_folder("", "2026").is_err());
        assert!(Model::default().create_folder("", "x").is_err());
    }

    #[test]
    fn library_copies_of_finds_same_content_by_hash() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "2024/beach.jpg", b"beach");
        write(lib.path(), "2025/beach_again.jpg", b"beach");
        write(stg.path(), "IMG_1.jpg", b"beach");
        write(stg.path(), "IMG_2.jpg", b"unique");
        let model = indexed(lib.path(), stg.path());

        assert_eq!(
            model.library_copies_of("IMG_1.jpg"),
            ["2024/beach.jpg", "2025/beach_again.jpg"]
        );
        assert!(model.library_copies_of("IMG_2.jpg").is_empty());
        assert!(model.library_copies_of("unknown.jpg").is_empty());
    }

    fn outcome_of<'a>(report: &'a BatchReport, rel: &str) -> &'a Outcome {
        &report.outcomes.iter().find(|(r, _)| r == rel).unwrap().1
    }

    #[test]
    fn move_many_moves_all_files_and_saves_the_indexes_once() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "2025/trip/old.jpg", b"old");
        write(stg.path(), "a.jpg", b"aaa");
        write(stg.path(), "2024/b.jpg", b"bbb");
        write(stg.path(), "c.png", b"ccc");
        let mut model = indexed(lib.path(), stg.path());
        model.run_check();
        let sources: Vec<String> = ["a.jpg", "2024/b.jpg", "c.png"].map(String::from).to_vec();

        let report = model.move_many(&sources, "2025/trip").unwrap();

        assert_eq!(
            (report.moved(), report.skipped(), report.failed()),
            (3, 0, 0)
        );
        assert!(report.save_error.is_none());
        assert_eq!(read(lib.path(), "2025/trip/b.jpg"), b"bbb");
        assert!(!stg.path().join("2024/b.jpg").exists());
        let library_index = &model.folder(Side::Library).unwrap().index;
        for name in ["a.jpg", "b.jpg", "c.png"] {
            assert!(library_index
                .files
                .contains_key(&format!("2025/trip/{name}")));
        }
        assert!(model.folder(Side::Staging).unwrap().index.files.is_empty());
        assert!(Index::load(lib.path())
            .unwrap()
            .files
            .contains_key("2025/trip/a.jpg"));
        assert!(Index::load(stg.path()).unwrap().files.is_empty());
        assert!(model.check().unwrap().is_empty());
    }

    #[test]
    fn move_many_skips_name_collisions_without_touching_either_file() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "a.jpg", b"existing");
        write(stg.path(), "a.jpg", b"incoming");
        write(stg.path(), "b.jpg", b"fresh");
        let mut model = indexed(lib.path(), stg.path());
        let sources: Vec<String> = ["a.jpg", "b.jpg"].map(String::from).to_vec();

        let report = model.move_many(&sources, "").unwrap();

        assert_eq!(
            (report.moved(), report.skipped(), report.failed()),
            (1, 1, 0)
        );
        assert!(matches!(outcome_of(&report, "a.jpg"), Outcome::Skipped(_)));
        assert_eq!(read(lib.path(), "a.jpg"), b"existing");
        assert_eq!(read(stg.path(), "a.jpg"), b"incoming");
        let staging_index = &model.folder(Side::Staging).unwrap().index;
        assert!(staging_index.files.contains_key("a.jpg"));
        assert!(!staging_index.files.contains_key("b.jpg"));
    }

    #[test]
    fn move_many_skips_the_second_file_with_the_same_name() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(stg.path(), "2024/IMG_1.jpg", b"first");
        write(stg.path(), "2025/IMG_1.jpg", b"second");
        let mut model = indexed(lib.path(), stg.path());
        let sources: Vec<String> = ["2024/IMG_1.jpg", "2025/IMG_1.jpg"]
            .map(String::from)
            .to_vec();

        let report = model.move_many(&sources, "").unwrap();

        assert_eq!((report.moved(), report.skipped()), (1, 1));
        assert_eq!(read(lib.path(), "IMG_1.jpg"), b"first");
        assert_eq!(read(stg.path(), "2025/IMG_1.jpg"), b"second");
    }

    #[test]
    fn move_many_continues_after_a_failure() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(stg.path(), "ok.jpg", b"ok");
        let mut model = indexed(lib.path(), stg.path());
        let sources: Vec<String> = ["gone.jpg", "../escape.jpg", "ok.jpg"]
            .map(String::from)
            .to_vec();

        let report = model.move_many(&sources, "").unwrap();

        assert_eq!(
            (report.moved(), report.skipped(), report.failed()),
            (1, 0, 2)
        );
        assert!(matches!(
            outcome_of(&report, "gone.jpg"),
            Outcome::Failed(_)
        ));
        assert!(matches!(
            outcome_of(&report, "../escape.jpg"),
            Outcome::Failed(_)
        ));
        assert!(lib.path().join("ok.jpg").exists());
        // Report order follows the request order.
        assert_eq!(report.outcomes[0].0, "gone.jpg");
    }

    #[test]
    fn move_many_with_nothing_moved_leaves_the_saved_indexes_alone() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "a.jpg", b"existing");
        write(stg.path(), "a.jpg", b"incoming");
        let mut model = indexed(lib.path(), stg.path());
        let before = Index::load(stg.path()).unwrap();

        let report = model.move_many(&["a.jpg".to_string()], "").unwrap();

        assert_eq!(report.moved(), 0);
        assert_eq!(Index::load(stg.path()).unwrap(), before);
    }

    #[test]
    fn move_many_fails_as_a_whole_for_an_unusable_destination() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(stg.path(), "a.jpg", b"x");
        let mut model = indexed(lib.path(), stg.path());
        let sources = ["a.jpg".to_string()];

        assert_eq!(
            model.move_many(&sources, "missing").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(model.move_many(&sources, "..").is_err());
        assert!(Model::default().move_many(&sources, "").is_err());
        assert!(stg.path().join("a.jpg").exists());
    }

    #[test]
    fn counts_staging_files_already_in_the_destination() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "beach.jpg", b"beach");
        write(stg.path(), "IMG_1.jpg", b"beach");
        write(stg.path(), "IMG_2.jpg", b"beach");
        write(stg.path(), "IMG_3.jpg", b"new");
        let model = indexed(lib.path(), stg.path());
        let all: Vec<String> = ["IMG_1.jpg", "IMG_2.jpg", "IMG_3.jpg", "unknown.jpg"]
            .map(String::from)
            .to_vec();

        assert_eq!(model.count_with_library_copies(&all), 2);
        assert_eq!(Model::default().count_with_library_copies(&all), 0);
    }

    /// Make `dir` read-only so files in it can be read but not removed. Returns false
    /// when permissions are not enforced (running as root): nothing to test then.
    #[cfg(unix)]
    fn make_read_only(dir: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
        fs::write(dir.join("probe"), b"x").is_err()
    }

    #[cfg(unix)]
    fn make_writable(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn move_many_reports_a_file_copied_but_not_removed() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(stg.path(), "2024/a.jpg", b"aaa");
        write(stg.path(), "b.jpg", b"bbb");
        let mut model = indexed(lib.path(), stg.path());
        let enforced = make_read_only(&stg.path().join("2024"));
        let sources: Vec<String> = ["2024/a.jpg", "b.jpg"].map(String::from).to_vec();

        let report = enforced.then(|| model.move_many(&sources, "").unwrap());

        make_writable(&stg.path().join("2024"));
        let Some(report) = report else {
            return;
        };
        assert_eq!((report.moved(), report.kept(), report.failed()), (1, 1, 0));
        assert!(matches!(
            outcome_of(&report, "2024/a.jpg"),
            Outcome::CopiedSourceKept { .. }
        ));
        // Both copies exist and both are indexed where they are.
        assert_eq!(read(lib.path(), "a.jpg"), b"aaa");
        assert!(stg.path().join("2024/a.jpg").exists());
        assert!(model
            .folder(Side::Library)
            .unwrap()
            .index
            .files
            .contains_key("a.jpg"));
        let staging_index = &model.folder(Side::Staging).unwrap().index;
        assert!(staging_index.files.contains_key("2024/a.jpg"));
        assert!(!staging_index.files.contains_key("b.jpg"));
        assert!(report.save_error.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn move_to_destination_reports_when_the_original_is_kept() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(stg.path(), "2024/a.jpg", b"aaa");
        let mut model = indexed(lib.path(), stg.path());
        let enforced = make_read_only(&stg.path().join("2024"));

        let moved = enforced.then(|| {
            model
                .move_to_destination("2024/a.jpg", "", "a.jpg")
                .unwrap()
        });

        make_writable(&stg.path().join("2024"));
        let Some(moved) = moved else {
            return;
        };
        assert!(moved.source_kept.is_some());
        assert_eq!(moved.dest, lib.path().join("a.jpg"));
        assert!(Index::load(lib.path()).unwrap().files.contains_key("a.jpg"));
    }

    #[test]
    fn build_index_survives_an_unreadable_entry_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ok.jpg", b"ok");
        write(dir.path(), "locked/inside.jpg", b"x");
        #[cfg(unix)]
        let enforced = {
            use std::os::unix::fs::PermissionsExt;
            let locked = dir.path().join("locked");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            fs::read_dir(&locked).is_err()
        };

        let result = build_index(dir.path(), Index::default());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path().join("locked"), fs::Permissions::from_mode(0o755))
                .unwrap();
            if !enforced {
                return;
            }
            let (index, stats) = result.unwrap();
            assert!(index.files.contains_key("ok.jpg"));
            assert_eq!(stats.skipped.len(), 1);
            assert!(stats.skipped[0].contains("locked"), "{:?}", stats.skipped);
        }
        #[cfg(not(unix))]
        assert!(result.is_ok());
    }

    #[test]
    fn trash_without_folder_or_with_bad_path_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut model = Model::default();
        assert!(model.trash(Side::Library, "a.jpg").is_err());

        model
            .set_root(Side::Library, dir.path().to_path_buf())
            .unwrap();
        assert!(model.trash(Side::Library, "../escape.jpg").is_err());
    }

    #[test]
    fn duplicate_status_is_unknown_until_indexed_and_checked() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "a.jpg", b"a");
        write(stg.path(), "b.jpg", b"b");

        let mut model = Model::default();
        model
            .set_root(Side::Library, lib.path().to_path_buf())
            .unwrap();
        model
            .set_root(Side::Staging, stg.path().to_path_buf())
            .unwrap();
        assert!(!model.folders_ready());
        assert_eq!(model.duplicate_status(), DuplicateStatus::default());

        let mut model = indexed(lib.path(), stg.path());
        assert!(model.folders_ready());
        let status = model.duplicate_status();
        assert_eq!((status.library, status.staging), (Some(0), Some(0)));
        assert_eq!(status.remaining(), None); // check not run yet

        model.run_check();
        assert_eq!(model.duplicate_status().remaining(), Some(0));
    }

    #[test]
    fn duplicate_status_counts_what_is_left() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "2024/a.jpg", b"same");
        write(lib.path(), "2025/a.jpg", b"same");
        write(stg.path(), "IMG_1.jpg", b"same");
        write(stg.path(), "IMG_2.jpg", b"twin");
        write(stg.path(), "IMG_3.jpg", b"twin");
        let mut model = indexed(lib.path(), stg.path());
        model.run_check();

        let status = model.duplicate_status();
        assert_eq!(status.library, Some(1));
        assert_eq!(status.staging, Some(1));
        assert_eq!(status.against_library, Some(1));
        assert_eq!(status.remaining(), Some(3));

        model.trash(Side::Staging, "IMG_1.jpg").unwrap();
        assert_eq!(model.duplicate_status().against_library, Some(0));
        assert_eq!(model.duplicate_status().remaining(), Some(2));
    }

    #[test]
    fn bulk_trash_candidates_come_from_the_check() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "2024/a.jpg", b"same");
        write(stg.path(), "IMG_1.jpg", b"same");
        write(stg.path(), "IMG_2.jpg", b"new");
        let mut model = indexed(lib.path(), stg.path());
        assert!(model.bulk_trash_candidates().is_empty()); // check not run yet

        model.run_check();

        assert_eq!(
            model.bulk_trash_candidates(),
            [Candidate {
                staging_rel: "IMG_1.jpg".into(),
                library_rels: vec!["2024/a.jpg".into()],
            }]
        );
    }

    #[test]
    fn forget_staging_files_updates_index_and_check() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "a.jpg", b"same");
        write(stg.path(), "IMG_1.jpg", b"same");
        let mut model = indexed(lib.path(), stg.path());
        model.run_check();
        assert_eq!(model.duplicate_status().against_library, Some(1));

        fs::remove_file(stg.path().join("IMG_1.jpg")).unwrap();
        model.forget_staging_files(["IMG_1.jpg"]).unwrap();

        assert_eq!(model.duplicate_status().against_library, Some(0));
        assert!(Index::load(stg.path()).unwrap().files.is_empty());
    }

    #[test]
    fn same_photo_with_other_metadata_is_found_and_counted() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        let heic = fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../demo/staging/IMG_0007.heic"),
        )
        .unwrap();
        // Same image data, other bytes outside the image: a second top-level box.
        let mut other = heic.clone();
        other.extend_from_slice(b"\0\0\0\x10freeexport-i");
        write(lib.path(), "2021/IMG_1.heic", &heic);
        write(lib.path(), "extra/IMG_1 2.heic", &other);
        write(stg.path(), "IMG_1 (1).heic", &other);
        let mut model = indexed(lib.path(), stg.path());
        model.run_check();

        let library = model.folder(Side::Library).unwrap();
        assert!(library.duplicates.is_empty());
        assert_eq!(library.same_image.len(), 1);
        let status = model.duplicate_status();
        assert_eq!(status.library, Some(1));
        assert_eq!(status.staging, Some(0));
        // "extra/IMG_1 2.heic" is identical, so it is an exact match.
        assert_eq!(status.against_library, Some(1));
    }

    #[test]
    fn an_old_index_leaves_the_status_unknown_until_rescanned() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        let old = r#"{"version":1,"files":{}}"#;
        write(lib.path(), ".photo_index.json", old.as_bytes());
        build_index(stg.path(), Index::default()).unwrap();
        let mut model = Model::default();
        model
            .set_root(Side::Library, lib.path().to_path_buf())
            .unwrap();
        model
            .set_root(Side::Staging, stg.path().to_path_buf())
            .unwrap();
        model.run_check();

        assert!(model.folder(Side::Library).unwrap().needs_rescan());
        let status = model.duplicate_status();
        assert_eq!((status.library, status.staging), (None, Some(0)));
        assert_eq!(status.against_library, None);

        let (index, _) = build_index(
            lib.path(),
            model.folder(Side::Library).unwrap().index.clone(),
        )
        .unwrap();
        model.apply_index(Side::Library, index);
        assert!(!model.folder(Side::Library).unwrap().needs_rescan());
        assert_eq!(model.duplicate_status().remaining(), Some(0));
    }

    #[test]
    fn metadata_differences_lists_only_fields_that_differ() {
        let field = |n: &str, v: &str| (n.to_string(), v.to_string());
        let a = vec![
            field("DateTimeOriginal", "2021-06-14"),
            field("Make", "Apple"),
            field("GPSLatitude", "52"),
        ];
        let b = vec![
            field("DateTimeOriginal", "2021-06-14"),
            field("Make", "Apple"),
            field("ImageUniqueID", "489C"),
        ];

        let diff = metadata_differences(&[a, b]);

        assert_eq!(
            diff,
            [
                (
                    "GPSLatitude".to_string(),
                    vec![Some("52".to_string()), None]
                ),
                (
                    "ImageUniqueID".to_string(),
                    vec![None, Some("489C".to_string())]
                ),
            ]
        );
    }

    #[test]
    fn rescan_finds_copied_files_and_refreshes_the_check() {
        let lib = tempfile::tempdir().unwrap();
        let stg = tempfile::tempdir().unwrap();
        write(lib.path(), "2021/a.jpg", b"old photo");
        write(stg.path(), "IMG_1.jpg", b"new photo");
        let mut model = indexed(lib.path(), stg.path());
        model.run_check();
        assert_eq!(model.duplicate_status().against_library, Some(0));

        // Copied by hand into a destination subfolder, then Rescan.
        write(lib.path(), "2021/trip/IMG_1.jpg", b"new photo");
        let index = model.folder(Side::Library).unwrap().index.clone();
        let (index, stats) = build_index(lib.path(), index).unwrap();
        model.apply_index(Side::Library, index);

        assert_eq!((stats.hashed, stats.reused), (1, 1));
        assert_eq!(model.folder(Side::Library).unwrap().index.files.len(), 2);
        assert_eq!(model.duplicate_status().against_library, Some(1));
    }

    #[test]
    fn saved_index_counts_as_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let mut model = Model::default();
        model
            .set_root(Side::Library, dir.path().to_path_buf())
            .unwrap();
        assert!(!model.folder(Side::Library).unwrap().is_indexed());

        build_index(dir.path(), Index::default()).unwrap();
        model
            .set_root(Side::Library, dir.path().to_path_buf())
            .unwrap();
        assert!(model.folder(Side::Library).unwrap().is_indexed());
    }
}
