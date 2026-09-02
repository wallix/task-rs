//! Expansion of `sources:`/`generates:` glob patterns into concrete file
//! lists.
//!
//! The Go implementation delegates to `mvdan.cc/sh`'s `expand.Fields` with
//! `GlobStar` and `NullGlob` enabled. That is reproduced here with a
//! hand-rolled recursive directory walk plus a small pattern matcher, so no
//! third-party glob crate is required. The supported syntax matches bash
//! pathname expansion with `globstar` on and `dotglob` off:
//!
//! - `*` and `?` match within a single path segment and never match a leading
//!   dot (dotfiles are hidden).
//! - `[...]` bracket expressions match a single character within a segment.
//! - `**` as a whole segment matches zero or more directory levels; a trailing
//!   `**` also matches every file below, like bash and mvdan.
//! - literal segments match verbatim.

use std::collections::BTreeSet;
use std::path::Path;

use crate::ast::Glob;
use crate::filepathext;

/// Expands glob patterns and returns matching files. For generates entries with
/// a `fingerprint` field, only the fingerprint file is returned (used for
/// checksum-based up-to-date detection).
pub fn globs(dir: &str, globs: &[Glob]) -> std::io::Result<Vec<String>> {
    let mut included: BTreeSet<String> = BTreeSet::new();
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    let mut patterns = Vec::new();

    for g in globs {
        if !g.fingerprint.is_empty() {
            let fp = join_pattern(dir, &g.fingerprint);
            if Path::new(&fp).exists() {
                mark(&mut included, &mut excluded, fp, g.negate);
            }
            continue;
        }
        patterns.push((join_pattern(dir, &g.glob), g.negate));
    }

    expand_patterns(&patterns, &mut included, &mut excluded);
    Ok(collect(&included, &excluded))
}

/// Expands glob patterns for cache operations. Unlike [`globs`], it always uses
/// the full glob pattern (ignoring `fingerprint`), so cache archives contain
/// all generated files. When a `fingerprint` is set the fingerprint file is
/// also included (it may not match the glob, e.g. dotfiles are not matched by
/// `**/*`).
pub fn cache_globs(dir: &str, globs: &[Glob]) -> std::io::Result<Vec<String>> {
    let mut included: BTreeSet<String> = BTreeSet::new();
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    let mut patterns = Vec::new();

    for g in globs {
        patterns.push((join_pattern(dir, &g.glob), g.negate));
        if !g.fingerprint.is_empty() && !g.negate {
            let fp = join_pattern(dir, &g.fingerprint);
            if Path::new(&fp).exists() {
                mark(&mut included, &mut excluded, fp, false);
            }
        }
    }

    expand_patterns(&patterns, &mut included, &mut excluded);
    Ok(collect(&included, &excluded))
}

/// Expands joined `(pattern, negate)` entries into the working sets.
///
/// The result is the order-independent `included - excluded`. A negated
/// whole-tree pattern (`<prefix>/**/*`) becomes a [`Pruner`] that keeps positive
/// walks out of covered directories. Other excludes expand normally.
fn expand_patterns(
    patterns: &[(String, bool)],
    included: &mut BTreeSet<String>,
    excluded: &mut BTreeSet<String>,
) {
    let includes: Vec<&str> = patterns
        .iter()
        .filter(|(_, negate)| !negate)
        .map(|(p, _)| p.as_str())
        .collect();

    let mut pruners = Vec::new();
    for (pattern, _) in patterns.iter().filter(|(_, negate)| *negate) {
        match Pruner::new(pattern, &includes) {
            Some(pruner) => {
                // Pruning only affects walks. Subtract covered fingerprint paths
                // inserted directly into `included` to preserve expanded-exclude
                // behavior.
                let segments = rooted_segments(pattern);
                let covered: Vec<String> = included
                    .iter()
                    .filter(|p| match_segments(&segments, &rooted_segments(p)))
                    .cloned()
                    .collect();
                for path in covered {
                    mark(included, excluded, path, true);
                }
                pruners.push(pruner);
            }
            None => {
                for m in expand(pattern, &[]) {
                    mark(included, excluded, m, true);
                }
            }
        }
    }

    for pattern in includes {
        for m in expand(pattern, &pruners) {
            mark(included, excluded, m, false);
        }
    }
}

