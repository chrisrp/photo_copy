use std::collections::{BTreeMap, BTreeSet};

use crate::index::Index;

/// Files with identical content. `paths` are relative to the indexed root, sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub hash: String,
    pub size: u64,
    pub paths: Vec<String>,
}

/// Group indexed files by content hash and return only groups with 2+ files.
pub fn find_duplicates(index: &Index) -> Vec<DuplicateGroup> {
    let mut by_hash: BTreeMap<&str, DuplicateGroup> = BTreeMap::new();
    for (path, record) in &index.files {
        by_hash
            .entry(record.hash.as_str())
            .or_insert_with(|| DuplicateGroup {
                hash: record.hash.clone(),
                size: record.size,
                paths: Vec::new(),
            })
            .paths
            .push(path.clone());
    }
    by_hash
        .into_values()
        .filter(|g| g.paths.len() > 1)
        .collect()
}

/// Files showing the same photo (same image data and orientation) while their
/// metadata differs. `paths` are relative to the indexed root, sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SameImageGroup {
    pub image_hash: String,
    pub paths: Vec<String>,
}

/// Group indexed files by image hash and return the groups with 2+ files that are
/// not all identical files: those are listed by [`find_duplicates`] already. A group
/// may contain exact copies of each other next to a copy with other metadata.
pub fn find_same_image(index: &Index) -> Vec<SameImageGroup> {
    let mut by_image: BTreeMap<&str, Vec<(&String, &str)>> = BTreeMap::new();
    for (path, record) in &index.files {
        if let Some(image) = &record.image {
            by_image
                .entry(image.as_str())
                .or_default()
                .push((path, record.hash.as_str()));
        }
    }
    by_image
        .into_iter()
        .filter(|(_, files)| {
            let distinct: BTreeSet<&str> = files.iter().map(|(_, hash)| *hash).collect();
            distinct.len() > 1
        })
        .map(|(image, files)| SameImageGroup {
            image_hash: image.to_string(),
            paths: files.into_iter().map(|(path, _)| path.clone()).collect(),
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
    fn same_image_groups_need_different_files() {
        let index = images_of(&[
            ("2021/IMG_1.jpeg", "f1", Some("i1")),
            ("extra/IMG_1 2.jpeg", "f2", Some("i1")),
            ("a.jpeg", "f3", Some("i2")), // exact duplicates only: not listed here
            ("b.jpeg", "f3", Some("i2")),
            ("clip.mov", "f4", None),
            ("clip 2.mov", "f5", None),
            ("unique.jpeg", "f6", Some("i3")),
        ]);

        let groups = find_same_image(&index);

        assert_eq!(
            groups,
            [SameImageGroup {
                image_hash: "i1".into(),
                paths: vec!["2021/IMG_1.jpeg".into(), "extra/IMG_1 2.jpeg".into()],
            }]
        );
    }

    #[test]
    fn same_image_group_keeps_exact_copies_next_to_a_different_one() {
        let index = images_of(&[
            ("a.jpeg", "f1", Some("i")),
            ("b.jpeg", "f1", Some("i")),
            ("c.jpeg", "f2", Some("i")),
        ]);

        assert_eq!(
            find_same_image(&index)[0].paths,
            ["a.jpeg", "b.jpeg", "c.jpeg"]
        );
    }

    #[test]
    fn groups_identical_hashes_across_folders_and_names() {
        let index = index_of(&[
            ("2024/IMG_1.jpg", "h1"),
            ("2025/party.jpg", "h1"),
            ("2025/other.jpg", "h2"),
        ]);

        let groups = find_duplicates(&index);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].hash, "h1");
        assert_eq!(groups[0].paths, ["2024/IMG_1.jpg", "2025/party.jpg"]);
    }

    #[test]
    fn unique_files_produce_no_groups() {
        let index = index_of(&[("a.jpg", "h1"), ("b.jpg", "h2")]);
        assert!(find_duplicates(&index).is_empty());
    }

    #[test]
    fn three_copies_form_one_group() {
        let index = index_of(&[("a.jpg", "h"), ("b.jpg", "h"), ("c.jpg", "h")]);

        let groups = find_duplicates(&index);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].paths.len(), 3);
    }
}
