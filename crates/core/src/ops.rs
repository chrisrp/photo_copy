//! File operations. These only run when the user asks for them; nothing here
//! hard-deletes a user file or overwrites an existing one.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::hash::hash_file;

pub const TRASH_DIR: &str = ".trash";

/// Validate a root-relative path such as `2024/trip/a.jpg` (the empty path is the
/// root itself). Absolute paths and `.`/`..` components are rejected so that the
/// result can never point outside the root it is joined to.
pub fn plain_relative(rel: &str) -> io::Result<&Path> {
    let path = Path::new(rel);
    if path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a plain relative path: {rel}"),
        ));
    }
    Ok(path)
}

fn structural_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        Some("The name must not be empty.".into())
    } else if name.starts_with('.') {
        Some("The name must not start with '.' (such files are hidden from the app).".into())
    } else if name.contains('/') || (cfg!(windows) && name.contains('\\')) {
        Some("The name must not contain '/'.".into())
    } else if name.chars().any(char::is_control) {
        Some("The name must not contain control characters.".into())
    } else {
        None
    }
}

/// Names Windows cannot store (or silently changes), which also matter when a
/// library is kept on NTFS/exFAT or shared with a Windows machine.
fn windows_problem(name: &str) -> Option<String> {
    if name.contains(['<', '>', ':', '"', '|', '?', '*', '\\']) {
        return Some(
            "The name must not contain any of < > : \" | ? * \\ (Windows cannot store them)."
                .into(),
        );
    }
    if name.ends_with(['.', ' ']) {
        return Some("The name must not end with a dot or a space (Windows would drop it).".into());
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|n| matches!(n.as_bytes(), [b'1'..=b'9']))
    };
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered("COM") || numbered("LPT")
    {
        return Some(format!("\"{stem}\" is a reserved name on Windows."));
    }
    None
}

/// Why `name` cannot be used as the name of an existing file or folder, if it
/// cannot: empty, hidden (`.`), containing `/` or control characters. On Windows the
/// Windows rules apply as well.
pub fn name_problem(name: &str) -> Option<String> {
    structural_problem(name).or_else(|| {
        if cfg!(windows) {
            windows_problem(name)
        } else {
            None
        }
    })
}

/// Like [`name_problem`], but with the Windows rules on every OS. For names the user
/// types (new folders, renames), so a library stays usable on Windows.
pub fn portable_name_problem(name: &str) -> Option<String> {
    structural_problem(name).or_else(|| windows_problem(name))
}

fn checked(name: &str, problem: Option<String>) -> io::Result<&str> {
    match problem {
        Some(message) => Err(io::Error::new(io::ErrorKind::InvalidInput, message)),
        None => Ok(name),
    }
}

/// Validate the name of a file or folder that already exists (or is moved as is).
pub fn plain_name(name: &str) -> io::Result<&str> {
    checked(name, name_problem(name))
}

/// Validate a name typed by the user (see [`portable_name_problem`]).
pub fn portable_name(name: &str) -> io::Result<&str> {
    checked(name, portable_name_problem(name))
}

/// Create the folder `name` inside `root/parent_rel`. Fails with `AlreadyExists`
/// if it is there already. Returns the new folder's path relative to `root`.
pub fn create_folder(root: &Path, parent_rel: &str, name: &str) -> io::Result<String> {
    let parent = root.join(plain_relative(parent_rel)?);
    let name = portable_name(name)?;
    fs::create_dir(parent.join(name))?;
    Ok(if parent_rel.is_empty() {
        name.to_string()
    } else {
        format!("{parent_rel}/{name}")
    })
}

/// Return `path` if nothing exists there, otherwise `stem (1).ext`, `stem (2).ext`, ...
pub fn suggest_free_name(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = path.parent().unwrap_or(Path::new(""));
    (1..)
        .map(|n| parent.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("unbounded range always yields a free name")
}

/// Move `root/rel` into `root/.trash/rel` (keeping its folder structure, never
/// overwriting an earlier trashed file). Returns the new location.
/// `rel` must be a plain relative path without `..` components.
pub fn move_to_trash(root: &Path, rel: &str) -> io::Result<PathBuf> {
    let rel_path = plain_relative(rel)?;
    let src = root.join(rel_path);
    if !src.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("not a file: {}", src.display()),
        ));
    }
    let dest = suggest_free_name(&root.join(TRASH_DIR).join(rel_path));
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&src, &dest)?;
    Ok(dest)
}

