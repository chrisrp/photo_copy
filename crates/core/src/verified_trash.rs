//! Trash staging files that are already in the library, in one batch. The saved
//! indexes may be out of date (a library file deleted or edited since the last
//! scan), so every pair is checked on disk right before the staging file is
//! trashed: both files are re-hashed and must be identical. Every decision is
//! written to a CSV log inside the staging `.trash` folder as it happens.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::hash::hash_file;
use crate::ops::{move_to_trash, plain_relative, suggest_free_name, TRASH_DIR};

/// A staging file and the library files the index says have the same content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub staging_rel: String,
    pub library_rels: Vec<String>,
}

/// What happened to one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// An identical library copy was confirmed and the staging file was trashed.
    Trashed {
        /// The library file (relative to the library root) that was confirmed identical.
        matched: String,
        trashed_to: PathBuf,
    },
    /// The staging file is gone (nothing to do).
    StagingMissing,
    /// None of the library copies exists any more. The staging file was kept.
    CopyMissing,
    /// Library copies exist but none is identical any more. The staging file was kept.
    CopyChanged,
    /// Could not read or trash a file; holds the error. The staging file was kept.
    Failed(String),
}

impl Outcome {
    fn log_status(&self) -> &'static str {
        match self {
            Outcome::Trashed { .. } => "trashed",
            Outcome::StagingMissing => "skipped: staging file missing",
            Outcome::CopyMissing => "skipped: destination copy missing",
            Outcome::CopyChanged => "skipped: destination copy changed",
            Outcome::Failed(_) => "failed",
        }
    }
}

#[derive(Debug)]
pub struct Report {
    /// `(staging path, outcome)` in candidate order. Shorter than the candidates
    /// when the batch was cancelled or stopped by a log error.
    pub outcomes: Vec<(String, Outcome)>,
    pub log_path: PathBuf,
    pub cancelled: bool,
    /// Set if the log could not be written; the batch stops at that point.
    pub log_error: Option<String>,
}

impl Report {
    fn count(&self, wanted: fn(&Outcome) -> bool) -> usize {
        self.outcomes.iter().filter(|(_, o)| wanted(o)).count()
    }

    pub fn trashed(&self) -> impl Iterator<Item = &str> {
        self.outcomes
            .iter()
            .filter(|(_, o)| matches!(o, Outcome::Trashed { .. }))
            .map(|(rel, _)| rel.as_str())
    }

    pub fn trashed_count(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Trashed { .. }))
    }

    /// Kept on purpose because the library copy could not be confirmed.
    pub fn skipped(&self) -> usize {
        self.count(|o| {
            matches!(
                o,
                Outcome::StagingMissing | Outcome::CopyMissing | Outcome::CopyChanged
            )
        })
    }

    pub fn failed(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Failed(_)))
    }
}

/// Check every candidate on disk and trash the staging file only when one of its
/// library copies is confirmed identical. `done` counts processed candidates (for
/// progress); setting `cancel` stops after the current file. Fails as a whole, with
/// nothing trashed, only if the log cannot be created.
pub fn trash_verified(
    staging_root: &Path,
    library_root: &Path,
    candidates: &[Candidate],
    done: &AtomicUsize,
    cancel: &AtomicBool,
) -> io::Result<Report> {
    let (log_path, mut log) = create_log(staging_root)?;
    let mut report = Report {
        outcomes: Vec::new(),
        log_path,
        cancelled: false,
        log_error: None,
    };
    for candidate in candidates {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            break;
        }
        let outcome = process(staging_root, library_root, candidate);
        let logged = write_row(&mut log, staging_root, candidate, &outcome);
        report
            .outcomes
            .push((candidate.staging_rel.clone(), outcome));
        done.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = logged {
            report.log_error = Some(e.to_string());
            break;
        }
    }
    Ok(report)
}

fn process(staging_root: &Path, library_root: &Path, candidate: &Candidate) -> Outcome {
    match find_identical_copy(staging_root, library_root, candidate) {
        Ok(matched) => match move_to_trash(staging_root, &candidate.staging_rel) {
            Ok(trashed_to) => Outcome::Trashed {
                matched,
                trashed_to,
            },
            Err(e) => Outcome::Failed(e.to_string()),
        },
        Err(outcome) => outcome,
    }
}

