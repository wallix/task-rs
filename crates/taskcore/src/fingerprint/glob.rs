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
//!
//! Cache collection ([`cache_globs`]) walks the same patterns in *tree* mode
//! instead: wildcards and `**` also match dot entries, and `**` does not
//! descend through a symlink to a directory — the link itself is the entry.
//! A generated tree is archived whole and restored as it was, whatever the
//! shell would have listed.

use std::collections::BTreeSet;
use std::path::Path;

use crate::ast::Glob;
use crate::filepathext;

/// Expands glob patterns and returns matching files. For generates entries with
/// a `fingerprint` field, only the fingerprint file is returned (used for
/// checksum-based up-to-date detection).
///
/// Entries folded in from other tasks ([`Glob::group`] above 0) are evaluated
/// group by group, so a folded task's exclusions remove only its own matches;
/// the task's own exclusions then apply to the whole result.
pub fn globs(dir: &str, globs: &[Glob]) -> std::io::Result<Vec<String>> {
    let mut folded = BTreeSet::new();
    for group in folded_groups(globs) {
        folded.extend(shell_globs(dir, group, BTreeSet::new()));
    }
    Ok(shell_globs(dir, own(globs), folded))
}

/// [`globs`] over a single group, starting from the `included` matches.
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

/// Expands glob patterns for cache operations. Unlike [`globs`], it always uses
/// the full glob pattern (ignoring `fingerprint`), so cache archives contain
/// all generated files, and it walks in tree mode ([`Options::TREE`]): hidden
/// entries are included and symlinked directories are stored as the link
/// rather than traversed. When a `fingerprint` is set the fingerprint file is
/// also included (it may not match the glob). Groups are evaluated as by
/// [`globs`].
pub fn cache_globs(dir: &str, globs: &[Glob]) -> std::io::Result<Vec<String>> {
    let mut folded = BTreeSet::new();
    for group in folded_groups(globs) {
        folded.extend(tree_globs(dir, group, BTreeSet::new()));
    }
    Ok(tree_globs(dir, own(globs), folded))
}

/// Match paths against [`cache_globs`]'s `globs` without walking the tree,
/// so a restore can reject entries outside the task's generates before writing.
/// Matches every path the walk lists, but may also match paths below directory
/// symlinks that a wildcard would not cross. A restore rejects those separately.
pub struct CacheMatcher {
    own: MatchGroup,
    folded: Vec<MatchGroup>,
}

/// One group's entries, joined and split as the walk splits them.
struct MatchGroup {
    includes: Vec<Vec<String>>,
    excludes: Vec<Vec<String>>,
    fingerprints: Vec<Vec<String>>,
}

/// `path`'s segments with the walk's root marker, dropping the `.` and empty
/// segments a pattern may carry (an absolute one is not cleaned when joined):
/// the walk lists such paths, and saving normalises them away.
fn normalized_segments(path: &str) -> Vec<String> {
    rooted_segments(path)
        .into_iter()
        .enumerate()
        .filter(|(i, s)| *i == 0 || !matches!(*s, "" | "."))
        .map(|(_, s)| s.to_string())
        .collect()
}

impl MatchGroup {
    fn new<'a>(dir: &str, globs: impl Iterator<Item = &'a Glob>) -> Self {
        let mut group = Self {
            includes: Vec::new(),
            excludes: Vec::new(),
            fingerprints: Vec::new(),
        };
        for g in globs {
            let segments = normalized_segments(&join_pattern(dir, &g.glob));
            if g.negate {
                group.excludes.push(segments);
            } else {
                group.includes.push(segments);
                if !g.fingerprint.is_empty() {
                    group
                        .fingerprints
                        .push(normalized_segments(&join_pattern(dir, &g.fingerprint)));
                }
            }
        }
        group
    }

    fn excluded(&self, segments: &[&str]) -> bool {
        self.excludes
            .iter()
            .any(|e| match_segments(e, segments, Options::TREE.dotglob))
    }

    fn lists(&self, segments: &[&str]) -> bool {
        (self.fingerprints.iter().any(|f| f.iter().eq(segments))
            || self
                .includes
                .iter()
                .any(|i| match_segments(i, segments, Options::TREE.dotglob)))
            && !self.excluded(segments)
    }
}

