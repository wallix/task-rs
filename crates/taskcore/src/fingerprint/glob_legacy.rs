//! The walker as it was before directory listings carried entry types: a
//! test-only reference the current one must agree with on every tree.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::Path;

use super::{
    Options, Pruner, collect, file_name, folded_groups, has_meta, join_pattern, join_seg, mark,
    match_segments, matches_segment, own, rooted_segments,
};
use crate::ast::Glob;

pub(super) fn globs(dir: &str, globs: &[Glob]) -> std::io::Result<Vec<String>> {
    let mut folded = BTreeSet::new();
    for group in folded_groups(globs) {
        folded.extend(shell_globs(dir, group, BTreeSet::new()));
    }
    Ok(shell_globs(dir, own(globs), folded))
}

fn shell_globs<'a>(
    dir: &str,
    globs: impl Iterator<Item = &'a Glob>,
    mut included: BTreeSet<String>,
) -> Vec<String> {
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

    expand_patterns(&patterns, &mut included, &mut excluded, Options::SHELL);
    collect(&included, &excluded)
}

pub(super) fn cache_globs(dir: &str, globs: &[Glob]) -> std::io::Result<Vec<String>> {
    let mut folded = BTreeSet::new();
    for group in folded_groups(globs) {
        folded.extend(tree_globs(dir, group, BTreeSet::new()));
    }
    Ok(tree_globs(dir, own(globs), folded))
}

fn tree_globs<'a>(
    dir: &str,
    globs: impl Iterator<Item = &'a Glob>,
    mut included: BTreeSet<String>,
) -> Vec<String> {
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

    expand_patterns(&patterns, &mut included, &mut excluded, Options::TREE);
    collect(&included, &excluded)
}

fn expand_patterns(
    patterns: &[(String, bool)],
    included: &mut BTreeSet<String>,
    excluded: &mut BTreeSet<String>,
    opts: Options,
) {
    let includes: Vec<&str> = patterns
        .iter()
        .filter(|(_, negate)| !negate)
        .map(|(p, _)| p.as_str())
        .collect();
    let fingerprints: Vec<String> = included
        .iter()
        .filter(|_| !opts.follow_symlinks)
        .cloned()
        .collect();
    let pruning_includes: Vec<&str> = includes
        .iter()
        .copied()
        .chain(fingerprints.iter().map(String::as_str))
        .collect();

    let mut pruners = Vec::new();
    for (pattern, _) in patterns.iter().filter(|(_, negate)| *negate) {
        match Pruner::new(pattern, &pruning_includes, opts) {
            Some(pruner) => {
                // Pruning only affects walks. Subtract covered fingerprint paths
                // inserted directly into `included` to preserve expanded-exclude
                // behavior.
                let segments = rooted_segments(pattern);
                let covered: Vec<String> = included
                    .iter()
                    .filter(|p| match_segments(&segments, &rooted_segments(p), opts.dotglob))
                    .cloned()
                    .collect();
                for path in covered {
                    mark(included, excluded, path, true);
                }
                pruners.push(pruner);
            }
            None => {
                for m in expand(pattern, &[], opts) {
                    mark(included, excluded, m, true);
                }
            }
        }
    }

    for pattern in includes {
        for m in expand(pattern, &pruners, opts) {
            mark(included, excluded, m, false);
        }
    }
}

fn expand(pattern: &str, pruners: &[Pruner], opts: Options) -> Vec<String> {
    let mut results: BTreeSet<String> = BTreeSet::new();
    for f in expand_fields(pattern, pruners, opts) {
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

fn expand_fields(pattern: &str, pruners: &[Pruner], opts: Options) -> Vec<String> {
    let segments: Vec<&str> = pattern.split('/').collect();
    // An absolute path begins with an empty first segment; expansion starts
    // from the filesystem root. A relative path starts from ".".
    let (root, rest) = match segments.split_first() {
        Some((&"", rest)) => ("/".to_string(), rest),
        _ => (".".to_string(), segments.as_slice()),
    };

    let mut walker = Walker {
        pruners,
        opts,
        out: Vec::new(),
    };
    walker.walk(&root, rest);
    walker.out
}

struct Walker<'a> {
    pruners: &'a [Pruner],
    opts: Options,
    out: Vec<String>,
}

impl Walker<'_> {
    fn enter(&self, dir: &str) -> bool {
        !self.pruners.iter().any(|p| p.covers(dir))
    }

    /// Reports whether a wildcard-matched `child` is a directory to descend
    /// into: a symlink to one counts only when following symlinks.
    fn descends(&self, child: &str) -> bool {
        if self.opts.follow_symlinks {
            Path::new(child).is_dir()
        } else {
            std::fs::symlink_metadata(child)
                .map(|m| m.file_type().is_dir())
                .unwrap_or(false)
        }
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

        for child in list_dir(base, self.opts.dotglob) {
            let name = file_name(&child);
            if matches_segment(segment, &name, self.opts.dotglob) {
                if rest.is_empty() {
                    self.out.push(child);
                } else if self.descends(&child) && self.enter(&child) {
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
        for child in list_dir(base, self.opts.dotglob) {
            if self.descends(&child) {
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
fn list_dir(dir: &str, dotglob: bool) -> Vec<String> {
    let mut entries = Vec::new();
    // A cancelled reading ends here: its result is discarded.
    if super::super::offload::cancelled() {
        return entries;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return entries;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !dotglob && name.starts_with('.') {
            continue;
        }
        entries.push(join_seg(dir, &name));
    }
    entries
}
