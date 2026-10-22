use std::collections::HashMap;

use crate::index::Index;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagingStatus {
    /// Content not found in the library nor elsewhere in staging.
    Unique,
    /// Same content already exists in the library (paths relative to the library root).
    DuplicateOfLibrary { library_paths: Vec<String> },
    /// No identical file in the library, but the same photo with other metadata
    /// (same image hash; paths relative to the library root).
    SameImageInLibrary { library_paths: Vec<String> },
    /// Not in the library, but other staging files have the same content
    /// (paths relative to the staging root, excluding this file).
    DuplicateInStaging { other_paths: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagingItem {
    /// Path relative to the staging root.
    pub path: String,
    pub status: StagingStatus,
}

/// Classify every staging file. An identical library file takes precedence over the
/// same photo in the library, which takes precedence over a staging-internal
/// duplicate. Result order follows the staging index (sorted by path).
pub fn compare_staging(staging: &Index, library: &Index) -> Vec<StagingItem> {
    let mut in_library: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut image_in_library: HashMap<&str, Vec<&str>> = HashMap::new();
    for (path, record) in &library.files {
        in_library
            .entry(record.hash.as_str())
            .or_default()
            .push(path);
        if let Some(image) = &record.image {
            image_in_library
                .entry(image.as_str())
                .or_default()
                .push(path);
        }
    }
    let mut in_staging: HashMap<&str, Vec<&str>> = HashMap::new();
    for (path, record) in &staging.files {
        in_staging
            .entry(record.hash.as_str())
            .or_default()
            .push(path);
    }

    staging
        .files
        .iter()
        .map(|(path, record)| {
            let hash = record.hash.as_str();
            let same_image = record
                .image
                .as_deref()
                .and_then(|image| image_in_library.get(image));
            let status = if let Some(paths) = in_library.get(hash) {
                StagingStatus::DuplicateOfLibrary {
                    library_paths: paths.iter().map(|p| p.to_string()).collect(),
                }
            } else if let Some(paths) = same_image {
                StagingStatus::SameImageInLibrary {
                    library_paths: paths.iter().map(|p| p.to_string()).collect(),
                }
            } else {
                let others: Vec<String> = in_staging[hash]
                    .iter()
                    .filter(|p| **p != path.as_str())
                    .map(|p| p.to_string())
                    .collect();
                if others.is_empty() {
                    StagingStatus::Unique
                } else {
                    StagingStatus::DuplicateInStaging {
                        other_paths: others,
                    }
                }
            };
            StagingItem {
                path: path.clone(),
                status,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexRecord;

    fn index_of(files: &[(&str, &str)]) -> Index {
        images_of(
            &files
                .iter()
                .map(|(p, h)| (*p, *h, None))
                .collect::<Vec<_>>(),
        )
    }

    /// `(path, full hash, image hash)`
    fn images_of(files: &[(&str, &str, Option<&str>)]) -> Index {
        let mut index = Index::default();
        for (path, hash, image) in files {
            index.files.insert(
                path.to_string(),
                IndexRecord {
                    size: 10,
                    mtime: 0,
                    hash: hash.to_string(),
                    image: image.map(str::to_string),
                },
            );
        }
        index
    }

    #[test]
    fn same_image_in_library_comes_after_an_identical_file() {
        let library = images_of(&[
            ("2021/IMG_1.jpeg", "f1", Some("i1")),
            ("2021/IMG_2.jpeg", "f2", Some("i2")),
        ]);
        let staging = images_of(&[
            ("IMG_1.jpeg", "f1", Some("i1")),   // identical file
            ("IMG_2 2.jpeg", "f9", Some("i2")), // same photo, other metadata
            ("IMG_2 3.jpeg", "f9", Some("i2")), // ... twice in staging
            ("new.jpeg", "f3", Some("i3")),
        ]);

        let items = compare_staging(&staging, &library);

        assert!(matches!(
            status_of(&items, "IMG_1.jpeg"),
            StagingStatus::DuplicateOfLibrary { .. }
        ));
        for path in ["IMG_2 2.jpeg", "IMG_2 3.jpeg"] {
            assert_eq!(
                status_of(&items, path),
                &StagingStatus::SameImageInLibrary {
                    library_paths: vec!["2021/IMG_2.jpeg".into()]
                }
            );
        }
        assert_eq!(status_of(&items, "new.jpeg"), &StagingStatus::Unique);
    }

    fn status_of<'a>(items: &'a [StagingItem], path: &str) -> &'a StagingStatus {
        &items.iter().find(|i| i.path == path).unwrap().status
    }

    #[test]
    fn classifies_unique_library_duplicate_and_staging_duplicate() {
        let library = index_of(&[("2024/party.jpg", "h1"), ("2025/trip.jpg", "h1")]);
        let staging = index_of(&[
            ("IMG_1.jpg", "h1"),
            ("IMG_2.jpg", "h2"),
            ("IMG_3.jpg", "h3"),
            ("IMG_4.jpg", "h3"),
        ]);

        let items = compare_staging(&staging, &library);

        assert_eq!(
            status_of(&items, "IMG_1.jpg"),
            &StagingStatus::DuplicateOfLibrary {
                library_paths: vec!["2024/party.jpg".into(), "2025/trip.jpg".into()]
            }
        );
        assert_eq!(status_of(&items, "IMG_2.jpg"), &StagingStatus::Unique);
        assert_eq!(
            status_of(&items, "IMG_3.jpg"),
            &StagingStatus::DuplicateInStaging {
                other_paths: vec!["IMG_4.jpg".into()]
            }
        );
        assert_eq!(
            status_of(&items, "IMG_4.jpg"),
            &StagingStatus::DuplicateInStaging {
                other_paths: vec!["IMG_3.jpg".into()]
            }
        );
    }

    #[test]
    fn library_match_wins_over_staging_duplicate() {
        let library = index_of(&[("a.jpg", "h")]);
        let staging = index_of(&[("x.jpg", "h"), ("y.jpg", "h")]);

        let items = compare_staging(&staging, &library);

        assert!(items
            .iter()
            .all(|i| matches!(i.status, StagingStatus::DuplicateOfLibrary { .. })));
    }

    #[test]
    fn empty_library_makes_everything_unique_or_staging_duplicate() {
        let staging = index_of(&[("x.jpg", "h1")]);

        let items = compare_staging(&staging, &Index::default());

        assert_eq!(status_of(&items, "x.jpg"), &StagingStatus::Unique);
    }
}
