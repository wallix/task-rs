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
//! Exclusions (negated entries) match with `dotglob` on: their wildcards and
//! `**` also match dot-prefixed names, so `exclude: dir/**` removes every
//! entry under `dir`, hidden ones included. Includes keep the shell rule.
//!
//! Cache collection ([`cache_globs`]) walks the same patterns in *tree* mode
//! instead: wildcards and `**` also match dot entries, and `**` does not
//! descend through a symlink to a directory — the link itself is the entry.
//! A generated tree is archived whole and restored as it was, whatever the
//! shell would have listed.

use std::collections::{BTreeSet, HashMap};
#[cfg(test)]
use std::path::Path;
use std::rc::Rc;

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
    let mut scan = Scan::default();
    let files = globs_scanned(dir, globs, &mut scan);
    scan.result(files)
}

/// [`globs`], with what the walk learned of each match's type, for the
/// checksum to skip a `stat` of its own.
pub(crate) fn hinted_globs(dir: &str, globs: &[Glob]) -> std::io::Result<Vec<(String, Hint)>> {
    let mut scan = Scan::default();
    let files = globs_scanned(dir, globs, &mut scan);
    let files = scan.result(files)?;
    Ok(files
        .into_iter()
        .map(|f| {
            let hint = scan.hints.get(&f).copied().unwrap_or(Hint::Unknown);
            (f, hint)
        })
        .collect())
}

fn globs_scanned(dir: &str, globs: &[Glob], scan: &mut Scan) -> Vec<String> {
    // A negated entry with a fingerprint excludes that file alone.
    let pruners = own_pruners(dir, globs, |g| g.fingerprint.is_empty());
    let mut folded = BTreeSet::new();
    for group in folded_groups(globs) {
        folded.extend(shell_globs(dir, group, BTreeSet::new(), &pruners, scan));
    }
    shell_globs(dir, own(globs), folded, &[], scan)
}

/// Pruners for the walks of folded groups, from the task's own whole-tree
/// exclusions (those `is_pattern` says match by their glob): whatever those
/// walks find in the directories they cover is removed afterwards anyway.
fn own_pruners(dir: &str, globs: &[Glob], is_pattern: impl Fn(&Glob) -> bool) -> Vec<Pruner> {
    own(globs)
        .filter(|g| g.negate && is_pattern(g))
        .filter_map(|g| Pruner::new(&join_pattern(dir, &g.glob)))
        .collect()
}

/// What the walk of one [`globs`] or [`cache_globs`] call learned.
///
/// Each directory is listed once per call, however many patterns or groups
/// walk it. No command runs during a call, but other processes may still
/// write meanwhile: overlapping patterns then see one listing, where they
/// used to list the directory again.
#[derive(Default)]
struct Scan {
    listings: HashMap<String, Rc<[Listed]>>,
    /// Each match's type as listed, when the listing knew it.
    hints: HashMap<String, Hint>,
    /// The first lookup error the shell would not have shrugged off (see
    /// [`ends_branch`]): it fails the call rather than leave entries out.
    error: Option<std::io::Error>,
}

impl Scan {
    /// Records the first error that fails the call, naming `path`.
    fn fail(&mut self, path: &str, e: std::io::Error) {
        if self.error.is_none() {
            self.error = Some(std::io::Error::new(e.kind(), format!("{path}: {e}")));
        }
    }

    /// Whether `path` names anything, following links, looked up one name at
    /// a time ([`lookup_by_handles`]): a lookup error that does not just end
    /// the branch fails the call.
    fn exists(&mut self, path: &str) -> bool {
        match lookup_by_handles(path) {
            Ok(found) => found.is_some(),
            Err(e) => {
                self.fail(path, e);
                false
            }
        }
    }

    /// `value`, or the error the walk ran into.
    fn result<T>(&mut self, value: T) -> std::io::Result<T> {
        self.error.take().map_or(Ok(value), Err)
    }
}

/// Whether a failed lookup just ends the walk's branch, as in the shell's
/// globbing: the name is gone, unreadable or not a directory, or a link the
/// kernel refuses to resolve (`ELOOP`: a cycle or an over-long chain in its
/// own target). Any other error (too many open files, an I/O error, no
/// memory) fails the walk, so a fingerprint never covers part of a tree.
fn ends_branch(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{NotADirectory, NotFound, PermissionDenied};
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::ELOOP) {
        return true;
    }
    matches!(e.kind(), NotFound | PermissionDenied | NotADirectory)
}

/// A directory entry and its type as listed (`d_type`, or an `lstat` relative
/// to its directory where the filesystem does not report one): never
/// followed. `None` only for a name that is not valid UTF-8, whose lossy
/// spelling names nothing reliably.
struct Listed {
    name: String,
    path: String,
    kind: Option<Ty>,
}

/// An entry's type, never followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    Dir,
    File,
    Link,
    Other,
}

impl Ty {
    fn of(ft: std::fs::FileType) -> Self {
        if ft.is_dir() {
            Ty::Dir
        } else if ft.is_file() {
            Ty::File
        } else if ft.is_symlink() {
            Ty::Link
        } else {
            Ty::Other
        }
    }
}

/// A match's type as its directory listing reported it, so a checksum need
/// not `lstat` it again. Only a hint: the file may change before it is read,
/// which the checksum detects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hint {
    /// A regular file.
    File,
    /// A symlink.
    Link,
    /// Not known: a literal path, a name not valid UTF-8 (its lossy path may
    /// not exist), or a listing that could not tell.
    Unknown,
}

