//! Helpers for `/`-separated paths relative to a library root.

use std::path::{Path, PathBuf, MAIN_SEPARATOR};

use crate::i18n::{tr, trf};

pub fn join_dir(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

pub fn parent_dir(dir: &str) -> &str {
    dir.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Last component of a relative path.
pub fn file_name(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, name)| name)
}

/// `(name, path relative to the root)` for every folder level in `dir`.
pub fn breadcrumbs(dir: &str) -> Vec<(String, String)> {
    let mut crumbs = Vec::new();
    let mut rel = String::new();
    for name in dir.split('/').filter(|n| !n.is_empty()) {
        rel = join_dir(&rel, name);
        crumbs.push((name.to_string(), rel.clone()));
    }
    crumbs
}

pub fn shorten(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{kept}...")
}

/// Human readable size such as `512 B` or `2.0 MB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// How long ago something happened, e.g. `just now`, `4 min ago`, `3 h ago`, `2 days ago`.
pub fn format_age(seconds: u64) -> String {
    match seconds {
        0..60 => tr("just now").to_string(),
        60..3600 => trf("{n} min ago", &[("n", &(seconds / 60))]),
        3600..86400 => trf("{n} h ago", &[("n", &(seconds / 3600))]),
        86400..172800 => tr("1 day ago").to_string(),
        _ => trf("{n} days ago", &[("n", &(seconds / 86400))]),
    }
}

/// The user's home folder, if known.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// `path` with the home folder written as `~`, e.g. `~/Pictures/Library`.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~{MAIN_SEPARATOR}{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2.0 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn directory_path_helpers() {
        assert_eq!(join_dir("", "2024"), "2024");
        assert_eq!(join_dir("2024", "trip"), "2024/trip");
        assert_eq!(parent_dir("2024/trip"), "2024");
        assert_eq!(parent_dir("2024"), "");
        assert_eq!(parent_dir(""), "");
        assert_eq!(file_name("2024/trip/a.jpg"), "a.jpg");
        assert_eq!(file_name("a.jpg"), "a.jpg");
        assert_eq!(
            breadcrumbs("2024/trip"),
            [
                ("2024".to_string(), "2024".to_string()),
                ("trip".to_string(), "2024/trip".to_string())
            ]
        );
        assert!(breadcrumbs("").is_empty());
    }

    #[test]
    fn shortens_long_names() {
        assert_eq!(shorten("short.jpg", 24), "short.jpg");
        let long = "a_very_long_photo_name_from_the_phone.jpg";
        let short = shorten(long, 24);
        assert_eq!(short.chars().count(), 24);
        assert!(short.ends_with("..."));
    }

    #[test]
    fn formats_ages() {
        assert_eq!(format_age(5), "just now");
        assert_eq!(format_age(4 * 60 + 10), "4 min ago");
        assert_eq!(format_age(3 * 3600), "3 h ago");
        assert_eq!(format_age(86400), "1 day ago");
        assert_eq!(format_age(5 * 86400), "5 days ago");
    }

    #[test]
    fn home_folder_is_shown_as_tilde() {
        let home = Path::new("/Users/me");
        let library = home.join("Pictures").join("Library");
        assert_eq!(
            tilde(&library, Some(home)),
            format!(
                "~{MAIN_SEPARATOR}{}",
                Path::new("Pictures").join("Library").display()
            )
        );
        assert_eq!(tilde(home, Some(home)), "~");
        assert_eq!(tilde(Path::new("/Volumes/x"), Some(home)), "/Volumes/x");
        assert_eq!(tilde(&library, None), library.display().to_string());
    }
}