/// Joins `pattern` onto `dir` the way every expansion in this module does.
fn join_pattern(dir: &str, pattern: &str) -> String {
    filepathext::smart_join(dir, pattern)
        .to_string_lossy()
        .into_owned()
}

/// Expands a single glob pattern rooted at `dir`, returning matching regular
/// files (directories are skipped). Symlinks are included as regular entries.
pub fn glob(dir: &str, pattern: &str) -> std::io::Result<Vec<String>> {
    Ok(expand(&join_pattern(dir, pattern), &[]))
}

/// Expands an already-joined pattern, skipping directories and never entering
/// a directory one of the `pruners` covers.
fn expand(pattern: &str, pruners: &[Pruner]) -> Vec<String> {
    let mut results: BTreeSet<String> = BTreeSet::new();
    for f in expand_fields(pattern, pruners) {
        let meta = match std::fs::symlink_metadata(&f) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        // A symlink to a directory should behave like a regular entry: only
        // skip real directories, matching how the shell lists path names.
        if meta.file_type().is_dir() {
            continue;
        }
        results.insert(f);
    }
    results.into_iter().collect()
}

/// Skips directories covered by a negated `<prefix>/**/*` or `<prefix>/**`
/// pattern while walking includes.
///
/// Pruning preserves `included - excluded`: the exclude would remove every
/// skipped path. Because `**/*` cannot reach dot-prefixed descendants,
/// [`Pruner::new`] rejects pruning when an include can name one below a covered
/// directory.
struct Pruner {
    /// Pattern segments before the trailing `**`, including the walk's root
    /// marker (`""` for absolute patterns and `"."` for relative ones).
    prefix: Vec<String>,
}

