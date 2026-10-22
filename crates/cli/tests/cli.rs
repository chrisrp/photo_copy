use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn cli(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_photo-copy"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn index_dups_compare_flow() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("library");
    let staging = dir.path().join("staging");
    fs::create_dir_all(library.join("2024")).unwrap();
    fs::create_dir_all(&staging).unwrap();
    fs::write(library.join("2024/beach.jpg"), b"beach").unwrap();
    fs::write(library.join("2024/beach_copy.jpg"), b"beach").unwrap();
    fs::write(staging.join("IMG_1.jpg"), b"beach").unwrap();
    fs::write(staging.join("IMG_2.jpg"), b"new photo").unwrap();

    assert!(cli(&["index", s(&library)]).status.success());
    assert!(cli(&["index", s(&staging)]).status.success());

    let dups = stdout(&cli(&["dups", s(&library)]));
    assert!(dups.contains("2024/beach.jpg") && dups.contains("2024/beach_copy.jpg"));
    assert!(dups.contains("1 duplicate group(s)"));

    let compare = stdout(&cli(&["compare", s(&staging), s(&library)]));
    assert!(compare.contains("IN-LIB   IMG_1.jpg"));
    assert!(compare.contains("NEW      IMG_2.jpg"));
    assert!(compare.contains("1 new, 1 already in library, 0 same photo in library with other metadata, 0 duplicated inside staging"));
}

#[test]
fn trash_and_move_commands() {
    let dir = tempfile::tempdir().unwrap();
    let library = dir.path().join("library");
    let staging = dir.path().join("staging");
    fs::create_dir_all(library.join("trip")).unwrap();
    fs::create_dir_all(&staging).unwrap();
    fs::write(library.join("trip/a.jpg"), b"a").unwrap();
    fs::write(staging.join("b.jpg"), b"b").unwrap();

    assert!(cli(&["trash", s(&library), "trip/a.jpg"]).status.success());
    assert!(library.join(".trash/trip/a.jpg").exists());

    let moved = cli(&["move", s(&staging.join("b.jpg")), s(&library.join("trip"))]);
    assert!(moved.status.success());
    assert!(library.join("trip/b.jpg").exists());
    assert!(!staging.join("b.jpg").exists());

    // Moving onto an existing name fails and keeps the source.
    fs::write(staging.join("b.jpg"), b"other").unwrap();
    let refused = cli(&["move", s(&staging.join("b.jpg")), s(&library.join("trip"))]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("already exists"));
    assert_eq!(fs::read(staging.join("b.jpg")).unwrap(), b"other");
    assert_eq!(fs::read(library.join("trip/b.jpg")).unwrap(), b"b");
}

#[test]
fn bad_usage_fails() {
    assert!(!cli(&["bogus"]).status.success());
}