/// The first library copy that is a different file with the same content as the
/// staging file, read from disk. On failure, the outcome explaining why.
fn find_identical_copy(
    staging_root: &Path,
    library_root: &Path,
    candidate: &Candidate,
) -> Result<String, Outcome> {
    let failed = |e: io::Error| Outcome::Failed(e.to_string());
    let src = staging_root.join(plain_relative(&candidate.staging_rel).map_err(failed)?);
    let src_meta = match fs::metadata(&src) {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Err(Outcome::StagingMissing),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Outcome::StagingMissing),
        Err(e) => return Err(failed(e)),
    };
    let src_real = fs::canonicalize(&src).map_err(failed)?;
    let mut src_hash = None;
    let mut any_exists = false;
    for rel in &candidate.library_rels {
        let path = library_root.join(plain_relative(rel).map_err(failed)?);
        let meta = match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(failed(e)),
        };
        // The very same file seen through both folders (overlapping roots or a
        // symlink) is not a second copy.
        if fs::canonicalize(&path).map_err(failed)? == src_real {
            continue;
        }
        any_exists = true;
        if meta.len() != src_meta.len() {
            continue;
        }
        let src_hash = match &src_hash {
            Some(hash) => hash,
            None => src_hash.insert(hash_file(&src).map_err(failed)?),
        };
        if hash_file(&path).map_err(failed)? == *src_hash {
            return Ok(rel.clone());
        }
    }
    Err(if any_exists {
        Outcome::CopyChanged
    } else {
        Outcome::CopyMissing
    })
}

fn create_log(staging_root: &Path) -> io::Result<(PathBuf, BufWriter<File>)> {
    let dir = staging_root.join(TRASH_DIR);
    fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S");
    let path = suggest_free_name(&dir.join(format!("trash-log-{stamp}.csv")));
    let mut log = BufWriter::new(File::create_new(&path)?);
    writeln!(
        log,
        "status,staging_path,destination_match,trashed_to,detail"
    )?;
    log.flush()?;
    Ok((path, log))
}