impl Pruner {
    fn new(pattern: &str, includes: &[&str]) -> Option<Self> {
        let segments = rooted_segments(pattern);
        let mut prefix = segments.as_slice();
        if let Some((&"*", rest)) = prefix.split_last() {
            prefix = rest;
        }
        let mut stars = 0usize;
        while let Some((&"**", rest)) = prefix.split_last() {
            prefix = rest;
            stars = stars.saturating_add(1);
        }
        if stars == 0 {
            return None;
        }
        // Count non-`**` prefix segments, including the root marker, to find the
        // shallowest covered directory. Never prune the walk root.
        let min_depth = prefix.iter().filter(|s| **s != "**").count();
        if min_depth < 2 {
            return None;
        }

        // A literal dot segment in an include is safe only at a fixed depth
        // within the prefix of every covered directory.
        for include in includes {
            let mut after_double_star = false;
            for (depth, seg) in rooted_segments(include).into_iter().enumerate() {
                if seg == "**" {
                    after_double_star = true;
                } else if depth > 0
                    && seg.starts_with('.')
                    && (after_double_star || depth >= min_depth)
                {
                    return None;
                }
            }
        }

        Some(Self {
            prefix: prefix.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    /// Reports whether `dir` (a path built by [`walk`]) matches the prefix.
    fn covers(&self, dir: &str) -> bool {
        let path: Vec<&str> = dir.split('/').collect();
        match_segments(&self.prefix, &path)
    }
}

/// Splits a pattern into segments with its walk root: `""` for absolute
/// patterns and `"."` for relative patterns.
fn rooted_segments(pattern: &str) -> Vec<&str> {
    let mut segments: Vec<&str> = pattern.split('/').collect();
    if segments.first() != Some(&"") {
        segments.insert(0, ".");
    }
    segments
}

/// Matches pattern segments against a whole path, with the same rules as the
/// walk: `**` spans zero or more non-dot segments, wildcards match within one
/// segment, literals match verbatim.
fn match_segments<S: AsRef<str>>(pat: &[S], path: &[&str]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((seg, rest)) if seg.as_ref() == "**" => {
            if match_segments(rest, path) {
                return true;
            }
            match path.split_first() {
                Some((name, tail)) if !name.starts_with('.') => match_segments(pat, tail),
                _ => false,
            }
        }
        Some((seg, rest)) => match path.split_first() {
            Some((name, tail)) => {
                let seg = seg.as_ref();
                let hit = if has_meta(seg) {
                    matches_segment(seg, name)
                } else {
                    seg == *name
                };
                hit && match_segments(rest, tail)
            }
            None => false,
        },
    }
}

/// Adds or removes `path` from the working sets depending on `negate`.
fn mark(
    included: &mut BTreeSet<String>,
    excluded: &mut BTreeSet<String>,
    path: String,
    negate: bool,
) {
    if negate {
        included.remove(&path);
        excluded.insert(path);
    } else if !excluded.contains(&path) {
        included.insert(path);
    }
}

/// Produces the sorted list of surviving matches.
fn collect(included: &BTreeSet<String>, excluded: &BTreeSet<String>) -> Vec<String> {
    included
        .iter()
        .filter(|p| !excluded.contains(*p))
        .cloned()
        .collect()
}

/// Expands an absolute glob path into the set of matching filesystem paths,
/// reproducing bash's `globstar`/no-`dotglob` pathname expansion. A pattern
/// with no metacharacters yields itself.
fn expand_fields(pattern: &str, pruners: &[Pruner]) -> Vec<String> {
    let segments: Vec<&str> = pattern.split('/').collect();
    // An absolute path begins with an empty first segment; expansion starts
    // from the filesystem root. A relative path starts from ".".
    let (root, rest) = match segments.split_first() {
        Some((&"", rest)) => ("/".to_string(), rest),
        _ => (".".to_string(), segments.as_slice()),
    };

    let mut walker = Walker {
        pruners,
        out: Vec::new(),
    };
    walker.walk(&root, rest);
    walker.out
}

struct Walker<'a> {
    pruners: &'a [Pruner],
    out: Vec<String>,
}

impl Walker<'_> {
    fn enter(&self, dir: &str) -> bool {
        !self.pruners.iter().any(|p| p.covers(dir))
    }

    /// Recursively matches the remaining glob segments against the tree rooted
    /// at `base`, appending matched paths to `self.out`.
    fn walk(&mut self, base: &str, segments: &[&str]) {
        let Some((segment, rest)) = segments.split_first() else {
            self.out.push(base.to_string());
            return;
        };

        if *segment == "**" {
            // `**` matches zero or more directory levels. Match here (zero
            // levels) then descend into every subdirectory.
            self.walk(base, rest);
            self.descend_double_star(base, rest);
            return;
        }

        if !has_meta(segment) {
            let next = join_seg(base, segment);
            if rest.is_empty() {
                if Path::new(&next).exists() {
                    self.out.push(next);
                }
            } else if Path::new(&next).is_dir() && self.enter(&next) {
                self.walk(&next, rest);
            }
            return;
        }

        for child in list_dir(base) {
            let name = file_name(&child);
            if matches_segment(segment, &name) {
                if rest.is_empty() {
                    self.out.push(child);
                } else if Path::new(&child).is_dir() && self.enter(&child) {
                    self.walk(&child, rest);
                }
            }
        }
    }

    /// Continues a `**` match: `base` is a directory already matched by `**`;
    /// apply the remaining segments here and keep descending.
    fn walk_double_star(&mut self, base: &str, rest: &[&str]) {
        self.walk(base, rest);
        self.descend_double_star(base, rest);
    }

    /// Descends into every subdirectory for `**`. When `**` is trailing, it
    /// also emits each file it passes, making `a/**` equivalent to `a/**/*` as
    /// in bash and mvdan.
    fn descend_double_star(&mut self, base: &str, rest: &[&str]) {
        for child in list_dir(base) {
            if Path::new(&child).is_dir() {
                if self.enter(&child) {
                    self.walk_double_star(&child, rest);
                }
            } else if rest.is_empty() {
                self.out.push(child);
            }
        }
    }
}

