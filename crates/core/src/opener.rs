//! Open files and folders with the operating system: the default application for a
//! file, or the file manager (Finder on macOS) showing a file or folder.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAction {
    /// Open a file with its default application, or a folder in the file manager.
    Open,
    /// Show a file in the file manager (selected in Finder on macOS; elsewhere its
    /// folder is opened, since there is no portable way to select it).
    Reveal,
}

/// Program and arguments that perform `action` on `path` for the given OS name
/// (as in `std::env::consts::OS`).
pub fn command_for(os: &str, action: OpenAction, path: &Path) -> (&'static str, Vec<OsString>) {
    let path_arg = path.as_os_str().to_owned();
    match (os, action) {
        ("macos", OpenAction::Open) => ("open", vec![path_arg]),
        ("macos", OpenAction::Reveal) => ("open", vec!["-R".into(), path_arg]),
        (_, OpenAction::Open) => ("xdg-open", vec![path_arg]),
        (_, OpenAction::Reveal) => (
            "xdg-open",
            vec![path.parent().unwrap_or(path).as_os_str().to_owned()],
        ),
    }
}

fn run(action: OpenAction, path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} does not exist", path.display()),
        ));
    }
    let (program, args) = command_for(std::env::consts::OS, action, path);
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reap the helper process in the background so it never lingers as a zombie.
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Open a file with its default application, or a folder in the file manager.
pub fn open_path(path: &Path) -> io::Result<()> {
    run(OpenAction::Open, path)
}

/// Show a file in the file manager.
pub fn reveal(path: &Path) -> io::Result<()> {
    run(OpenAction::Reveal, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn macos_uses_open_and_open_dash_r() {
        let path = Path::new("/photos/2025/clip.mov");

        assert_eq!(
            command_for("macos", OpenAction::Open, path),
            ("open", args(&["/photos/2025/clip.mov"]))
        );
        assert_eq!(
            command_for("macos", OpenAction::Reveal, path),
            ("open", args(&["-R", "/photos/2025/clip.mov"]))
        );
    }

    #[test]
    fn linux_uses_xdg_open_and_reveals_the_parent_folder() {
        let path = Path::new("/photos/2025/clip.mov");

        assert_eq!(
            command_for("linux", OpenAction::Open, path),
            ("xdg-open", args(&["/photos/2025/clip.mov"]))
        );
        assert_eq!(
            command_for("linux", OpenAction::Reveal, path),
            ("xdg-open", args(&["/photos/2025"]))
        );
    }

    #[test]
    fn paths_with_spaces_stay_a_single_argument() {
        let path = Path::new("/photos/Birthday party/my clip.mov");

        let (_, macos) = command_for("macos", OpenAction::Open, path);

        assert_eq!(macos, args(&["/photos/Birthday party/my clip.mov"]));
    }

    #[test]
    fn missing_paths_fail_without_running_anything() {
        let dir = tempfile::tempdir().unwrap();

        for result in [
            open_path(&dir.path().join("nope.mov")),
            reveal(&dir.path().join("nope.mov")),
        ] {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        }
    }
}