impl CacheMatcher {
    /// The matcher for `globs`, relative to the task directory `dir`.
    pub fn new(dir: &str, globs: &[Glob]) -> Self {
        Self {
            own: MatchGroup::new(dir, own(globs)),
            folded: folded_groups(globs)
                .map(|group| MatchGroup::new(dir, group))
                .collect(),
        }
    }

    /// Whether `rel`, a slash-separated path relative to `root`, is one the
    /// walk can list. A folded group's exclusions apply to its own entries;
    /// the task's own apply to all.
    pub fn matches(&self, root: &str, rel: &str) -> bool {
        let path = normalized_segments(&join_pattern(root, rel));
        let segments: Vec<&str> = path.iter().map(String::as_str).collect();
        (self.own.lists(&segments) || self.folded.iter().any(|g| g.lists(&segments)))
            && !self.own.excluded(&segments)
    }
}

/// [`cache_globs`] over a single group, starting from the `included` matches.
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

/// The task's own entries (group 0).
fn own(globs: &[Glob]) -> impl Iterator<Item = &Glob> {
    globs.iter().filter(|g| g.group == 0)
}

/// The entries folded in from other tasks, one iterator per group, in group
/// order.
fn folded_groups(globs: &[Glob]) -> impl Iterator<Item = impl Iterator<Item = &Glob>> {
    let groups: BTreeSet<u32> = globs.iter().map(|g| g.group).filter(|&g| g != 0).collect();
    groups
        .into_iter()
        .map(move |group| globs.iter().filter(move |g| g.group == group))
}

/// How a walk treats the entries a shell listing would hide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Options {
    /// Wildcards match dot-prefixed names and `**` crosses dot directories
    /// (bash `dotglob`).
    dotglob: bool,
    /// Wildcards and `**` descend through a symlink to a directory. Literal
    /// segments always resolve through symlinks, like the shell.
    follow_symlinks: bool,
}

impl Options {
    /// Bash pathname expansion with `globstar` on and `dotglob` off: what the
    /// Go implementation lists, and what the fingerprint checksums cover.
    const SHELL: Self = Self {
        dotglob: false,
        follow_symlinks: true,
    };
    /// The whole tree under a pattern, as an archive has to capture it.
    const TREE: Self = Self {
        dotglob: true,
        follow_symlinks: false,
    };
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

/// Joins `pattern` onto `dir` the way every expansion in this module does.
fn join_pattern(dir: &str, pattern: &str) -> String {
    filepathext::smart_join(dir, pattern)
        .to_string_lossy()
        .into_owned()
}

/// Expands a single glob pattern rooted at `dir`, returning matching regular
/// files (directories are skipped). Symlinks are included as regular entries.
pub fn glob(dir: &str, pattern: &str) -> std::io::Result<Vec<String>> {
    Ok(expand(&join_pattern(dir, pattern), &[], Options::SHELL))
}

/// Expands an already-joined pattern, skipping directories and never entering
/// a directory one of the `pruners` covers.
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

/// Skips directories covered by a negated `<prefix>/**/*` or `<prefix>/**`
/// pattern while walking includes.
///
/// Pruning preserves `included - excluded`: the exclude would remove every
/// skipped path. Without `dotglob`, `**/*` cannot reach dot-prefixed
/// descendants, so [`Pruner::new`] rejects pruning when an include can name one
/// below a covered directory.
struct Pruner {
    /// Pattern segments before the trailing `**`, including the walk's root
    /// marker (`""` for absolute patterns and `"."` for relative ones).
    prefix: Vec<String>,
    /// Whether `**` in the prefix spans dot directories.
    dotglob: bool,
}

impl Pruner {
    fn new(pattern: &str, includes: &[&str], opts: Options) -> Option<Self> {
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

        if !opts.follow_symlinks {
            // A trailing `**` also emits its base. A literal base may be a
            // symlink, which must be subtracted as an entry, not just pruned
            // as a directory. Wildcard bases never traverse symlinks here.
            if segments.last() == Some(&"**") && prefix.last().is_some_and(|s| !has_meta(s)) {
                return None;
            }
            for include in includes {
                let segments = rooted_segments(include);
                let Some((last, directories)) = segments.split_last() else {
                    continue;
                };
                // Pruning the base would also suppress an included symlink
                // emitted by a trailing `**`.
                if *last == "**"
                    && directories
                        .iter()
                        .rev()
                        .find(|s| **s != "**")
                        .is_some_and(|s| !has_meta(s))
                {
                    return None;
                }
                // Beyond a shared literal prefix, a literal include can cross
                // a directory symlink that the exclusion's wildcard skips.
                // Expand that exclusion normally to preserve set subtraction.
                let shared = directories
                    .iter()
                    .zip(prefix)
                    .take_while(|(a, b)| a == b && !has_meta(a))
                    .count();
                if directories.iter().skip(shared).any(|s| !has_meta(s)) {
                    return None;
                }
            }
        }

        // A literal dot segment in an include is safe only at a fixed depth
        // within the prefix of every covered directory. With `dotglob` the
        // exclude reaches every descendant, so any include is safe.
        for include in includes.iter().filter(|_| !opts.dotglob) {
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
            dotglob: opts.dotglob,
        })
    }