/// Append one row and flush it, so the log is complete up to the last file even
/// if the app stops in the middle of the batch.
fn write_row(
    log: &mut BufWriter<File>,
    staging_root: &Path,
    candidate: &Candidate,
    outcome: &Outcome,
) -> io::Result<()> {
    let (matched, trashed_to, detail) = match outcome {
        Outcome::Trashed {
            matched,
            trashed_to,
        } => {
            let rel = trashed_to.strip_prefix(staging_root).unwrap_or(trashed_to);
            (matched.clone(), rel.display().to_string(), String::new())
        }
        Outcome::Failed(error) => (String::new(), String::new(), error.clone()),
        _ => (
            String::new(),
            String::new(),
            candidate.library_rels.join(" | "),
        ),
    };
    let fields = [
        outcome.log_status(),
        &candidate.staging_rel,
        &matched,
        &trashed_to,
        &detail,
    ];
    let row: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
    writeln!(log, "{}", row.join(","))?;
    log.flush()
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn candidate(staging_rel: &str, library_rels: &[&str]) -> Candidate {
        Candidate {
            staging_rel: staging_rel.into(),
            library_rels: library_rels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn run(staging: &Path, library: &Path, candidates: &[Candidate]) -> Report {
        trash_verified(
            staging,
            library,
            candidates,
            &AtomicUsize::new(0),
            &AtomicBool::new(false),
        )
        .unwrap()
    }

    #[test]
    fn trashes_a_confirmed_copy_and_logs_it() {
        let stg = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        write(stg.path(), "phone/IMG_1.jpg", b"beach");
        write(lib.path(), "2024/beach.jpg", b"beach");

        let report = run(
            stg.path(),
            lib.path(),
            &[candidate("phone/IMG_1.jpg", &["2024/beach.jpg"])],
        );

        let trashed_to = stg.path().join(".trash/phone/IMG_1.jpg");
        assert_eq!(
            report.outcomes,
            [(
                "phone/IMG_1.jpg".to_string(),
                Outcome::Trashed {
                    matched: "2024/beach.jpg".into(),
                    trashed_to: trashed_to.clone()
                }
            )]
        );
        assert!(!stg.path().join("phone/IMG_1.jpg").exists());
        assert!(trashed_to.exists());
        assert!(lib.path().join("2024/beach.jpg").exists());

        assert!(report.log_path.starts_with(stg.path().join(".trash")));
        let log = fs::read_to_string(&report.log_path).unwrap();
        let rows: Vec<&str> = log.lines().collect();
        assert_eq!(
            rows[0],
            "status,staging_path,destination_match,trashed_to,detail"
        );
        let expected = Path::new(".trash/phone/IMG_1.jpg").display().to_string();
        assert_eq!(
            rows[1],
            format!("trashed,phone/IMG_1.jpg,2024/beach.jpg,{expected},")
        );
    }

    #[test]
    fn keeps_staging_file_when_destination_copy_was_deleted() {
        let stg = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        write(stg.path(), "IMG_1.jpg", b"beach");

        let report = run(
            stg.path(),
            lib.path(),
            &[candidate("IMG_1.jpg", &["2024/beach.jpg"])],
        );

        assert_eq!(report.outcomes[0].1, Outcome::CopyMissing);
        assert!(stg.path().join("IMG_1.jpg").exists());
        let log = fs::read_to_string(&report.log_path).unwrap();
        assert!(log.contains("skipped: destination copy missing,IMG_1.jpg,,,2024/beach.jpg"));
    }

    #[test]
    fn keeps_staging_file_when_destination_copy_was_edited() {
        let stg = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        write(stg.path(), "IMG_1.jpg", b"beach");
        write(lib.path(), "same_size.jpg", b"BEACH"); // same size, other content
        write(lib.path(), "other_size.jpg", b"beach, cropped");

        let report = run(
            stg.path(),
            lib.path(),
            &[candidate("IMG_1.jpg", &["same_size.jpg", "other_size.jpg"])],
        );

        assert_eq!(report.outcomes[0].1, Outcome::CopyChanged);
        assert!(stg.path().join("IMG_1.jpg").exists());
    }

    #[test]
    fn keeps_staging_file_that_was_edited_since_indexing() {
        let stg = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        write(stg.path(), "IMG_1.jpg", b"edited");
        write(lib.path(), "a.jpg", b"beach!");

        let report = run(
            stg.path(),
            lib.path(),
            &[candidate("IMG_1.jpg", &["a.jpg"])],
        );

        assert_eq!(report.outcomes[0].1, Outcome::CopyChanged);
        assert!(stg.path().join("IMG_1.jpg").exists());
    }

    #[test]
    fn any_identical_copy_is_enough() {
        let stg = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        write(stg.path(), "IMG_1.jpg", b"beach");
        write(lib.path(), "b.jpg", b"beach");

        let report = run(
            stg.path(),
            lib.path(),
            &[candidate("IMG_1.jpg", &["gone.jpg", "b.jpg"])],
        );

        assert!(
            matches!(&report.outcomes[0].1, Outcome::Trashed { matched, .. } if matched == "b.jpg")
        );
    }

    #[test]
    fn same_file_seen_from_both_roots_is_not_a_copy() {
        // Staging inside the library: the library index lists the staging file itself.
        let lib = tempfile::tempdir().unwrap();
        write(lib.path(), "staging/IMG_1.jpg", b"only copy");
        let stg = lib.path().join("staging");

        let report = run(
            &stg,
            lib.path(),
            &[candidate("IMG_1.jpg", &["staging/IMG_1.jpg"])],
        );

        assert_eq!(report.outcomes[0].1, Outcome::CopyMissing);
        assert!(stg.join("IMG_1.jpg").exists());
    }

    #[test]
    fn missing_staging_file_is_skipped() {
        let stg = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        write(lib.path(), "a.jpg", b"beach");

        let report = run(
            stg.path(),
            lib.path(),
            &[candidate("IMG_1.jpg", &["a.jpg"])],
        );

        assert_eq!(report.outcomes[0].1, Outcome::StagingMissing);
        assert_eq!((report.trashed_count(), report.skipped()), (0, 1));
    }

    #[test]
    fn one_skip_does_not_stop_the_batch() {
        let stg = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        write(stg.path(), "1.jpg", b"one");
        write(stg.path(), "2.jpg", b"two");
        write(lib.path(), "two.jpg", b"two");

        let report = run(
            stg.path(),
            lib.path(),
            &[
                candidate("1.jpg", &["one.jpg"]),
                candidate("2.jpg", &["two.jpg"]),
            ],
        );

        assert_eq!(report.trashed().collect::<Vec<_>>(), ["2.jpg"]);
        assert_eq!(report.skipped(), 1);
        let log = fs::read_to_string(&report.log_path).unwrap();
        assert_eq!(log.lines().count(), 3);
    }

    #[test]
    fn cancel_stops_before_the_next_file() {
        let stg = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        write(stg.path(), "1.jpg", b"one");
        write(lib.path(), "one.jpg", b"one");

        let done = AtomicUsize::new(0);
        let report = trash_verified(
            stg.path(),
            lib.path(),
            &[candidate("1.jpg", &["one.jpg"])],
            &done,
            &AtomicBool::new(true),
        )
        .unwrap();

        assert!(report.cancelled);
        assert!(report.outcomes.is_empty());
        assert_eq!(done.load(Ordering::Relaxed), 0);
        assert!(stg.path().join("1.jpg").exists());
    }

    #[test]
    fn paths_escaping_the_roots_fail() {
        let stg = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();

        let report = run(stg.path(), lib.path(), &[candidate("../x.jpg", &["a.jpg"])]);

        assert!(matches!(report.outcomes[0].1, Outcome::Failed(_)));
    }

    #[test]
    fn csv_fields_with_commas_or_quotes_are_quoted() {
        assert_eq!(csv_field("plain.jpg"), "plain.jpg");
        assert_eq!(csv_field("a, b.jpg"), "\"a, b.jpg\"");
        assert_eq!(csv_field("say \"hi\".jpg"), "\"say \"\"hi\"\".jpg\"");
    }
}
