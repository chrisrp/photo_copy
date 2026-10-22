//! Rows and sorting for the Browse list view. UI independent.

use photo_copy_core::browse::Listing;
use photo_copy_core::meta::{kind_label, natural_cmp, PhotoDate, FOLDER_KIND};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    DateTaken,
    Size,
    Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    pub column: Column,
    pub ascending: bool,
}

impl Default for Sort {
    fn default() -> Self {
        Sort {
            column: Column::Name,
            ascending: true,
        }
    }
}

impl Sort {
    /// The sort after the user clicked `column`'s header: a new column starts
    /// ascending, clicking the current column again reverses it.
    pub fn click(self, column: Column) -> Sort {
        if self.column == column {
            Sort {
                column,
                ascending: !self.ascending,
            }
        } else {
            Sort {
                column,
                ascending: true,
            }
        }
    }
}

/// Which entry of the `Listing` a row stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Index into `Listing::dirs`.
    Dir(usize),
    /// Index into `Listing::files`.
    File(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRow {
    pub target: Target,
    pub name: String,
    /// `None` for folders.
    pub date: Option<PhotoDate>,
    /// `None` for folders.
    pub size: Option<u64>,
    pub kind: &'static str,
}

/// One row per folder and file. `dates` must be in the order of `listing.files`.
pub fn build_rows(listing: &Listing, dates: &[PhotoDate]) -> Vec<ListRow> {
    let dirs = listing.dirs.iter().enumerate().map(|(i, name)| ListRow {
        target: Target::Dir(i),
        name: name.clone(),
        date: None,
        size: None,
        kind: FOLDER_KIND,
    });
    let files = listing.files.iter().enumerate().map(|(i, file)| ListRow {
        target: Target::File(i),
        name: file
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        date: dates.get(i).copied(),
        size: Some(file.size),
        kind: kind_label(&file.path),
    });
    dirs.chain(files).collect()
}

/// Sort rows by the given column. Folders always stay on top; ties are broken by
/// name (natural order, ascending).
pub fn sort_rows(rows: &mut [ListRow], sort: Sort) {
    rows.sort_by(|a, b| {
        let is_dir = |r: &ListRow| matches!(r.target, Target::Dir(_));
        is_dir(b).cmp(&is_dir(a)).then_with(|| {
            let by_name = natural_cmp(&a.name, &b.name);
            let primary = match sort.column {
                Column::Name => by_name,
                Column::DateTaken => a.date.map(|d| d.when).cmp(&b.date.map(|d| d.when)),
                Column::Size => a.size.cmp(&b.size),
                Column::Kind => a.kind.cmp(b.kind),
            };
            let primary = if sort.ascending {
                primary
            } else {
                primary.reverse()
            };
            primary.then(by_name)
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use photo_copy_core::scan::FileEntry;
    use std::path::PathBuf;

    fn file(name: &str, size: u64) -> FileEntry {
        FileEntry {
            path: PathBuf::from("/lib").join(name),
            size,
            mtime: 0,
        }
    }

    fn day(d: u32) -> PhotoDate {
        PhotoDate {
            when: NaiveDate::from_ymd_opt(2024, 5, d)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .unwrap(),
            from_exif: true,
        }
    }

    /// Folders `Zoo`, `alpha`; files with sizes/dates chosen so each column orders differently.
    fn sample() -> Vec<ListRow> {
        let listing = Listing {
            dirs: vec!["Zoo".into(), "alpha".into()],
            files: vec![
                file("IMG_10.jpg", 300),
                file("IMG_2.heic", 100),
                file("clip.mov", 200),
            ],
        };
        build_rows(&listing, &[day(3), day(9), day(1)])
    }

    fn names(rows: &[ListRow]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    fn sorted(column: Column, ascending: bool) -> Vec<ListRow> {
        let mut rows = sample();
        sort_rows(&mut rows, Sort { column, ascending });
        rows
    }

    #[test]
    fn builds_a_row_per_folder_and_file_with_details() {
        let rows = sample();

        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].target, Target::Dir(0));
        assert_eq!(
            (rows[0].date, rows[0].size, rows[0].kind),
            (None, None, "Folder")
        );
        assert_eq!(rows[2].target, Target::File(0));
        assert_eq!(rows[2].size, Some(300));
        assert_eq!(rows[2].kind, "JPEG image");
        assert_eq!(rows[4].kind, "QuickTime movie");
        assert_eq!(rows[3].date, Some(day(9)));
    }

    #[test]
    fn sorts_by_name_naturally_with_folders_on_top() {
        assert_eq!(
            names(&sorted(Column::Name, true)),
            ["alpha", "Zoo", "clip.mov", "IMG_2.heic", "IMG_10.jpg"]
        );
        assert_eq!(
            names(&sorted(Column::Name, false)),
            ["Zoo", "alpha", "IMG_10.jpg", "IMG_2.heic", "clip.mov"]
        );
    }

    #[test]
    fn sorts_by_date_taken() {
        assert_eq!(
            names(&sorted(Column::DateTaken, true)),
            ["alpha", "Zoo", "clip.mov", "IMG_10.jpg", "IMG_2.heic"]
        );
        assert_eq!(
            names(&sorted(Column::DateTaken, false)),
            ["alpha", "Zoo", "IMG_2.heic", "IMG_10.jpg", "clip.mov"]
        );
    }

    #[test]
    fn sorts_by_size() {
        assert_eq!(
            names(&sorted(Column::Size, true)),
            ["alpha", "Zoo", "IMG_2.heic", "clip.mov", "IMG_10.jpg"]
        );
        assert_eq!(
            names(&sorted(Column::Size, false)),
            ["alpha", "Zoo", "IMG_10.jpg", "clip.mov", "IMG_2.heic"]
        );
    }

    #[test]
    fn sorts_by_kind_then_name() {
        // "HEIC image" < "JPEG image" < "QuickTime movie"
        assert_eq!(
            names(&sorted(Column::Kind, true)),
            ["alpha", "Zoo", "IMG_2.heic", "IMG_10.jpg", "clip.mov"]
        );
    }

    #[test]
    fn equal_values_fall_back_to_name_order() {
        let listing = Listing {
            dirs: vec![],
            files: vec![file("b.jpg", 5), file("a.jpg", 5), file("c.jpg", 1)],
        };
        let mut rows = build_rows(&listing, &[day(1), day(1), day(1)]);

        sort_rows(
            &mut rows,
            Sort {
                column: Column::Size,
                ascending: false,
            },
        );

        assert_eq!(names(&rows), ["a.jpg", "b.jpg", "c.jpg"]);
    }

    #[test]
    fn clicking_headers_selects_then_reverses() {
        let sort = Sort::default();
        assert_eq!(
            sort,
            Sort {
                column: Column::Name,
                ascending: true
            }
        );

        let sort = sort.click(Column::Name);
        assert_eq!(
            sort,
            Sort {
                column: Column::Name,
                ascending: false
            }
        );

        let sort = sort.click(Column::Size);
        assert_eq!(
            sort,
            Sort {
                column: Column::Size,
                ascending: true
            }
        );

        assert!(!sort.click(Column::Size).ascending);
        assert!(sort.click(Column::Size).click(Column::Kind).ascending);
    }

    #[test]
    fn missing_dates_do_not_break_row_building() {
        let listing = Listing {
            dirs: vec![],
            files: vec![file("a.jpg", 1)],
        };

        assert_eq!(build_rows(&listing, &[])[0].date, None);
    }
}
