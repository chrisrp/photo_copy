use std::fs::File;
use std::io;
use std::path::Path;

/// BLAKE3 hash of the file content, as a hex string.
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn same_content_same_hash_regardless_of_name() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("IMG_0001.jpg");
        let b = dir.path().join("other_name.jpg");
        fs::write(&a, b"pixels").unwrap();
        fs::write(&b, b"pixels").unwrap();

        assert_eq!(hash_file(&a).unwrap(), hash_file(&b).unwrap());
    }

    #[test]
    fn different_content_different_hash() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.jpg");
        let b = dir.path().join("b.jpg");
        fs::write(&a, b"pixels one").unwrap();
        fs::write(&b, b"pixels two").unwrap();

        assert_ne!(hash_file(&a).unwrap(), hash_file(&b).unwrap());
    }

    #[test]
    fn missing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(hash_file(&dir.path().join("nope.jpg")).is_err());
    }
}
