use std::error::Error;
use std::path::Path;
use std::process::ExitCode;

use photo_copy_core::compare::{compare_staging, StagingStatus};
use photo_copy_core::duplicates::{find_duplicates, find_same_image};
use photo_copy_core::image_hash::hash_file_and_image;
use photo_copy_core::index::Index;
use photo_copy_core::ops::{move_file, move_to_trash, suggest_free_name, MoveOutcome};
use photo_copy_core::scan::scan;

const USAGE: &str = "usage:
  photo-copy index <folder>              build/update the hash index of a folder
  photo-copy dups <folder>               list duplicate groups (uses the saved index)
  photo-copy compare <staging> <library> check staging files against the library (uses saved indexes)
  photo-copy trash <folder> <rel-path>   move a file to <folder>/.trash
  photo-copy move <file> <dest-folder>   copy+verify+remove a file into a folder (never overwrites)
  photo-copy hash <file>...              print the full-file hash and the image-data hash (- if none)";

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn load_index(root: &Path) -> Result<Index> {
    let index = Index::load(root)?;
    if index.files.is_empty() {
        eprintln!(
            "warning: no index for {} (run `photo-copy index {}` first)",
            root.display(),
            root.display()
        );
    }
    Ok(index)
}

fn run(args: &[&str]) -> Result<()> {
    match args {
        ["index", folder] => {
            let root = Path::new(folder);
            let scanned = scan(root)?;
            let mut index = Index::load(root)?;
            let stats = index.update(root, &scanned.entries)?;
            index.save(root)?;
            let problems: Vec<&String> = scanned.warnings.iter().chain(&stats.skipped).collect();
            for problem in &problems {
                eprintln!("warning: skipped {problem}");
            }
            println!(
                "{} files: {} hashed, {} reused, {} removed, {} skipped",
                index.files.len(),
                stats.hashed,
                stats.reused,
                stats.removed,
                problems.len()
            );
        }
        ["dups", folder] => {
            let index = load_index(Path::new(folder))?;
            let groups = find_duplicates(&index);
            for group in &groups {
                println!("{} ({} bytes)", &group.hash[..12], group.size);
                for path in &group.paths {
                    println!("  {path}");
                }
            }
            println!("{} duplicate group(s)", groups.len());
            if !index.is_current() {
                eprintln!(
                    "warning: the index has no image hashes yet (run `photo-copy index {folder}`)"
                );
            }
            let same = find_same_image(&index);
            for group in &same {
                println!(
                    "same photo, different metadata ({})",
                    &group.image_hash[..12]
                );
                for path in &group.paths {
                    println!("  {path}");
                }
            }
            println!("{} same-photo group(s)", same.len());
        }
        ["compare", staging, library] => {
            let staging_index = load_index(Path::new(staging))?;
            let library_index = load_index(Path::new(library))?;
            let (mut unique, mut in_library, mut same_in_library, mut in_staging) = (0, 0, 0, 0);
            for item in compare_staging(&staging_index, &library_index) {
                match item.status {
                    StagingStatus::Unique => {
                        unique += 1;
                        println!("NEW      {}", item.path);
                    }
                    StagingStatus::DuplicateOfLibrary { library_paths } => {
                        in_library += 1;
                        println!("IN-LIB   {}  ==  {}", item.path, library_paths.join(", "));
                    }
                    StagingStatus::SameImageInLibrary { library_paths } => {
                        same_in_library += 1;
                        println!("SAME-IMG {}  ~~  {}", item.path, library_paths.join(", "));
                    }
                    StagingStatus::DuplicateInStaging { other_paths } => {
                        in_staging += 1;
                        println!("IN-STAGE {}  ==  {}", item.path, other_paths.join(", "));
                    }
                }
            }
            println!("{unique} new, {in_library} already in library, {same_in_library} same photo in library with other metadata, {in_staging} duplicated inside staging");
        }
        ["trash", folder, rel] => {
            let dest = move_to_trash(Path::new(folder), rel)?;
            println!("moved to {}", dest.display());
            println!("re-run `photo-copy index {folder}` to refresh the index");
        }
        ["move", file, dest_folder] => {
            let src = Path::new(file);
            let name = src.file_name().ok_or("source is not a file path")?;
            let dest = Path::new(dest_folder).join(name);
            if dest.exists() {
                return Err(format!(
                    "{} already exists; nothing was moved (a free name would be {})",
                    dest.display(),
                    suggest_free_name(&dest).display()
                )
                .into());
            }
            if let MoveOutcome::SourceKept(e) = move_file(src, &dest)? {
                return Err(format!(
                    "copied to {} but the original could not be removed ({e}); both copies exist now",
                    dest.display()
                )
                .into());
            }
            println!("moved to {}", dest.display());
            println!("re-run `photo-copy index` on the source and destination roots to refresh the indexes");
        }
        ["hash", files @ ..] if !files.is_empty() => {
            for file in files {
                let hashes = hash_file_and_image(Path::new(file))?;
                let image = hashes.image.as_deref().unwrap_or("-");
                println!("{}  {image}  {file}", hashes.full);
            }
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