    /// Reports whether `dir` (a path built by [`Walker::walk`]) matches the
    /// prefix.
    fn covers(&self, dir: &str) -> bool {
        let path: Vec<&str> = dir.split('/').collect();
        match_segments(&self.prefix, &path, self.dotglob)
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
/// walk: `**` spans zero or more segments (non-dot unless `dotglob`),
/// wildcards match within one segment, literals match verbatim. Runs in
/// pattern × path steps (see [`star_match`]): restores match names an
/// archive chooses against these patterns.
fn match_segments<S: AsRef<str>>(pat: &[S], path: &[&str], dotglob: bool) -> bool {
    star_match(
        pat,
        path,
        |seg| seg.as_ref() == "**",
        |name| dotglob || !name.starts_with('.'),
        |seg, name| {
            let seg = seg.as_ref();
            if has_meta(seg) {
                matches_segment(seg, name, dotglob)
            } else {
                seg == *name
            }
        },
    )
}

/// Whether `toks` matches all of `text`, where a token for which `is_star`
/// holds spans zero or more items (each passing `star_ok`) and any other
/// matches exactly one item it `hit`s. Dynamic programming over suffixes, so
/// time is `toks × text` whatever the stars: backtracking is exponential in
/// them.
fn star_match<T, U>(
    toks: &[T],
    text: &[U],
    is_star: impl Fn(&T) -> bool,
    star_ok: impl Fn(&U) -> bool,
    hit: impl Fn(&T, &U) -> bool,
) -> bool {
    // `next[j]`: whether the tokens after the current one match `text[j..]`.
    let mut next = vec![false; text.len().saturating_add(1)];
    if let Some(end) = next.last_mut() {
        *end = true;
    }
    for tok in toks.iter().rev() {
        let mut cur = vec![false; next.len()];
        if is_star(tok) {
            // Fill from the end: `cur[j]` needs `cur[j + 1]`.
            let mut after = false;
            for j in (0..cur.len()).rev() {
                let spans = text.get(j).is_some_and(&star_ok) && after;
                let here = next.get(j).copied().unwrap_or(false) || spans;
                if let Some(c) = cur.get_mut(j) {
                    *c = here;
                }
                after = here;
            }
        } else {
            for (j, item) in text.iter().enumerate() {
                let rest = next.get(j.saturating_add(1)).copied().unwrap_or(false);
                if let Some(c) = cur.get_mut(j) {
                    *c = rest && hit(tok, item);
                }
            }
        }
        next = cur;
    }
    next.first().copied().unwrap_or(false)
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
/// reproducing bash's `globstar` pathname expansion under `opts`. A pattern
/// with no metacharacters yields itself.
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
    if super::offload::cancelled() {
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

/// Matches a single-segment glob (`*`, `?`, `[...]`) against a file name. With
/// `dotglob` off a leading dot in `name` is never matched by a wildcard;
/// callers already exclude dotfiles, but this keeps the matcher
/// self-consistent.
fn matches_segment(pattern: &str, name: &str, dotglob: bool) -> bool {
    if !dotglob && name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    let pat: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = name.chars().collect();
    glob_match(&pat, &text)
}

/// One element of a single-segment glob.
enum Tok<'a> {
    /// `*`: any run of characters.
    Star,
    /// `?`: any one character.
    Any,
    /// `[...]`, the slice just after its `[`.
    Class(&'a [char]),
    /// An unterminated `[`, which matches nothing.
    Never,
    Lit(char),
}

/// Split a single-segment glob into [`Tok`]s.
fn tokens(mut pat: &[char]) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    while let Some((c, rest)) = pat.split_first() {
        match c {
            '*' => out.push(Tok::Star),
            '?' => out.push(Tok::Any),
            '[' => match bracket_len(rest) {
                Some(len) => {
                    out.push(Tok::Class(rest));
                    pat = rest.get(len..).unwrap_or_default();
                    continue;
                }
                None => {
                    out.push(Tok::Never);
                    break;
                }
            },
            _ => out.push(Tok::Lit(*c)),
        }
        pat = rest;
    }
    out
}

/// Glob matcher for a single path segment, in pattern × text steps.
fn glob_match(pat: &[char], text: &[char]) -> bool {
    star_match(
        &tokens(pat),
        text,
        |t| matches!(t, Tok::Star),
        |_| true,
        |t, c| match t {
            Tok::Any => true,
            Tok::Class(class) => match_bracket(class, *c).is_some(),
            Tok::Lit(l) => l == c,
            Tok::Star | Tok::Never => false,
        },
    )
}

/// The length of the bracket expression `pat` starts (just after its `[`),
/// through its closing `]`, or `None` if it has none. [`match_bracket`]'s walk,
/// which does not depend on the character matched.
fn bracket_len(pat: &[char]) -> Option<usize> {
    let negated = matches!(pat.first(), Some('!') | Some('^'));
    let mut i = usize::from(negated);
    while let Some(&ch) = pat.get(i) {
        if ch == ']' && i > usize::from(negated) {
            return Some(i.saturating_add(1));
        }
        let range_end = pat.get(i.saturating_add(2)).filter(|&&e| e != ']');
        if pat.get(i.saturating_add(1)) == Some(&'-') && range_end.is_some() {
            i = i.saturating_add(3);
        } else {
            i = i.saturating_add(1);
        }
    }
    None
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

    fn grouped(mut g: Glob, group: u32) -> Glob {
        g.group = group;
        g
    }

    /// A folded group's exclusions cut its own matches only; the task's own
    /// (group 0) cut every group's, in both walks.
    #[test]
    fn groups_scope_exclusions() {
        let dir = tmp();
        for f in ["a/x.tmp", "a/x.txt", "b/y.tmp", "own.tmp", "c/z.tmp"] {
            write_file(&dir, f, f);
        }
        let list = [
            g("own.tmp"),
            g_neg("c/**"),
            grouped(g("a/**"), 1),
            grouped(g_neg("**/*.tmp"), 1),
            grouped(g("b/**"), 2),
            grouped(g("c/**"), 2),
        ];
        let want = vec![
            join(&dir, "a/x.txt"),
            join(&dir, "b/y.tmp"),
            join(&dir, "own.tmp"),
        ];
        assert_eq!(globs(&dir, &list).unwrap(), want);
        assert_eq!(cache_globs(&dir, &list).unwrap(), want);
        // The same entries as one list: the exclusion is global.
        let flat: Vec<Glob> = list.iter().map(|g| grouped(g.clone(), 0)).collect();
        assert_eq!(globs(&dir, &flat).unwrap(), vec![join(&dir, "a/x.txt")]);
    }

    /// A fingerprint stands for its group's glob; another group's exclusion
    /// leaves it alone.
    #[test]
    fn groups_scope_exclusions_of_fingerprints() {
        let dir = tmp();
        write_file(&dir, "fp/f.txt", "f");
        write_file(&dir, "fp/stamp.tmp", "s");
        let list = [
            grouped(g_neg("**/*.tmp"), 1),
            grouped(g_fp("fp/**", "fp/stamp.tmp"), 2),
        ];
        assert_eq!(
            globs(&dir, &list).unwrap(),
            vec![join(&dir, "fp/stamp.tmp")]
        );
        assert_eq!(
            cache_globs(&dir, &list).unwrap(),
            vec![join(&dir, "fp/f.txt"), join(&dir, "fp/stamp.tmp")]
        );
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

    /// Every regular file or symlink under `dir`, slash-separated and relative.
    fn tree_files(dir: &str) -> Vec<String> {
        fn walk(base: &Path, rel: &str, out: &mut Vec<String>) {
            for e in std::fs::read_dir(base.join(rel)).unwrap().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let child = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                if e.file_type().unwrap().is_dir() {
                    walk(base, &child, out);
                } else {
                    out.push(child);
                }
            }
        }
        let mut out = Vec::new();
        walk(Path::new(dir), "", &mut out);
        out.sort();
        out
    }

    #[test]
    fn cache_matcher_agrees_with_cache_globs() {
        let dir = setup_node_modules();
        write_file(&dir, "dist/app.js", "a");
        write_file(&dir, "dist/.map/app.js.map", "m");
        write_file(&dir, "dist/tmp/scratch", "s");
        write_file(&dir, "gen/a.pb.go", "g");
        write_file(&dir, "gen/keep.txt", "k");
        write_file(&dir, "Taskfile.yml", "t");
        write_file(&dir, ".git/hooks/pre-commit", "h");
        let cases: Vec<Vec<Glob>> = vec![
            vec![g("dist/**")],
            vec![g("dist/**/*"), g_neg("dist/tmp/**")],
            vec![
                g_fp("node_modules/**/*", "node_modules/.yarn-state.yml"),
                g_neg("node_modules/react/**/*"),
            ],
            vec![g("gen/*.go"), g("dist/app.js")],
            // A folded group's exclusion stays in its group; the task's own
            // applies to everything.
            vec![
                grouped(g("gen/*"), 1),
                grouped(g_neg("gen/keep.txt"), 1),
                g("gen/keep.txt"),
                g("dist/**"),
                g_neg("dist/.map/**"),
            ],
        ];
        for globs in cases {
            let listed: Vec<String> = cache_globs(&dir, &globs)
                .unwrap()
                .into_iter()
                .map(|f| f.strip_prefix(&format!("{dir}/")).unwrap().to_string())
                .collect();
            let matcher = CacheMatcher::new(&dir, &globs);
            for rel in tree_files(&dir) {
                assert_eq!(
                    matcher.matches(&dir, &rel),
                    listed.contains(&rel),
                    "{rel} with {globs:?}"
                );
            }
        }
    }

    /// Names an archive chooses are matched against these patterns: inputs
    /// that take a backtracking matcher minutes (`**` and `*` runs that never
    /// match) must stay linear in each. A regression hangs this test.
    #[test]
    fn matching_is_polynomial_in_stars() {
        let root = "/proj";
        let matcher = CacheMatcher::new(root, &[g("**/a/**/b/**/*.go")]);
        let mut rel = "a/b/".repeat(2048);
        rel.push_str("zz");
        assert!(!matcher.matches(root, &rel));
        let name = "a".repeat(255);
        assert!(!matches_segment("*a*a*a*a*a*a*a*a*b", &name, true));
        assert!(matches_segment("*a*a*a*a*a*a*a*a*", &name, true));
        assert!(matches_segment("[a-c]*[!x]?", "abcd", true));
        assert!(!matches_segment("[ab", "a", true));
    }

    #[test]
    fn cache_matcher_joins_the_task_dir_onto_patterns() {
        let root = tmp();
        let matcher = CacheMatcher::new(&format!("{root}/sub"), &[g("out/**")]);
        assert!(matcher.matches(&root, "sub/out/x"));
        assert!(!matcher.matches(&root, "out/x"));
        // An absolute pattern keeps its `./` when joined; saving drops it.
        let matcher = CacheMatcher::new(&root, &[g(&format!("{root}/./gen//*.go"))]);
        assert!(matcher.matches(&root, "gen/a.go"));
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
            assert!(
                Pruner::new(p, &include_refs, Options::SHELL).is_some(),
                "{p} should prune"
            );
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
        assert!(Pruner::new(&join(&dir, "target/**/*"), &include_refs, Options::SHELL).is_none());
        // Tree mode reaches dot paths, but `.keep` could be a directory
        // symlink: literal traversal still prevents pruning.
        assert!(Pruner::new(&join(&dir, "target/**/*"), &include_refs, Options::TREE).is_none());
        assert!(
            cache_globs(&dir, &[g("target/.keep/*.o"), g_neg("target/**/*")])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn pruner_classification() {
        fn new(pattern: &str, includes: &[&str]) -> Option<Pruner> {
            Pruner::new(pattern, includes, Options::SHELL)
        }
        let inc = ["/w/**/*.rs"];
        assert!(new("/w/target/**/*", &inc).is_some());
        assert!(new("/w/target/**/**/*", &inc).is_some());
        assert!(new("/w/*/target/**/*", &inc).is_some());
        assert!(new("/w/**/target/**/*", &inc).is_some());
        // These patterns do not cover a whole directory.
        assert!(new("/w/target/**/*.o", &inc).is_none());
        assert!(new("/w/target/*", &inc).is_none());
        // Both forms cover the same files.
        assert!(new("/w/target/**", &inc).is_some());
        assert!(new("/w/target/**/**", &inc).is_some());
        // This covers each child directory at depth 2 or greater under `target`.
        assert!(new("/w/target/**/*/**/*", &inc).is_some());
        // These would prune the walk root.
        assert!(new("/**/*", &inc).is_none());
        assert!(new("**/*", &["**/*.rs"]).is_none());
        // A fixed dot segment above covered directories is safe. One below a
        // covered directory or after `**` is not.
        assert!(
            new(
                "/home/u/.local/w/target/**/*",
                &["/home/u/.local/w/**/*.rs"]
            )
            .is_some()
        );
        assert!(new("/w/target/**/*", &["/w/target/.cache/*"]).is_none());
        assert!(new("/w/target/**/*", &["/w/**/.cache/*"]).is_none());
        // `**` cannot span `.git`, matching the walk.
        assert!(new("/w/**/target/**/*", &["/w/.git/**/*"]).is_some());
        assert!(new("/w/**/target/**/*", &["/w/**/.git/**/*"]).is_none());
    }

    #[test]
    fn pruner_covers() {
        let p = Pruner::new("/w/**/target*/**/*", &[], Options::SHELL).unwrap();
        assert!(p.covers("/w/target"));
        assert!(p.covers("/w/a/b/target-x"));
        assert!(!p.covers("/w/a/.hidden/target"));
        assert!(!p.covers("/w/a/target/sub"));
        assert!(!p.covers("/w/a/starget"));
        assert!(!p.covers("/w"));
        let rel = Pruner::new("target/**/*", &[], Options::SHELL).unwrap();
        assert!(rel.covers("./target"));
        assert!(!rel.covers("./src/target"));
        // `**` spans dot directories in tree mode.
        let tree = Pruner::new("/w/**/target/**/*", &[], Options::TREE).unwrap();
        assert!(tree.covers("/w/a/.hidden/target"));
    }

    #[test]
    fn cache_globs_include_hidden_entries() {
        let dir = tmp();
        write_file(&dir, "node_modules/.yarn-state.yml", "state");
        write_file(&dir, "node_modules/.bin/tsc", "shim");
        write_file(&dir, "node_modules/pkg/.github/FUNDING.yml", "f");
        write_file(&dir, "node_modules/pkg/index.js", "i");
        let patterns = [g("node_modules/**/*")];
        // The fingerprint walk keeps the shell's view.
        assert_eq!(
            globs(&dir, &patterns).unwrap(),
            vec![join(&dir, "node_modules/pkg/index.js")]
        );
        assert_eq!(
            cache_globs(&dir, &patterns).unwrap(),
            vec![
                join(&dir, "node_modules/.bin/tsc"),
                join(&dir, "node_modules/.yarn-state.yml"),
                join(&dir, "node_modules/pkg/.github/FUNDING.yml"),
                join(&dir, "node_modules/pkg/index.js"),
            ]
        );
        // A negated whole-tree pattern prunes dot directories too.
        let pruned = [g("node_modules/**/*"), g_neg("node_modules/pkg/**/*")];
        assert_eq!(
            cache_globs(&dir, &pruned).unwrap(),
            vec![
                join(&dir, "node_modules/.bin/tsc"),
                join(&dir, "node_modules/.yarn-state.yml"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_globs_store_symlinked_directories_as_links() {
        let dir = tmp();
        write_file(&dir, "packages/lib/index.js", "lib");
        write_file(&dir, "node_modules/pkg/index.js", "i");
        std::fs::create_dir_all(join(&dir, "node_modules/@ws")).unwrap();
        std::os::unix::fs::symlink("../../packages/lib", join(&dir, "node_modules/@ws/lib"))
            .unwrap();
        let patterns = [g("node_modules/**/*")];
        // The shell walk follows the link, as Go Task does.
        assert_eq!(
            globs(&dir, &patterns).unwrap(),
            vec![
                join(&dir, "node_modules/@ws/lib"),
                join(&dir, "node_modules/@ws/lib/index.js"),
                join(&dir, "node_modules/pkg/index.js"),
            ]
        );
        // The archive stores the link and never its target's contents.
        assert_eq!(
            cache_globs(&dir, &patterns).unwrap(),
            vec![
                join(&dir, "node_modules/@ws/lib"),
                join(&dir, "node_modules/pkg/index.js"),
            ]
        );
        // A literal segment still resolves through a symlink.
        assert_eq!(
            cache_globs(&dir, &[g("node_modules/@ws/lib/*")]).unwrap(),
            vec![join(&dir, "node_modules/@ws/lib/index.js")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_exclusions_preserve_literal_paths_through_symlinks() {
        let dir = tmp();
        write_file(&dir, "target/file.txt", "data");
        std::fs::create_dir_all(join(&dir, "out")).unwrap();
        std::os::unix::fs::symlink("../target", join(&dir, "out/link")).unwrap();

        for exclude in ["out/link/**", "out/**/link/**"] {
            assert!(
                cache_globs(&dir, &[g("out/*"), g_neg(exclude)])
                    .unwrap()
                    .is_empty(),
                "exclude {exclude}"
            );
        }
        for include in ["out/link/**", "out/link/**/**"] {
            assert_eq!(
                cache_globs(&dir, &[g(include), g_neg("out/link/**/*")]).unwrap(),
                vec![join(&dir, "out/link")],
                "include {include}"
            );
        }

        for pattern in ["out/link/file.txt", "out/link/*", "out/**/link/*"] {
            let include = g(pattern);
            let excluded = cache_globs(&dir, &[g("out/**/*")]).unwrap();
            assert_eq!(excluded, vec![join(&dir, "out/link")]);
            let expected: Vec<String> = cache_globs(&dir, std::slice::from_ref(&include))
                .unwrap()
                .into_iter()
                .filter(|path| !excluded.contains(path))
                .collect();
            assert_eq!(expected, vec![join(&dir, "out/link/file.txt")]);
            assert_eq!(
                cache_globs(&dir, &[include, g_neg("out/**/*")]).unwrap(),
                expected,
                "include {pattern}"
            );
        }

        let mut include = g("out/**/*");
        include.fingerprint = "out/link/file.txt".into();
        assert_eq!(
            cache_globs(&dir, &[include, g_neg("out/**/*")]).unwrap(),
            vec![join(&dir, "out/link/file.txt")]
        );
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
