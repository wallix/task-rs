//! Filesystem search helpers for locating a Taskfile from an entrypoint and
//! working directory.
//!
//! A `NotFound` search result is distinguished from a `PermissionDenied` one
//! because the reader reports them with different messages: walking up the
//! tree stops early if directory ownership changes, which is treated as a
//! permission error rather than a plain "not found".

use std::io;
use std::path::{Path, PathBuf};

use crate::filepathext;
use crate::sysinfo;

/// Returns the default directory given an entrypoint or directory.
///
/// When `dir` is set it is made absolute and returned. When both are empty the
/// directory defaults to the current working directory. When only `entrypoint`
/// is set the directory is left blank so that it can later be derived from the
/// resolved entrypoint.
pub fn default_dir(entrypoint: &str, dir: &str) -> String {
    if !dir.is_empty() {
        return std::fs::canonicalize(dir)
            .or_else(|_| abs(dir))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
    }

    if entrypoint.is_empty() {
        return std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
    }

    String::new()
}

/// Returns the absolute directory a task should run in.
///
/// When both an entrypoint and a directory are provided, the Taskfile does not
/// sit inside `dir`, so the given directory is made absolute. Otherwise the
/// directory is the parent of the resolved entrypoint.
pub fn resolve_dir(entrypoint: &str, resolved_entrypoint: &str, dir: &str) -> io::Result<String> {
    if !entrypoint.is_empty() && !dir.is_empty() {
        return Ok(abs(dir)?.to_string_lossy().into_owned());
    }
    Ok(parent_dir(resolved_entrypoint))
}

/// The outcome of a filesystem search that failed to find a match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchError {
    /// No matching file exists at or above the search location.
    NotFound,
    /// The walk aborted because directory ownership changed.
    PermissionDenied,
    /// An unexpected I/O failure occurred.
    Io,
}

/// Looks for a Taskfile using the given entrypoint and directory.
///
/// If an entrypoint is set it is resolved directly. Otherwise the tree is
/// walked upwards from `dir` (defaulting to the working directory when empty),
/// searching each directory for one of `possible_filenames`.
pub fn search(
    entrypoint: &str,
    dir: &str,
    possible_filenames: &[&str],
) -> Result<String, SearchError> {
    if !entrypoint.is_empty() {
        return search_path(entrypoint, possible_filenames);
    }
    let start = if dir.is_empty() {
        std::env::current_dir()
            .map_err(|_| SearchError::Io)?
            .to_string_lossy()
            .into_owned()
    } else {
        dir.to_string()
    };
    search_path_recursively(&start, possible_filenames)
}

/// Checks whether a file exists at `path`. If so its absolute path is returned.
/// If `path` is a directory, each of `possible_filenames` is tried inside it
/// and the first match returned.
pub fn search_path(path: &str, possible_filenames: &[&str]) -> Result<String, SearchError> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            return Err(SearchError::PermissionDenied);
        }
        Err(_) => return Err(SearchError::NotFound),
    };

    if meta.is_file() {
        return abs(path)
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|_| SearchError::Io);
    }

    if meta.is_dir() {
        for filename in possible_filenames {
            let alt = filepathext::smart_join(path, filename);
            if alt.exists() {
                return abs(&alt.to_string_lossy())
                    .map(|p| p.to_string_lossy().into_owned())
                    .map_err(|_| SearchError::Io);
            }
        }
    }

    Err(SearchError::NotFound)
}

/// Walks up the directory tree from `path`, searching each directory until a
/// match is found or the root is reached. Aborts with a permission error if the
/// directory owner changes between levels.
pub fn search_path_recursively(
    path: &str,
    possible_filenames: &[&str],
) -> Result<String, SearchError> {
    let (paths, err) = search_n_path_recursively(path, possible_filenames, Some(1));
    if let Some(first) = paths.into_iter().next() {
        return Ok(first);
    }
    Err(err.unwrap_or(SearchError::NotFound))
}

/// Walks up the directory tree collecting matches until the root is reached or
/// `n` matches are found (when `n` is `Some`). Returns the matches gathered so
/// far alongside a terminating error, if any.
pub fn search_n_path_recursively(
    path: &str,
    possible_filenames: &[&str],
    n: Option<usize>,
) -> (Vec<String>, Option<SearchError>) {
    let mut paths: Vec<String> = Vec::new();
    let mut current = PathBuf::from(path);

    let mut owner = match sysinfo::owner(&current) {
        Ok(o) => o,
        Err(_) => return (paths, Some(SearchError::Io)),
    };

    loop {
        if let Some(limit) = n
            && paths.len() >= limit
        {
            break;
        }

        if let Ok(fpath) = search_path(&current.to_string_lossy(), possible_filenames) {
            paths.push(fpath);
        }

        let parent = parent_path(&current);
        let parent_owner = match sysinfo::owner(&parent) {
            Ok(o) => o,
            Err(_) => return (paths, Some(SearchError::Io)),
        };

        if current == parent {
            return (paths, None);
        } else if parent_owner != owner {
            return (paths, Some(SearchError::PermissionDenied));
        }

        owner = parent_owner;
        current = parent;
    }

    (paths, None)
}

fn abs(path: &str) -> io::Result<PathBuf> {
    let p = Path::new(path);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(p))
}

fn parent_dir(path: &str) -> String {
    parent_path(Path::new(path)).to_string_lossy().into_owned()
}

fn parent_path(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp() -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "taskcore-fsext-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn search_finds_named_file_in_dir() {
        let d = tmp();
        let f = d.join("Taskfile.yml");
        fs::write(&f, "version: '3'\n").unwrap();
        let got = search(&d.to_string_lossy(), "", &["Taskfile.yml"]).unwrap();
        assert_eq!(
            fs::canonicalize(&got).unwrap(),
            fs::canonicalize(&f).unwrap()
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn search_missing_is_not_found() {
        let d = tmp();
        let err = search(&d.join("nope.yml").to_string_lossy(), "", &["Taskfile.yml"]).unwrap_err();
        assert_eq!(err, SearchError::NotFound);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn search_walks_up_to_parent() {
        let d = tmp();
        let sub = d.join("a").join("b");
        fs::create_dir_all(&sub).unwrap();
        let f = d.join("Taskfile.yml");
        fs::write(&f, "version: '3'\n").unwrap();
        let got = search("", &sub.to_string_lossy(), &["Taskfile.yml"]).unwrap();
        assert_eq!(
            fs::canonicalize(&got).unwrap(),
            fs::canonicalize(&f).unwrap()
        );
        fs::remove_dir_all(&d).ok();
    }
}