/// Lists directory entries. With `dotglob` disabled bash hides names beginning
/// with a dot; `**` traversal also never crosses into dot directories.
fn list_dir(dir: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let Ok(read) = std::fs::read_dir(dir) else {
        return entries;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        entries.push(join_seg(dir, &name));
    }
    entries
}

/// Joins a base directory with a single path segment.
fn join_seg(base: &str, seg: &str) -> String {
    if base == "/" {
        format!("/{seg}")
    } else {
        format!("{base}/{seg}")
    }
}

/// Extracts the final path component.
fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Reports whether a segment contains glob metacharacters.
fn has_meta(segment: &str) -> bool {
    segment.contains(['*', '?', '['])
}

/// Matches a single-segment glob (`*`, `?`, `[...]`) against a file name. A
/// leading dot in `name` is never matched by a wildcard (dotglob off); callers
/// already exclude dotfiles, but this keeps the matcher self-consistent.
fn matches_segment(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    let pat: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = name.chars().collect();
    glob_match(&pat, &text)
}

/// Recursive glob matcher for a single path segment.
fn glob_match(pat: &[char], text: &[char]) -> bool {
    match pat.split_first() {
        None => text.is_empty(),
        Some(('*', rest)) => {
            // `*` matches any run of characters within the segment.
            if glob_match(rest, text) {
                return true;
            }
            match text.split_first() {
                Some((_, tail)) => glob_match(pat, tail),
                None => false,
            }
        }
        Some(('?', rest)) => match text.split_first() {
            Some((_, tail)) => glob_match(rest, tail),
            None => false,
        },
        Some(('[', rest)) => match text.split_first() {
            Some((c, tail)) => match match_bracket(rest, *c) {
                Some(after) => glob_match(after, tail),
                None => false,
            },
            None => false,
        },
        Some((p, rest)) => match text.split_first() {
            Some((c, tail)) if c == p => glob_match(rest, tail),
            _ => false,
        },
    }
}