/// [`globs`] over a single group, starting from the `included` matches.
fn shell_globs<'a>(
    dir: &str,
    globs: impl Iterator<Item = &'a Glob>,
    mut included: BTreeSet<String>,
    pruners: &[Pruner],
    scan: &mut Scan,
) -> Vec<String> {
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    let mut patterns = Vec::new();
    let mut named = Vec::new();

    for g in globs {
        if !g.fingerprint.is_empty() {
            let fp = join_path(dir, &g.fingerprint);
            if scan.exists(&fp) {
                #[cfg(test)]
                hit("fingerprint");
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
            named.extend(expand(&pattern, &[], Options::SHELL, scan));
            continue;
        }
        patterns.push(Pattern::of(g, pattern));
    }

    expand_patterns(
        &patterns,
        named,
        &mut included,
        &mut excluded,
        pruners,
        Options::SHELL,
        scan,
    );
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
    let scan = &mut Scan::default();
    let pruners = own_pruners(dir, globs, |_| true);
    let mut folded = BTreeSet::new();
    for group in folded_groups(globs) {
        folded.extend(tree_globs(dir, group, BTreeSet::new(), &pruners, scan));
    }
    let files = tree_globs(dir, own(globs), folded, &[], scan);
    scan.result(files)
}

/// Match paths against [`cache_globs`]'s `globs` without walking the tree,
/// so a restore can reject entries outside the task's generates before writing.
/// Matches every path the walk lists, but may also match paths below directory
/// symlinks that a wildcard would not cross. A restore rejects those separately.
///
/// It also matches a link at a directory an exclusion names (a trailing `/`,
/// or a trailing `**` matching nothing): the walk drops it only when it leads
/// to a directory, which a name cannot tell. Likewise an include ending in
/// `/` matches every path its name fits, though the walk lists only links to
/// directories there. A restore checks every link's target separately.
pub struct CacheMatcher {
    own: MatchGroup,
    folded: Vec<MatchGroup>,
}

/// One group's entries, joined and split as the walk splits them.
struct MatchGroup {
    includes: Vec<Vec<String>>,
    excludes: Vec<Vec<String>>,
    /// Paths named explicitly: includes without wildcards and fingerprints.
    named: Vec<Vec<String>>,
    /// The exclusions without wildcards, the only ones removing a named path.
    literal: Vec<Vec<String>>,
}

/// `path`'s segments with the walk's root marker, dropping the `.` and empty
/// segments a pattern may carry (an absolute one is not cleaned when joined,
/// nor is any pattern's trailing `/`): the walk lists such paths, and saving
/// normalises them away.
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
            named: Vec::new(),
            literal: Vec::new(),
        };
        for g in globs {
            let pattern = join_pattern(dir, &g.glob);
            let mut segments = normalized_segments(&pattern);
            if g.negate {
                // A trailing `/`, or a `**` matching nothing, matches only a
                // directory ([`Exclusion`]), and archive entries are files and
                // links: keep only what matches below it, so a link at such a
                // directory is accepted (see [`CacheMatcher`]).
                if pattern.ends_with('/') {
                    continue;
                }
                if !g.has_wildcards() {
                    group.literal.push(segments.clone());
                }
                if segments.len() > 1 && segments.last().is_some_and(|s| s == "**") {
                    segments.push("*".to_string());
                }
                group.excludes.push(segments);
            } else {
                if g.is_named() && !g.has_wildcards() {
                    group.named.push(segments.clone());
                }
                group.includes.push(segments);
                if g.is_named() && !g.fingerprint.is_empty() {
                    group
                        .named
                        .push(normalized_segments(&join_path(dir, &g.fingerprint)));
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

    fn names(&self, segments: &[&str]) -> bool {
        self.named.iter().any(|f| f.iter().eq(segments))
    }

    /// Whether the group's exclusions remove `segments`: only one without
    /// wildcards, matched by equality, when the group names it too.
    fn removes(&self, segments: &[&str]) -> bool {
        self.literal.iter().any(|l| l.iter().eq(segments))
            || (!self.names(segments) && self.excluded(segments))
    }

    fn lists(&self, segments: &[&str]) -> bool {
        (self.names(segments)
            || self
                .includes
                .iter()
                .any(|i| match_segments(i, segments, Options::TREE.dotglob)))
            && !self.removes(segments)
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
            && !self.own.removes(&segments)
    }
}

/// [`cache_globs`] over a single group, starting from the `included` matches.
fn tree_globs<'a>(
    dir: &str,
    globs: impl Iterator<Item = &'a Glob>,
    mut included: BTreeSet<String>,
    pruners: &[Pruner],
    scan: &mut Scan,
) -> Vec<String> {
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    let mut patterns = Vec::new();
    let mut named = Vec::new();

    for g in globs {
        let pattern = join_pattern(dir, &g.glob);
        if g.is_named() && !g.has_wildcards() {
            named.extend(expand(&pattern, &[], Options::TREE, scan));
        } else {
            patterns.push(Pattern::of(g, pattern));
        }
        if !g.fingerprint.is_empty() && !g.negate {
            let fp = join_path(dir, &g.fingerprint);
            if scan.exists(&fp) {
                #[cfg(test)]
                hit("fingerprint");
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
        pruners,
        Options::TREE,
        scan,
    );
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

/// An entry joined onto the task directory, for [`expand_patterns`].
struct Pattern {
    path: String,
    negate: bool,
    /// Written without wildcards ([`Glob::has_wildcards`]).
    literal: bool,
}

impl Pattern {
    fn of(g: &Glob, path: String) -> Self {
        Self {
            path,
            negate: g.negate,
            literal: !g.has_wildcards(),
        }
    }
}

/// Expands joined entries into the working sets.
///
/// The result is the order-independent `included - excluded`. Exclusions are
/// matched against paths rather than walked ([`Exclusion`]): one removes every
/// included path its pattern matches, however the include reached it, dot
/// names included. A whole-tree exclusion (`<prefix>/**/*` or `<prefix>/**`)
/// also becomes a [`Pruner`] that keeps positive walks out of the directories
/// it covers, as do the `extra` pruners of exclusions applied afterwards.
///
/// The paths in `named`, which entries name explicitly ([`Glob::is_named`])
/// and were looked up past every pruner, are kept unless an exclusion
/// without wildcards names them or `excluded` held them on entry.
fn expand_patterns(
    patterns: &[Pattern],
    named: Vec<String>,
    included: &mut BTreeSet<String>,
    excluded: &mut BTreeSet<String>,
    extra: &[Pruner],
    opts: Options,
    scan: &mut Scan,
) {
    let includes: Vec<&str> = patterns
        .iter()
        .filter(|p| !p.negate)
        .map(|p| p.path.as_str())
        .collect();
    let mut pruners = Vec::new();
    let mut exclusions = Vec::new();
    for p in patterns.iter().filter(|p| p.negate) {
        let pruner = Pruner::new(&p.path).filter(|_| pruning());
        exclusions.push(Exclusion::new(&p.path, p.literal, true, pruner.is_some()));
        pruners.extend(pruner);
    }
    if pruning() {
        pruners.extend(extra.iter().cloned());
    }

    let mut exclusions = Exclusions::new(exclusions);
    // Excluded by name before any pattern applies (negated fingerprints).
    let fixed = excluded.clone();
    // Paths already included (fingerprints, folded groups' matches) may lie
    // anywhere, pruned directories included.
    if !exclusions.is_empty() {
        let matched: Vec<String> = included
            .iter()
            .filter(|p| exclusions.excludes(p, false, scan))
            .cloned()
            .collect();
        for path in matched {
            #[cfg(test)]
            hit("included before, excluded");
            mark(included, excluded, path, true);
        }
    }
    for pattern in includes {
        for m in expand(pattern, &pruners, opts, scan) {
            let decided = || included.contains(&m) || excluded.contains(&m);
            if exclusions.is_empty() || decided() {
                mark(included, excluded, m, false);
                continue;
            }
            let negate = exclusions.excludes(&m, true, scan);
            mark(included, excluded, m, negate);
        }
    }
    // Last, over whatever a wildcard's exclusion decided for the same path.
    for m in named {
        if fixed.contains(&m) || exclusions.names(&m, scan) {
            mark(included, excluded, m, true);
        } else {
            #[cfg(test)]
            hit("named");
            excluded.remove(&m);
            included.insert(m);
        }
    }
}

/// An exclusion, compiled for matching listed paths.
///
/// It matches a path as the walk of its pattern would have listed it, by
/// name: `**` and wildcards per [`match_segments`], empty segments skipped,
/// except that a trailing `/`, or a trailing `**` matching nothing, matches
/// only a directory (an included path is one only as a link to it).
struct Exclusion {
    /// Segments with the root marker and without empty ones; for a trailing
    /// `**`, the base before it.
    segments: Vec<String>,
    /// For a trailing `**`: the segments matching anything below the base.
    below: Option<Vec<String>>,
    tail: Tail,
    /// For an entry written without wildcards, its joined pattern as a path,
    /// matched by equality (with a trailing `/`, a directory only).
    literal: Option<String>,
    /// The leading literal segments and `/`: every matched path starts with
    /// it or equals `base`.
    prefix: String,
    /// `prefix` without its trailing `/`.
    base: String,
    /// Literal segments past the prefix, as `/name/`: a matched path has
    /// each, or ends with it (without the last `/`).
    needles: Vec<String>,
    /// The last segment, as `/name`, when literal: what a path matched by
    /// `segments` ends with.
    last: Option<String>,
    /// For a wildcard last segment, its literal start and end (end empty when
    /// it has a `[`): the last name of a path `segments` match has both.
    affix: Option<(String, String)>,
    dotglob: bool,
    /// Whether a [`Pruner`] keeps walks out of every directory it covers.
    pruned: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tail {
    /// Matches by its segments only.
    Name,
    /// Ends in `/`: a directory its segments match.
    Dir,
    /// Ends in `**`: a directory its segments match, or anything below one.
    Tree,
}

impl Exclusion {
    /// `pattern` is the entry joined onto the task directory; `literal` says
    /// the entry as written has no wildcards ([`Glob::has_wildcards`]).
    fn new(pattern: &str, literal: bool, dotglob: bool, pruned: bool) -> Self {
        let mut segments: Vec<String> = normalized(pattern)
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut tail = if pattern.ends_with('/') {
            Tail::Dir
        } else {
            Tail::Name
        };
        if tail == Tail::Name && segments.len() > 1 && segments.last().is_some_and(|s| s == "**") {
            while segments.len() > 1 && segments.last().is_some_and(|s| s == "**") {
                segments.pop();
            }
            tail = Tail::Tree;
        }
        let below = (tail == Tail::Tree).then(|| {
            segments
                .iter()
                .cloned()
                .chain(["*".to_string(), "**".to_string()])
                .collect()
        });
        let lead = segments.iter().take_while(|s| !has_meta(s)).count();
        let literal = (literal && tail != Tail::Tree).then(|| segments.join("/"));
        let base = segments.get(..lead).unwrap_or_default().join("/");
        let prefix = format!("{base}/");
        let needles = segments
            .get(lead..)
            .unwrap_or_default()
            .iter()
            .filter(|s| !has_meta(s))
            .map(|s| format!("/{s}/"))
            .collect();
        let last = segments
            .last()
            .filter(|s| segments.len() > 1 && !has_meta(s) && s.as_str() != "**")
            .map(|s| format!("/{s}"));
        let affix = segments
            .last()
            .filter(|s| segments.len() > 1 && has_meta(s) && s.as_str() != "**")
            .map(|s| {
                let start = s.split(['*', '?', '[']).next().unwrap_or_default();
                let end = if s.contains('[') {
                    ""
                } else {
                    s.rsplit(['*', '?']).next().unwrap_or_default()
                };
                (start.to_string(), end.to_string())
            });
        Self {
            segments,
            below,
            tail,
            literal,
            prefix,
            base,
            needles,
            last,
            affix,
            dotglob,
            pruned,
        }
    }
}

/// A group's exclusions: those without wildcards looked up by path, the
/// others matched in turn.
struct Exclusions {
    literal: std::collections::HashSet<String>,
    others: Vec<Exclusion>,
    matcher: Matcher,
}

impl Exclusions {
    fn new(all: Vec<Exclusion>) -> Self {
        let (literal, others): (Vec<Exclusion>, Vec<Exclusion>) = all
            .into_iter()
            .partition(|e| e.literal.is_some() && e.tail == Tail::Name);
        Self {
            literal: literal.into_iter().filter_map(|e| e.literal).collect(),
            others,
            matcher: Matcher::default(),
        }
    }

    fn is_empty(&self) -> bool {
        self.literal.is_empty() && self.others.is_empty()
    }

    /// Whether one without wildcards names `path` (with a trailing `/`, as
    /// a directory).
    fn names(&mut self, path: &str, scan: &mut Scan) -> bool {
        let clean = clean_path(path);
        self.literal.contains(clean.as_ref())
            || self.others.iter().any(|e| {
                e.literal.is_some() && self.matcher.excludes_clean(e, &clean, path, false, scan)
            })
    }

    /// Whether one excludes `path` (see [`Matcher::excludes`]).
    fn excludes(&mut self, path: &str, walked: bool, scan: &mut Scan) -> bool {
        let clean = clean_path(path);
        self.literal.contains(clean.as_ref())
            || self
                .others
                .iter()
                .any(|e| self.matcher.excludes_clean(e, &clean, path, walked, scan))
    }
}

/// `path` as the walk spells it: rooted, without empty segments. The walk's
/// own paths already are.
fn clean_path(path: &str) -> std::borrow::Cow<'_, str> {
    if (path.starts_with('/') || path.starts_with("./"))
        && !path.contains("//")
        && !path.ends_with('/')
    {
        std::borrow::Cow::Borrowed(path)
    } else {
        std::borrow::Cow::Owned(path_segments(path).join("/"))
    }
}

/// Matches paths against [`Exclusion`]s, reusing its buffers.
#[derive(Default)]
struct Matcher {
    rows: (Vec<bool>, Vec<bool>),
}

impl Matcher {
    /// Whether `e` excludes `path`: one a walk listed when `walked` (so not
    /// below a directory a pruner covers), else one included before.
    #[cfg(test)]
    fn excludes(&mut self, e: &Exclusion, path: &str, walked: bool, scan: &mut Scan) -> bool {
        self.excludes_clean(e, &clean_path(path), path, walked, scan)
    }

    /// [`Self::excludes`] of `raw`, spelled as its [`clean_path`] `path`.
    /// Cheap checks on the path's text reject most paths first; `raw` is
    /// what a directory check looks up.
    fn excludes_clean(
        &mut self,
        e: &Exclusion,
        path: &str,
        raw: &str,
        walked: bool,
        scan: &mut Scan,
    ) -> bool {
        // Below a covered directory, the only paths `<prefix>/**/*` matches.
        let below = e.below.as_ref().filter(|_| !(walked && e.pruned));
        if walked && e.pruned && e.tail == Tail::Name {
            return false;
        }
        if let Some(literal) = &e.literal {
            return literal == path && (e.tail == Tail::Name || is_dir(raw, scan));
        }
        if !path.starts_with(&e.prefix) && path != e.base {
            return false;
        }
        let ends = |n: &str| path.ends_with(n.get(..n.len().saturating_sub(1)).unwrap_or(n));
        if !e
            .needles
            .iter()
            .all(|n| path.contains(n.as_str()) || ends(n))
        {
            return false;
        }
        // Whether the last segment can match the path's last name.
        let last_fits = |path: &str| {
            let name = path.rsplit('/').next().unwrap_or_default();
            e.last.as_ref().is_none_or(|l| path.ends_with(l.as_str()))
                && e.affix.as_ref().is_none_or(|(start, end)| {
                    name.starts_with(start.as_str()) && name.ends_with(end.as_str())
                })
        };
        if below.is_none() && !last_fits(path) {
            return false;
        }

        let segments = path_segments(path);
        let dotglob = e.dotglob;
        let rows = &mut self.rows;
        let mut matches = |pat: &[String]| {
            star_match_with(
                pat,
                &segments,
                |seg| seg == "**",
                |name| dotglob || !name.starts_with('.'),
                |seg, name| {
                    if has_meta(seg) {
                        matches_segment(seg, name, dotglob)
                    } else {
                        seg == name
                    }
                },
                rows,
            )
        };
        match e.tail {
            Tail::Name => matches(&e.segments),
            Tail::Dir => matches(&e.segments) && is_dir(raw, scan),
            Tail::Tree => {
                if below.is_some_and(|b| matches(b)) {
                    return true;
                }
                last_fits(path) && matches(&e.segments) && is_dir(raw, scan)
            }
        }
    }
}

/// Whether `path`, an included entry as its walk spelled it, is a directory
/// or a link to one. A file listed as such is not; anything else is looked
/// up one name at a time ([`lookup_by_handles`]), following links. A lookup
/// error the shell shrugs off means not; any other fails the walk.
fn is_dir(path: &str, scan: &mut Scan) -> bool {
    if scan.hints.get(path) == Some(&Hint::File) {
        return false;
    }
    #[cfg(test)]
    hit("directory check");
    match lookup_by_handles(path) {
        Ok(kind) => kind == Some(Ty::Dir),
        Err(e) => {
            scan.fail(path, e);
            false
        }
    }
}

/// Joins `pattern` onto `dir` the way every expansion in this module does.
fn join_pattern(dir: &str, pattern: &str) -> String {
    filepathext::join_glob(dir, pattern)
}

/// Joins a `fingerprint` path onto `dir`: a path, not a pattern, so a
/// trailing `/` is dropped rather than kept.
fn join_path(dir: &str, path: &str) -> String {
    let joined = filepathext::smart_join(dir, path)
        .to_string_lossy()
        .into_owned();
    match joined.trim_end_matches('/') {
        "" => joined,
        trimmed => trimmed.to_string(),
    }
}

/// Expands a single glob pattern rooted at `dir`, returning matching regular
/// files (directories are skipped). Symlinks are included as regular entries.
pub fn glob(dir: &str, pattern: &str) -> std::io::Result<Vec<String>> {
    let mut scan = Scan::default();
    let files = expand(&join_pattern(dir, pattern), &[], Options::SHELL, &mut scan);
    scan.result(files)
}

/// Expands an already-joined pattern, skipping directories and never entering
/// a directory one of the `pruners` covers.
///
/// A match whose listing gave its type is taken by that type; any other is
/// checked with `lstat`, and skipped when that ends its branch.
fn expand(pattern: &str, pruners: &[Pruner], opts: Options, scan: &mut Scan) -> Vec<String> {
    let mut results: BTreeSet<String> = BTreeSet::new();
    for (f, kind) in expand_fields(pattern, pruners, opts, scan) {
        let kind = match kind {
            Some(kind) => kind,
            None => match std::fs::symlink_metadata(&f) {
                Ok(meta) => Ty::of(meta.file_type()),
                Err(e) if ends_branch(&e) => continue,
                Err(e) => {
                    scan.fail(&f, e);
                    continue;
                }
            },
        };
        // A symlink to a directory should behave like a regular entry: only
        // skip real directories, matching how the shell lists path names.
        let hint = match kind {
            Ty::Dir => continue,
            Ty::Link => Hint::Link,
            Ty::File => Hint::File,
            Ty::Other => Hint::Unknown,
        };
        scan.hints.insert(f.clone(), hint);
        results.insert(f);
    }
    results.into_iter().collect()
}

/// Skips directories covered by a negated `<prefix>/**/*` or `<prefix>/**`
/// pattern while walking includes.
///
/// Pruning preserves `included - excluded`: the exclusion matches every path
/// below a covered directory, dot names included, which is all the pruned walk
/// leaves out. A link to a covered directory that the walk would list from
/// inside it is listed by [`Walker::pruned_link`].
#[derive(Clone)]
struct Pruner {
    /// Pattern segments before the trailing `**`, including the walk's root
    /// marker (`""` for absolute patterns and `"."` for relative ones),
    /// without empty segments.
    prefix: Vec<String>,
}

impl Pruner {
    fn new(pattern: &str) -> Option<Self> {
        // `<prefix>/**/` matches directories only, not the files below.
        if pattern.ends_with('/') {
            return None;
        }
        let segments = normalized(pattern);
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

        Some(Self {
            prefix: prefix.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    /// Reports whether `dir` (a path built by [`Walker::walk`]) matches the
    /// prefix.
    fn covers(&self, dir: &str) -> bool {
        let path: Vec<&str> = dir.split('/').collect();
        match_segments(&self.prefix, &path, true)
    }
}

/// [`rooted_segments`] without the empty segments past the root marker,
/// which the walk skips (`a//b` is `a/b`).
fn normalized(path: &str) -> Vec<&str> {
    rooted_segments(path)
        .into_iter()
        .enumerate()
        .filter(|(i, s)| *i == 0 || !s.is_empty())
        .map(|(_, s)| s)
        .collect()
}

/// Splits a listed path into segments as [`rooted_segments`] splits its
/// pattern: the walk lists a relative pattern's matches under `./`. Empty
/// segments are dropped past the root marker.
fn path_segments(path: &str) -> Vec<&str> {
    let mut segments: Vec<&str> = path.split('/').collect();
    if !matches!(segments.first(), Some(&"" | &".")) {
        segments.insert(0, ".");
    }
    segments
        .into_iter()
        .enumerate()
        .filter(|(i, s)| *i == 0 || !s.is_empty())
        .map(|(_, s)| s)
        .collect()
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
    star_match_with(
        toks,
        text,
        is_star,
        star_ok,
        hit,
        &mut (Vec::new(), Vec::new()),
    )
}

/// [`star_match`] in the given row buffers, which it reuses.
fn star_match_with<T, U>(
    toks: &[T],
    text: &[U],
    is_star: impl Fn(&T) -> bool,
    star_ok: impl Fn(&U) -> bool,
    hit: impl Fn(&T, &U) -> bool,
    rows: &mut (Vec<bool>, Vec<bool>),
) -> bool {
    let (next, cur) = rows;
    // `next[j]`: whether the tokens after the current one match `text[j..]`.
    next.clear();
    next.resize(text.len().saturating_add(1), false);
    if let Some(end) = next.last_mut() {
        *end = true;
    }
    for tok in toks.iter().rev() {
        cur.clear();
        cur.resize(next.len(), false);
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
        std::mem::swap(next, cur);
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
///
/// The walk moves from directory handle to directory handle, one name per
/// lookup, so the kernel only ever follows the link at that name (and what
/// its own target spells out): the links behind a path never add up to the
/// kernel's path-wide limit, which past about 20 links fails intermittently
/// while other processes rename entries. A wildcard does not descend into a
/// directory already on its way down (a symlink loop): the link is then an
/// entry, as a link to a file is. Handles are search-only where the platform
/// allows (Linux `O_PATH`), so a directory that can be searched but not read
/// is still crossed by a literal path; only listing needs read permission.
///
/// Each match comes with its listed type, `None` when it was not listed
/// under a valid UTF-8 name.
fn expand_fields(
    pattern: &str,
    pruners: &[Pruner],
    opts: Options,
    scan: &mut Scan,
) -> Vec<(String, Option<Ty>)> {
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
        scan,
        chain: Vec::new(),
        out: Vec::new(),
    };
    if walker.start(&root) {
        walker.walk(&root, rest);
    }
    walker.out
}

/// How many directory handles a walk keeps open along its way down. Deeper,
/// the shallowest are closed and reopened, one name at a time from the
/// deepest open ancestor, when the walk climbs back to them.
const OPEN_HANDLES: usize = 32;

/// A directory on the walk's way down.
struct Node {
    /// The name it was opened by from its parent (unused for the root).
    name: String,
    /// Whether that opening followed a link.
    follow: bool,
    /// Its device and inode, for loop checks and to recognise it reopened.
    id: handle::Id,
    /// Its own entry's type: a directory, or a link the walk followed into
    /// one. A `**` matching zero levels yields it, kept when it is a link.
    kind: Ty,
    /// Its handle, while open.
    handle: Option<Rc<handle::Handle>>,
}

struct Walker<'a> {
    pruners: &'a [Pruner],
    opts: Options,
    scan: &'a mut Scan,
    /// The directories from the root down to the current one.
    chain: Vec<Node>,
    out: Vec<(String, Option<Ty>)>,
}

/// What a listed entry is to a wildcard walk.
enum Child {
    /// A directory to descend into: opened, with its identity and type.
    Dir(handle::Handle, handle::Id, Ty),
    /// A directory not to descend into: gone or unreadable since its
    /// listing.
    Skip,
    /// A directory a pruner covers, of this type (a directory, or a link
    /// followed to one).
    Pruned(Ty),
    /// Not a directory to this walk: a file, a link not followed or not to a
    /// directory, or one back to a directory on the way down.
    NotDir,
}

/// The pattern step that reached an entry.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Literal,
    Wildcard,
    DoubleStar,
}

impl Walker<'_> {
    fn enter(&self, dir: &str) -> bool {
        !self.pruners.iter().any(|p| p.covers(dir))
    }

    /// The walk's verdict on a lookup error: `true` when it just ends the
    /// branch; any other error fails the walk.
    fn ends(&mut self, path: &str, e: std::io::Error) -> bool {
        if ends_branch(&e) {
            return true;
        }
        self.scan.fail(path, e);
        false
    }

    /// Opens the walk's root, `false` when there is nothing to walk.
    fn start(&mut self, root: &str) -> bool {
        let opened = handle::Handle::root(root).and_then(|h| Ok((h.id()?, h)));
        match opened {
            Ok((id, h)) => {
                self.chain.push(Node {
                    name: String::new(),
                    follow: true,
                    id,
                    kind: Ty::Dir,
                    handle: Some(Rc::new(h)),
                });
                true
            }
            Err(e) => {
                self.ends(root, e);
                false
            }
        }
    }

    /// The current directory's handle, reopened if it was closed: from the
    /// deepest open ancestor, one name at a time, each checked to be the
    /// directory it was. `None` when the walk failed or the directory is gone
    /// or changed since (which ends its branch).
    fn current(&mut self, path: &str) -> Option<Rc<handle::Handle>> {
        let top = self.chain.len().checked_sub(1)?;
        if let Some(h) = self.chain.get(top).and_then(|n| n.handle.clone()) {
            return Some(h);
        }
        let from = self.chain.iter().rposition(|n| n.handle.is_some())?;
        let mut h = self.chain.get(from).and_then(|n| n.handle.clone())?;
        for i in from.saturating_add(1)..=top {
            let node = self.chain.get(i)?;
            let reopened = h.child(&node.name, node.follow).and_then(|c| {
                if c.id()? == node.id {
                    Ok(c)
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "replaced during the walk",
                    ))
                }
            });
            match reopened {
                Ok(c) => h = Rc::new(c),
                Err(e) => {
                    self.ends(path, e);
                    return None;
                }
            }
        }
        if let Some(node) = self.chain.get_mut(top) {
            node.handle = Some(Rc::clone(&h));
        }
        self.evict();
        Some(h)
    }

    /// Closes the shallowest handles beyond [`OPEN_HANDLES`]; the root's
    /// stays.
    fn evict(&mut self) {
        let mut open = self.chain.iter().filter(|n| n.handle.is_some()).count();
        for node in self.chain.iter_mut().skip(1) {
            if open <= OPEN_HANDLES {
                break;
            }
            if node.handle.take().is_some() {
                open = open.saturating_sub(1);
            }
        }
    }

    /// Classifies `child` of `dir` for a wildcard walk, opening it when it is
    /// a directory to descend into. The listed type answers without a
    /// syscall except for a symlink to follow; a symlink counts only when
    /// following symlinks.
    fn child(&mut self, dir: &handle::Handle, child: &Listed) -> Child {
        let Some(kind) = child.kind else {
            // Its lossy name may name nothing: not descended, as before.
            return Child::NotDir;
        };
        match kind {
            Ty::Dir => {}
            Ty::Link if self.opts.follow_symlinks => match dir.stat(&child.name, true) {
                Ok(Ty::Dir) => {}
                Ok(_) => return Child::NotDir,
                Err(e) => {
                    self.ends(&child.path, e);
                    return Child::NotDir;
                }
            },
            _ => return Child::NotDir,
        }
        if !self.enter(&child.path) {
            #[cfg(test)]
            hit("pruned");
            return Child::Pruned(kind);
        }
        let opened = dir
            .child(&child.name, kind == Ty::Link)
            .and_then(|h| Ok((h.id()?, h)));
        match opened {
            Ok((id, _)) if self.chain.iter().any(|n| n.id == id) => Child::NotDir,
            Ok((id, h)) => Child::Dir(h, id, kind),
            Err(e) => {
                self.ends(&child.path, e);
                Child::Skip
            }
        }
    }

    /// `base`'s entries, listed once per [`Scan`]. Nothing once the reading
    /// is cancelled, cached or not.
    fn list(&mut self, base: &str) -> Rc<[Listed]> {
        if super::offload::cancelled() {
            return Rc::from([]);
        }
        if let Some(l) = self.scan.listings.get(base) {
            return Rc::clone(l);
        }
        let Some(dir) = self.current(base) else {
            return Rc::from([]);
        };
        let l: Rc<[Listed]> = match list_dir(base, &dir) {
            Ok(l) => l.into(),
            Err(e) => {
                #[cfg(test)]
                hit("unlisted directory");
                self.ends(base, e);
                Rc::from([])
            }
        };
        self.scan.listings.insert(base.to_string(), Rc::clone(&l));
        l
    }

    /// Walks into the directory `path`, opened as `h` by `name` from the
    /// current one, then climbs back.
    fn down(
        &mut self,
        path: &str,
        name: &str,
        h: handle::Handle,
        id: handle::Id,
        kind: Ty,
        f: impl FnOnce(&mut Self, &str),
    ) {
        self.chain.push(Node {
            name: name.to_string(),
            follow: kind == Ty::Link,
            id,
            kind,
            handle: Some(Rc::new(h)),
        });
        self.evict();
        f(self, path);
        self.chain.pop();
    }

    fn stopped(&self) -> bool {
        super::offload::cancelled() || self.scan.error.is_some()
    }

    /// Recursively matches the remaining glob segments against the tree rooted
    /// at `base`, the current directory, appending matched paths to
    /// `self.out`.
    fn walk(&mut self, base: &str, segments: &[&str]) {
        // A cancelled or failed reading stops here: its result is discarded.
        if self.stopped() {
            return;
        }
        let Some((segment, rest)) = segments.split_first() else {
            let kind = self.chain.last().map(|n| n.kind);
            self.out.push((base.to_string(), kind));
            return;
        };

        // `a//b` is `a/b`, as for the kernel.
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
            self.literal(base, segment, rest);
            return;
        }

        let dotglob = self.opts.dotglob;
        let listing = self.list(base);
        for child in listing.iter().filter(|c| shown(c, dotglob)) {
            if self.stopped() {
                return;
            }
            if !matches_segment(segment, &child.name, dotglob) {
                continue;
            }
            if rest.is_empty() {
                self.out.push((child.path.clone(), child.kind));
                continue;
            }
            if trailing_slash(rest) {
                self.slashed(base, child);
                continue;
            }
            let Some(dir) = self.current(base) else {
                return;
            };
            match self.child(&dir, child) {
                Child::Dir(h, id, kind) => {
                    drop(dir);
                    self.down(&child.path, &child.name, h, id, kind, |w, p| {
                        w.walk(p, rest)
                    });
                }
                Child::Pruned(Ty::Link) => {
                    self.pruned_link(
                        &dir,
                        &child.name,
                        Some(Ty::Link),
                        &child.path,
                        rest,
                        Step::Wildcard,
                    );
                }
                Child::Pruned(_) | Child::Skip | Child::NotDir => {}
            }
        }
    }

    /// Matches the literal `segment` in `base`, the current directory. A
    /// literal name resolves through a symlink in either mode, and is not
    /// checked for loops: the pattern itself is finite.
    fn literal(&mut self, base: &str, segment: &str, rest: &[&str]) {
        let next = join_seg(base, segment);
        let Some(dir) = self.current(base) else {
            return;
        };
        if rest.is_empty() {
            // Present (following links), typed by its own entry.
            let found = dir
                .stat(segment, true)
                .and_then(|_| dir.stat(segment, false));
            match found {
                Ok(kind) => self.out.push((next, Some(kind))),
                Err(e) => {
                    self.ends(&next, e);
                }
            }
            return;
        }
        if !self.enter(&next) {
            self.pruned_link(&dir, segment, None, &next, rest, Step::Literal);
            return;
        }
        let opened = dir.child(segment, true).and_then(|h| {
            let kind = dir.stat(segment, false)?;
            Ok((h.id()?, kind, h))
        });
        drop(dir);
        match opened {
            Ok((id, kind, h)) => {
                self.down(&next, segment, h, id, kind, |w, p| w.walk(p, rest));
            }
            Err(e) => {
                self.ends(&next, e);
            }
        }
    }

    /// Lists what an unpruned walk would list at `name`, a link in the
    /// current directory `dir` to a directory a pruner covers, with `rest`
    /// left to match after a `step` of the pattern: the link itself, as an
    /// entry, when `rest` can match nothing below it (all `**`), or when a
    /// final `**` or `/` stops at a wildcard's link back to a directory on
    /// the way down. Whatever else it would list below is excluded.
    ///
    /// `kind` is `name`'s own type when its listing gave it; only a link is
    /// opened, for its identity.
    fn pruned_link(
        &mut self,
        dir: &handle::Handle,
        name: &str,
        kind: Option<Ty>,
        path: &str,
        rest: &[&str],
        step: Step,
    ) {
        let kind = match kind.map_or_else(|| dir.stat(name, false), Ok) {
            Ok(kind) => kind,
            Err(e) => {
                self.ends(path, e);
                return;
            }
        };
        if kind != Ty::Link {
            return;
        }
        let id = match dir.child(name, true).and_then(|h| h.id()) {
            Ok(id) => id,
            Err(e) => {
                self.ends(path, e);
                return;
            }
        };
        let looped = step != Step::Literal && self.chain.iter().any(|n| n.id == id);
        let listed = if looped {
            (step == Step::DoubleStar && rest.is_empty()) || trailing_slash(rest)
        } else {
            rest.iter().all(|s| *s == "**" || s.is_empty())
        };
        #[cfg(test)]
        hit(if looped { "pruned loop" } else { "pruned link" });
        if listed {
            #[cfg(test)]
            hit("pruned link listed");
            self.out.push((path.to_string(), Some(Ty::Link)));
        }
    }

    /// Lists `child` of `base` for a pattern with only a trailing `/` left:
    /// a link to a directory is the entry, in either mode and whether or not
    /// it loops back; a directory itself lists nothing, so it is not entered.
    fn slashed(&mut self, base: &str, child: &Listed) {
        if child.kind != Some(Ty::Link) {
            return;
        }
        let Some(dir) = self.current(base) else {
            return;
        };
        match dir.stat(&child.name, true) {
            Ok(Ty::Dir) => self.out.push((child.path.clone(), Some(Ty::Link))),
            Ok(_) => {}
            Err(e) => {
                self.ends(&child.path, e);
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
        let dotglob = self.opts.dotglob;
        let listing = self.list(base);
        for child in listing.iter().filter(|c| shown(c, dotglob)) {
            if self.stopped() {
                return;
            }
            let Some(dir) = self.current(base) else {
                return;
            };
            match self.child(&dir, child) {
                Child::Dir(h, id, kind) => {
                    drop(dir);
                    self.down(&child.path, &child.name, h, id, kind, |w, p| {
                        w.walk_double_star(p, rest)
                    });
                }
                Child::Pruned(Ty::Link) => {
                    self.pruned_link(
                        &dir,
                        &child.name,
                        Some(Ty::Link),
                        &child.path,
                        rest,
                        Step::DoubleStar,
                    );
                }
                Child::Pruned(_) | Child::Skip => {}
                Child::NotDir => {
                    if rest.is_empty() {
                        self.out.push((child.path.clone(), child.kind));
                    } else if trailing_slash(rest) {
                        self.slashed(base, child);
                    }
                }
            }
        }
    }
}

/// What `path` names, following links, looked up one name at a time from
/// the root as a walk does: `Ok(None)` when the lookup just ends its branch
/// ([`ends_branch`]).
fn lookup_by_handles(path: &str) -> std::io::Result<Option<Ty>> {
    let parts: Vec<&str> = path.split('/').collect();
    let (root, rest) = match parts.split_first() {
        Some((&"", rest)) => ("/", rest),
        _ => (".", parts.as_slice()),
    };
    let names: Vec<&str> = rest.iter().copied().filter(|p| !p.is_empty()).collect();
    let found = (|| {
        let mut dir = handle::Handle::root(root)?;
        let Some((last, parents)) = names.split_last() else {
            return Ok(Ty::Dir);
        };
        for name in parents {
            dir = dir.child(name, true)?;
        }
        dir.stat(last, true)
    })();
    match found {
        Ok(found) => Ok(Some(found)),
        Err(e) if ends_branch(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Lists the directory `path` (opened as `dir`) with its entries' types,
/// hidden ones included: see [`shown`]. A failure while reading it is an
/// error; one opening it was the caller's to judge. An entry the filesystem
/// does not type is `lstat`ed relative to `dir`; one gone meanwhile is left
/// out, any other failure is an error.
fn list_dir(path: &str, dir: &handle::Handle) -> std::io::Result<Vec<Listed>> {
    #[cfg(test)]
    LISTINGS.with(|n| n.set(n.get().saturating_add(1)));
    #[cfg(test)]
    if FAIL_LISTING.with(|f| f.borrow().as_deref().is_some_and(|d| path.ends_with(d))) {
        return Err(std::io::Error::from_raw_os_error(5));
    }
    let mut entries = Vec::new();
    for entry in dir.list()? {
        // A large directory is enumerated entry by entry: stop there too.
        if super::offload::cancelled() {
            return Ok(Vec::new());
        }
        #[cfg(test)]
        let entry = match entry {
            handle::Entry::Named(name, _) if NO_DTYPE.with(std::cell::Cell::get) => {
                handle::Entry::Named(name, None)
            }
            other => other,
        };
        let (name, kind) = match entry {
            handle::Entry::Named(name, Some(kind)) => (name, Some(kind)),
            handle::Entry::Named(name, None) => match dir.stat(&name, false) {
                Ok(kind) => (name, Some(kind)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            },
            handle::Entry::Lossy(name) => (name, None),
        };
        let path = join_seg(path, &name);
        entries.push(Listed { name, path, kind });
    }
    Ok(entries)
}

#[cfg(test)]
thread_local! {
    /// Injects `EIO` when listing a directory whose path ends with this
    /// suffix, to test the walk's error handling.
    static FAIL_LISTING: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Makes listings report no entry types, as some filesystems do.
    static NO_DTYPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
thread_local! {
    /// Directories listed on this thread, to test that pruning saves them.
    static LISTINGS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Turns pruning off on this thread: every exclusion is expanded, the
    /// reference pruning must agree with.
    static NO_PRUNING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
thread_local! {
    /// How often this thread's walks took each edge case, by name.
    static HITS: std::cell::RefCell<std::collections::BTreeMap<&'static str, u64>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// Counts an edge case on this thread ([`HITS`]).
#[cfg(test)]
fn hit(case: &'static str) {
    HITS.with(|h| {
        let mut h = h.borrow_mut();
        let n = h.entry(case).or_insert(0);
        *n = n.saturating_add(1);
    });
}

/// Whether exclusions may prune walks ([`Pruner`]).
#[cfg(not(test))]
fn pruning() -> bool {
    true
}

#[cfg(test)]
fn pruning() -> bool {
    !NO_PRUNING.with(std::cell::Cell::get)
}

/// Sets this thread's [`FAIL_LISTING`].
#[cfg(test)]
pub(crate) fn fail_listing(dir: Option<&str>) {
    FAIL_LISTING.with(|f| *f.borrow_mut() = dir.map(str::to_string));
}

/// Directory handles the walk moves between.
mod handle {
    use super::Ty;

    /// A listed name: valid UTF-8 with its type when the listing gave one, or
    /// a lossy spelling.
    pub(super) enum Entry {
        Named(String, Option<Ty>),
        Lossy(String),
    }

    #[cfg(not(unix))]
    pub(super) use other::{Handle, Id};
    #[cfg(unix)]
    pub(super) use unix::{Handle, Id};

    /// Handles are open directories; lookups are relative to them, one name
    /// at a time.
    #[cfg(unix)]
    mod unix {
        use super::{Entry, Ty};
        use rustix::fd::OwnedFd;
        use rustix::fs::{AtFlags, FileType, Mode, OFlags};

        /// A directory's device and inode.
        pub(in super::super) type Id = (u64, u64);

        pub(in super::super) struct Handle(OwnedFd);

        fn ty(ft: FileType) -> Option<Ty> {
            match ft {
                FileType::Directory => Some(Ty::Dir),
                FileType::RegularFile => Some(Ty::File),
                FileType::Symlink => Some(Ty::Link),
                FileType::Unknown => None,
                _ => Some(Ty::Other),
            }
        }

        /// Flags opening a directory to search it, not read it: `O_PATH`
        /// where there is one, so a directory without read permission can
        /// still be crossed; elsewhere a plain read-only open.
        fn flags(follow: bool) -> OFlags {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let access = OFlags::PATH;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            let access = OFlags::RDONLY;
            let base = access | OFlags::DIRECTORY | OFlags::CLOEXEC;
            if follow {
                base
            } else {
                base | OFlags::NOFOLLOW
            }
        }

        /// `st_dev`/`st_ino` as `u64` whatever their platform type (macOS has
        /// a signed `st_dev`); only compared for equality.
        #[allow(clippy::unnecessary_cast, clippy::cast_sign_loss)]
        fn id_of(st: &rustix::fs::Stat) -> Id {
            (st.st_dev as u64, st.st_ino as u64)
        }

        impl Handle {
            pub(in super::super) fn root(path: &str) -> std::io::Result<Self> {
                Ok(Self(rustix::fs::open(path, flags(true), Mode::empty())?))
            }

            pub(in super::super) fn id(&self) -> std::io::Result<Id> {
                Ok(id_of(&rustix::fs::fstat(&self.0)?))
            }

            /// Opens the directory `name`, following a link there if `follow`.
            pub(in super::super) fn child(
                &self,
                name: &str,
                follow: bool,
            ) -> std::io::Result<Self> {
                Ok(Self(rustix::fs::openat(
                    &self.0,
                    name,
                    flags(follow),
                    Mode::empty(),
                )?))
            }

            /// The type of `name`, following a link there if `follow`.
            pub(in super::super) fn stat(&self, name: &str, follow: bool) -> std::io::Result<Ty> {
                let at = if follow {
                    AtFlags::empty()
                } else {
                    AtFlags::SYMLINK_NOFOLLOW
                };
                let st = rustix::fs::statat(&self.0, name, at)?;
                Ok(ty(FileType::from_raw_mode(st.st_mode)).unwrap_or(Ty::Other))
            }

            pub(in super::super) fn list(&self) -> std::io::Result<Vec<Entry>> {
                let mut out = Vec::new();
                // Listing is what needs read permission: open for it here.
                let readable = rustix::fs::openat(
                    &self.0,
                    ".",
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                )?;
                for entry in rustix::fs::Dir::new(readable)? {
                    let entry = entry?;
                    let raw = entry.file_name().to_bytes();
                    if raw == b"." || raw == b".." {
                        continue;
                    }
                    out.push(match std::str::from_utf8(raw) {
                        Ok(name) => Entry::Named(name.to_string(), ty(entry.file_type())),
                        Err(_) => Entry::Lossy(String::from_utf8_lossy(raw).into_owned()),
                    });
                }
                Ok(out)
            }
        }
    }

    /// Without directory handles, lookups go through the path: loops are
    /// told apart by canonical paths.
    #[cfg(not(unix))]
    mod other {
        use super::{Entry, Ty};
        use std::path::PathBuf;

        pub(in super::super) type Id = PathBuf;

        pub(in super::super) struct Handle(PathBuf);

        impl Handle {
            pub(in super::super) fn root(path: &str) -> std::io::Result<Self> {
                let p = PathBuf::from(path);
                if !std::fs::metadata(&p)?.is_dir() {
                    return Err(std::io::ErrorKind::NotADirectory.into());
                }
                Ok(Self(p))
            }

            pub(in super::super) fn id(&self) -> std::io::Result<Id> {
                std::fs::canonicalize(&self.0)
            }

            pub(in super::super) fn child(
                &self,
                name: &str,
                follow: bool,
            ) -> std::io::Result<Self> {
                let p = self.0.join(name);
                let meta = if follow {
                    std::fs::metadata(&p)?
                } else {
                    std::fs::symlink_metadata(&p)?
                };
                if !meta.is_dir() {
                    return Err(std::io::ErrorKind::NotADirectory.into());
                }
                Ok(Self(p))
            }

            pub(in super::super) fn stat(&self, name: &str, follow: bool) -> std::io::Result<Ty> {
                let p = self.0.join(name);
                let meta = if follow {
                    std::fs::metadata(&p)?
                } else {
                    std::fs::symlink_metadata(&p)?
                };
                Ok(Ty::of(meta.file_type()))
            }

            pub(in super::super) fn list(&self) -> std::io::Result<Vec<Entry>> {
                let mut out = Vec::new();
                for entry in std::fs::read_dir(&self.0)? {
                    let entry = entry?;
                    out.push(match entry.file_name().into_string() {
                        Ok(name) => Entry::Named(name, Some(Ty::of(entry.file_type()?))),
                        Err(raw) => Entry::Lossy(raw.to_string_lossy().into_owned()),
                    });
                }
                Ok(out)
            }
        }
    }
}

/// With `dotglob` disabled bash hides names beginning with a dot, even from
/// `.*`; `**` traversal also never crosses into dot directories.
fn shown(entry: &Listed, dotglob: bool) -> bool {
    dotglob || !entry.name.starts_with('.')
}

/// Whether only a pattern's trailing `/` is left to match (`a/*/` past `*`).
fn trailing_slash(rest: &[&str]) -> bool {
    !rest.is_empty() && rest.iter().all(|s| s.is_empty())
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
#[cfg(test)]
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
#[path = "glob_legacy.rs"]
mod legacy;

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
        // The same entries as one list: the exclusion is global, but for the
        // path named.
        let flat: Vec<Glob> = list.iter().map(|g| grouped(g.clone(), 0)).collect();
        assert_eq!(
            globs(&dir, &flat).unwrap(),
            vec![join(&dir, "a/x.txt"), join(&dir, "own.tmp")]
        );
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

    /// A fingerprint names its file: only an exclusion naming it too
    /// removes it, not one of its directory.
    #[test]
    fn exclusions_keep_injected_fingerprint_file() {
        let dir = tmp();
        write_file(&dir, "build/gen/fp.txt", "f");
        write_file(&dir, "src/a.rs", "a");
        let want = vec![join(&dir, "build/gen/fp.txt"), join(&dir, "src/a.rs")];
        for exclude in ["build/gen/**/*", "build/gen/**", "**/*.txt"] {
            let patterns = [
                g("**/*.rs"),
                g_fp("build/**/*", "build/gen/fp.txt"),
                g_neg(exclude),
            ];
            assert_eq!(globs(&dir, &patterns).unwrap(), want, "{exclude}");
        }
        let named = [
            g("**/*.rs"),
            g_fp("build/**", "build/gen/fp.txt"),
            g_neg("build/gen/fp.txt"),
        ];
        assert_eq!(globs(&dir, &named).unwrap(), vec![join(&dir, "src/a.rs")]);
    }

    #[test]
    fn cache_globs_exclude_keeps_injected_fingerprint_file() {
        let dir = tmp();
        write_file(&dir, "build/gen/fp.txt", "f");
        write_file(&dir, "build/gen/other.txt", "o");
        write_file(&dir, "build/out.js", "o");
        let patterns = [
            g_fp("build/**/*", "build/gen/fp.txt"),
            g_neg("build/gen/**/*"),
        ];
        let files = cache_globs(&dir, &patterns).unwrap();
        assert_eq!(
            files,
            vec![join(&dir, "build/gen/fp.txt"), join(&dir, "build/out.js")]
        );
        let matcher = CacheMatcher::new(&dir, &patterns);
        assert!(matcher.matches(&dir, "build/gen/fp.txt"));
        assert!(!matcher.matches(&dir, "build/gen/other.txt"));
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
        for p in &joined {
            assert!(Pruner::new(p).is_some(), "{p} should prune");
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

    /// An exclusion matches dot names a wildcard include reaches; a dot
    /// include outside it leaves the walk pruned. A path an include names
    /// stays.
    #[test]
    fn exclusions_match_dot_entries() {
        let dir = tmp();
        write_file(&dir, "target/a.o", "a");
        write_file(&dir, "target/.keep/b.o", "b");
        write_file(&dir, "target/.c.o", "c");
        write_file(&dir, "src/.env", "e");
        write_file(&dir, "src/x.o", "x");
        let list = [
            g("**/*.o"),
            g("target/.keep/*.o"),
            g("target/.c.o"),
            g("src/.env"),
            g_neg("target/**/*"),
        ];
        let (shell, tree, _, _) = agreed(&dir, &list);
        assert_eq!(
            shell,
            [
                join(&dir, "src/.env"),
                join(&dir, "src/x.o"),
                join(&dir, "target/.c.o")
            ]
        );
        assert_eq!(tree, shell);
        // `**` matches dot directories, `*` dot files.
        let list = [g("src/*"), g("src/.*"), g_neg("**/.*")];
        assert_eq!(agreed(&dir, &list).0, [join(&dir, "src/x.o")]);
        let list = [g("target/.*/*.o"), g("src/x.o"), g_neg("**/b.o")];
        assert_eq!(agreed(&dir, &list).0, [join(&dir, "src/x.o")]);
        // Named: kept, but by an exclusion naming it.
        let list = [g("src/.env"), g("src/x.o"), g_neg("**/.*")];
        assert_eq!(
            agreed(&dir, &list).0,
            [join(&dir, "src/.env"), join(&dir, "src/x.o")]
        );
        let list = [g("src/.env"), g("src/x.o"), g_neg("src/.env")];
        assert_eq!(agreed(&dir, &list).0, [join(&dir, "src/x.o")]);
    }

    #[test]
    fn pruner_classification() {
        let new = Pruner::new;
        assert!(new("/w/target/**/*").is_some());
        assert!(new("/w/target/**/**/*").is_some());
        assert!(new("/w/*/target/**/*").is_some());
        assert!(new("/w/**/target/**/*").is_some());
        assert!(new("/w/.cache/**/*").is_some());
        // These patterns do not cover a whole directory.
        assert!(new("/w/target/**/*.o").is_none());
        assert!(new("/w/target/*").is_none());
        // Both forms cover the same files.
        assert!(new("/w/target/**").is_some());
        assert!(new("/w/target/**/**").is_some());
        // This covers each child directory at depth 2 or greater under `target`.
        assert!(new("/w/target/**/*/**/*").is_some());
        // These would prune the walk root.
        assert!(new("/**/*").is_none());
        assert!(new("**/*").is_none());
        assert!(new("//**").is_none());
    }

    #[test]
    fn pruner_covers() {
        let p = Pruner::new("/w/**/target*/**/*").unwrap();
        assert!(p.covers("/w/target"));
        assert!(p.covers("/w/a/b/target-x"));
        // `**` spans dot directories, as the exclusion matches them.
        assert!(p.covers("/w/a/.hidden/target"));
        assert!(!p.covers("/w/a/target/sub"));
        assert!(!p.covers("/w/a/starget"));
        assert!(!p.covers("/w"));
        let rel = Pruner::new("target/**/*").unwrap();
        assert!(rel.covers("./target"));
        assert!(!rel.covers("./src/target"));
        let sep = Pruner::new("/w//a/**").unwrap();
        assert!(sep.covers("/w/a"));
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

    /// A link at a directory an exclusion names is dropped by the walk only
    /// when it leads to one; the matcher, which cannot tell, accepts it
    /// either way, and still rejects what lies below.
    #[cfg(unix)]
    #[test]
    fn cache_matcher_accepts_links_directory_exclusions_name() {
        let dir = tmp();
        write_file(&dir, "target/file.txt", "data");
        write_file(&dir, "a.txt", "a");
        std::os::unix::fs::symlink("target", join(&dir, "dlink")).unwrap();
        std::os::unix::fs::symlink("a.txt", join(&dir, "flink")).unwrap();
        for exclude in ["*/**", "*/"] {
            let list = [g("*"), g_neg(exclude)];
            assert_eq!(
                cache_globs(&dir, &list).unwrap(),
                vec![join(&dir, "a.txt"), join(&dir, "flink")],
                "{exclude}"
            );
            let matcher = CacheMatcher::new(&dir, &list);
            for rel in ["dlink", "flink", "a.txt"] {
                assert!(matcher.matches(&dir, rel), "{rel} with {exclude}");
            }
            assert!(!matcher.matches(&dir, "dlink/file.txt"), "{exclude}");
        }
    }

    /// An exclusion written without wildcards matches by equality, even when
    /// the task directory's own name holds glob characters (which the walk of
    /// an include still reads as a pattern).
    #[test]
    fn literal_exclusions_match_by_equality() {
        let root = tmp();
        let dir = join(&root, "proj[old]");
        write_file(&dir, "build/v.txt", "v");
        let path = join(&dir, "build/v.txt");
        let exclusion = Exclusion::new(&path, true, true, false);
        assert_eq!(exclusion.literal.as_deref(), Some(path.as_str()));
        let mut exclusions = Exclusions::new(vec![exclusion]);
        assert!(exclusions.names(&path, &mut Scan::default()));
        let dir_exclusion = Exclusion::new(&join(&dir, "build/"), true, true, false);
        assert!(Matcher::default().excludes(
            &dir_exclusion,
            &join(&dir, "build"),
            false,
            &mut Scan::default()
        ));
        let matcher = CacheMatcher::new(&dir, &[g("build/v.txt"), g_neg("build/*")]);
        assert!(matcher.matches(&dir, "build/v.txt"));
        let matcher = CacheMatcher::new(&dir, &[g("build/v.txt"), g_neg("build/v.txt")]);
        assert!(!matcher.matches(&dir, "build/v.txt"));
    }

    /// A trailing `/` or `**` matches a directory by a lookup of the path as
    /// listed; a file the listing typed is not looked up at all.
    #[test]
    fn directory_exclusions_look_up_listed_paths() {
        let dir = tmp();
        write_file(&dir, "sub/x.txt", "x");
        let scan = &mut Scan::default();
        let mut m = Matcher::default();
        for pattern in ["sub/", "sub/**", "s*/"] {
            let e = Exclusion::new(&join(&dir, pattern), !pattern.contains('*'), true, false);
            assert!(m.excludes(&e, &join(&dir, "sub"), false, scan), "{pattern}");
            let below = m.excludes(&e, &join(&dir, "sub/x.txt"), false, scan);
            assert_eq!(below, pattern.ends_with("**"), "{pattern}");
        }
        let file = join(&dir, "sub/x.txt");
        let e = Exclusion::new(&join(&dir, "sub/x.txt/"), true, true, false);
        scan.hints.insert(file.clone(), Hint::File);
        HITS.with(|h| h.borrow_mut().clear());
        assert!(!m.excludes(&e, &file, false, scan));
        assert!(!HITS.with(|h| h.borrow().contains_key("directory check")));
        assert!(scan.result(()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn cache_exclusions_match_literal_paths_through_symlinks() {
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

        // The exclusion's own walk would not cross the link; it excludes
        // what it matches however the include reached it, as a restore
        // checks it.
        for pattern in ["out/link/*", "out/**/link/*"] {
            let include = g(pattern);
            assert_eq!(
                cache_globs(&dir, std::slice::from_ref(&include)).unwrap(),
                vec![join(&dir, "out/link/file.txt")]
            );
            let list = [include, g_neg("out/**/*")];
            assert!(cache_globs(&dir, &list).unwrap().is_empty(), "{pattern}");
            assert!(!CacheMatcher::new(&dir, &list).matches(&dir, "out/link/file.txt"));
        }

        // A path named is kept, as a restore accepts it.
        let mut include = g("out/**/*");
        include.fingerprint = "out/link/file.txt".into();
        for include in [g("out/link/file.txt"), include] {
            let list = [include, g_neg("out/**/*")];
            assert_eq!(
                cache_globs(&dir, &list).unwrap(),
                vec![join(&dir, "out/link/file.txt")]
            );
            assert!(CacheMatcher::new(&dir, &list).matches(&dir, "out/link/file.txt"));
        }
    }

    #[test]
    fn wildcard_skips_dotfiles() {
        let dir = tmp();
        write_file(&dir, "visible.txt", "v");
        write_file(&dir, ".hidden", "h");
        let files = glob(&dir, "*").unwrap();
        assert_eq!(files, vec![join(&dir, "visible.txt")]);
    }

    /// A tree with what a walk can trip over: hidden entries, symlinks to a
    /// file, a directory, nothing, a loop and a chain, a FIFO, odd and non-UTF-8
    /// names, an unreadable directory, plus seeded random files.
    ///
    /// It sits one level below a fresh directory, so `../*` lists only that
    /// directory, not the temp dir other tests write to.
    fn tricky_tree(seed: u64) -> String {
        let dir = join(&tmp(), "root");
        for f in [
            "a/x.txt",
            "a/b/y.txt",
            "a/b/c/z.c",
            "a/.h/hid.txt",
            ".hid/top.txt",
            ".dot",
            "c/sp ace.txt",
            "c/br[a]ck.txt",
            "c/st*r.txt",
            "c/q?.txt",
            "d/e/f/g/deep.c",
            "keep.tmp",
            "a/b/drop.tmp",
            "shared/s.txt",
        ] {
            write_file(&dir, f, f);
        }
        std::fs::create_dir_all(join(&dir, "empty/inner")).unwrap();
        let mut x = seed;
        for i in 0..40 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let d = ["a", "a/b", "c", "d/e", ".hid", "r"][(x % 6) as usize];
            let ext = ["txt", "c", "tmp", "md"][((x >> 8) % 4) as usize];
            write_file(&dir, &format!("{d}/r{i}.{ext}"), "r");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("x.txt", join(&dir, "a/flink")).unwrap();
            symlink("../a", join(&dir, "c/dlink")).unwrap();
            symlink("nowhere", join(&dir, "c/dangling")).unwrap();
            symlink(".", join(&dir, "d/loop")).unwrap();
            symlink("flink", join(&dir, "a/chain")).unwrap();
            symlink("../.hid", join(&dir, "c/hidlink")).unwrap();
            let fifo = std::ffi::CString::new(join(&dir, "c/pipe")).unwrap();
            // SAFETY: a valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            use std::os::unix::ffi::OsStrExt;
            let raw = std::path::Path::new(&dir).join(std::ffi::OsStr::from_bytes(b"c/n\xffu.txt"));
            std::fs::write(raw, "x").unwrap();
            write_file(&dir, "locked/in.txt", "x");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(join(&dir, "locked"), std::fs::Permissions::from_mode(0o000))
                .unwrap();
        }
        dir
    }

    /// Entry lists the current walker must agree with the old one on.
    fn corpus_lists(dir: &str) -> Vec<Vec<Glob>> {
        let patterns = [
            "**/*",
            "**",
            "a/**",
            "**/*.c",
            "*/x.txt",
            "*/*",
            ".*",
            "*",
            "[ac]/*",
            "?/*",
            "c/*",
            "a/b/**/*",
            "d/**",
            "c/dlink/**/*",
            "c/dlink/*",
            "c/hidlink/*",
            "a/x.txt",
            "a/flink",
            ".hid/top.txt",
            "locked/*",
            "locked/in.txt",
            "../*",
            "empty/**",
            "c/sp ace.txt",
            "nope/**/*",
            "*/",
            "**/",
            "c/dlink/",
            "a/x.txt/",
        ];
        let mut lists: Vec<Vec<Glob>> = patterns.iter().map(|p| vec![g(p)]).collect();
        let abs = format!("{dir}/a/**/*");
        lists.push(vec![g(&abs)]);
        lists.push(vec![g("**/*"), g_neg("**/*.tmp")]);
        lists.push(vec![g("**/*"), g_neg("a/**/*")]);
        lists.push(vec![g("**/*"), g_neg("a/**")]);
        lists.push(vec![g("**/*"), g_neg("c/**/")]);
        lists.push(vec![g("c/*"), g_neg("c/dlink/")]);
        lists.push(vec![g("a/**/*"), g_neg("a/.h/**/*"), g("a/.h/hid.txt")]);
        lists.push(vec![g_fp("a/**/*", "keep.tmp"), g("c/*")]);
        lists.push(vec![g_fp("d/**/*", "a/x.txt"), g_neg("a/**/*")]);
        lists.push(vec![
            grouped(g("**/*.txt"), 1),
            grouped(g_neg("a/**/*"), 1),
            grouped(g("a/**/*"), 2),
            g_neg("**/*.tmp"),
        ]);
        lists
    }

    /// Only what the shell's globbing shrugs off ends a branch; resource and
    /// I/O errors fail the walk instead of leaving entries out.
    #[cfg(unix)]
    #[test]
    fn only_shell_like_errors_end_a_branch() {
        let e = std::io::Error::from_raw_os_error;
        for ok in [libc::ENOENT, libc::EACCES, libc::ENOTDIR, libc::ELOOP] {
            assert!(ends_branch(&e(ok)), "{ok}");
        }
        for fail in [
            libc::EMFILE,
            libc::ENFILE,
            libc::EIO,
            libc::ENOMEM,
            libc::ENAMETOOLONG,
        ] {
            assert!(!ends_branch(&e(fail)), "{fail}");
        }
        let mut scan = Scan::default();
        scan.fail("/x", e(libc::EMFILE));
        scan.fail("/y", e(libc::EIO));
        let err = scan.result(()).unwrap_err();
        assert!(err.to_string().starts_with("/x: "), "{err}");
    }

    /// `n` distinct directories, each reached through a symlink in the one
    /// before (`l0/l1/…`), the last holding `f`.
    #[cfg(unix)]
    fn link_chain(n: usize) -> String {
        let dir = join(&tmp(), "root");
        for i in 0..n {
            write_file(&dir, &format!("r{i}/keep"), "k");
            let link = if i == 0 {
                join(&dir, "l0")
            } else {
                join(&dir, &format!("r{}/l{i}", i - 1))
            };
            std::os::unix::fs::symlink(join(&dir, &format!("r{i}")), link).unwrap();
        }
        write_file(&dir, &format!("r{}/f", n - 1), "f");
        dir
    }

    /// More links behind a path than the kernel's path-wide limit allows, by
    /// literal segments and by wildcards: every lookup crosses one.
    #[cfg(unix)]
    #[test]
    fn deep_link_chains_are_walked() {
        let n = 45;
        let dir = link_chain(n);
        let literal: Vec<String> = (0..n).map(|i| format!("l{i}")).collect();
        let pattern = format!("{}/f", literal.join("/"));
        let want = join(&dir, &pattern);
        assert_eq!(
            globs(&dir, &[g(&pattern)]).unwrap(),
            std::slice::from_ref(&want)
        );
        assert!(globs(&dir, &[g("**/f")]).unwrap().contains(&want));
        // A link whose own target crosses links.
        std::os::unix::fs::symlink("l0/l1/l2", join(&dir, "via")).unwrap();
        let through = globs(&dir, &[g("via/*")]).unwrap();
        assert!(through.contains(&join(&dir, "via/keep")), "{through:?}");
    }

    /// A directory that can be searched but not read is crossed by a literal
    /// path; only a wildcard, which must list it, cannot see into it.
    #[cfg(unix)]
    #[test]
    fn search_only_directories_are_crossed_by_literals() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root reads everything
        }
        let dir = tmp();
        write_file(&dir, "x/in/f.txt", "f");
        std::fs::set_permissions(join(&dir, "x"), std::fs::Permissions::from_mode(0o111)).unwrap();
        let literal = globs(&dir, &[g("x/in/f.txt")]);
        let below = globs(&dir, &[g("x/in/*")]);
        let wildcard = globs(&dir, &[g("x/*/f.txt")]);
        std::fs::set_permissions(join(&dir, "x"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(literal.unwrap(), [join(&dir, "x/in/f.txt")]);
        assert_eq!(below.unwrap(), [join(&dir, "x/in/f.txt")]);
        assert!(wildcard.unwrap().is_empty());
    }

    /// Repeated separators mean one, relative or absolute.
    #[test]
    fn repeated_separators_are_one() {
        let dir = tmp();
        write_file(&dir, "a/x.txt", "x");
        let want = [join(&dir, "a/x.txt")];
        assert_eq!(globs(&dir, &[g("a//*.txt")]).unwrap().len(), 1);
        assert_eq!(
            globs(&dir, &[g(&format!("{dir}//a//*.txt"))])
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            globs(&dir, &[g(&format!("{dir}/a//x.txt"))]).unwrap().len(),
            1
        );
        assert_eq!(globs(&dir, &[g("a/x.txt")]).unwrap(), want);
    }

    /// A fingerprint file behind more links than the kernel resolves in one
    /// lookup is found, as the walk finds files there.
    #[cfg(unix)]
    #[test]
    fn fingerprint_files_behind_link_chains_are_found() {
        let n = 45;
        let dir = link_chain(n);
        let literal: Vec<String> = (0..n).map(|i| format!("l{i}")).collect();
        let fp = format!("{}/f", literal.join("/"));
        let found = globs(&dir, &[g_fp("nothing/*", &fp)]).unwrap();
        assert_eq!(found, [join(&dir, &fp)]);
    }

    /// A deep tree is walked with a bounded number of open directories: run
    /// in a child process with a low descriptor limit.
    #[cfg(unix)]
    #[test]
    fn deep_trees_walk_within_bounded_descriptors() {
        const DEPTH: usize = 200;
        if let Ok(dir) = std::env::var("TASKCORE_DEEP_WALK") {
            let mut lim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: valid pointers to an rlimit.
            unsafe {
                assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim), 0);
                lim.rlim_cur = 64;
                assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &lim), 0);
            }
            let deep = format!("{dir}/{}f.txt", "d/".repeat(DEPTH));
            for list in [vec![g("**/*")], vec![g("d/**/f.txt")]] {
                let found = globs(&dir, &list).unwrap();
                assert!(found.contains(&deep), "{list:?}");
            }
            return;
        }
        let dir = tmp();
        write_file(&dir, &format!("{}f.txt", "d/".repeat(DEPTH)), "f");
        write_file(&dir, "d/d/side.txt", "s");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fingerprint::glob::tests::deep_trees_walk_within_bounded_descriptors",
                "--test-threads=1",
            ])
            .env("TASKCORE_DEEP_WALK", &dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"));
    }

    /// Without entry types from the listing, entries are typed relative to
    /// their directory and walked alike, loops included.
    #[cfg(unix)]
    #[test]
    fn untyped_listings_walk_alike() {
        let dir = tricky_tree(7);
        let lists = [vec![g("**/*")], vec![g("**")], vec![g("*/*")]];
        let typed: Vec<_> = lists
            .iter()
            .map(|l| hinted_globs(&dir, l).unwrap())
            .collect();
        NO_DTYPE.with(|f| f.set(true));
        let untyped: Vec<_> = lists
            .iter()
            .map(|l| hinted_globs(&dir, l).unwrap())
            .collect();
        NO_DTYPE.with(|f| f.set(false));
        assert_eq!(typed, untyped);
        use std::os::unix::fs::PermissionsExt;
        let _ =
            std::fs::set_permissions(join(&dir, "locked"), std::fs::Permissions::from_mode(0o755));
    }

    /// A listing that fails for a reason the shell would not shrug off fails
    /// the walk, naming the directory.
    #[test]
    fn listing_errors_fail_the_walk() {
        let dir = tmp();
        write_file(&dir, "a/b/x", "x");
        fail_listing(Some("/a/b"));
        let err = globs(&dir, &[g("**/*")]).unwrap_err();
        fail_listing(None);
        assert!(err.to_string().starts_with(&join(&dir, "a/b")), "{err}");
    }

    /// The loop cut is the same on every walk while another thread renames
    /// entries inside the loop, the load that makes deep lookups flaky.
    #[cfg(unix)]
    fn loop_under_renames(walks: usize) {
        let dir = tmp();
        write_file(&dir, "d/e/f", "f");
        std::os::unix::fs::symlink(".", join(&dir, "d/loop")).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let churn: Vec<_> = (0..4)
            .map(|t| {
                let (stop, e) = (std::sync::Arc::clone(&stop), join(&dir, "d/e"));
                std::thread::spawn(move || {
                    let mut i = 0u64;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let a = format!("{e}/.x{t}-{}", i % 50);
                        let _ = std::fs::write(&a, "");
                        let _ = std::fs::rename(&a, format!("{a}r"));
                        let _ = std::fs::remove_file(format!("{a}r"));
                        i += 1;
                    }
                })
            })
            .collect();
        let want = vec![join(&dir, "d/e/f"), join(&dir, "d/loop")];
        let mut seen = Vec::new();
        for _ in 0..walks {
            seen.push(globs(&dir, &[g("**/*")]).unwrap());
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for t in churn {
            t.join().unwrap();
        }
        assert!(
            seen.iter().all(|s| *s == want),
            "{:?}",
            seen.iter().find(|s| **s != want)
        );
    }

    #[cfg(unix)]
    #[test]
    fn loop_cut_is_stable_under_renames() {
        loop_under_renames(200);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "stress: run explicitly with --ignored"]
    fn loop_cut_is_stable_under_renames_stress() {
        loop_under_renames(20_000);
    }

    /// A cancelled reading stops walking, listings it already holds included.
    #[test]
    fn cancelled_walks_stop_on_cached_listings() {
        let dir = tricky_tree(3);
        let mut scan = Scan::default();
        let pattern = join(&dir, "**/*");
        let before = expand(&pattern, &[], Options::SHELL, &mut scan);
        assert!(!before.is_empty());
        assert!(scan.listings.len() > 1);
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        super::super::offload::set_cancel(Some(flag));
        let after = expand(&pattern, &[], Options::SHELL, &mut scan);
        let fresh = globs(&dir, &[g("**/*")]).unwrap();
        super::super::offload::set_cancel(None);
        assert!(after.is_empty(), "{after:?}");
        assert!(fresh.is_empty(), "{fresh:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                join(&dir, "locked"),
                std::fs::Permissions::from_mode(0o755),
            );
        }
    }

    /// The walker lists each directory once and trusts listed types; it
    /// matches what the old walker matched, in both modes, on every list.
    #[test]
    fn walker_matches_the_old_walker() {
        for seed in [1, 5, 9] {
            let dir = tricky_tree(seed);
            // The old walker followed the loop until the kernel refused (past
            // about 20 links at a varying depth under load); the new one stops
            // at the first directory it revisits. Compared away from the loop.
            let followed = |v: Vec<String>| -> Vec<String> {
                v.into_iter().filter(|p| !p.contains("/loop")).collect()
            };
            for list in corpus_lists(&dir) {
                assert_eq!(
                    followed(globs(&dir, &list).unwrap()),
                    followed(legacy::globs(&dir, &list).unwrap()),
                    "globs {list:?}"
                );
                assert_eq!(
                    cache_globs(&dir, &list).unwrap(),
                    legacy::cache_globs(&dir, &list).unwrap(),
                    "cache_globs {list:?}"
                );
                for (f, hint) in hinted_globs(&dir, &list).unwrap() {
                    let ft = std::fs::symlink_metadata(&f).map(|m| m.file_type());
                    match hint {
                        Hint::File => assert!(ft.unwrap().is_file(), "{f}"),
                        Hint::Link => assert!(ft.unwrap().is_symlink(), "{f}"),
                        Hint::Unknown => {}
                    }
                }
            }
            // The corpus exercises what it is meant to.
            #[cfg(unix)]
            {
                let all = globs(&dir, &[g("**/*")]).unwrap();
                assert!(all.contains(&join(&dir, "c/dlink/x.txt")), "{all:?}");
                // The loop back to `d` is not followed: the link is an entry.
                let loops = all.iter().map(|p| p.matches("/loop").count()).max();
                assert_eq!(loops, Some(1), "{all:?}");
                assert!(all.contains(&join(&dir, "d/loop")), "{all:?}");
                // The same on every walk, however busy the machine.
                for _ in 0..3 {
                    assert_eq!(globs(&dir, &[g("**/*")]).unwrap(), all);
                }
                assert!(!all.iter().any(|f| f.contains(".hid")), "{all:?}");
                let tree = cache_globs(&dir, &[g("**/*")]).unwrap();
                assert!(tree.contains(&join(&dir, "c/dlink")), "{tree:?}");
                assert!(tree.contains(&join(&dir, ".hid/top.txt")), "{tree:?}");
                let hints = hinted_globs(&dir, &[g("**/*")]).unwrap();
                assert!(hints.contains(&(join(&dir, "a/x.txt"), Hint::File)));
                assert!(hints.contains(&(join(&dir, "a/flink"), Hint::Link)));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    join(&dir, "locked"),
                    std::fs::Permissions::from_mode(0o755),
                );
            }
        }
    }

    /// A seeded xorshift stream for the randomized tests.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
            from[(self.next() % from.len() as u64) as usize]
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A small random tree of what pruning has to get right: dot entries,
    /// links back up (loops), across into other trees, to files and to
    /// nothing, and directories that can be searched but not read.
    fn random_tree(rng: &mut Rng) -> (String, Vec<String>) {
        let dir = join(&tmp(), "root");
        let mut dirs = vec![String::new()];
        for _ in 0..6 + rng.below(6) {
            let parent = dirs[rng.below(dirs.len() as u64) as usize].clone();
            if parent.matches('/').count() >= 3 {
                continue;
            }
            let name = rng.pick(&["a", "b", "nm", "t", ".h"]);
            let d = if parent.is_empty() {
                name.to_string()
            } else {
                format!("{parent}/{name}")
            };
            std::fs::create_dir_all(join(&dir, &d)).unwrap();
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
        let at = |d: &str, name: &str| {
            if d.is_empty() {
                join(&dir, name)
            } else {
                join(&dir, &format!("{d}/{name}"))
            }
        };
        for d in &dirs {
            for f in ["f.txt", "g.rs", ".d"] {
                if rng.below(3) != 0 {
                    std::fs::write(at(d, f), f).unwrap();
                }
            }
        }
        let mut locked = Vec::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            for _ in 0..3 + rng.below(5) {
                let d = dirs[rng.below(dirs.len() as u64) as usize].clone();
                let other = dirs[rng.below(dirs.len() as u64) as usize].clone();
                let (name, target) = match rng.below(7) {
                    0 => ("lp", ".".to_string()),
                    1 => ("up", "..".to_string()),
                    2 => ("x", at(&other, "")),
                    3 => ("fl", "f.txt".to_string()),
                    4 => ("dg", "nowhere".to_string()),
                    // Named as directories exclusions cover.
                    _ => (rng.pick(&["nm", "t"]), at(&other, "")),
                };
                let link = at(&d, name);
                if std::fs::symlink_metadata(&link).is_err() {
                    symlink(target, link).unwrap();
                }
            }
            // SAFETY: geteuid has no preconditions.
            if unsafe { libc::geteuid() } != 0 && rng.below(3) == 0 {
                use std::os::unix::fs::PermissionsExt;
                let d = dirs[rng.below(dirs.len() as u64) as usize].clone();
                if !d.is_empty() {
                    let path = at(&d, "");
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o311))
                        .unwrap();
                    locked.push(path);
                }
            }
        }
        (dir, locked)
    }

    /// A random pattern over the names [`random_tree`] uses: absolute (under
    /// `dir`) one time in four.
    fn random_pattern(rng: &mut Rng, dir: &str, max: u64) -> String {
        let n = 1 + rng.below(max);
        let rel = (0..n)
            .map(|_| {
                rng.pick(&[
                    "a", "b", "nm", "t", ".h", "lp", "up", "x", "fl", "*", "*", "?", "[ab]", "**",
                    "**", "f.txt", "*.rs", ".d",
                ])
            })
            .collect::<Vec<_>>()
            .join("/");
        if rng.below(4) == 0 {
            join(dir, &rel)
        } else {
            rel
        }
    }

    /// A random entry list: includes, whole-tree exclusions (what prunes),
    /// other exclusions, some ending in `/`, fingerprints and excluded
    /// fingerprints, and folded groups with their own.
    fn random_list(rng: &mut Rng, dir: &str) -> Vec<Glob> {
        let mut list = Vec::new();
        // A broad include and an exclusion covering what links are named,
        // half the time: random prefixes alone rarely cover a link.
        if rng.below(2) == 0 {
            list.push(g(
                rng.pick(&["**", "*/**", "**/*", "*", "*/*", "**/**", "*/", "**/"])
            ));
            list.push(g_neg(rng.pick(&[
                "**/nm/**",
                "**/t/**/*",
                "*/nm/**/*",
                "x/**",
                "**/x/**/*",
                "**/lp/**",
                "*/up/**",
            ])));
        }
        for group in 0..1 + rng.below(3) as u32 {
            for _ in 0..1 + rng.below(3) {
                let mut inc = g(&random_pattern(rng, dir, 4));
                if rng.below(6) == 0 {
                    inc.fingerprint = random_pattern(rng, dir, 3);
                }
                list.push(grouped(inc, group));
            }
            for _ in 0..rng.below(3) {
                let tail = rng.pick(&["**", "**/*", "**/*.rs", "*", "", "**/"]);
                let prefix = random_pattern(rng, dir, 3);
                let mut exc = g_neg(&format!("{prefix}/{tail}"));
                if rng.below(10) == 0 {
                    exc = g_neg(&random_pattern(rng, dir, 2));
                    exc.fingerprint = random_pattern(rng, dir, 3);
                }
                list.push(grouped(exc, group));
            }
        }
        list
    }

    /// What an exclusion matches, spelled out without [`Matcher`]'s
    /// shortcuts: by name, a trailing `/` or a `**` matching nothing only a
    /// directory.
    fn reference_excludes(pattern: &str, path: &str, dotglob: bool) -> bool {
        let segs = normalized(pattern);
        let p = path_segments(path);
        let is_dir = || std::fs::metadata(path).is_ok_and(|m| m.is_dir());
        if pattern.ends_with('/') {
            return match_segments(&segs, &p, dotglob) && is_dir();
        }
        let mut base = segs.clone();
        while base.len() > 1 && base.last() == Some(&"**") {
            base.pop();
        }
        if base.len() == segs.len() {
            return match_segments(&segs, &p, dotglob);
        }
        (match_segments(&base, &p, dotglob) && is_dir())
            || (1..p.len()).any(|n| {
                match_segments(&base, &p[..n], dotglob)
                    && p[n..].iter().all(|s| dotglob || !s.starts_with('.'))
            })
    }

    /// Pruning is an optimisation: on random trees and entry lists, every
    /// walk lists exactly what it lists with every exclusion expanded, and
    /// the exclusion matcher's shortcuts agree with [`reference_excludes`].
    /// The edge cases each come up, not just agreement.
    #[test]
    fn pruning_lists_what_expanding_lists() {
        // SAFETY: geteuid has no preconditions.
        #[cfg(unix)]
        let root = unsafe { libc::geteuid() } == 0;
        #[cfg(not(unix))]
        let root = true;
        HITS.with(|h| h.borrow_mut().clear());
        let mut rng = Rng(0x5eed_cafe);
        let (mut pruned, mut retained, mut excluding, mut folded, mut checked) = (0, 0, 0, 0, 0);
        for _ in 0..45 {
            let (dir, locked) = random_tree(&mut rng);
            let mut paths: BTreeSet<String> = BTreeSet::new();
            paths.extend(globs(&dir, &[g("**")]).unwrap());
            paths.extend(cache_globs(&dir, &[g("**")]).unwrap());
            for _ in 0..15 {
                let list = random_list(&mut rng, &dir);
                let walk = |prune: bool| {
                    NO_PRUNING.with(|p| p.set(!prune));
                    LISTINGS.with(|n| n.set(0));
                    let shell = hinted_globs(&dir, &list).map_err(|e| e.to_string());
                    let tree = cache_globs(&dir, &list).map_err(|e| e.to_string());
                    NO_PRUNING.with(|p| p.set(false));
                    (shell, tree, LISTINGS.with(std::cell::Cell::get))
                };
                let (shell, tree, with) = walk(true);
                let (shell_ref, tree_ref, without) = walk(false);
                // Hints come from whichever walk listed a path first: only
                // their truth is compared.
                let paths_of = |r: &Result<Vec<(String, Hint)>, String>| {
                    r.clone()
                        .map(|v| v.into_iter().map(|(f, _)| f).collect::<Vec<_>>())
                };
                assert_eq!(
                    paths_of(&shell),
                    paths_of(&shell_ref),
                    "globs {list:?} in {dir}"
                );
                assert_eq!(tree, tree_ref, "cache_globs {list:?} in {dir}");
                pruned += u32::from(with < without);
                for (f, hint) in shell.iter().flatten() {
                    let ft = std::fs::symlink_metadata(f).map(|m| m.file_type());
                    match hint {
                        Hint::File => assert!(ft.unwrap().is_file(), "{f}"),
                        Hint::Link => assert!(ft.unwrap().is_symlink(), "{f}"),
                        Hint::Unknown => {}
                    }
                }
                let shell = paths_of(&shell).unwrap();
                retained += u32::from(!shell.is_empty());
                let positive: Vec<Glob> = list.iter().filter(|g| !g.negate).cloned().collect();
                excluding += u32::from(globs(&dir, &positive).unwrap().len() > shell.len());
                folded += u32::from(list.iter().any(|g| g.group > 0) && !shell.is_empty());
                // A restore accepts every entry an archive holds.
                let matcher = CacheMatcher::new(&dir, &list);
                for f in tree.unwrap() {
                    let rel = f.strip_prefix(&format!("{dir}/")).unwrap();
                    assert!(matcher.matches(&dir, rel), "{rel} with {list:?}");
                }
                // The matcher, shortcuts and all, against the reference.
                let scan = &mut Scan::default();
                let mut m = Matcher::default();
                for e in list.iter().filter(|g| g.negate) {
                    let pattern = join_pattern(&dir, &e.glob);
                    let dotglob = rng.below(2) == 0;
                    {
                        let x = Exclusion::new(&pattern, !e.has_wildcards(), dotglob, false);
                        for p in &paths {
                            let spelled = if checked % 7 == 0 {
                                p.replacen('/', "//", 2)
                            } else {
                                p.clone()
                            };
                            {
                                let p = &spelled;
                                assert_eq!(
                                    m.excludes(&x, p, false, scan),
                                    reference_excludes(&pattern, p, dotglob),
                                    "{pattern} on {p}, dotglob {dotglob}"
                                );
                                checked += 1;
                            }
                        }
                    }
                }
            }
            #[cfg(unix)]
            for path in locked {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let hits = HITS.with(|h| h.borrow().clone());
        let count = |case: &str| hits.get(case).copied().unwrap_or(0);
        for case in [
            "pruned",
            "pruned link",
            "pruned link listed",
            "pruned loop",
            "fingerprint",
            "included before, excluded",
            "directory check",
            "named",
        ] {
            assert!(count(case) > 5, "{case}: {hits:?}");
        }
        // Root reads everything: no directory is search-only then.
        if root {
            eprintln!("running as root: search-only directories not covered");
        } else {
            assert!(count("unlisted directory") > 5, "{hits:?}");
        }
        for (case, n) in [
            ("pruned", pruned),
            ("retained", retained),
            ("excluding", excluding),
            ("folded", folded),
        ] {
            assert!(n > 50, "{case}: {n}");
        }
        assert!(checked > 10_000, "{checked}");
    }

    /// `globs` and `cache_globs` of `list`, checked equal with pruning off,
    /// with the directories `globs` listed with pruning on and off.
    fn agreed(dir: &str, list: &[Glob]) -> (Vec<String>, Vec<String>, u64, u64) {
        let walk = |prune: bool| {
            NO_PRUNING.with(|p| p.set(!prune));
            LISTINGS.with(|n| n.set(0));
            let shell = globs(dir, list).unwrap();
            let listed = LISTINGS.with(std::cell::Cell::get);
            let tree = cache_globs(dir, list).unwrap();
            NO_PRUNING.with(|p| p.set(false));
            (shell, tree, listed)
        };
        let (shell, tree, with) = walk(true);
        let (shell_ref, tree_ref, without) = walk(false);
        assert_eq!(shell, shell_ref, "globs {list:?}");
        assert_eq!(tree, tree_ref, "cache_globs {list:?}");
        (shell, tree, with, without)
    }

    /// A covered directory reached through a symlink is an entry of its
    /// own: a `<prefix>/**` exclusion lists it, `<prefix>/**/*` does not,
    /// and pruning keeps both so.
    #[cfg(unix)]
    #[test]
    fn pruning_keeps_symlinked_covered_directories() {
        let dir = tmp();
        write_file(&dir, "src/a.c", "a");
        write_file(&dir, "real/b.c", "b");
        std::os::unix::fs::symlink(join(&dir, "real"), join(&dir, "src/gen")).unwrap();
        let src = |rel: &str| join(&dir, &format!("src/{rel}"));
        for (inc, exc, want) in [
            ("src/*", "src/gen/**", vec![src("a.c")]),
            ("src/*", "src/gen/**/*", vec![src("a.c"), src("gen")]),
            ("src/**", "src/gen/**", vec![src("a.c")]),
            ("src/**", "src/gen/**/*", vec![src("a.c"), src("gen")]),
            ("src/**", "src/*/**/*", vec![src("a.c"), src("gen")]),
            ("src/gen/**", "src/gen/**/*", vec![src("gen")]),
            ("src/*/**", "src/gen/**", vec![]),
            ("src/gen", "src/gen/**", vec![src("gen")]),
        ] {
            let list = [g(inc), g_neg(exc)];
            assert!(Pruner::new(&join(&dir, exc)).is_some(), "{inc} - {exc}");
            assert_eq!(agreed(&dir, &list).0, want, "{inc} - {exc}");
        }
    }

    /// A literal segment crosses a link back up, which the exclusion's
    /// own walk would not: it still excludes the path, so pruning holds.
    /// The path named in full is kept.
    #[cfg(unix)]
    #[test]
    fn exclusions_match_literal_paths_through_loops() {
        let dir = tmp();
        write_file(&dir, "build/x.o", "x");
        write_file(&dir, "src/y.o", "y");
        std::os::unix::fs::symlink(".", join(&dir, "build/again")).unwrap();
        let list = [g("build/again/*.o"), g("src/*.o"), g_neg("build/**")];
        assert!(Pruner::new(&join(&dir, "build/**")).is_some());
        assert_eq!(agreed(&dir, &list).0, [join(&dir, "src/y.o")]);
        let list = [g("build/again/x.o"), g("src/*.o"), g_neg("build/**")];
        assert_eq!(
            agreed(&dir, &list).0,
            [join(&dir, "build/again/x.o"), join(&dir, "src/y.o")]
        );
    }

    /// An exclusion spelled with `//` covers what the walk reads it as.
    #[test]
    fn exclusions_with_repeated_separators_still_exclude() {
        let dir = tmp();
        write_file(&dir, "a/b/x.c", "x");
        write_file(&dir, "a/y.c", "y");
        let (shell, tree, _, _) = agreed(&dir, &[g("**/*.c"), g_neg("a//b/**")]);
        assert_eq!(shell, [join(&dir, "a/y.c")]);
        assert_eq!(tree, [join(&dir, "a/y.c")]);
    }

    /// An exclusion the walk can prune saves listing what it covers.
    #[test]
    fn pruning_skips_covered_directories() {
        let dir = tmp();
        write_file(&dir, "src/a.rs", "a");
        write_file(&dir, "src/web/.yarnrc.yml", "y");
        write_file(&dir, ".tools/t", "t");
        for i in 0..20 {
            write_file(&dir, &format!("src/node_modules/p{i}/q/i.js"), "i");
        }
        // Dot includes, here outside the excluded tree, no longer keep it
        // walked.
        let list = [
            g("src/**/*.rs"),
            g("src/web/.yarnrc.yml"),
            g(".tools/**"),
            g_neg("src/**/node_modules/**"),
        ];
        let (shell, _, with, without) = agreed(&dir, &list);
        assert_eq!(
            shell,
            [
                join(&dir, ".tools/t"),
                join(&dir, "src/a.rs"),
                join(&dir, "src/web/.yarnrc.yml")
            ]
        );
        assert!(with < without / 4, "{with} listings vs {without}");
    }

    /// Excluding dot entries changes no checksum unless an include names
    /// one inside the excluded tree: this one is pinned from before.
    #[test]
    fn dot_exclusions_keep_checksums_without_dot_includes_inside() {
        let dir = tmp();
        for f in [
            "src/a.rs",
            "src/.env",
            "src/.cache/k.rs",
            "src/web/.yarnrc.yml",
            "node_modules/.yarn-state.yml",
            "node_modules/p/i.js",
            "node_modules/.bin/x",
            "src/node_modules/q/j.js",
            ".git/HEAD",
        ] {
            write_file(&dir, f, f);
        }
        let list = [
            g("**/*"),
            g("src/web/.yarnrc.yml"),
            g_neg("**/node_modules/**"),
            g_neg("src/.cache/**"),
        ];
        let files = agreed(&dir, &list).0;
        assert_eq!(
            files,
            [join(&dir, "src/a.rs"), join(&dir, "src/web/.yarnrc.yml")]
        );
        let sum = crate::fingerprint::checksum_files(&dir, &files, &[]).unwrap();
        assert_eq!(sum, "778b62a0d5a976fc4122d637ea23286f");
    }

    /// A trailing `/`, or a trailing `**` matching nothing, matches only a
    /// directory, as the exclusion's walk would: a file of that name stays,
    /// whether the pattern is relative or absolute.
    #[cfg(unix)]
    #[test]
    fn exclusions_keep_trailing_directory_constraint() {
        let dir = tmp();
        write_file(&dir, "input.txt", "i");
        write_file(&dir, "gen", "g");
        write_file(&dir, "real/r.txt", "r");
        std::os::unix::fs::symlink(join(&dir, "real"), join(&dir, "link")).unwrap();
        let abs = |rel: &str| join(&dir, rel);
        for (list, want) in [
            (
                vec![g("input.txt"), g_neg(&abs("input.txt/"))],
                vec![abs("input.txt")],
            ),
            (
                vec![g("input.txt"), g_neg(&abs("input.txt//"))],
                vec![abs("input.txt")],
            ),
            (
                vec![g("input.txt"), g_neg("input.txt/")],
                vec![abs("input.txt")],
            ),
            (vec![g("gen"), g_neg("*/")], vec![abs("gen")]),
            (vec![g("link"), g_neg("link/")], vec![]),
            // `**/` matches directories at any depth, not the files below.
            (
                vec![g("real/*"), g_neg("real/**/")],
                vec![abs("real/r.txt")],
            ),
            // An include ending in `/` lists a link to a directory, never a
            // file or a directory itself, in both walks.
            (vec![g("input.txt/")], vec![]),
            (vec![g("link/")], vec![abs("link")]),
            (vec![g("*/")], vec![abs("link")]),
            (vec![g("**/")], vec![abs("link")]),
            (vec![g("*/"), g_neg("link/")], vec![]),
            (vec![g("gen"), g_neg(&abs("gen/**"))], vec![abs("gen")]),
            (vec![g("gen"), g_neg("gen/**")], vec![abs("gen")]),
            (vec![g("gen"), g_neg(&abs("*/"))], vec![abs("gen")]),
            // A link to a directory is one.
            (vec![g("link"), g_neg(&abs("link/"))], vec![]),
            (vec![g("link"), g_neg("link/**")], vec![abs("link")]),
            (
                vec![g("*"), g_neg("link/**")],
                vec![abs("gen"), abs("input.txt")],
            ),
            (vec![g("link"), g_neg("link/**/*")], vec![abs("link")]),
            (
                vec![g("*"), g_neg(&abs("*/"))],
                vec![abs("gen"), abs("input.txt")],
            ),
        ] {
            let (shell, tree, _, _) = agreed(&dir, &list);
            assert_eq!(shell, want, "{list:?}");
            assert_eq!(tree, want, "tree {list:?}");
            let matcher = CacheMatcher::new(&dir, &list);
            for f in &tree {
                let rel = f.strip_prefix(&format!("{dir}/")).unwrap();
                assert!(matcher.matches(&dir, rel), "{rel} with {list:?}");
            }
        }
        // A fingerprint is a path: a trailing `/` on it is dropped.
        let list = [g_fp("gen", "gen/")];
        assert_eq!(globs(&dir, &list).unwrap(), [abs("gen")]);
        assert_eq!(cache_globs(&dir, &list).unwrap(), [abs("gen")]);
        let files = globs(&dir, &list).unwrap();
        assert!(crate::fingerprint::checksum_files(&dir, &files, &[]).is_ok());
    }

    /// The task's own whole-tree exclusions also keep folded groups' walks
    /// out of what they cover: those matches would be removed anyway.
    #[test]
    fn own_exclusions_prune_folded_groups() {
        let dir = tmp();
        write_file(&dir, "src/a.rs", "a");
        write_file(&dir, "src/web/.yarnrc.yml", "y");
        for i in 0..20 {
            write_file(&dir, &format!("src/web/node_modules/p{i}/q/i.rs"), "i");
            write_file(&dir, &format!("src/web/node_modules/p{i}/.bin/b.rs"), "b");
        }
        let list = [
            grouped(g("**/*.rs"), 1),
            grouped(g("src/web/node_modules/p0/.bin/b.rs"), 1),
            grouped(g_neg("target/**"), 1),
            grouped(g("src/**/node_modules/p1/**"), 2),
            g("src/web/.yarnrc.yml"),
            g_neg("src/**/node_modules/**"),
        ];
        let (shell, tree, with, without) = agreed(&dir, &list);
        assert_eq!(
            shell,
            [join(&dir, "src/a.rs"), join(&dir, "src/web/.yarnrc.yml")]
        );
        assert_eq!(tree, shell);
        assert!(with < without / 4, "{with} listings vs {without}");
    }
}
