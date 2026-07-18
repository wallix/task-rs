//! Path-joining helpers that layer task-specific rules on top of
//! `std::path`: right-to-left absolute-path override, recognition of the
//! special `.ROOT_DIR`/`.TASKFILE_DIR`/`.USER_WORKING_DIR` markers as
//! absolute, and small classification helpers.

use std::path::{Path, PathBuf};

/// Directory placeholders that always denote an absolute location even
/// though they are not syntactically absolute paths.
const KNOWN_ABS_DIRS: [&str; 3] = [".ROOT_DIR", ".TASKFILE_DIR", ".USER_WORKING_DIR"];

/// Joins directory components, scanning right-to-left for the rightmost
/// absolute path and joining from there forward. Later (more specific)
/// absolute paths override earlier ones. Returns an empty path when given
/// no components.
pub fn join_dirs<S: AsRef<str>>(dirs: &[S]) -> PathBuf {
    let mut i = dirs.len();
    while i > 0 {
        i = i.wrapping_sub(1);
        let is_first = i == 0;
        let component = dirs.get(i).map(AsRef::as_ref).unwrap_or_default();
        if is_first || Path::new(component).is_absolute() {
            let mut out = PathBuf::new();
            for part in dirs.get(i..).unwrap_or_default() {
                out = join(&out, part.as_ref());
            }
            return out;
        }
    }
    PathBuf::new()
}

/// Ports Go's `filepath.Join`: concatenates the non-empty elements with `/`
/// and cleans the result. Unlike [`join_dirs`], a later absolute element does
/// not discard the earlier ones — Go simply joins. Returns an empty string when
/// every element is empty.
pub fn join_path<S: AsRef<str>>(elems: &[S]) -> String {
    let joined = elems
        .iter()
        .map(AsRef::as_ref)
        .filter(|e| !e.is_empty())
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        return String::new();
    }
    clean(&joined).to_string_lossy().into_owned()
}

/// Joins `a` and `b`, but only if `b` is not already an absolute path.
pub fn smart_join(a: &str, b: &str) -> PathBuf {
    if is_abs(b) {
        return PathBuf::from(b);
    }
    join(Path::new(a), b)
}

/// Reports whether `path` is absolute, treating the special directory
/// markers as absolute.
pub fn is_abs(path: &str) -> bool {
    if is_special_dir(path) {
        return true;
    }
    Path::new(path).is_absolute()
}

fn is_special_dir(dir: &str) -> bool {
    KNOWN_ABS_DIRS.iter().any(|d| dir.contains(d))
}

/// Tries to convert an absolute path to one relative to the process
/// working directory. Falls back to the input on any failure.
pub fn try_abs_to_rel(abs: &str) -> PathBuf {
    let Ok(wd) = std::env::current_dir() else {
        return PathBuf::from(abs);
    };
    match rel(&wd, Path::new(abs)) {
        Some(r) => r,
        None => PathBuf::from(abs),
    }
}

/// Computes `path` relative to `base`, mirroring `filepath.Rel`. Returns
/// `None` when the relationship cannot be expressed.
pub fn rel_str(base: &str, path: &str) -> Option<String> {
    rel(Path::new(base), Path::new(path)).map(|p| p.to_string_lossy().into_owned())
}

/// Reports whether `path` points to a file with no name but with an
/// extension, e.g. `.yaml`.
pub fn is_ext_only(path: &str) -> bool {
    let p = Path::new(path);
    let base = p.file_name().map(|s| s.to_string_lossy().into_owned());
    let ext = go_ext(path);
    match base {
        Some(b) => b == ext,
        None => ext.is_empty(),
    }
}

/// Joins a base path with an appended component using Go's `filepath.Join`
/// semantics: the result is cleaned, and an empty base yields the cleaned
/// component alone.
fn join(base: &Path, component: &str) -> PathBuf {
    if component.is_empty() {
        return clean(&base.to_string_lossy());
    }
    if base.as_os_str().is_empty() {
        return clean(component);
    }
    let joined = format!("{}/{}", base.to_string_lossy(), component);
    clean(&joined)
}

