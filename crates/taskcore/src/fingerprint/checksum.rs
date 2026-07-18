use std::io::Read;
use std::path::Path;

use twox_hash::XxHash3_128;

use crate::filepathext;

/// Computes a hash over the given files and data strings. File paths are hashed
/// as relative paths from `basedir` so a checksum changes when a file is moved.
/// Symlinks are hashed by their link target string rather than the content they
/// point to.
///
/// The hash is 128-bit xxHash3 rendered as the high then low 64-bit halves in
/// lower-hex without zero-padding, matching the Go implementation's framing and
/// output byte-for-byte, so a `.task` cache written by either tool is
/// interchangeable. Fast and non-cryptographic — a change-detection
/// fingerprint, not a security digest.
pub fn checksum_files(basedir: &str, files: &[String], data: &[String]) -> std::io::Result<String> {
    let mut hasher = XxHash3_128::new();
    let mut buf = vec![0u8; 128 * 1024];

    for f in files {
        // Hash the relative path so the checksum changes when a file moves.
        match filepathext::rel_str(basedir, f) {
            Some(rel) => hasher.write(rel.as_bytes()),
            None => hasher.write(f.as_bytes()),
        }

        let path = Path::new(f);
        let is_symlink = std::fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);

        if is_symlink {
            let link = std::fs::read_link(path)?;
            hasher.write(link.to_string_lossy().as_bytes());
        } else {
            let mut file = std::fs::File::open(path)?;
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.write(buf.get(..n).unwrap_or(&[]));
            }
        }
    }

    for d in data {
        hasher.write(d.as_bytes());
    }

    // Go renders `fmt.Sprintf("%x%x", hi, lo)`: the two 64-bit halves in hex,
    // most-significant first, each without zero-padding.
    let digest = hasher.finish_128();
    let bytes = digest.to_be_bytes();
    let hi = u64::from_be_bytes(
        bytes
            .get(..8)
            .and_then(|b| b.try_into().ok())
            .unwrap_or_default(),
    );
    let lo = u64::from_be_bytes(
        bytes
            .get(8..)
            .and_then(|b| b.try_into().ok())
            .unwrap_or_default(),
    );
    Ok(format!("{hi:x}{lo:x}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::testutil::{join, tmp};
    use std::fs;

    #[test]
    fn regular_files_deterministic() {
        let dir = tmp();
        let f1 = join(&dir, "a.txt");
        let f2 = join(&dir, "b.txt");
        fs::write(&f1, b"hello").unwrap();
        fs::write(&f2, b"world").unwrap();
        let files = vec![f1, f2];
        let h1 = checksum_files(&dir, &files, &[]).unwrap();
        let h2 = checksum_files(&dir, &files, &[]).unwrap();
        assert!(!h1.is_empty());
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_content_different_hash() {
        let dir = tmp();
        let f = join(&dir, "a.txt");
        fs::write(&f, b"hello").unwrap();
        let h1 = checksum_files(&dir, std::slice::from_ref(&f), &[]).unwrap();
        fs::write(&f, b"changed").unwrap();
        let h2 = checksum_files(&dir, &[f], &[]).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn relative_path_hashing() {
        let dir1 = tmp();
        let dir2 = tmp();
        fs::create_dir_all(join(&dir1, "sub")).unwrap();
        let f1 = join(&dir1, "sub/a.txt");
        fs::write(&f1, b"hello").unwrap();
        let f2 = join(&dir2, "a.txt");
        fs::write(&f2, b"hello").unwrap();
        let h1 = checksum_files(&dir1, &[f1], &[]).unwrap();
        let h2 = checksum_files(&dir2, &[f2], &[]).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn rename_changes_hash() {
        let dir = tmp();
        let f1 = join(&dir, "a.txt");
        fs::write(&f1, b"hello").unwrap();
        let h1 = checksum_files(&dir, std::slice::from_ref(&f1), &[]).unwrap();
        let f2 = join(&dir, "b.txt");
        fs::rename(&f1, &f2).unwrap();
        let h2 = checksum_files(&dir, &[f2], &[]).unwrap();
        assert_ne!(h1, h2);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_hashes_target_not_content() {
        let dir = tmp();
        let target = join(&dir, "target.txt");
        fs::write(&target, b"data").unwrap();
        let link = join(&dir, "link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let h_link = checksum_files(&dir, &[link], &[]).unwrap();
        let h_target = checksum_files(&dir, &[target], &[]).unwrap();
        assert_ne!(h_link, h_target);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_retarget_changes_hash() {
        let dir = tmp();
        let t1 = join(&dir, "t1.txt");
        let t2 = join(&dir, "t2.txt");
        fs::write(&t1, b"a").unwrap();
        fs::write(&t2, b"a").unwrap();
        let link = join(&dir, "link.txt");
        std::os::unix::fs::symlink(&t1, &link).unwrap();
        let h1 = checksum_files(&dir, std::slice::from_ref(&link), &[]).unwrap();
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&t2, &link).unwrap();
        let h2 = checksum_files(&dir, &[link], &[]).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn data_strings() {
        let dir = tmp();
        let h1 = checksum_files(&dir, &[], &["foo".to_string(), "bar".to_string()]).unwrap();
        let h2 = checksum_files(&dir, &[], &["foo".to_string(), "baz".to_string()]).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn missing_file_errors() {
        let dir = tmp();
        let missing = join(&dir, "nope.txt");
        assert!(checksum_files(&dir, &[missing], &[]).is_err());
    }
}