/// What [`move_file`] did.
#[derive(Debug)]
pub enum MoveOutcome {
    /// Copied, verified, and the source removed.
    Moved,
    /// Copied and verified, but the source could not be removed (locked, read-only,
    /// ...). Both copies exist now.
    SourceKept(io::Error),
}

/// Move a file to `dest` (possibly on another filesystem): copy, verify the
/// content hash, then remove the source. Fails with `AlreadyExists` if `dest`
/// exists, leaving both files untouched. If the copy or verification fails, the
/// partial destination is removed and the source is kept. If only removing the
/// source fails, the result is `Ok(MoveOutcome::SourceKept(..))`: the file is safely
/// at `dest`, so callers should record it there. The source's mtime is preserved on
/// the copy.
pub fn move_file(src: &Path, dest: &Path) -> io::Result<MoveOutcome> {
    let mut input = File::open(src)?;
    let modified = input.metadata()?.modified().ok();
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = OpenOptions::new().write(true).create_new(true).open(dest)?;

    let copied = (|| -> io::Result<()> {
        io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        if let Some(t) = modified {
            output.set_modified(t)?;
        }
        if hash_file(src)? != hash_file(dest)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "copy verification failed",
            ));
        }
        Ok(())
    })();

    // Close both files before deleting anything: Windows cannot remove open files.
    drop(input);
    drop(output);
    if let Err(e) = copied {
        let _ = fs::remove_file(dest);
        return Err(e);
    }
    Ok(match fs::remove_file(src) {
        Ok(()) => MoveOutcome::Moved,
        Err(e) => MoveOutcome::SourceKept(e),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn secs(path: &Path) -> u64 {
        fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn plain_name_checks_the_structure_of_names() {
        for ok in [
            "IMG_1.jpg",
            "Birthday party",
            "a (1).png",
            "Photo: Mom?.jpg",
        ] {
            // Names of existing files pass, even if Windows could not store them
            // (on Windows the stricter rules apply, see below).
            assert_eq!(
                plain_name(ok).is_ok(),
                !cfg!(windows) || portable_name(ok).is_ok(),
                "{ok}"
            );
        }
        for bad in ["", ".hidden", "..", "a/b", "nul\0", "tab\there"] {
            assert!(plain_name(bad).is_err(), "{bad:?}");
        }
        assert!(plain_name("IMG_1.jpg").is_ok());
        // A backslash is a legal character on Linux and macOS.
        assert_eq!(plain_name("a\\b.jpg").is_ok(), !cfg!(windows));
    }

    #[test]
    fn portable_name_applies_the_windows_rules_on_every_os() {
        for ok in [
            "IMG_1.jpg",
            "Birthday party",
            "a (1).png",
            "Console.jpg",
            "COM10.jpg",
            "COM0.jpg",
            "Photo 1.heic",
        ] {
            assert!(portable_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "x:y.jpg",
            "a<b",
            "a>b",
            "q?.jpg",
            "a*b",
            "a|b",
            "a\"b",
            "a\\b",
            "trailing.",
            "trailing ",
            "CON",
            "con.txt",
            "NUL.jpg",
            "Prn",
            "aux.tar.gz",
            "COM1.jpg",
            "lpt9",
            "COM3 .txt",
            "",
            ".hidden",
            "a/b",
        ] {
            assert!(portable_name(bad).is_err(), "{bad:?}");
            assert!(portable_name_problem(bad).is_some(), "{bad:?}");
        }
    }

    #[test]
    fn name_problems_explain_what_is_wrong() {
        assert!(portable_name_problem("x:y.jpg").unwrap().contains(':'));
        assert!(portable_name_problem("CON.jpg").unwrap().contains("CON"));
        assert!(portable_name_problem("a.").unwrap().contains("dot"));
        assert!(name_problem("").unwrap().contains("empty"));
        assert_eq!(portable_name_problem("fine.jpg"), None);
    }

    #[test]
    fn create_folder_makes_nested_folders_and_returns_relative_path() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(create_folder(dir.path(), "", "2025").unwrap(), "2025");
        assert_eq!(
            create_folder(dir.path(), "2025", "trip").unwrap(),
            "2025/trip"
        );
        assert!(dir.path().join("2025/trip").is_dir());
    }

    #[test]
    fn create_folder_refuses_existing_bad_names_and_escapes() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("lib")).unwrap();
        let lib = dir.path().join("lib");
        create_folder(&lib, "", "2025").unwrap();

        assert_eq!(
            create_folder(&lib, "", "2025").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(create_folder(&lib, "", "a/b").is_err());
        assert!(create_folder(&lib, "", "x:y").is_err());
        assert!(create_folder(&lib, "", "CON").is_err());
        assert!(create_folder(&lib, "", ".trash").is_err());
        assert!(create_folder(&lib, "..", "evil").is_err());
        assert!(!dir.path().join("evil").exists());
        assert_eq!(
            create_folder(&lib, "missing", "x").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn suggest_free_name_keeps_free_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        assert_eq!(suggest_free_name(&path), path);
    }

    #[test]
    fn suggest_free_name_adds_counter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        fs::write(&path, b"x").unwrap();
        assert_eq!(suggest_free_name(&path), dir.path().join("a (1).jpg"));

        fs::write(dir.path().join("a (1).jpg"), b"x").unwrap();
        assert_eq!(suggest_free_name(&path), dir.path().join("a (2).jpg"));
    }

    #[test]
    fn trash_keeps_structure_and_moves_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("2024")).unwrap();
        fs::write(dir.path().join("2024/a.jpg"), b"aaa").unwrap();

        let trashed = move_to_trash(dir.path(), "2024/a.jpg").unwrap();

        assert_eq!(trashed, dir.path().join(".trash/2024/a.jpg"));
        assert_eq!(fs::read(&trashed).unwrap(), b"aaa");
        assert!(!dir.path().join("2024/a.jpg").exists());
    }

    #[test]
    fn trash_never_overwrites_earlier_trashed_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.jpg"), b"first").unwrap();
        let first = move_to_trash(dir.path(), "a.jpg").unwrap();
        fs::write(dir.path().join("a.jpg"), b"second").unwrap();
        let second = move_to_trash(dir.path(), "a.jpg").unwrap();

        assert_ne!(first, second);
        assert_eq!(fs::read(first).unwrap(), b"first");
        assert_eq!(fs::read(second).unwrap(), b"second");
    }

    #[test]
    fn trash_rejects_paths_escaping_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        fs::create_dir(&root).unwrap();
        fs::write(dir.path().join("outside.jpg"), b"x").unwrap();

        let err = move_to_trash(&root, "../outside.jpg").unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(dir.path().join("outside.jpg").exists());
        assert!(move_to_trash(&root, "/etc/hosts").is_err());
    }

    #[test]
    fn trash_missing_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = move_to_trash(dir.path(), "nope.jpg").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn move_file_copies_content_removes_source_and_creates_folders() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("IMG_1.jpg");
        let dest = dir.path().join("lib/2025/party/IMG_1.jpg");
        fs::write(&src, b"pixels").unwrap();

        let outcome = move_file(&src, &dest).unwrap();

        assert!(matches!(outcome, MoveOutcome::Moved));
        assert_eq!(fs::read(&dest).unwrap(), b"pixels");
        assert!(!src.exists());
    }

    #[test]
    fn move_file_preserves_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.jpg");
        let dest = dir.path().join("b.jpg");
        fs::write(&src, b"pixels").unwrap();
        let old = UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(old)
            .unwrap();

        move_file(&src, &dest).unwrap();

        assert_eq!(secs(&dest), 1_500_000_000);
    }

    #[cfg(unix)]
    #[test]
    fn move_file_reports_when_only_the_source_cannot_be_removed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir(&src_dir).unwrap();
        let src = src_dir.join("a.jpg");
        let dest = dir.path().join("b.jpg");
        fs::write(&src, b"pixels").unwrap();
        // A read-only folder lets us read the file but not remove it. Permissions are
        // not enforced when running as root: nothing to test then.
        fs::set_permissions(&src_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let enforced = fs::write(src_dir.join("probe"), b"x").is_err();
        let result = enforced.then(|| move_file(&src, &dest));
        fs::set_permissions(&src_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let Some(result) = result else {
            return;
        };

        assert!(matches!(result.unwrap(), MoveOutcome::SourceKept(_)));
        assert_eq!(fs::read(&dest).unwrap(), b"pixels");
        assert!(src.exists());
    }

    #[test]
    fn move_file_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.jpg");
        let dest = dir.path().join("b.jpg");
        fs::write(&src, b"new").unwrap();
        fs::write(&dest, b"existing").unwrap();

        let err = move_file(&src, &dest).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&src).unwrap(), b"new");
        assert_eq!(fs::read(&dest).unwrap(), b"existing");
    }

    #[test]
    fn move_file_missing_source_leaves_no_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("b.jpg");

        assert!(move_file(&dir.path().join("nope.jpg"), &dest).is_err());
        assert!(!dest.exists());
    }
}