/// Matches a bracket expression `[...]` against `c`. `pat` starts just after
/// the opening `[`. Returns the pattern slice after the closing `]` on a match.
fn match_bracket(pat: &[char], c: char) -> Option<&[char]> {
    let (negated, mut i) = match pat.first() {
        Some('!') | Some('^') => (true, 1usize),
        _ => (false, 0usize),
    };

    let mut matched = false;
    while let Some(&ch) = pat.get(i) {
        if ch == ']' && i > usize::from(negated) {
            let after = pat.get(i.saturating_add(1)..)?;
            return if matched != negated {
                Some(after)
            } else {
                None
            };
        }
        // Range `a-z`.
        let range_end = pat.get(i.saturating_add(2)).filter(|&&e| e != ']').copied();
        if let (Some(&'-'), Some(end)) = (pat.get(i.saturating_add(1)), range_end) {
            if ch <= c && c <= end {
                matched = true;
            }
            i = i.saturating_add(3);
            continue;
        }
        if ch == c {
            matched = true;
        }
        i = i.saturating_add(1);
    }
    // No closing bracket: not a valid bracket expression.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Glob;
    use crate::fingerprint::testutil::{setup_node_modules, tmp, write_file};

    fn g(pattern: &str) -> Glob {
        Glob {
            glob: pattern.to_string(),
            ..Default::default()
        }
    }

    fn g_fp(pattern: &str, fingerprint: &str) -> Glob {
        Glob {
            glob: pattern.to_string(),
            fingerprint: fingerprint.to_string(),
            ..Default::default()
        }
    }

    fn g_neg(pattern: &str) -> Glob {
        Glob {
            glob: pattern.to_string(),
            negate: true,
            ..Default::default()
        }
    }

    fn join(dir: &str, rel: &str) -> String {
        format!("{dir}/{rel}")
    }

    #[test]
    fn globs_simple_glob() {
        let dir = setup_node_modules();
        let files = globs(&dir, &[g("node_modules/.yarn-state.yml")]).unwrap();
        assert_eq!(files, vec![join(&dir, "node_modules/.yarn-state.yml")]);
    }

    #[test]
    fn globs_with_fingerprint_returns_only_fingerprint_file() {
        let dir = setup_node_modules();
        let files = globs(
            &dir,
            &[g_fp("node_modules/**/*", "node_modules/.yarn-state.yml")],
        )
        .unwrap();
        assert_eq!(files, vec![join(&dir, "node_modules/.yarn-state.yml")]);
    }

    #[test]
    fn globs_with_fingerprint_missing_file() {
        let dir = tmp();
        let files = globs(
            &dir,
            &[g_fp("node_modules/**/*", "node_modules/.yarn-state.yml")],
        )
        .unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn cache_globs_simple_glob() {
        let dir = setup_node_modules();
        let files = cache_globs(&dir, &[g("node_modules/.yarn-state.yml")]).unwrap();
        assert_eq!(files, vec![join(&dir, "node_modules/.yarn-state.yml")]);
    }

    #[test]
    fn cache_globs_with_fingerprint_returns_glob_files_and_fingerprint_file() {
        let dir = setup_node_modules();
        let files = cache_globs(
            &dir,
            &[g_fp("node_modules/**/*", "node_modules/.yarn-state.yml")],
        )
        .unwrap();
        assert_eq!(
            files,
            vec![
                join(&dir, "node_modules/.yarn-state.yml"),
                join(&dir, "node_modules/react/index.js"),
                join(&dir, "node_modules/vite/bin/vite.js"),
            ]
        );
    }

    #[test]
    fn cache_globs_with_exclude() {
        let dir = setup_node_modules();
        let files = cache_globs(
            &dir,
            &[
                g_fp("node_modules/**/*", "node_modules/.yarn-state.yml"),
                g_neg("node_modules/react/**/*"),
            ],
        )
        .unwrap();
        assert_eq!(
            files,
            vec![
                join(&dir, "node_modules/.yarn-state.yml"),
                join(&dir, "node_modules/vite/bin/vite.js"),
            ]
        );
    }

    #[test]
    fn exclude_prunes_injected_fingerprint_file() {
        let dir = tmp();
        write_file(&dir, "build/gen/fp.txt", "f");
        write_file(&dir, "src/a.rs", "a");
        // Pruning must still exclude fingerprint files inserted without walking.
        let patterns = [
            g("**/*.rs"),
            g_fp("build/**/*", "build/gen/fp.txt"),
            g_neg("build/gen/**/*"),
        ];
        let files = globs(&dir, &patterns).unwrap();
        assert_eq!(files, vec![join(&dir, "src/a.rs")]);

        // A trailing `**` exclude must also remove the injected fingerprint.
        let bare = [
            g("**/*.rs"),
            g_fp("build/**", "build/gen/fp.txt"),
            g_neg("build/gen/**"),
        ];
        assert_eq!(globs(&dir, &bare).unwrap(), vec![join(&dir, "src/a.rs")]);
    }

    #[test]
    fn cache_globs_exclude_drops_injected_fingerprint_file() {
        let dir = tmp();
        write_file(&dir, "build/gen/fp.txt", "f");
        write_file(&dir, "build/out.js", "o");
        let patterns = [
            g_fp("build/**/*", "build/gen/fp.txt"),
            g_neg("build/gen/**/*"),
        ];
        let files = cache_globs(&dir, &patterns).unwrap();
        assert_eq!(files, vec![join(&dir, "build/out.js")]);
    }

    #[test]
    fn globs_mixed_entries() {
        let dir = tmp();
        for (rel, content) in [
            ("build/app.js", "app"),
            ("build/app.css", "css"),
            ("node_modules/.yarn-state.yml", "state"),
            ("node_modules/pkg/index.js", "pkg"),
        ] {
            write_file(&dir, rel, content);
        }

        let patterns = [
            g("build/**/*"),
            g_fp("node_modules/**/*", "node_modules/.yarn-state.yml"),
        ];

        let fingerprint_files = globs(&dir, &patterns).unwrap();
        assert_eq!(
            fingerprint_files,
            vec![
                join(&dir, "build/app.css"),
                join(&dir, "build/app.js"),
                join(&dir, "node_modules/.yarn-state.yml"),
            ]
        );

        let cache_files = cache_globs(&dir, &patterns).unwrap();
        assert_eq!(
            cache_files,
            vec![
                join(&dir, "build/app.css"),
                join(&dir, "build/app.js"),
                join(&dir, "node_modules/.yarn-state.yml"),
                join(&dir, "node_modules/pkg/index.js"),
            ]
        );
    }

    #[test]
    fn double_star_matches_direct_children() {
        let dir = tmp();
        write_file(&dir, "build/app.js", "a");
        write_file(&dir, "build/sub/x.js", "b");
        let files = glob(&dir, "build/**/*").unwrap();
        assert_eq!(
            files,
            vec![join(&dir, "build/app.js"), join(&dir, "build/sub/x.js")]
        );
    }

    #[test]
    fn trailing_double_star_matches_files() {
        let dir = tmp();
        write_file(&dir, "target/a", "a");
        write_file(&dir, "target/sub/b", "b");
        write_file(&dir, "target/.hidden/c", "c");
        write_file(&dir, "target/sub/.d", "d");
        write_file(&dir, "other/e", "e");
        let expected = vec![join(&dir, "target/a"), join(&dir, "target/sub/b")];
        assert_eq!(glob(&dir, "target/**").unwrap(), expected);
        assert_eq!(glob(&dir, "target/**/*").unwrap(), expected);
        assert_eq!(
            glob(&dir, "target/*").unwrap(),
            vec![join(&dir, "target/a")]
        );

        let files = globs(&dir, &[g("**/*"), g_neg("target/**")]).unwrap();
        assert_eq!(files, vec![join(&dir, "other/e")]);
    }

    #[test]
    fn leading_double_star_matches_root() {
        let dir = tmp();
        write_file(&dir, "a.go", "x");
        write_file(&dir, "src/b.go", "y");
        write_file(&dir, "src/c.txt", "z");
        let files = glob(&dir, "**/*.go").unwrap();
        assert_eq!(files, vec![join(&dir, "a.go"), join(&dir, "src/b.go")]);
    }

    #[test]
    fn bracket_and_question() {
        let dir = tmp();
        write_file(&dir, "a1.txt", "1");
        write_file(&dir, "a2.txt", "2");
        write_file(&dir, "b1.txt", "3");
        let files = glob(&dir, "[ab]?.txt").unwrap();
        assert_eq!(
            files,
            vec![
                join(&dir, "a1.txt"),
                join(&dir, "a2.txt"),
                join(&dir, "b1.txt"),
            ]
        );
    }

    #[test]
    fn exclude_covering_directory_prunes_walk() {
        let dir = tmp();
        write_file(&dir, "src/a.rs", "a");
        write_file(&dir, "src/target/debug/b.rs", "b");
        write_file(&dir, "target/release/c.rs", "c");
        write_file(&dir, "target-x/d.rs", "d");
        write_file(&dir, "vendor/e.rs", "e");
        let patterns = [
            g("**/*.rs"),
            g_neg("target*/**/*"),
            g_neg("**/target/**/**/*"),
        ];
        let joined: Vec<String> = patterns
            .iter()
            .filter(|g| g.negate)
            .map(|g| join(&dir, &g.glob))
            .collect();
        let includes = [join(&dir, "**/*.rs")];
        let include_refs: Vec<&str> = includes.iter().map(String::as_str).collect();
        for p in &joined {
            assert!(Pruner::new(p, &include_refs).is_some(), "{p} should prune");
        }

        let files = globs(&dir, &patterns).unwrap();
        assert_eq!(
            files,
            vec![join(&dir, "src/a.rs"), join(&dir, "vendor/e.rs")]
        );
        // Includes and excludes remain order-independent.
        let reversed: Vec<Glob> = patterns.iter().rev().cloned().collect();
        assert_eq!(globs(&dir, &reversed).unwrap(), files);
    }

    #[test]
    fn exclude_prune_keeps_literal_dot_paths() {
        let dir = tmp();
        write_file(&dir, "target/a.o", "a");
        write_file(&dir, "target/.keep/b.o", "b");
        let files = globs(
            &dir,
            &[
                g("target/**/*"),
                g("target/.keep/*.o"),
                g_neg("target/**/*"),
            ],
        )
        .unwrap();
        // `**/*` cannot reach the literal dot path, so pruning is unsafe.
        assert_eq!(files, vec![join(&dir, "target/.keep/b.o")]);
        let includes = [join(&dir, "target/.keep/*.o")];
        let include_refs: Vec<&str> = includes.iter().map(String::as_str).collect();
        assert!(Pruner::new(&join(&dir, "target/**/*"), &include_refs).is_none());
    }

    #[test]
    fn pruner_classification() {
        let inc = ["/w/**/*.rs"];
        assert!(Pruner::new("/w/target/**/*", &inc).is_some());
        assert!(Pruner::new("/w/target/**/**/*", &inc).is_some());
        assert!(Pruner::new("/w/*/target/**/*", &inc).is_some());
        assert!(Pruner::new("/w/**/target/**/*", &inc).is_some());
        // These patterns do not cover a whole directory.
        assert!(Pruner::new("/w/target/**/*.o", &inc).is_none());
        assert!(Pruner::new("/w/target/*", &inc).is_none());
        // Both forms cover the same files.
        assert!(Pruner::new("/w/target/**", &inc).is_some());
        assert!(Pruner::new("/w/target/**/**", &inc).is_some());
        // This covers each child directory at depth 2 or greater under `target`.
        assert!(Pruner::new("/w/target/**/*/**/*", &inc).is_some());
        // These would prune the walk root.
        assert!(Pruner::new("/**/*", &inc).is_none());
        assert!(Pruner::new("**/*", &["**/*.rs"]).is_none());
        // A fixed dot segment above covered directories is safe. One below a
        // covered directory or after `**` is not.
        assert!(
            Pruner::new(
                "/home/u/.local/w/target/**/*",
                &["/home/u/.local/w/**/*.rs"]
            )
            .is_some()
        );
        assert!(Pruner::new("/w/target/**/*", &["/w/target/.cache/*"]).is_none());
        assert!(Pruner::new("/w/target/**/*", &["/w/**/.cache/*"]).is_none());
        // `**` cannot span `.git`, matching the walk.
        assert!(Pruner::new("/w/**/target/**/*", &["/w/.git/**/*"]).is_some());
        assert!(Pruner::new("/w/**/target/**/*", &["/w/**/.git/**/*"]).is_none());
    }

    #[test]
    fn pruner_covers() {
        let p = Pruner::new("/w/**/target*/**/*", &[]).unwrap();
        assert!(p.covers("/w/target"));
        assert!(p.covers("/w/a/b/target-x"));
        assert!(!p.covers("/w/a/.hidden/target"));
        assert!(!p.covers("/w/a/target/sub"));
        assert!(!p.covers("/w/a/starget"));
        assert!(!p.covers("/w"));
        let rel = Pruner::new("target/**/*", &[]).unwrap();
        assert!(rel.covers("./target"));
        assert!(!rel.covers("./src/target"));
    }

    #[test]
    fn wildcard_skips_dotfiles() {
        let dir = tmp();
        write_file(&dir, "visible.txt", "v");
        write_file(&dir, ".hidden", "h");
        let files = glob(&dir, "*").unwrap();
        assert_eq!(files, vec![join(&dir, "visible.txt")]);
    }
}