/// Computes `path` relative to `base`, mirroring `filepath.Rel`. Returns
/// `None` when the relationship cannot be expressed (e.g. one path is
/// absolute and the other is not).
fn rel(base: &Path, path: &Path) -> Option<PathBuf> {
    let base = clean(&base.to_string_lossy());
    let target = clean(&path.to_string_lossy());
    if base == target {
        return Some(PathBuf::from("."));
    }
    if base.is_absolute() != target.is_absolute() {
        return None;
    }

    let base_parts: Vec<&str> = split_clean(&base);
    let target_parts: Vec<&str> = split_clean(&target);

    let mut common = 0usize;
    loop {
        let b = base_parts.get(common);
        let t = target_parts.get(common);
        match (b, t) {
            (Some(b), Some(t)) if b == t => common = common.saturating_add(1),
            _ => break,
        }
    }

    // Any remaining base component that is ".." cannot be undone.
    if base_parts.get(common..).is_some_and(|s| s.contains(&"..")) {
        return None;
    }

    let up = base_parts.len().saturating_sub(common);
    let mut out_parts: Vec<&str> = std::iter::repeat_n("..", up).collect();
    if let Some(rest) = target_parts.get(common..) {
        out_parts.extend_from_slice(rest);
    }
    if out_parts.is_empty() {
        return Some(PathBuf::from("."));
    }
    Some(PathBuf::from(out_parts.join("/")))
}

/// Splits a cleaned path into non-empty components, discarding a leading
/// root separator.
fn split_clean(p: &Path) -> Vec<&str> {
    p.to_str()
        .unwrap_or_default()
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect()
}

/// Reimplements Go's `filepath.Clean` for slash-separated paths: collapses
/// separators, resolves `.` and `..`, and preserves a leading root.
fn clean(path: &str) -> PathBuf {
    if path.is_empty() {
        return PathBuf::from(".");
    }
    let rooted = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => match out.last() {
                Some(&"..") => out.push(".."),
                Some(_) => {
                    out.pop();
                }
                None => {
                    if !rooted {
                        out.push("..");
                    }
                }
            },
            other => out.push(other),
        }
    }
    let body = out.join("/");
    let result = if rooted {
        format!("/{body}")
    } else if body.is_empty() {
        ".".to_string()
    } else {
        body
    };
    PathBuf::from(result)
}

/// Reimplements Go's `filepath.Ext`: the suffix beginning at the final `.`
/// in the last path element, or an empty string if there is none.
fn go_ext(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    match base.rfind('.') {
        Some(idx) => base.get(idx..).unwrap_or_default().to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_join_relative() {
        assert_eq!(smart_join("/a/b", "c/d"), PathBuf::from("/a/b/c/d"));
    }

    #[test]
    fn smart_join_absolute_second() {
        assert_eq!(smart_join("/a/b", "/c/d"), PathBuf::from("/c/d"));
    }

    #[test]
    fn smart_join_special_dir() {
        assert_eq!(
            smart_join("/a/b", ".ROOT_DIR/x"),
            PathBuf::from(".ROOT_DIR/x")
        );
    }

    #[test]
    fn join_dirs_rightmost_absolute_wins() {
        let dirs = ["/base", "sub", "/override", "leaf"];
        assert_eq!(join_dirs(&dirs), PathBuf::from("/override/leaf"));
    }

    #[test]
    fn join_dirs_all_relative_joins_from_first() {
        let dirs = ["base", "sub", "leaf"];
        assert_eq!(join_dirs(&dirs), PathBuf::from("base/sub/leaf"));
    }

    #[test]
    fn join_dirs_empty() {
        let dirs: [&str; 0] = [];
        assert_eq!(join_dirs(&dirs), PathBuf::new());
    }

    #[test]
    fn is_abs_special_and_plain() {
        assert!(is_abs("/etc"));
        assert!(is_abs("foo/.TASKFILE_DIR/bar"));
        assert!(!is_abs("relative/path"));
    }

    #[test]
    fn is_ext_only_cases() {
        assert!(is_ext_only(".yaml"));
        assert!(is_ext_only("/some/dir/.gitignore"));
        assert!(!is_ext_only("file.yaml"));
        assert!(!is_ext_only("noext"));
    }

    #[test]
    fn ext_matches_go() {
        assert_eq!(go_ext("a/b.txt"), ".txt");
        assert_eq!(go_ext("a/b"), "");
        assert_eq!(go_ext(".hidden"), ".hidden");
    }

    #[test]
    fn clean_resolves_dots() {
        assert_eq!(clean("a/b/../c"), PathBuf::from("a/c"));
        assert_eq!(clean("/a//b/./c"), PathBuf::from("/a/b/c"));
        assert_eq!(clean(""), PathBuf::from("."));
    }
}
