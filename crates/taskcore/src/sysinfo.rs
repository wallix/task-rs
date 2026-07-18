//! Filesystem ownership queries.

use std::io;
use std::path::Path;

/// Returns the numeric owner (UID) of the file at `path`.
///
/// On Windows there is no straightforward way to obtain file ownership, so
/// this always yields `-1`.
#[cfg(unix)]
pub fn owner(path: &Path) -> io::Result<i64> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    Ok(i64::from(meta.uid()))
}

/// Returns the numeric owner (UID) of the file at `path`.
///
/// On Windows there is no straightforward way to obtain file ownership, so
/// this always yields `-1`.
#[cfg(not(unix))]
pub fn owner(_path: &Path) -> io::Result<i64> {
    Ok(-1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn owner_of_existing_file_matches_process() {
        use std::io::Write;
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("taskcore-sysinfo-{}", std::process::id()));
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(b"x").unwrap();
        drop(f);

        // SAFETY: getuid is always safe to call.
        let expected = unsafe { libc_getuid() };
        let got = owner(&tmp).unwrap();
        std::fs::remove_file(&tmp).ok();
        assert_eq!(got, i64::from(expected));
    }

    #[cfg(unix)]
    #[test]
    fn owner_of_missing_file_errors() {
        let missing = Path::new("/nonexistent/taskcore/definitely/missing");
        assert!(owner(missing).is_err());
    }

    #[cfg(unix)]
    unsafe extern "C" {
        #[link_name = "getuid"]
        fn libc_getuid() -> u32;
    }
}
