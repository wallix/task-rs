//! The walker as it was before directory listings carried entry types, with
//! today's exclusion semantics (exclusions walked with `dotglob`, named paths
//! exempt) and no pruning: a test-only reference the current one must agree
//! with on every tree.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::Path;

use super::{
    Options, Pattern, Pruner, collect, file_name, folded_groups, has_meta, join_path, join_pattern,
    join_seg, mark, matches_segment, own, trailing_slash,
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
    let mut named = Vec::new();

    for g in globs {
        if !g.fingerprint.is_empty() {
            let fp = join_path(dir, &g.fingerprint);
            if Path::new(&fp).exists() {
                if g.negate || !g.is_named() {
                    mark(&mut included, &mut excluded, fp, g.negate);
                } else {
                    named.push(fp);
                }
            }
            continue;
        }
        let pattern = join_pattern(dir, &g.glob);
        if g.is_named() {
            named.extend(expand(&pattern, &[], Options::SHELL));
            continue;
        }
        patterns.push(Pattern::of(g, pattern));
    }

    expand_patterns(
        &patterns,
        named,
        &mut included,
        &mut excluded,
        Options::SHELL,
    );
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
    let mut named = Vec::new();

    for g in globs {
        let pattern = join_pattern(dir, &g.glob);
        if g.is_named() && !g.has_wildcards() {
            named.extend(expand(&pattern, &[], Options::TREE));
        } else {
            patterns.push(Pattern::of(g, pattern));
        }
        if !g.fingerprint.is_empty() && !g.negate {
            let fp = join_path(dir, &g.fingerprint);
            if Path::new(&fp).exists() {
                if g.is_named() {
                    named.push(fp);
                } else {
                    mark(&mut included, &mut excluded, fp, false);
                }
            }
        }
    }

    expand_patterns(
        &patterns,
        named,
        &mut included,
        &mut excluded,
        Options::TREE,
    );
    collect(&included, &excluded)
}

/// Every exclusion expanded by a walk of its own, with `dotglob` on, and
/// nothing pruned: a reference independent of the current matcher. A path
/// named explicitly (an include without wildcards, a fingerprint) is then
/// kept unless an exclusion without wildcards, or a negated fingerprint,
/// names it too.
fn expand_patterns(
    patterns: &[Pattern],
    named: Vec<String>,
    included: &mut BTreeSet<String>,
    excluded: &mut BTreeSet<String>,
    opts: Options,
) {
    let exclusion = Options {
        dotglob: true,
        ..opts
    };
    let mut fixed = excluded.clone();
    for p in patterns {
        let walk = if p.negate { exclusion } else { opts };
        let matches = expand(&p.path, &[], walk);
        if p.negate && p.literal {
            fixed.extend(matches.iter().cloned());
        }
        for m in matches {
            mark(included, excluded, m, p.negate);
        }
    }
    for m in named {
        if fixed.contains(&m) {
            mark(included, excluded, m, true);
        } else {
            excluded.remove(&m);
            included.insert(m);
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

        // `a//b` is `a/b`; a trailing `/` lists `base` itself.
        if segment.is_empty() {
            self.walk(base, rest);
            return;
        }

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
                } else if trailing_slash(rest) {
                    self.slashed(child);
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
            } else if trailing_slash(rest) {
                self.slashed(child);
            }
        }
    }

    /// A link to a directory, before a trailing `/`, is the entry.
    fn slashed(&mut self, child: String) {
        let link = std::fs::symlink_metadata(&child).is_ok_and(|m| m.file_type().is_symlink());
        if link && Path::new(&child).is_dir() {
            self.out.push(child);
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
