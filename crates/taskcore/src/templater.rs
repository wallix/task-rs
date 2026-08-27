//! Taskfile variable interpolation.
//!
//! Taskfiles were authored against Go's `text/template` (`{{.VAR}}`, pipelines,
//! and a set of [slim-sprig] helpers). This module renders those templates with
//! [`minijinja`], which uses Jinja2 syntax. The two dialects overlap for the
//! common case — plain interpolation and simple filter application — but diverge
//! for control flow and for the long tail of sprig helpers.
//!
//! The design accepts a deliberate compatibility break: only the common case is
//! supported, and anything this module cannot render faithfully produces a clear
//! error instead of silently-wrong output. Two mechanisms enforce that:
//!
//! 1. A [preflight](preflight) scan runs before every render. It walks each
//!    `{{ ... }}` action and rejects Go control words and unmapped function
//!    calls, naming the offending construct and snippet.
//! 2. Go dotted field access (`{{.Foo.Bar}}`) is translated to the minijinja
//!    equivalent (`{{ Foo.Bar }}`). The translation is conservative: any action
//!    it cannot confidently rewrite is routed to the preflight error path.
//!
//! # Control flow
//!
//! Go's conditional actions are translated to their minijinja equivalents:
//! `{{if pipeline}}` → `{% if expr %}`, `{{else if pipeline}}` → `{% elif expr %}`,
//! `{{else}}` → `{% else %}`, and the matching `{{end}}` → `{% endif %}`. The
//! condition pipeline is translated with the same rules as an output action, so
//! `{{if eq .OS "linux"}}` becomes `{% if eq(OS, "linux") %}`.
//!
//! # Rejected Go constructs
//!
//! The preflight rejects the `text/template` control words that have no faithful
//! minijinja mapping: `range` and `with` rebind the `.` cursor to an element, and
//! `define`, `template`, and `block` are template composition. It also rejects any
//! identifier used as a function call (`{{ foo ... }}` or `... | foo`) that is
//! not in the mapped-function set below. Go comments (`{{/* … */}}`) render to
//! nothing.
//!
//! # Sealed syntax
//!
//! Go `text/template` reserves only `{{`/`}}`; `{%` and `{#` are ordinary text.
//! The environment therefore uses sentinel block/comment delimiters (see
//! [`GO_BLOCK_START`]) so a literal `{%`/`{#` in a Taskfile is passed through
//! verbatim instead of being interpreted as Jinja. The translated control-flow
//! blocks are emitted with those same sentinels.
//!
//! # Mapped functions
//!
//! These sprig / Task helpers are registered as minijinja globals and filters:
//! `OS`, `ARCH`, `numCPU`, `catLines`, `splitLines`, `fromSlash`, `toSlash`,
//! `exeExt`, `trim`, `trimAll`, `trimPrefix`, `trimSuffix`, `lower`,
//! `upper`, `title`, `contains`, `hasPrefix`, `hasSuffix`, `replace`, `quote`,
//! `squote`, `urlsafe`, `splitList`, `join`, `first`, `last`, `base`, `dir`,
//! `ext`, `isAbs`, and the Go builtins `printf` and `print`. `title`, `join`,
//! `first` and `last` are globals only, their filter spellings staying
//! minijinja's; sprig's `default` is not registered at all, since minijinja's
//! builtin `default` filter *is* it once the `boolean` argument is set, which
//! is what both Go spellings translate to. Every other sprig helper
//! (`range`-style list builders, YAML/UUID/spew helpers, shell quoting,
//! `merge`, …) is intentionally left unmapped so it hits the preflight error
//! rather than being dropped.
//!
//! [slim-sprig]: https://github.com/go-task/slim-sprig
//! [`minijinja`]: https://docs.rs/minijinja

use std::fmt;
use std::rc::Rc;

use indexmap::IndexMap;
use minijinja::value::{Rest, Value as JinjaValue};
use minijinja::{Environment, context, escape_formatter};
use serde_yaml_ng::Value as YamlValue;

use crate::ast::{Dialect, Glob, Var, Vars};

thread_local! {
    // The environments are immutable after construction and identical for every
    // Cache, so they are built once per thread and shared by cheap `Rc` clone.
    // The engine runs single-threaded (`!Send`), so a thread-local is sufficient
    // and avoids rebuilding ~45 function/filter registrations per Cache.
    static GO_ENV: Rc<Environment<'static>> = Rc::new(build_go_environment());
    static JINJA_ENV: Rc<Environment<'static>> = Rc::new(build_jinja_environment());
}

/// The helpers that stay a *call* after a pipe, taking the subject as their
/// last argument — sprig's own order — instead of becoming a minijinja filter.
///
/// The first four are sprig helpers whose meaning differs from the minijinja
/// builtin filter of the same name, so the Go meaning survives without the
/// builtins being overridden for a native Jinja Taskfile. `printf` and `print`
/// take the subject last too, and minijinja has no filter of either name to
/// pass it first. Either way `--migrate` writes a file that keeps rendering
/// what it rendered as Go.
///
/// `default` is deliberately absent: minijinja's builtin filter reproduces
/// sprig's meaning exactly once its `boolean` argument is set, so both Go
/// spellings translate to that filter instead (see [`translate_default`]) and
/// no `default` global is registered at all.
///
/// Every name here must also be in [`MAPPED_FUNCS`], which the preflight checks
/// first; one that is not would be rejected before ever reaching the rewrite.
const CALL_AFTER_PIPE: &[&str] = &["title", "join", "first", "last", "printf", "print"];

/// The set of function names this module maps to minijinja. A Go action that
/// calls any other identifier is rejected by the preflight.
const MAPPED_FUNCS: &[&str] = &[
    "OS",
    "ARCH",
    "numCPU",
    "catLines",
    "splitLines",
    "fromSlash",
    "toSlash",
    "exeExt",
    // Accepted in Go source but not registered: `translate_default` rewrites it
    // to minijinja's builtin `default` filter.
    "default",
    "trim",
    "trimAll",
    "trimPrefix",
    "trimSuffix",
    "lower",
    "upper",
    "title",
    "contains",
    "hasPrefix",
    "hasSuffix",
    "replace",
    "quote",
    "squote",
    "urlsafe",
    "splitList",
    "join",
    "first",
    "last",
    "base",
    "dir",
    "ext",
    "isAbs",
    "index",
    "eq",
    "ne",
    "lt",
    "le",
    "gt",
    "ge",
    "splitArgs",
    "len",
    "printf",
    "print",
    "joinPath",
    "trunc",
    "regexReplaceAll",
    "env",
    "and",
    "or",
    "not",
];

/// The Go `text/template` control words this module cannot render. `if`, `else`,
/// and `end` are handled by [`control_kind`] and translated to minijinja blocks;
/// these have no faithful mapping and are rejected.
const REJECTED_KEYWORDS: &[&str] = &["range", "with", "define", "template", "block"];

// Sentinel block/comment delimiters for the Go-mode environment. Go
// `text/template` reserves only `{{`/`}}`; `{%`, `{#`, etc. are literal text.
// minijinja would otherwise interpret them, so the Go renderer swaps its block
// and comment markers for sentinels containing U+0001 (which cannot appear in a
// Taskfile). [`translate`] emits control flow with these markers, while any
// literal `{%`/`{#` in the source is left untouched — matching Go. The variable
// markers stay `{{`/`}}`, which both dialects share.
const GO_BLOCK_START: &str = "\u{1}%";
const GO_BLOCK_END: &str = "%\u{1}";
const GO_COMMENT_START: &str = "\u{1}#";
const GO_COMMENT_END: &str = "#\u{1}";

/// An error raised while templating a Taskfile field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplaterError {
    /// The template uses a Go `text/template` construct that cannot be rendered.
    UnsupportedConstruct {
        /// The construct name (e.g. `"range"` or an unmapped function name).
        construct: String,
        /// The `{{ … }}` action that used it, rather than the whole template —
        /// under `--migrate` that is the entire Taskfile.
        action: String,
        /// The action's 1-based line, set when translating for migration
        /// ([`to_jinja`]), whose input is a whole Taskfile. Rendering
        /// translates one field's value at a time, whose lines say nothing
        /// about where in the Taskfile it sits.
        line: Option<usize>,
    },
    /// The translated template failed to render. Reported without a line, even
    /// under `--migrate`: a failure here is the engine's, not a construct the
    /// author has to go and rewrite.
    Render {
        /// The template string as the user wrote it, before translation, or the
        /// offending `{{ … }}` action alone when the failure is local to one.
        template: String,
        /// The underlying engine message.
        message: String,
    },
}

impl fmt::Display for TemplaterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedConstruct {
                construct,
                action,
                line,
            } => {
                write!(
                    f,
                    "template uses unsupported Go construct {construct:?} in {action:?}"
                )?;
                if let Some(line) = line {
                    write!(f, " on line {line}")?;
                }
                Ok(())
            }
            Self::Render { template, message } => {
                write!(f, "template failed to render {template:?}: {message}")
            }
        }
    }
}

impl std::error::Error for TemplaterError {}

/// Applies templates to Taskfile fields, accumulating the first error.
///
/// It mirrors the behavior of the Go `templater.Cache`: the `replace_*` methods
/// may be called repeatedly without checking for errors each time. Once one call
/// fails, [`Cache::err`] holds that first error and every subsequent call is a
/// no-op returning the input unchanged.
pub struct Cache {
    vars: Vars,
    go_env: Rc<Environment<'static>>,
    jinja_env: Rc<Environment<'static>>,
    dialect: Dialect,
    cache_map: Option<IndexMap<String, YamlValue>>,
    err: Option<TemplaterError>,
}

impl Cache {
    /// Creates a cache backed by `vars` as the variable source, defaulting to the
    /// Go dialect. Use [`Cache::set_dialect`] to render Jinja-mode strings.
    pub fn new(vars: Vars) -> Self {
        Self {
            vars,
            go_env: GO_ENV.with(Rc::clone),
            jinja_env: JINJA_ENV.with(Rc::clone),
            dialect: Dialect::Go,
            cache_map: None,
            err: None,
        }
    }

    /// Sets the template dialect for the plain-string render methods (`replace`,
    /// `replace_vec`, `replace_globs`, `resolve_ref`, and variable values). Set
    /// this to the owning task's or file's dialect before rendering its strings.
    pub fn set_dialect(&mut self, dialect: Dialect) {
        self.dialect = dialect;
    }

    /// Selects the environment for `dialect`.
    fn env_for(&self, dialect: Dialect) -> &Environment<'static> {
        match dialect {
            Dialect::Go => &self.go_env,
            Dialect::Jinja => &self.jinja_env,
        }
    }

    /// Rebuilds the cached variable map from the current [`Vars`].
    pub fn reset_cache(&mut self) {
        self.cache_map = Some(self.vars.to_cache_map());
    }

    /// Returns the first error that occurred, if any.
    pub fn err(&self) -> Option<&TemplaterError> {
        self.err.as_ref()
    }

    /// Reports whether an error has been recorded.
    pub fn is_err(&self) -> bool {
        self.err.is_some()
    }

    /// Ensures the cache map is populated, returning a reference to it.
    fn ensure_cache_map(&mut self) -> &IndexMap<String, YamlValue> {
        if self.cache_map.is_none() {
            self.cache_map = Some(self.vars.to_cache_map());
        }
        // The map was just populated above if it was absent.
        self.cache_map.get_or_insert_with(IndexMap::new)
    }

    /// Renders `tmpl` against the current variables, merging `extra` on top.
    ///
    /// If an error was already recorded, or rendering fails, the input is
    /// returned unchanged and the error is recorded (first error wins).
    pub fn replace_with_extra(
        &mut self,
        tmpl: &str,
        extra: &IndexMap<String, YamlValue>,
    ) -> String {
        if self.err.is_some() {
            return tmpl.to_string();
        }
        self.ensure_cache_map();
        // The map is present after `ensure_cache_map`.
        let base = self.cache_map.clone().unwrap_or_default();
        let mut data = base;
        for (k, v) in extra {
            data.insert(k.clone(), v.clone());
        }
        match render(self.env_for(self.dialect), self.dialect, tmpl, &data) {
            Ok(out) => out,
            Err(e) => {
                self.err = Some(e);
                tmpl.to_string()
            }
        }
    }

    /// Renders `tmpl` against the current variables.
    pub fn replace(&mut self, tmpl: &str) -> String {
        self.replace_with_extra(tmpl, &IndexMap::new())
    }

    /// Renders each string in `list`.
    pub fn replace_vec(&mut self, list: &[String]) -> Vec<String> {
        list.iter().map(|s| self.replace(s)).collect()
    }

    /// Renders the glob and fingerprint patterns of each entry, preserving the
    /// other fields. Returns an empty list on a prior error or empty input.
    pub fn replace_globs(&mut self, globs: &[Glob]) -> Vec<Glob> {
        if self.err.is_some() || globs.is_empty() {
            return Vec::new();
        }
        globs
            .iter()
            .map(|g| Glob {
                glob: self.replace(&g.glob),
                negate: g.negate,
                fingerprint: self.replace(&g.fingerprint),
                from: g.from.clone(),
            })
            .collect()
    }

    /// Resolves a `ref` expression (a bare Go action body without braces) to a
    /// value. `"."` yields the whole variable map. Returns [`YamlValue::Null`]
    /// on a prior error or on failure (recording the error).
    pub fn resolve_ref(&mut self, ref_: &str) -> YamlValue {
        if self.err.is_some() {
            return YamlValue::Null;
        }
        self.ensure_cache_map();
        let data = self.cache_map.clone().unwrap_or_default();
        if ref_ == "." {
            return YamlValue::Mapping(
                data.into_iter()
                    .map(|(k, v)| (YamlValue::String(k), v))
                    .collect(),
            );
        }
        // A plain dotted path (`.FOO.BAR`) is looked up directly so its value
        // keeps its original type (map, list, or scalar). Anything more complex
        // (pipelines, function calls) falls back to string rendering.
        if let Some(value) = lookup_path(&data, ref_) {
            return value;
        }
        let wrapped = format!("{{{{{ref_}}}}}");
        match resolve(self.env_for(self.dialect), self.dialect, &wrapped, &data) {
            Ok(value) => value,
            Err(e) => {
                self.err = Some(e);
                YamlValue::Null
            }
        }
    }

    /// Renders both static and dynamic forms of `var`, merging `extra` on top.
    /// A `ref` variable is resolved and its result stored in `value`.
    pub fn replace_var_with_extra(
        &mut self,
        var: &Var,
        extra: &IndexMap<String, YamlValue>,
    ) -> Var {
        if !var.ref_.is_empty() {
            return Var {
                value: Some(self.resolve_ref(&var.ref_)),
                ..Default::default()
            };
        }
        Var {
            value: var
                .value
                .as_ref()
                .map(|v| self.replace_yaml_value(v, extra)),
            sh: var.sh.as_ref().map(|s| self.replace_with_extra(s, extra)),
            live: var.live.clone(),
            ref_: var.ref_.clone(),
            dir: var.dir.clone(),
            dialect: var.dialect,
        }
    }

    /// Renders `var` with no extra variables.
    pub fn replace_var(&mut self, var: &Var) -> Var {
        self.replace_var_with_extra(var, &IndexMap::new())
    }

    /// Renders every variable in `vars`, merging `extra` on top. Returns `None`
    /// on a prior error or empty input, matching the Go nil-return contract.
    pub fn replace_vars_with_extra(
        &mut self,
        vars: &Vars,
        extra: &IndexMap<String, YamlValue>,
    ) -> Option<Vars> {
        if self.err.is_some() || vars.is_empty() {
            return None;
        }
        let mut new_vars = Vars::new();
        for (k, v) in vars.all() {
            let replaced = self.replace_var_with_extra(v, extra);
            new_vars.set(k.clone(), replaced);
        }
        Some(new_vars)
    }

    /// Renders every variable in `vars` with no extra variables.
    pub fn replace_vars(&mut self, vars: &Vars) -> Option<Vars> {
        self.replace_vars_with_extra(vars, &IndexMap::new())
    }

    /// Renders any string leaves inside a YAML value, leaving other scalars,
    /// lists, and maps structurally intact.
    fn replace_yaml_value(
        &mut self,
        value: &YamlValue,
        extra: &IndexMap<String, YamlValue>,
    ) -> YamlValue {
        match value {
            YamlValue::String(s) => YamlValue::String(self.replace_with_extra(s, extra)),
            YamlValue::Sequence(seq) => YamlValue::Sequence(
                seq.iter()
                    .map(|v| self.replace_yaml_value(v, extra))
                    .collect(),
            ),
            YamlValue::Mapping(map) => {
                let mut out = serde_yaml_ng::Mapping::new();
                for (k, v) in map {
                    out.insert(k.clone(), self.replace_yaml_value(v, extra));
                }
                YamlValue::Mapping(out)
            }
            other => other.clone(),
        }
    }
}

/// Renders `tmpl` to a string. In [`Dialect::Go`] the string is translated from
/// Go `text/template` first; in [`Dialect::Jinja`] it is rendered as-is. `env`
/// must be the environment matching `dialect`.
fn render(
    env: &Environment<'static>,
    dialect: Dialect,
    tmpl: &str,
    data: &IndexMap<String, YamlValue>,
) -> Result<String, TemplaterError> {
    let source = match dialect {
        Dialect::Go => translate(tmpl)?,
        Dialect::Jinja => tmpl.to_string(),
    };
    let ctx = build_context(data);
    let rendered = env
        .render_str(&source, ctx)
        .map_err(|e| TemplaterError::Render {
            template: tmpl.to_string(),
            message: e.to_string(),
        })?;
    // Go substitutes missing values with "<no value>" and Task strips it; an
    // absent minijinja variable renders empty, so no post-processing is needed.
    Ok(rendered)
}

/// Looks up a plain dotted path (`.FOO.BAR`) in `data`, returning the value with
/// its original type. Returns `None` if `ref_` is not a plain dotted path or the
/// path is not present, letting the caller fall back to string rendering.
fn lookup_path(data: &IndexMap<String, YamlValue>, ref_: &str) -> Option<YamlValue> {
    let path = ref_.strip_prefix('.')?;
    if path.is_empty() {
        return None;
    }
    let mut segments = path.split('.');
    let first = segments.next()?;
    if !is_bare_identifier(first) {
        return None;
    }
    let mut current = data.get(first)?.clone();
    for seg in segments {
        if !is_bare_identifier(seg) {
            return None;
        }
        match current {
            YamlValue::Mapping(map) => {
                current = map.get(YamlValue::String(seg.to_string()))?.clone();
            }
            _ => return None,
        }
    }
    Some(current)
}

/// Renders `wrapped` (a single `{{ ... }}` action) and returns the raw value it
/// produced rather than its string form.
fn resolve(
    env: &Environment<'static>,
    dialect: Dialect,
    wrapped: &str,
    data: &IndexMap<String, YamlValue>,
) -> Result<YamlValue, TemplaterError> {
    // Rendering to a string loses type information; for a bare reference the
    // string form is what callers store and compare, so it is preserved here.
    let out = render(env, dialect, wrapped, data)?;
    Ok(YamlValue::String(out))
}

/// Builds the minijinja context from the variable map. `serde_yaml_ng::Value`
/// serializes into the shapes minijinja expects (maps, sequences, scalars).
fn build_context(data: &IndexMap<String, YamlValue>) -> JinjaValue {
    let map: IndexMap<String, JinjaValue> = data
        .iter()
        .map(|(k, v)| (k.clone(), JinjaValue::from_serialize(v)))
        .collect();
    context!(..JinjaValue::from_serialize(&map))
}

/// Known Go template functions that take no arguments, so a bare `{{OS}}`
/// (no parentheses) is a Go signal — Jinja would call them as `{{ OS() }}`.
const GO_NULLARY_FUNCS: &[&str] = &["OS", "ARCH", "numCPU", "exeExt"];

/// Guesses the template dialect of a Taskfile from its syntax.
///
/// Returns [`Dialect::Go`] as soon as any `{{ … }}` action shows unambiguous Go
/// `text/template` syntax — leading-dot access (`{{.VAR}}`), a control word
/// (`{{if}}`/`{{range}}`/…), a Go comment (`{{/* */}}`), or a space-separated
/// call to a known Go function (`{{OS}}`, `{{trunc 48 .X}}`). None of these are
/// valid Jinja. Otherwise returns [`Dialect::Jinja`], which is both the "clearly
/// Jinja" answer and the fallback when a file has no templates or only shapes
/// that render identically in either dialect.
pub fn detect_dialect(src: &str) -> Dialect {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if starts_action(bytes, i)
            && let Ok((action, next)) = read_action(src, i)
        {
            if action_looks_go(action_body(&action).trim()) {
                return Dialect::Go;
            }
            i = next;
            continue;
        }
        let ch = src.get(i..).and_then(|s| s.chars().next());
        match ch {
            Some(c) => i = i.saturating_add(c.len_utf8()),
            None => break,
        }
    }
    Dialect::Jinja
}

/// Reports whether a `{{ … }}` action body uses syntax that is valid Go but not
/// valid Jinja. See [`detect_dialect`].
fn action_looks_go(body: &str) -> bool {
    if body.is_empty() {
        return false;
    }
    // Go comment, or a control word (if/range/with/end/else).
    if body.starts_with("/*") || control_kind(body).is_some() {
        return true;
    }
    // Dotted access anywhere: `.Foo`, `f .Foo`, `(index .M 0)`.
    if has_dotted_access(body) {
        return true;
    }
    // A call to a known Go function in Go's space-separated form. Jinja would use
    // parentheses (`OS()`, `trunc(48, x)`), so `{{OS}}` or `{{trunc 48 x}}` is Go.
    let head = body
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or_default();
    if GO_NULLARY_FUNCS.contains(&head) && body == head {
        return true;
    }
    MAPPED_FUNCS.contains(&head) && body.split_whitespace().count() > 1
}

/// Reports whether `body` contains Go dotted field access: a `.` that begins an
/// identifier and is not preceded by an identifier character, a `)`, or a quote
/// (so `x.y` attribute access, `1.5`, and `"a.b"` do not count).
///
/// The check is positional rather than literal-aware, and the preceding-byte set
/// is narrow: any dot starting an identifier and preceded by something outside
/// it counts, *including* one inside a literal (`replace("/.git", "")`). A Jinja
/// file holding such a string is therefore misread as Go. `rewrite_dots` keeps
/// the literal intact, but the dialect is decided per file, so that action —
/// and every other native Jinja pipeline in the file — fails to translate, its
/// variable rejected as an unsupported construct.
fn has_dotted_access(body: &str) -> bool {
    let b = body.as_bytes();
    for (idx, &c) in b.iter().enumerate() {
        if c != b'.' {
            continue;
        }
        let next_ident = b
            .get(idx.saturating_add(1))
            .is_some_and(|n| n.is_ascii_alphabetic() || *n == b'_');
        let prev_ident = idx
            .checked_sub(1)
            .and_then(|p| b.get(p))
            .is_some_and(|p| p.is_ascii_alphanumeric() || matches!(p, b'_' | b')' | b'"' | b'\''));
        if next_ident && !prev_ident {
            return true;
        }
    }
    false
}

/// How [`translate_impl`] emits the translated block/comment delimiters. The
/// render path uses sealed sentinels; the migration path uses readable Jinja.
struct TranslateStyle {
    /// Block start/end markers (e.g. the sentinels, or `{%`/`%}`).
    block: (&'static str, &'static str),
    /// If set, a Go comment is re-emitted as a Jinja comment with these markers;
    /// otherwise it renders to nothing (matching Go's runtime behavior).
    comment: Option<(&'static str, &'static str)>,
    /// Whether an error carries the line the offending action sits on. Only the
    /// migration translates a whole Taskfile, so only there does a line in the
    /// template correspond to a line of the file the user must open.
    locate: bool,
}

/// The style used when rendering: sentinel delimiters, comments dropped.
const RENDER_STYLE: TranslateStyle = TranslateStyle {
    block: (GO_BLOCK_START, GO_BLOCK_END),
    comment: None,
    locate: false,
};

/// The style used when migrating a Taskfile to Jinja: readable delimiters,
/// comments preserved as Jinja comments.
const MIGRATE_STYLE: TranslateStyle = TranslateStyle {
    block: ("{%", "%}"),
    comment: Some(("{#", "#}")),
    locate: true,
};

/// Rewrites a Go `text/template` string into native minijinja syntax, ready to
/// render or (with [`to_jinja`]) to write back to a migrated Taskfile.
///
/// Only the text inside `{{ ... }}` actions is examined; surrounding literal
/// text is copied verbatim. Inside each action, leading dots on identifiers
/// (`.Foo.Bar`) are stripped so field access maps onto minijinja variable
/// access, control words become `{% … %}` blocks, and any function-position
/// identifier that is not mapped is rejected.
fn translate(tmpl: &str) -> Result<String, TemplaterError> {
    translate_impl(tmpl, &RENDER_STYLE)
}

/// Converts a Go `text/template` string to native minijinja for migration,
/// preserving comments and using readable `{% %}`/`{# #}` delimiters. Errors on
/// any construct that cannot be converted (`range`/`with`/unmapped functions),
/// so the caller can flag it for manual fixup.
pub fn to_jinja(tmpl: &str) -> Result<String, TemplaterError> {
    translate_impl(tmpl, &MIGRATE_STYLE)
}

fn translate_impl(tmpl: &str, style: &TranslateStyle) -> Result<String, TemplaterError> {
    let (block_start, block_end) = style.block;
    let mut out = String::with_capacity(tmpl.len());
    let bytes = tmpl.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if starts_action(bytes, i) {
            let (action, next) = read_action(tmpl, i)?;
            let body = action_body(&action).trim();
            // A Go comment `{{/* … */}}` is dropped when rendering, or re-emitted
            // as a Jinja comment when migrating.
            if let Some(inner) = body.strip_prefix("/*").and_then(|s| s.strip_suffix("*/")) {
                if let Some((cs, ce)) = style.comment {
                    out.push_str(&format!("{cs}{inner}{ce}"));
                }
                i = next;
                continue;
            }
            match control_kind(body) {
                // Conditional actions become `{% … %}` blocks. The `end` maps to
                // `endif` because `range`/`with` (the other openers) are rejected,
                // so an open block is always an `if`.
                Some(Control::If(cond)) => {
                    let c =
                        translate_action(cond, &action).map_err(|e| at_line(e, style, tmpl, i))?;
                    out.push_str(&format!("{block_start} if {} {block_end}", c.trim()));
                }
                Some(Control::ElseIf(cond)) => {
                    let c =
                        translate_action(cond, &action).map_err(|e| at_line(e, style, tmpl, i))?;
                    out.push_str(&format!("{block_start} elif {} {block_end}", c.trim()));
                }
                Some(Control::Else) => {
                    out.push_str(&format!("{block_start} else {block_end}"));
                }
                Some(Control::End) => {
                    out.push_str(&format!("{block_start} endif {block_end}"));
                }
                Some(Control::Rejected(word)) => {
                    return Err(TemplaterError::UnsupportedConstruct {
                        construct: word,
                        action: action.clone(),
                        line: line_at(style, tmpl, i),
                    });
                }
                None => {
                    let translated_body =
                        translate_action(body, &action).map_err(|e| at_line(e, style, tmpl, i))?;
                    out.push_str("{{");
                    // Preserve a single interior space for readability.
                    out.push(' ');
                    out.push_str(translated_body.trim());
                    out.push(' ');
                    out.push_str("}}");
                }
            }
            i = next;
        } else {
            // Copy one UTF-8 code point verbatim.
            let ch = tmpl.get(i..).and_then(|s| s.chars().next());
            match ch {
                Some(c) => {
                    out.push(c);
                    i = i.saturating_add(c.len_utf8());
                }
                None => break,
            }
        }
    }
    Ok(out)
}

/// A Go `text/template` control action, as classified by [`control_kind`].
enum Control<'a> {
    /// `{{if pipeline}}` — the `pipeline` is the condition expression.
    If(&'a str),
    /// `{{else if pipeline}}` — the `pipeline` is the condition expression.
    ElseIf(&'a str),
    /// `{{else}}`.
    Else,
    /// `{{end}}`.
    End,
    /// A control word with no faithful minijinja mapping (see [`REJECTED_KEYWORDS`]).
    Rejected(String),
}

/// Classifies an action body as a control action, or `None` for an output
/// action (a value or pipeline that renders to text).
fn control_kind(body: &str) -> Option<Control<'_>> {
    if let Some(cond) = strip_keyword(body, "if") {
        return Some(Control::If(cond));
    }
    if let Some(rest) = strip_keyword(body, "else") {
        if rest.is_empty() {
            return Some(Control::Else);
        }
        if let Some(cond) = strip_keyword(rest, "if") {
            return Some(Control::ElseIf(cond));
        }
        // `{{else <anything-but-if>}}` is malformed in Go too.
        return Some(Control::Rejected("else".to_string()));
    }
    if strip_keyword(body, "end").is_some() {
        return Some(Control::End);
    }
    for kw in REJECTED_KEYWORDS {
        if strip_keyword(body, kw).is_some() {
            return Some(Control::Rejected((*kw).to_string()));
        }
    }
    None
}

/// If `s` begins with the whole word `kw` (followed by whitespace or end of
/// string, so `if` does not match `ifname`), returns the remainder with leading
/// whitespace trimmed; otherwise `None`.
fn strip_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(kw)?;
    match rest.chars().next() {
        None => Some(""),
        Some(c) if c.is_whitespace() => Some(rest.trim_start()),
        _ => None,
    }
}

/// Reports whether an action delimiter `{{` starts at `i`.
fn starts_action(bytes: &[u8], i: usize) -> bool {
    matches!(
        (bytes.get(i), bytes.get(i.saturating_add(1))),
        (Some(b'{'), Some(b'{'))
    )
}

/// Reads a full `{{ ... }}` action starting at `start`, returning the action
/// text (including delimiters) and the index just past it. An unterminated
/// action is a render error.
fn read_action(tmpl: &str, start: usize) -> Result<(String, usize), TemplaterError> {
    let rest = tmpl.get(start..).unwrap_or_default();
    match rest.find("}}") {
        Some(rel_end) => {
            let end = start.saturating_add(rel_end).saturating_add(2);
            let action = tmpl.get(start..end).unwrap_or_default().to_string();
            Ok((action, end))
        }
        None => Err(TemplaterError::Render {
            template: tmpl.to_string(),
            message: "unterminated \"{{\" action".to_string(),
        }),
    }
}

/// Stamps the line the failing action starts on onto an unsupported-construct
/// error. The error is raised where the action body is translated, which knows
/// the action but not where in the template it sits — and under `--migrate` the
/// template is the whole Taskfile.
fn at_line(
    err: TemplaterError,
    style: &TranslateStyle,
    tmpl: &str,
    offset: usize,
) -> TemplaterError {
    match err {
        TemplaterError::UnsupportedConstruct {
            construct, action, ..
        } => TemplaterError::UnsupportedConstruct {
            construct,
            action,
            line: line_at(style, tmpl, offset),
        },
        other => other,
    }
}

/// The 1-based line of byte `offset` in `tmpl`, or `None` when the style does
/// not locate errors or the offset is past the end.
fn line_at(style: &TranslateStyle, tmpl: &str, offset: usize) -> Option<usize> {
    if !style.locate {
        return None;
    }
    let before = tmpl.as_bytes().get(..offset)?;
    Some(
        before
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count()
            .saturating_add(1),
    )
}

/// Returns the body of an action with the `{{`/`}}` delimiters and any Go
/// trim markers (`{{-`, `-}}`) removed.
fn action_body(action: &str) -> &str {
    let inner = action
        .strip_prefix("{{")
        .and_then(|s| s.strip_suffix("}}"))
        .unwrap_or(action);
    let inner = inner.strip_prefix('-').unwrap_or(inner);
    inner.strip_suffix('-').unwrap_or(inner)
}

/// Translates a single action body, rejecting unsupported constructs. `action`
/// is the enclosing `{{ … }}` text, quoted in any error so it points at the
/// offending action rather than at the whole template. Where in the template
/// the action sits is not known here; the caller stamps it with [`at_line`].
///
/// The body is split into `|`-delimited pipeline segments. The head segment is
/// a variable reference or a function call; each following segment is a filter.
/// Go's space-separated call syntax (`f a b`) is rewritten to minijinja's call
/// syntax (`f(a, b)`), and the sprig value-last convention is honored by
/// registering multi-argument filters with the piped value as their first
/// parameter (which is what minijinja supplies).
///
/// The exception is [`CALL_AFTER_PIPE`]: after a pipe those become a *call*
/// taking the expression built so far as their last argument, because the
/// minijinja filter of each of those names means something else, or does not
/// exist.
fn translate_action(body: &str, action: &str) -> Result<String, TemplaterError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    // A leading control word is Go flow control and cannot be rendered.
    let head_word = trimmed
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or_default();
    if REJECTED_KEYWORDS.contains(&head_word) {
        return Err(TemplaterError::UnsupportedConstruct {
            construct: head_word.to_string(),
            action: action.to_string(),
            line: None,
        });
    }

    let segments = split_pipeline(trimmed);
    // The expression built so far. A segment after a pipe either becomes a
    // filter applied to it, or — for the helpers that minijinja spells
    // differently — a call taking it as the last argument.
    let mut expr = String::new();
    for (idx, segment) in segments.iter().enumerate() {
        let tokens = tokenize(segment);
        let Some(first) = tokens.first() else {
            return Err(TemplaterError::Render {
                template: action.to_string(),
                message: "empty pipeline segment".to_string(),
            });
        };
        let args = tokens.get(1..).unwrap_or_default();
        // Go's `and`/`or`/`not` builtins are minijinja keywords, so they cannot
        // be rendered as function calls; translate them to the operator form.
        // Go returns the operand value while minijinja yields a bool, but both
        // are equivalent under the truthiness these are used for.
        if matches!(first.as_str(), "and" | "or" | "not") {
            let rendered_args: Vec<String> = args
                .iter()
                .map(|a| translate_arg(a, action))
                .collect::<Result<_, _>>()?;
            let rendered = match first.as_str() {
                "not" => format!("(not {})", rendered_args.join(" ")),
                op => format!("({})", rendered_args.join(&format!(" {op} "))),
            };
            expr = join_segment(expr, &rendered);
            continue;
        }
        if is_bare_identifier(first) {
            // A bare identifier in either position is a function/filter call.
            if !MAPPED_FUNCS.contains(&first.as_str()) {
                return Err(TemplaterError::UnsupportedConstruct {
                    construct: first.clone(),
                    action: action.to_string(),
                    line: None,
                });
            }
            let mut rendered_args: Vec<String> = args
                .iter()
                .map(|a| translate_arg(a, action))
                .collect::<Result<_, _>>()?;
            // `default` is the one helper with no call spelling in the target
            // environment, so both Go forms become the builtin filter instead.
            if first == "default" {
                expr = translate_default(idx, rendered_args, expr, action)?;
                continue;
            }
            // After a pipe, these take the piped value as their last argument
            // — sprig's own order — instead of becoming a minijinja filter,
            // whose builtin of the same name means something else, or is absent.
            if idx != 0 && CALL_AFTER_PIPE.contains(&first.as_str()) {
                rendered_args.push(expr);
                expr = format!("{first}({})", rendered_args.join(", "));
                continue;
            }
            let rendered = format!("{first}({})", rendered_args.join(", "));
            expr = join_segment(expr, &rendered);
        } else {
            // The head is a value expression (`.Foo`, a literal, a parenthesized
            // sub-call, …). Values are only valid as the first segment; a value
            // after a pipe is invalid.
            if idx != 0 || !args.is_empty() {
                return Err(TemplaterError::Render {
                    template: action.to_string(),
                    message: format!("cannot translate pipeline segment {segment:?}"),
                });
            }
            expr = translate_arg(first, action)?;
        }
    }
    Ok(expr)
}

/// Emits sprig's `default` as minijinja's builtin `default` filter with the
/// `boolean` argument set, which substitutes for any empty value exactly as
/// sprig does — where the bare filter would only cover an undefined one. No
/// `default` global is registered, so this is the only spelling that carries
/// the Go meaning into a migrated Taskfile.
///
/// `args` are the already-translated arguments and `expr` the expression built
/// so far. The subject is that expression after a pipe (`.X | default "y"`) or
/// `default`'s second argument in call position (`default "y" .X`). sprig's
/// `default` is variadic but consults only the first given value, and a bare
/// `default "y"` with no subject at all yields its fallback, so that form
/// translates to the fallback alone. Its remaining arities — `default "y" .A
/// .B`, and `.X | default` with no fallback — stay unsupported, as they were
/// before the translation existed, rather than being guessed at.
fn translate_default(
    idx: usize,
    args: Vec<String>,
    expr: String,
    action: &str,
) -> Result<String, TemplaterError> {
    let len = args.len();
    let mut args = args.into_iter();
    let (fallback, subject) = match (idx, len, args.next(), args.next()) {
        (0, 1, Some(fallback), _) => return Ok(fallback),
        (0, 2, Some(fallback), Some(subject)) => (fallback, subject),
        (_, 1, Some(fallback), _) => (fallback, expr),
        _ => {
            return Err(TemplaterError::Render {
                template: action.to_string(),
                message: format!(
                    "cannot translate `default` with {len} argument(s): it takes one after a \
                     pipe, or one or two in call position"
                ),
            });
        }
    };
    Ok(format!("{subject} | default({fallback}, true)"))
}

/// Appends a translated segment to the expression built so far: the first
/// segment *is* the expression, a later one is a filter applied to it.
fn join_segment(expr: String, rendered: &str) -> String {
    if expr.is_empty() {
        rendered.to_string()
    } else {
        format!("{expr} | {rendered}")
    }
}

/// Translates a single argument token into a minijinja expression. A
/// parenthesized token is a Go sub-expression (`(trunc 48 .TASK)`) and is
/// translated recursively into a call (`trunc(48, TASK)`); anything else is a
/// value with Go dotted-field access rewritten.
fn translate_arg(token: &str, action: &str) -> Result<String, TemplaterError> {
    if let Some(inner) = token.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        return translate_action(inner, action);
    }
    Ok(rewrite_dots(token))
}

/// Splits an action body into `|`-delimited pipeline segments, keeping quoted
/// strings and parenthesized sub-expressions intact so a pipe inside a literal
/// or a nested call is not treated as a delimiter. Literals are consumed with
/// `copy_literal`, so an escaped quote (`"\""`) does not end one early.
fn split_pipeline(body: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' | '`' => {
                cur.push(c);
                copy_literal(c, &mut chars, &mut cur);
            }
            '(' => {
                depth = depth.saturating_add(1);
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            '|' if depth == 0 => {
                segments.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    segments.push(cur.trim().to_string());
    segments
}

/// Splits a pipeline segment into whitespace-separated tokens, keeping quoted
/// strings and parenthesized sub-expressions (`(f a b)`) intact as one token.
/// Literals are consumed with `copy_literal`, so an escaped quote does not end
/// one early and merge the following literal into this token.
fn tokenize(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' | '\'' | '`' => {
                cur.push(c);
                copy_literal(c, &mut chars, &mut cur);
            }
            '(' => {
                depth = depth.saturating_add(1);
                cur.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Reports whether `token` is a bare identifier: alphabetic-led, no leading dot,
/// not quoted, not numeric.
fn is_bare_identifier(token: &str) -> bool {
    match token.chars().next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Copies a Go literal into `out`, starting just after the opening `quote` and
/// stopping after the matching closing one: interpreted (`"…"`, where a
/// backslash escapes the next character), raw (backquoted, no escapes), or a
/// rune (`'…'`). An unterminated literal consumes the rest of the input rather
/// than guessing where it ended.
///
/// A raw literal keeps its backquotes: minijinja has no backquoted string to
/// translate one into, so the rewrite leaves it alone rather than inventing a
/// spelling for it. Such an action fails to render either way, and `--migrate`
/// writes it out unchanged — it is not render-checked.
fn copy_literal<I: Iterator<Item = char>>(quote: char, chars: &mut I, out: &mut String) {
    let escapes = quote != '`';
    while let Some(c) = chars.next() {
        out.push(c);
        if escapes && c == '\\' {
            if let Some(escaped) = chars.next() {
                out.push(escaped);
            }
            continue;
        }
        if c == quote {
            break;
        }
    }
}

/// Rewrites Go dotted field access into minijinja variable access by dropping a
/// dot that immediately precedes an identifier character. `.Foo.Bar` becomes
/// `Foo.Bar`; interior dots (`Foo.Bar`) are left untouched.
///
/// String literals are copied verbatim, so the `.` in `trimSuffix ".po" .ITEM`
/// stays part of the suffix instead of being read as field access. An
/// unterminated literal swallows the remainder, so dots after it are left
/// alone too — the same way `tokenize` treats it.
fn rewrite_dots(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut prev: Option<char> = None;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        // String and rune literals are opaque to the rewrite.
        if matches!(c, '"' | '\'' | '`') {
            out.push(c);
            copy_literal(c, &mut chars, &mut out);
            // `prev` records the opening quote, not the closing one. Both are
            // the same character, and neither is an identifier character, so a
            // `.Foo` right after the literal is still rewritten either way.
            prev = Some(c);
            continue;
        }
        if c == '.' {
            let next_is_ident = chars
                .peek()
                .is_some_and(|n| n.is_ascii_alphabetic() || *n == '_');
            let prev_is_ident = prev.is_some_and(|p| p.is_ascii_alphanumeric() || p == '_');
            // A leading dot (root field access) is dropped; a dot between two
            // identifiers (nested access) is kept.
            if next_is_ident && !prev_is_ident {
                prev = Some(c);
                continue;
            }
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// Builds the Go-mode environment: sealed sentinel block/comment delimiters so a
/// literal `{%`/`{#` in a Taskfile string is passed through verbatim (as Go
/// does). [`translate`] rewrites Go actions and emits control flow with these
/// same sentinels. The delimiters are compile-time constants known to be valid
/// and distinct; a build error is surfaced by falling back to default syntax
/// rather than panicking.
fn build_go_environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Lenient);
    if let Ok(syntax) = minijinja::syntax::SyntaxConfig::builder()
        .block_delimiters(GO_BLOCK_START, GO_BLOCK_END)
        .comment_delimiters(GO_COMMENT_START, GO_COMMENT_END)
        .build()
    {
        env.set_syntax(syntax);
    }
    register_helpers(&mut env);
    env
}

/// Builds the Jinja-mode environment: standard minijinja delimiters, rendered
/// directly with no Go translation. Shares the same helper functions/filters as
/// Go mode so `{{ joinPath(DIR, "bin") }}`, `{% if CI %}`, etc. work natively.
fn build_jinja_environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Lenient);
    register_helpers(&mut env);
    env
}

/// Registers the mapped Task/sprig globals and filters shared by both dialects.
///
/// Missing variables render as empty rather than raising, matching Go's
/// "<no value>" behavior after Task strips the marker.
fn register_helpers(env: &mut Environment<'static>) {
    // minijinja renders booleans Python-style ("True"/"False") since 2.22; Go's
    // text/template renders them "true"/"false". Taskfiles compare the rendered
    // text — `cache.enabled: '{{ne .FOO ""}}'` is read back as a string and
    // checked against "false" — so "False" would read as enabled.
    env.set_formatter(|out, state, value| {
        if value.kind() == minijinja::value::ValueKind::Bool {
            out.write_str(if value.is_true() { "true" } else { "false" })?;
            Ok(())
        } else {
            escape_formatter(out, state, value)
        }
    });

    env.add_function("OS", func_os);
    env.add_function("ARCH", func_arch);
    env.add_function("numCPU", func_num_cpu);
    env.add_function("exeExt", func_exe_ext);

    // Every mapped *string* helper gets a function registration here: Go
    // Taskfiles call them in function position (`{{urlsafe .TASK}}`) as often
    // as in a pipeline (`{{.TASK | urlsafe}}`), and `translate_action` emits a
    // call for the head segment and a filter for every segment after a pipe —
    // except the names in `CALL_AFTER_PIPE`, which become a call there too.
    // `trunc`, `regexReplaceAll`, `joinPath`, `splitArgs`, `env`, `index` and
    // the comparisons are still function-only, so `{{.P | trunc 3}}` fails with
    // "unknown filter".
    //
    // The filter side is registered further down; for `trim`, `lower`, `upper`
    // and `replace` it is minijinja's builtin, which already matches sprig.
    // `title`, `first`, `last` and `join` do not match, so those builtins are
    // deliberately left alone — a native Jinja Taskfile keeps Jinja's meaning —
    // and the Go dialect reaches sprig's through the call `translate_action`
    // emits instead of a filter. `default` is a fifth mismatch, and the one
    // mapped helper with no function registration at all: the builtin filter's
    // `boolean` argument closes it, which is what `translate_default` emits.
    //
    // These are globals, so a Taskfile variable of the same name shadows one —
    // a `vars:` entry called `join` wins over the helper — but an *undefined*
    // variable does not: `{{.first}}` finds the global and renders the helper's
    // Rust path where Go would render nothing.
    //
    // The two positions take their arguments in different orders. sprig's
    // helpers put the subject last (`trimSuffix ".po" .ITEM`), which is what
    // makes them pipeable — Go appends the piped value as the final argument.
    // minijinja does the opposite and passes the piped value as the *first*
    // filter argument. So the filter registrations below take the subject
    // first, and the function registrations here take it last, matching sprig.
    env.add_function("urlsafe", filter_urlsafe);
    env.add_function("base", filter_base);
    env.add_function("dir", filter_dir);
    env.add_function("ext", filter_ext);
    env.add_function("isAbs", filter_is_abs);
    env.add_function("quote", filter_quote);
    env.add_function("squote", filter_squote);
    env.add_function("catLines", filter_cat_lines);
    env.add_function("splitLines", filter_split_lines);
    env.add_function("fromSlash", filter_from_slash);
    env.add_function("toSlash", filter_to_slash);
    env.add_function("trim", func_trim);
    env.add_function("lower", func_lower);
    env.add_function("upper", func_upper);
    env.add_function("title", func_title);
    env.add_function("first", func_first);
    env.add_function("last", func_last);

    // Subject-last sprig helpers, none of which minijinja offers as a function.
    // The first seven have no builtin at all and are registered as filters by
    // this module; `replace` and `join` shadow a builtin in function position
    // only.
    env.add_function("splitList", func_split_list);
    env.add_function("trimAll", func_trim_all);
    env.add_function("trimPrefix", func_trim_prefix);
    env.add_function("trimSuffix", func_trim_suffix);
    env.add_function("hasPrefix", func_has_prefix);
    env.add_function("hasSuffix", func_has_suffix);
    env.add_function("contains", func_contains);
    env.add_function("replace", func_replace);
    env.add_function("join", func_join);

    // Go builtin `index` and the comparison functions (`eq`/`ne`/`lt`/…), used
    // in function position (e.g. `{{index .MATCH 0}}`, `{{ne .X ""}}`).
    env.add_function("index", func_index);
    env.add_function("eq", func_eq);
    env.add_function("ne", func_ne);
    env.add_function("lt", func_lt);
    env.add_function("le", func_le);
    env.add_function("gt", func_gt);
    env.add_function("ge", func_ge);

    // Task `splitArgs` (shell field split) and the Go builtin `len`, both usable
    // in function or pipeline position.
    env.add_function("splitArgs", func_split_args);
    env.add_function("len", func_len);
    env.add_filter("len", func_len);

    // Task `joinPath` (filepath.Join), the sprig helpers `trunc`,
    // `regexReplaceAll` and `env`, and the Go builtins `printf` and `print`,
    // all called in function position.
    env.add_function("joinPath", func_join_path);
    env.add_function("trunc", func_trunc);
    env.add_function("regexReplaceAll", func_regex_replace_all);
    env.add_function("env", func_env);
    env.add_function("printf", func_printf);
    env.add_function("print", func_print);

    env.add_filter("catLines", filter_cat_lines);
    env.add_filter("splitLines", filter_split_lines);
    env.add_filter("fromSlash", filter_from_slash);
    env.add_filter("toSlash", filter_to_slash);
    env.add_filter("urlsafe", filter_urlsafe);
    env.add_filter("splitList", filter_split_list);

    // These sprig-compatible helpers are also usable as filters. minijinja
    // ships its own `trim`, `lower`, `upper` and `replace`, which already match
    // sprig; register the remaining ones. Four of its builtins do *not* match
    // and are deliberately left in place, so a Jinja Taskfile keeps Jinja's
    // meaning; the Go dialect gets sprig's from the call `translate_action`
    // emits instead (see `CALL_AFTER_PIPE`). They differ in that `title`
    // lowercases the tail of each word where sprig only re-cases the leading
    // letter; `join` iterates a string's characters where sprig treats it as
    // one element; and `first`/`last` fail on a non-sequence where the Go
    // dialect renders empty. minijinja's `default` is a fifth mismatch, but
    // its `boolean` argument closes the gap, so `translate_default` targets
    // the builtin rather than shadowing it.
    env.add_filter("trimAll", filter_trim_all);
    env.add_filter("trimPrefix", filter_trim_prefix);
    env.add_filter("trimSuffix", filter_trim_suffix);
    env.add_filter("hasPrefix", filter_has_prefix);
    env.add_filter("hasSuffix", filter_has_suffix);
    env.add_filter("contains", filter_contains);
    env.add_filter("quote", filter_quote);
    env.add_filter("squote", filter_squote);
    env.add_filter("base", filter_base);
    env.add_filter("dir", filter_dir);
    env.add_filter("ext", filter_ext);
    env.add_filter("isAbs", filter_is_abs);
}

/// Returns the Go `GOOS` name for the target operating system. Rust's
/// `std::env::consts::OS` matches Go for most platforms; `macos` is the sole
/// spelling difference (`darwin` in Go).
fn go_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// Returns the Go `GOARCH` name for the target architecture. Rust and Go differ
/// on the two common 64-bit spellings (`x86_64`/`aarch64` vs `amd64`/`arm64`).
fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "powerpc64" => "ppc64",
        other => other,
    }
}

fn func_os() -> String {
    go_os().to_string()
}

fn func_arch() -> String {
    go_arch().to_string()
}

fn func_num_cpu() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

fn func_exe_ext() -> String {
    if go_os() == "windows" {
        ".exe".to_string()
    } else {
        String::new()
    }
}

/// Go builtin `index coll k1 k2 …`: successive item lookups into a sequence or
/// map, so `index .MATCH 0` yields the first captured wildcard.
fn func_index(value: JinjaValue, keys: Rest<JinjaValue>) -> Result<JinjaValue, minijinja::Error> {
    let mut current = value;
    for key in keys.iter() {
        current = current.get_item(key)?;
    }
    Ok(current)
}

// Go comparison builtins. minijinja `Value` is totally ordered, so the operators
// map directly; `eq`/`ne` also cover strings, numbers, and bools.
fn func_eq(a: JinjaValue, b: JinjaValue) -> bool {
    a == b
}
fn func_ne(a: JinjaValue, b: JinjaValue) -> bool {
    a != b
}
fn func_lt(a: JinjaValue, b: JinjaValue) -> bool {
    a < b
}
fn func_le(a: JinjaValue, b: JinjaValue) -> bool {
    a <= b
}
fn func_gt(a: JinjaValue, b: JinjaValue) -> bool {
    a > b
}
fn func_ge(a: JinjaValue, b: JinjaValue) -> bool {
    a >= b
}

/// Task `joinPath` (Go `filepath.Join`): joins the string arguments with `/`
/// and cleans the result. Non-string arguments render via their display form.
fn func_join_path(parts: Rest<JinjaValue>) -> String {
    let segments: Vec<String> = parts.iter().map(ToString::to_string).collect();
    crate::filepathext::join_path(&segments)
}

/// Sprig `trunc n s`: the first `n` characters of `s`, or the last `-n` when
/// `n` is negative. `n` outside the string length returns `s` unchanged.
fn func_trunc(n: i64, s: String) -> String {
    let len = s.chars().count();
    if n < 0 {
        let keep = n.unsigned_abs() as usize;
        if keep >= len {
            return s;
        }
        return s.chars().skip(len.saturating_sub(keep)).collect();
    }
    s.chars().take(n.unsigned_abs() as usize).collect()
}

/// Sprig `regexReplaceAll pattern s repl`: replaces every match of `pattern` in
/// `s` with `repl`. An invalid pattern is a render error (Go panics via
/// `MustCompile`; a clear error is friendlier and keeps the process alive).
fn func_regex_replace_all(
    pattern: String,
    s: String,
    repl: String,
) -> Result<String, minijinja::Error> {
    let re = regex::Regex::new(&pattern).map_err(|e| {
        minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            format!("invalid regexReplaceAll pattern {pattern:?}: {e}"),
        )
    })?;
    Ok(re.replace_all(&s, repl.as_str()).into_owned())
}

/// Sprig `env name`: the value of the named environment variable, or empty if
/// it is unset.
fn func_env(name: String) -> String {
    std::env::var(&name).unwrap_or_default()
}

/// Go builtin `printf` (`fmt.Sprintf`) over the verbs a Taskfile composes
/// strings with: `%s`, `%v`, `%q`, `%d` and `%%`, each with the `-` and `0`
/// flags and a minimum width (`%-10s`, `%03d`). A precision, any other verb, a
/// width no Taskfile would ask for, an argument its verb cannot render, and an
/// argument count that does not match the format are render errors, rather
/// than the `%!f(int=1)` / `%!(EXTRA …)` markers Go writes into its output —
/// this module reports what it cannot render faithfully.
///
/// `%s` and `%v` render a scalar the way the rest of this module does
/// (Go-cased booleans, empty for a missing variable) instead of applying Go's
/// per-type formatting, and `%q` escapes the way Rust does, which differs from
/// Go for control and non-ASCII characters (`\u{1}` where Go writes `\x01`).
fn func_printf(format: String, args: Rest<JinjaValue>) -> Result<String, minijinja::Error> {
    let mut out = String::with_capacity(format.len());
    let mut used = 0usize;
    let mut chars = format.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let spec = parse_printf_spec(&mut chars, &format)?;
        // `%%` is a literal percent — whatever flags and width precede it, as
        // in Go — and consumes no argument.
        if spec.verb == '%' {
            out.push('%');
            continue;
        }
        let Some(arg) = args.get(used) else {
            return Err(printf_error(format!(
                "format {format:?} wants more arguments than the {} given",
                args.len()
            )));
        };
        used = used.saturating_add(1);
        out.push_str(&format_printf_arg(&spec, arg, &format)?);
    }
    if used != args.len() {
        return Err(printf_error(format!(
            "format {format:?} takes {used} argument(s), {} given",
            args.len()
        )));
    }
    Ok(out)
}

/// Go builtin `print` (`fmt.Sprint`): the operands concatenated, with a space
/// added between two neighbours when neither of them is a string. A non-scalar
/// operand is a render error for the same reason it is under `%v` — see
/// [`is_printf_scalar`].
fn func_print(args: Rest<JinjaValue>) -> Result<String, minijinja::Error> {
    let mut out = String::new();
    let mut prev_was_string = true;
    for (idx, arg) in args.iter().enumerate() {
        if !is_printf_scalar(arg) {
            return Err(printf_error(format!("print wants scalars, given {arg}")));
        }
        let is_string = arg.as_str().is_some();
        if idx != 0 && !is_string && !prev_was_string {
            out.push(' ');
        }
        out.push_str(&go_string(arg));
        prev_was_string = is_string;
    }
    Ok(out)
}

/// One `%…` verb of a [`func_printf`] format string.
struct PrintfSpec {
    /// The `-` flag: pad on the right instead of the left.
    left: bool,
    /// The `0` flag: pad with leading zeros instead of spaces.
    zero: bool,
    /// The minimum field width, or `0` for none.
    width: usize,
    /// The verb character, one of [`PRINTF_VERBS`].
    verb: char,
}

/// The verbs [`func_printf`] renders. `%` is the literal-percent verb, which
/// consumes no argument.
const PRINTF_VERBS: &[char] = &['s', 'v', 'q', 'd', '%'];

/// The widest field [`func_printf`] pads to. Go gives up on a width around ten
/// million and writes a `%!(NOVERB)` marker instead of formatting; this stops a
/// magnitude earlier, since no Taskfile pads that far, so that a runaway
/// `%99999…9d` is a render error rather than an allocation the size of the
/// number.
const PRINTF_MAX_WIDTH: usize = 1_000_000;

/// Parses the flags, width and verb just past a `%`, leaving the iterator on
/// the first character after the verb.
fn parse_printf_spec<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
    format: &str,
) -> Result<PrintfSpec, minijinja::Error> {
    // Go ignores `0` when `-` is set; both flags are kept as given and
    // `pad_printf` honors that rule.
    let mut left = false;
    let mut zero = false;
    while let Some(&c) = chars.peek() {
        match c {
            '-' => left = true,
            '0' => zero = true,
            _ => break,
        }
        chars.next();
    }
    // The flags Go accepts and this does not, named as flags rather than
    // reported as the verb they precede.
    if let Some(&c) = chars.peek()
        && matches!(c, '+' | ' ' | '#' | '*' | '[')
    {
        return Err(printf_error(format!(
            "unsupported flag \"%{c}\" in format {format:?}"
        )));
    }
    let mut width = 0usize;
    while let Some(digit) = chars.peek().and_then(|c| c.to_digit(10)) {
        width = width
            .saturating_mul(10)
            .saturating_add(digit.try_into().unwrap_or(usize::MAX));
        if width > PRINTF_MAX_WIDTH {
            return Err(printf_error(format!(
                "a width above {PRINTF_MAX_WIDTH} is not supported in format {format:?}"
            )));
        }
        chars.next();
    }
    match chars.next() {
        Some(verb) if PRINTF_VERBS.contains(&verb) => Ok(PrintfSpec {
            left,
            zero,
            width,
            verb,
        }),
        Some('.') => Err(printf_error(format!(
            "a precision is not supported in format {format:?}"
        ))),
        Some(verb) => Err(printf_error(format!(
            "unsupported verb \"%{verb}\" in format {format:?}"
        ))),
        None => Err(printf_error(format!(
            "format {format:?} ends with a lone \"%\""
        ))),
    }
}

/// Renders one argument through its verb, padded to the requested width.
fn format_printf_arg(
    spec: &PrintfSpec,
    arg: &JinjaValue,
    format: &str,
) -> Result<String, minijinja::Error> {
    let text = match spec.verb {
        's' | 'v' => {
            if !is_printf_scalar(arg) {
                return Err(printf_error(format!(
                    "\"%{}\" in format {format:?} wants a scalar, given {arg}",
                    spec.verb
                )));
            }
            go_string(arg)
        }
        // Go's `%q` of a number is a rune literal (`'A'` for 65) and of a bool
        // an error marker, so only a string — or a missing variable, which
        // this module renders empty — formats faithfully.
        'q' => {
            if !(arg.is_undefined() || arg.is_none() || arg.as_str().is_some()) {
                return Err(printf_error(format!(
                    "\"%q\" in format {format:?} wants a string, given {arg}"
                )));
            }
            format!("{:?}", go_string(arg))
        }
        'd' => {
            let Some(n) = arg.as_i64() else {
                return Err(printf_error(format!(
                    "\"%d\" in format {format:?} wants a number, given {arg}"
                )));
            };
            n.to_string()
        }
        // `parse_printf_spec` accepts no other verb, and `%` never reaches
        // here.
        other => {
            return Err(printf_error(format!(
                "unsupported verb \"%{other}\" in format {format:?}"
            )));
        }
    };
    Ok(pad_printf(&text, spec))
}

/// Whether `%s` / `%v` can render this value the way Go would: Go writes a
/// list as `[a b]` and a map as `map[k:v]`, neither of which minijinja's
/// stringification matches, so only a scalar formats faithfully.
fn is_printf_scalar(arg: &JinjaValue) -> bool {
    use minijinja::value::ValueKind;
    matches!(
        arg.kind(),
        ValueKind::Undefined
            | ValueKind::None
            | ValueKind::Bool
            | ValueKind::Number
            | ValueKind::String
    )
}

/// Pads `text` to the spec's width: on the right for `-`, otherwise on the
/// left, with zeros for `0`.
fn pad_printf(text: &str, spec: &PrintfSpec) -> String {
    let Some(missing) = spec.width.checked_sub(text.chars().count()) else {
        return text.to_string();
    };
    if spec.left {
        return format!("{text}{}", " ".repeat(missing));
    }
    if spec.zero {
        // Go zero-pads a number after its sign (`-07`) but pads a string
        // whole, sign or not (`000-a`).
        let signed = spec.verb == 'd' && text.starts_with(['-', '+']);
        let (sign, digits) = text.split_at(usize::from(signed));
        return format!("{sign}{}{digits}", "0".repeat(missing));
    }
    format!("{}{text}", " ".repeat(missing))
}

fn printf_error(message: String) -> minijinja::Error {
    minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, message)
}

/// Task `splitArgs`: shell-style field splitting honoring single and double
/// quotes. Ports `shell.Fields` for the common (expansion-free) case.
fn func_split_args(s: String) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                started = true;
                if c == q {
                    quote = None;
                } else {
                    current.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    started = true;
                }
                w if w.is_whitespace() => {
                    if started {
                        args.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                other => {
                    current.push(other);
                    started = true;
                }
            },
        }
    }
    if started {
        args.push(current);
    }
    args
}

/// Go builtin `len`: the element count of a sequence or map (or a string's
/// length).
fn func_len(value: JinjaValue) -> usize {
    value.len().unwrap_or(0)
}

// The sprig helpers below are reachable in function position, where the subject
// is the last argument (`trimSuffix ".po" .ITEM`). Those with a `filter_*`
// counterpart in this module delegate to it with the arguments swapped, since a
// filter takes the subject first; the rest — the ones minijinja only ships as
// builtin filters — are implemented here.

fn func_trim(s: String) -> String {
    s.trim().to_string()
}

fn func_lower(s: String) -> String {
    s.to_lowercase()
}

fn func_upper(s: String) -> String {
    s.to_uppercase()
}

/// Sprig `title`: uppercases the first letter of every word and leaves the rest
/// of each word as it was, as Go's `strings.Title` does — unlike minijinja's
/// `title` filter, which also lowercases the tail.
///
/// Rust has no titlecase mapping, so `to_uppercase` stands in; it differs from
/// Go for the few runes whose upper- and title-case forms diverge (`ß`, `ǳ`, …).
fn func_title(s: String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_word_start = true;
    for c in s.chars() {
        if at_word_start {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
        at_word_start = is_go_word_separator(c);
    }
    out
}

/// Go's `strings.isSeparator`, the word boundary `strings.Title` uses: ASCII
/// alphanumerics and `_` never separate words, every other ASCII character does,
/// and a non-ASCII character only when it is whitespace and neither letter nor
/// digit.
fn is_go_word_separator(c: char) -> bool {
    if c.is_ascii() {
        return !(c.is_ascii_alphanumeric() || c == '_');
    }
    !c.is_alphabetic() && !c.is_numeric() && c.is_whitespace()
}

/// Sprig `first list` / `last list`: the first or last element, or undefined for
/// an empty or non-iterable value, which renders as empty — the lenient
/// treatment this module gives missing values, where sprig itself errors.
///
/// Both iterate rather than index: indexing a map would look the position up as
/// a *key*, and a lazy iterable has no length to index from. minijinja iterates
/// a string by character, so `first` of one is its first character where sprig
/// rejects the operand outright.
fn func_first(value: JinjaValue) -> JinjaValue {
    value
        .try_iter()
        .ok()
        .and_then(|mut items| items.next())
        .unwrap_or(JinjaValue::UNDEFINED)
}

fn func_last(value: JinjaValue) -> JinjaValue {
    value
        .try_iter()
        .ok()
        .and_then(Iterator::last)
        .unwrap_or(JinjaValue::UNDEFINED)
}

fn func_split_list(sep: String, s: String) -> Vec<String> {
    filter_split_list(s, sep)
}

fn func_trim_all(cutset: String, s: String) -> String {
    filter_trim_all(s, cutset)
}

fn func_trim_prefix(prefix: String, s: String) -> String {
    filter_trim_prefix(s, prefix)
}

fn func_trim_suffix(suffix: String, s: String) -> String {
    filter_trim_suffix(s, suffix)
}

fn func_has_prefix(prefix: String, s: String) -> bool {
    filter_has_prefix(s, prefix)
}

fn func_has_suffix(suffix: String, s: String) -> bool {
    filter_has_suffix(s, suffix)
}

fn func_contains(needle: String, s: String) -> bool {
    filter_contains(s, needle)
}

/// Sprig `replace old new s`: replaces every occurrence of `old` in `s`.
fn func_replace(old: String, new: String, s: String) -> String {
    s.replace(&old, &new)
}

/// Fixes Python-cased booleans on their way out of a helper. `set_formatter`
/// does it for a value rendered by a template, but one stringified *inside* a
/// helper never passes through it, so `join` has to do it here. Other kinds are
/// left to minijinja's `Display`, which still differs from Go's `%v` in places
/// (a whole float renders `2.0`, where Go gives `2`).
fn go_string(value: &JinjaValue) -> String {
    if value.kind() == minijinja::value::ValueKind::Bool {
        return if value.is_true() { "true" } else { "false" }.to_string();
    }
    value.to_string()
}

/// Sprig `join sep list`: concatenates the elements with `sep`. sprig's
/// `strslice` wraps an operand it cannot treat as a list in a one-element list
/// instead of failing, so a string joins to itself (`join "," "a.b"` is `a.b`,
/// not `a,.,b`) and a scalar stringifies — where minijinja would iterate the
/// string's characters and reject the scalar. It also drops nil elements, so
/// none and undefined are skipped rather than rendered. A map still iterates,
/// yielding its keys, where sprig would stringify the whole map.
fn func_join(sep: String, list: JinjaValue) -> String {
    if let Some(s) = list.as_str() {
        return s.to_string();
    }
    match list.try_iter() {
        Ok(items) => items
            .filter(|v| !v.is_none() && !v.is_undefined())
            .map(|v| go_string(&v))
            .collect::<Vec<_>>()
            .join(&sep),
        // Not iterable: sprig joins the one-element list holding it.
        Err(_) => go_string(&list),
    }
}

fn filter_cat_lines(s: String) -> String {
    s.replace("\r\n", " ").replace('\n', " ")
}

fn filter_split_lines(s: String) -> Vec<String> {
    s.replace("\r\n", "\n")
        .split('\n')
        .map(str::to_string)
        .collect()
}

fn filter_from_slash(s: String) -> String {
    if std::path::MAIN_SEPARATOR == '/' {
        s
    } else {
        s.replace('/', std::path::MAIN_SEPARATOR_STR)
    }
}

fn filter_to_slash(s: String) -> String {
    if std::path::MAIN_SEPARATOR == '/' {
        s
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

fn filter_urlsafe(s: String) -> String {
    // Percent-encode everything that is not an unreserved URL path character,
    // then map "@" to "|" for use in cache keys.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b':') {
            out.push(b as char);
        } else if b == b'@' {
            out.push('|');
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// Sprig `splitList sep s`, in both spellings. An empty `s` yields no elements
/// where Go's `strings.Split` yields one empty one, so
/// `{{len (splitList "," .EMPTY)}}` renders `0` rather than `1`.
fn filter_split_list(s: String, sep: String) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(&sep).map(str::to_string).collect()
}

fn filter_trim_all(s: String, cutset: String) -> String {
    let chars: Vec<char> = cutset.chars().collect();
    s.trim_matches(|c| chars.contains(&c)).to_string()
}

fn filter_trim_prefix(s: String, prefix: String) -> String {
    s.strip_prefix(&prefix).unwrap_or(&s).to_string()
}

fn filter_trim_suffix(s: String, suffix: String) -> String {
    s.strip_suffix(&suffix).unwrap_or(&s).to_string()
}

fn filter_has_prefix(s: String, prefix: String) -> bool {
    s.starts_with(&prefix)
}

fn filter_has_suffix(s: String, suffix: String) -> bool {
    s.ends_with(&suffix)
}

fn filter_contains(s: String, needle: String) -> bool {
    s.contains(&needle)
}

fn filter_quote(s: String) -> String {
    format!("\"{s}\"")
}

fn filter_squote(s: String) -> String {
    format!("'{s}'")
}

fn filter_base(s: String) -> String {
    std::path::Path::new(&s)
        .file_name()
        .and_then(|n| n.to_str())
        .map_or_else(|| s.clone(), str::to_string)
}

fn filter_dir(s: String) -> String {
    std::path::Path::new(&s)
        .parent()
        .and_then(|p| p.to_str())
        .map_or_else(String::new, str::to_string)
}

fn filter_ext(s: String) -> String {
    std::path::Path::new(&s)
        .extension()
        .and_then(|e| e.to_str())
        .map_or_else(String::new, |e| format!(".{e}"))
}

fn filter_is_abs(s: String) -> bool {
    crate::filepathext::is_abs(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_with(pairs: &[(&str, &str)]) -> Cache {
        let mut vars = Vars::new();
        for (k, v) in pairs {
            vars.set(
                (*k).to_string(),
                Var {
                    value: Some(YamlValue::String((*v).to_string())),
                    ..Default::default()
                },
            );
        }
        Cache::new(vars)
    }

    // Booleans must render Go-style. minijinja renders them "True"/"False"
    // (Python) since 2.22, and Taskfiles read the rendered text back as a string
    // — `cache.enabled: '{{ne .FOO ""}}'` is compared against "false".
    #[test]
    fn bools_render_go_style() {
        let mut c = cache_with(&[("EMPTY", "")]);
        assert_eq!(c.replace(r#"{{ne .EMPTY ""}}"#), "false");
        assert_eq!(c.replace(r#"{{eq .EMPTY ""}}"#), "true");
    }

    #[test]
    fn detect_go_signals() {
        // Validated to misdetect none of the 64 templated Go corpus Taskfiles.
        for src in [
            "cmds: ['echo {{.VAR}}']",           // leading-dot access
            "x: '{{ .A.B }}'",                   // spaced leading-dot
            "x: '{{if .CI}}a{{end}}'",           // control word
            "x: '{{range .Items}}{{.}}{{end}}'", // range
            "x: '{{ joinPath .ROOT \"b\" }}'",   // dotted arg
            "x: '{{ index .M 0 }}'",             // dotted arg in call
            "x: '{{OS}}'",                       // bare nullary Go func
            "x: '{{ trunc 48 \"s\" }}'",         // space-separated call
            "x: 'a{{/* c */}}b'",                // Go comment
        ] {
            assert_eq!(detect_dialect(src), Dialect::Go, "should be Go: {src}");
        }
    }

    #[test]
    fn detect_jinja_or_default() {
        for src in [
            "x: '{{ joinPath(\"a\", \"b\") }}'", // call syntax
            "x: '{% if CI %}a{% endif %}'",      // Jinja block
            "x: 'a{# c #}b'",                    // Jinja comment
            "x: '{{ VAR | upper }}'",            // no dot, no Go call
            "cmds: ['echo hello']",              // no templates at all
            "x: '{{ NAME }}'",                   // bare var (ambiguous → Jinja)
        ] {
            assert_eq!(
                detect_dialect(src),
                Dialect::Jinja,
                "should be Jinja: {src}"
            );
        }
    }

    #[test]
    fn jinja_mode_renders_natively() {
        let mut c = cache_with(&[("NAME", "world")]);
        c.set_dialect(Dialect::Jinja);
        // Native Jinja: no leading-dot access, native filters, blocks, and the
        // mapped helper functions all work without translation.
        assert_eq!(c.replace("hi {{ NAME | upper }}"), "hi WORLD");
        assert_eq!(
            c.replace("{% if NAME == \"world\" %}yes{% else %}no{% endif %}"),
            "yes"
        );
        assert_eq!(c.replace(r#"{{ joinPath("/a", "b") }}"#), "/a/b");
        assert_eq!(c.replace("{% for i in [1, 2] %}{{ i }}{% endfor %}"), "12");
        assert!(!c.is_err());
    }

    #[test]
    fn jinja_and_go_modes_differ_on_control_flow() {
        // The same source is a live block in Jinja mode and literal text in Go
        // mode (Go text/template does not recognise `{%`).
        let src = "{% if true %}X{% endif %}";
        let mut go = cache_with(&[]);
        assert_eq!(go.replace(src), src);
        let mut jinja = cache_with(&[]);
        jinja.set_dialect(Dialect::Jinja);
        assert_eq!(jinja.replace(src), "X");
    }

    #[test]
    fn simple_interpolation() {
        let mut c = cache_with(&[("FOO", "bar")]);
        assert_eq!(c.replace("{{.FOO}}"), "bar");
        assert!(!c.is_err());
    }

    #[test]
    fn interpolation_whitespace_variants() {
        let mut c = cache_with(&[("FOO", "bar")]);
        assert_eq!(c.replace("{{ .FOO }}"), "bar");
        assert_eq!(c.replace("{{.FOO }}"), "bar");
        assert_eq!(c.replace("{{  .FOO}}"), "bar");
        assert_eq!(c.replace("x-{{.FOO}}-y"), "x-bar-y");
    }

    #[test]
    fn nested_field_access() {
        let mut vars = Vars::new();
        let mut inner = serde_yaml_ng::Mapping::new();
        inner.insert(
            YamlValue::String("BAR".to_string()),
            YamlValue::String("deep".to_string()),
        );
        vars.set(
            "FOO".to_string(),
            Var {
                value: Some(YamlValue::Mapping(inner)),
                ..Default::default()
            },
        );
        let mut c = Cache::new(vars);
        assert_eq!(c.replace("{{.FOO.BAR}}"), "deep");
        assert_eq!(c.replace("{{ .FOO.BAR }}"), "deep");
    }

    #[test]
    fn missing_variable_renders_empty() {
        let mut c = cache_with(&[]);
        assert_eq!(c.replace("{{.MISSING}}"), "");
        assert!(!c.is_err());
    }

    #[test]
    fn literal_text_passthrough() {
        let mut c = cache_with(&[]);
        assert_eq!(c.replace("no templates here"), "no templates here");
    }

    #[test]
    fn extra_overrides() {
        let mut c = cache_with(&[("FOO", "base")]);
        let mut extra = IndexMap::new();
        extra.insert("FOO".to_string(), YamlValue::String("over".to_string()));
        assert_eq!(c.replace_with_extra("{{.FOO}}", &extra), "over");
    }

    #[test]
    fn rejects_range() {
        let mut c = cache_with(&[]);
        c.replace("{{range .Items}}{{.}}{{end}}");
        let err = c.err().unwrap();
        match err {
            TemplaterError::UnsupportedConstruct { construct, .. } => {
                assert_eq!(construct, "range");
            }
            other => panic!("expected UnsupportedConstruct, got {other:?}"),
        }
    }

    #[test]
    fn rejects_control_words() {
        for kw in ["with", "define", "template", "block", "range"] {
            let mut c = cache_with(&[]);
            let tmpl = format!("{{{{{kw} .X}}}}");
            c.replace(&tmpl);
            assert!(c.is_err(), "expected {kw} to be rejected");
            match c.err().unwrap() {
                TemplaterError::UnsupportedConstruct { construct, .. } => {
                    assert_eq!(construct, kw);
                }
                other => panic!("expected UnsupportedConstruct for {kw}, got {other:?}"),
            }
        }
    }

    #[test]
    fn if_else_end() {
        // The GOTESTSUM_FORMAT idiom from go-task's own Taskfile.
        let tmpl = "{{if .CI}}github-actions{{else}}pkgname{{end}}";
        let mut absent = cache_with(&[]);
        assert_eq!(absent.replace(tmpl), "pkgname");
        assert!(!absent.is_err());

        let mut present = cache_with(&[("CI", "true")]);
        assert_eq!(present.replace(tmpl), "github-actions");
        assert!(!present.is_err());
    }

    #[test]
    fn if_condition_uses_functions() {
        let go_os = go_os();
        let tmpl = format!("{{{{if eq .OS {go_os:?}}}}}match{{{{else}}}}no{{{{end}}}}");
        let mut c = cache_with(&[("OS", go_os)]);
        assert_eq!(c.replace(&tmpl), "match");
        assert!(!c.is_err());
    }

    #[test]
    fn else_if_chain() {
        let tmpl = "{{if .A}}a{{else if .B}}b{{else}}c{{end}}";
        let mut only_b = cache_with(&[("B", "yes")]);
        assert_eq!(only_b.replace(tmpl), "b");
        assert!(!only_b.is_err());

        let mut neither = cache_with(&[]);
        assert_eq!(neither.replace(tmpl), "c");
    }

    #[test]
    fn join_path_cleans() {
        let mut c = cache_with(&[("DIR", "/opt/wab")]);
        assert_eq!(c.replace(r#"{{ joinPath .DIR "bin" }}"#), "/opt/wab/bin");
        assert_eq!(c.replace(r#"{{ joinPath .DIR "../lib/x" }}"#), "/opt/lib/x");
        assert!(!c.is_err());
    }

    #[test]
    fn trunc_first_and_last() {
        let mut c = cache_with(&[("S", "abcdefgh")]);
        assert_eq!(c.replace("{{ trunc 3 .S }}"), "abc");
        assert_eq!(c.replace("{{ trunc -3 .S }}"), "fgh");
        assert_eq!(c.replace("{{ trunc 99 .S }}"), "abcdefgh");
    }

    #[test]
    fn nested_regex_replace_all() {
        // The cache-key idiom: sanitize a truncated task name.
        let mut c = cache_with(&[("TASK", "build:the app@v2")]);
        assert_eq!(
            c.replace(r#"{{regexReplaceAll "[^a-zA-Z0-9._-]" (trunc 48 .TASK) "-"}}"#),
            "build-the-app-v2"
        );
        assert!(!c.is_err());
    }

    #[test]
    fn logical_and_or_not() {
        let mut c = cache_with(&[("A", "x"), ("B", "")]);
        // and: false when any operand is falsy (empty string).
        assert_eq!(c.replace(r#"{{and (ne .A "") (ne .B "")}}"#), "false");
        assert_eq!(c.replace(r#"{{and (ne .A "") (eq .B "")}}"#), "true");
        assert_eq!(c.replace(r#"{{or (ne .A "") (ne .B "")}}"#), "true");
        assert_eq!(c.replace(r#"{{not (ne .B "")}}"#), "true");
        assert!(!c.is_err());
    }

    #[test]
    fn raw_jinja_syntax_is_literal() {
        // Go text/template treats `{%` and `{#` as ordinary text; the sealed
        // delimiters must too (no execution, no stripping).
        let mut c = cache_with(&[]);
        assert_eq!(
            c.replace("{% if true %}X{% endif %}"),
            "{% if true %}X{% endif %}"
        );
        assert_eq!(c.replace("a{# c #}b"), "a{# c #}b");
        assert!(!c.is_err());
    }

    #[test]
    fn go_comment_renders_empty() {
        let mut c = cache_with(&[]);
        assert_eq!(c.replace("x{{/* a comment */}}y"), "xy");
        assert!(!c.is_err());
    }

    #[test]
    fn if_still_works_with_sealed_delimiters() {
        let mut c = cache_with(&[("CI", "1")]);
        assert_eq!(
            c.replace("{{if .CI}}github-actions{{else}}pkgname{{end}}"),
            "github-actions"
        );
        assert!(!c.is_err());
    }

    #[test]
    fn env_reads_process_environment() {
        // SAFETY: single-threaded unit test; no other thread reads the env here.
        unsafe {
            std::env::set_var("TASKCORE_TEST_ENV", "present");
        }
        let mut c = cache_with(&[]);
        assert_eq!(c.replace(r#"{{ env "TASKCORE_TEST_ENV" }}"#), "present");
        assert_eq!(c.replace(r#"{{ env "TASKCORE_TEST_ABSENT" }}"#), "");
        unsafe {
            std::env::remove_var("TASKCORE_TEST_ENV");
        }
    }

    #[test]
    fn rejects_unmapped_function() {
        let mut c = cache_with(&[]);
        c.replace("{{ uuid }}");
        match c.err().unwrap() {
            TemplaterError::UnsupportedConstruct { construct, .. } => {
                assert_eq!(construct, "uuid");
            }
            other => panic!("expected UnsupportedConstruct, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unmapped_pipeline_function() {
        let mut c = cache_with(&[("FOO", "x")]);
        c.replace("{{ .FOO | spew }}");
        match c.err().unwrap() {
            TemplaterError::UnsupportedConstruct { construct, .. } => {
                assert_eq!(construct, "spew");
            }
            other => panic!("expected UnsupportedConstruct, got {other:?}"),
        }
    }

    #[test]
    fn first_error_wins_and_is_noop() {
        let mut c = cache_with(&[("FOO", "bar")]);
        c.replace("{{range .X}}");
        assert!(c.is_err());
        // Subsequent calls return input unchanged and keep the first error.
        let first = c.err().cloned().unwrap();
        assert_eq!(c.replace("{{.FOO}}"), "{{.FOO}}");
        assert_eq!(c.err().cloned().unwrap(), first);
    }

    #[test]
    fn mapped_os_function() {
        let mut c = cache_with(&[]);
        assert_eq!(c.replace("{{ OS }}"), go_os());
    }

    #[test]
    fn mapped_cat_lines() {
        let mut c = cache_with(&[("TEXT", "a\nb\nc")]);
        assert_eq!(c.replace("{{ .TEXT | catLines }}"), "a b c");
    }

    #[test]
    fn mapped_split_lines() {
        let mut c = cache_with(&[("TEXT", "a\nb")]);
        // splitLines yields a list; joining it back proves the split happened.
        assert_eq!(c.replace("{{ .TEXT | splitLines | join \",\" }}"), "a,b");
    }

    #[test]
    fn mapped_to_slash_default_filter() {
        let mut c = cache_with(&[("P", "a/b/c")]);
        // On unix toSlash is a no-op; the point is that it renders without error.
        assert_eq!(c.replace("{{ .P | toSlash }}"), "a/b/c");
        assert!(!c.is_err());
    }

    #[test]
    fn mapped_default_after_a_pipe() {
        let mut c = cache_with(&[]);
        // Go pipeline syntax (`| default "x"`) is rewritten to minijinja's
        // builtin `default` filter with the `boolean` argument set, which is
        // what gives it sprig's any-empty-value meaning.
        assert_eq!(
            c.replace("{{ .MISSING | default \"fallback\" }}"),
            "fallback"
        );
        assert!(!c.is_err());
    }

    // Every mapped sprig helper is callable in function position, where the
    // subject is the *last* argument. These were previously filter-only, so a
    // real Taskfile line like `msgfmt "{{.ITEM}}" -o '{{trimSuffix ".po" .ITEM}}.mo'`
    // failed to render with "unknown function".
    #[test]
    fn sprig_helpers_in_function_position() {
        let cases = [
            (r#"{{ trimSuffix ".po" .P }}"#, "dir/fr"),
            (r#"{{ trimPrefix "dir/" .P }}"#, "fr.po"),
            (r#"{{ trimAll "dpo/." .P }}"#, "ir/fr"),
            (r#"{{ hasPrefix "dir/" .P }}"#, "true"),
            (r#"{{ hasSuffix ".po" .P }}"#, "true"),
            (r#"{{ hasSuffix ".mo" .P }}"#, "false"),
            (r#"{{ contains "/fr" .P }}"#, "true"),
            (r#"{{ replace "/" "_" .P }}"#, "dir_fr.po"),
            (r#"{{ join "," (splitList "/" .P) }}"#, "dir,fr.po"),
            (r#"{{ first (splitList "/" .P) }}"#, "dir"),
            (r#"{{ last (splitList "/" .P) }}"#, "fr.po"),
            (r#"{{ upper .P }}"#, "DIR/FR.PO"),
            (r#"{{ lower "AB" }}"#, "ab"),
            (r#"{{ title "hello wide world" }}"#, "Hello Wide World"),
            (r#"{{ trim "  x  " }}"#, "x"),
            (r#"{{ default "fallback" .MISSING }}"#, "fallback"),
            (r#"{{ default "fallback" .P }}"#, "dir/fr.po"),
        ];
        for (tmpl, want) in cases {
            let mut c = cache_with(&[("P", "dir/fr.po")]);
            let got = c.replace(tmpl);
            // Checked first: once the cache records an error `replace` returns
            // its input, which would make the mismatch the misleading failure.
            assert!(!c.is_err(), "{tmpl} recorded {:?}", c.err());
            assert_eq!(got, want, "rendering {tmpl}");
        }
    }

    // Go's `printf` and `print` are how a Taskfile assembles a path out of
    // several variables (`{{printf "%s/lib/python%s" .DESTDIR .MAJOR}}`), so
    // both are mapped; the verbs a Taskfile uses are the string ones.
    #[test]
    fn go_printf_and_print() {
        let cases = [
            (r#"{{ printf "%s/fr.po" .D }}"#, "dir/fr.po"),
            (r#"{{ printf "%s-%v" .D 3 }}"#, "dir-3"),
            (r#"{{ printf "%q" .D }}"#, "\"dir\""),
            (r#"{{ printf "%d%%" 50 }}"#, "50%"),
            (r#"{{ printf "%-5s|" .D }}"#, "dir  |"),
            (r#"{{ printf "%5s|" .D }}"#, "  dir|"),
            (r#"{{ printf "%03d" 7 }}"#, "007"),
            // The zero padding goes after a number's sign, as it does in Go,
            // and around a string whole.
            (r#"{{ printf "%03d" -7 }}"#, "-07"),
            (r#"{{ printf "%05s" .D }}"#, "00dir"),
            (r#"{{ printf "%s" .MISSING }}"#, ""),
            (r#"{{ printf "%q" .MISSING }}"#, "\"\""),
            // A flag and a width before `%%` are ignored, as in Go.
            (r#"{{ printf "%5%" }}"#, "%"),
            // `print` separates two operands only when neither is a string.
            (r#"{{ print .D "/fr.po" }}"#, "dir/fr.po"),
            (r#"{{ print 1 2 }}"#, "1 2"),
            (r#"{{ .D | printf "%s.mo" }}"#, "dir.mo"),
        ];
        for (tmpl, want) in cases {
            let mut c = cache_with(&[("D", "dir")]);
            let got = c.replace(tmpl);
            assert!(!c.is_err(), "{tmpl} recorded {:?}", c.err());
            assert_eq!(got, want, "rendering {tmpl}");
        }
    }

    // A format this module cannot render the way Go would is an error, not
    // output that only looks right.
    #[test]
    fn printf_rejects_what_it_cannot_render() {
        for tmpl in [
            // Unsupported verb.
            r#"{{ printf "%f" 1 }}"#,
            // Precision.
            r#"{{ printf "%.2s" .D }}"#,
            // A lone trailing `%`, the likeliest real mistake.
            r#"{{ printf "100%" }}"#,
            // A width nothing could pad to, which Go gives up on too.
            r#"{{ printf "%99999999999999999999d" 1 }}"#,
            // `%d` of a string, which Go renders as `%!d(string=dir)`.
            r#"{{ printf "%d" .D }}"#,
            // `%q` of a number, which Go renders as the rune `'A'`.
            r#"{{ printf "%q" 65 }}"#,
            // A list, which Go renders as `[dir fr.po]`, through either
            // builtin.
            r#"{{ printf "%v" (splitList "/" .D) }}"#,
            r#"{{ print (splitList "/" .D) }}"#,
            // A flag Go accepts and this does not.
            r#"{{ printf "%+d" 7 }}"#,
            // Argument count mismatches, either way.
            r#"{{ printf "%s %s" .D }}"#,
            r#"{{ printf "%s" .D .D }}"#,
        ] {
            let mut c = cache_with(&[("D", "dir")]);
            c.replace(tmpl);
            assert!(c.is_err(), "{tmpl} rendered without an error");
        }
    }

    // A `.` inside a string literal is part of the literal, not field access.
    #[test]
    fn dots_inside_string_literals_are_preserved() {
        let mut c = cache_with(&[("P", "a.b.c")]);
        assert_eq!(c.replace(r#"{{ .P | replace ".b." "-" }}"#), "a-c");
        assert_eq!(c.replace(r#"{{ .P | splitList "." | join "/" }}"#), "a/b/c");
        assert_eq!(c.replace(r#"{{ if eq .P "a.b.c" }}yes{{ end }}"#), "yes");
        assert_eq!(c.replace(r#"{{ .P | splitList "." | join "." }}"#), "a.b.c");
        assert!(!c.is_err(), "recorded {:?}", c.err());
    }

    // `to_jinja` shares `rewrite_dots`, so `--migrate` preserves them too.
    #[test]
    fn migration_preserves_dots_inside_string_literals() {
        assert_eq!(
            to_jinja(r#"{{ .P | replace ".b." "-" }}"#).expect("migrates"),
            r#"{{ P | replace(".b.", "-") }}"#
        );
        assert_eq!(
            to_jinja(r#"{{trimSuffix ".po" .ITEM}}"#).expect("migrates"),
            r#"{{ trimSuffix(".po", ITEM) }}"#
        );
    }

    // `printf` and `print` migrate to the same call form, so a converted
    // Taskfile keeps composing the strings it did as Go.
    #[test]
    fn migration_converts_printf() {
        assert_eq!(
            to_jinja(r#"{{printf "%s/lib/python%s" .DESTDIR .MAJOR}}"#).expect("migrates"),
            r#"{{ printf("%s/lib/python%s", DESTDIR, MAJOR) }}"#
        );
        assert_eq!(
            to_jinja(r#"{{.KEY | default (print (joinPath .DIR "k.pem"))}}"#).expect("migrates"),
            r#"{{ KEY | default(print(joinPath(DIR, "k.pem")), true) }}"#
        );
        // After a pipe too: the subject is the last argument, not a filter's
        // first one.
        assert_eq!(
            to_jinja(r#"{{.ITEM | printf "%s.mo"}}"#).expect("migrates"),
            r#"{{ printf("%s.mo", ITEM) }}"#
        );
    }

    // A raw (backquoted) Go literal survives the rewrite intact like the other
    // literal forms, but unlike them it has no minijinja spelling, so the
    // action fails to render whether or not its body was mangled.
    #[test]
    fn raw_literals_survive_the_rewrite_but_do_not_render() {
        let mut c = cache_with(&[("P", "a.b.c")]);
        let raw = "{{ .P | replace `.b.` \"-\" }}";
        // The rewrite carries the `.b.` through untouched...
        assert_eq!(
            to_jinja(raw).expect("migrates"),
            "{{ P | replace(`.b.`, \"-\") }}"
        );
        // ...but minijinja cannot parse the result, so the action is left as
        // written and the failure is recorded.
        assert_eq!(c.replace(raw), raw);
        assert!(c.is_err());
    }

    // A dot starting an identifier and preceded by anything outside the
    // identifier set still reads as field access, even inside a literal, so a
    // native Jinja file holding one is misdetected as Go. Locked in as known
    // behaviour: `templater: jinja` is the way out.
    #[test]
    fn dot_after_a_non_identifier_inside_a_literal_looks_go() {
        assert!(has_dotted_access(r#"X | replace("/.git", "")"#));
        assert!(has_dotted_access(r#"X | replace(" .b", "")"#));
        assert!(has_dotted_access(r#"X | replace("*.rs", "")"#));
        // A dot straight after an identifier character or a quote does not.
        assert!(!has_dotted_access(r#"X | replace(".b", "")"#));
        assert!(!has_dotted_access(r#"X | replace("a.b", "")"#));
    }

    // The same helpers in pipeline position take the subject first, because
    // that is the order minijinja passes a piped value to a filter. Both
    // spellings must agree.
    #[test]
    fn sprig_helpers_agree_in_both_positions() {
        let pairs = [
            (
                r#"{{ trimSuffix ".po" .P }}"#,
                r#"{{ .P | trimSuffix ".po" }}"#,
            ),
            (
                r#"{{ trimAll "dpo/." .P }}"#,
                r#"{{ .P | trimAll "dpo/." }}"#,
            ),
            (
                r#"{{ hasPrefix "dir/" .P }}"#,
                r#"{{ .P | hasPrefix "dir/" }}"#,
            ),
            (r#"{{ contains "/fr" .P }}"#, r#"{{ .P | contains "/fr" }}"#),
            (
                r#"{{ replace "/" "_" .P }}"#,
                r#"{{ .P | replace "/" "_" }}"#,
            ),
            (
                r#"{{ splitList "/" .P | join "," }}"#,
                r#"{{ .P | splitList "/" | join "," }}"#,
            ),
            (
                r#"{{ default "fb" .MISSING }}"#,
                r#"{{ .MISSING | default "fb" }}"#,
            ),
            (r#"{{ title .P }}"#, r#"{{ .P | title }}"#),
            // `dir/fr.po` cannot tell the two `title`s apart: minijinja's
            // builtin also breaks on ASCII punctuation, and an all-lowercase
            // input hides its tail-lowercasing. These can.
            (
                r#"{{ title "HELLO world" }}"#,
                r#"{{ "HELLO world" | title }}"#,
            ),
            (r#"{{ title "a_b c" }}"#, r#"{{ "a_b c" | title }}"#),
            (r#"{{ upper .P }}"#, r#"{{ .P | upper }}"#),
            (r#"{{ lower .P }}"#, r#"{{ .P | lower }}"#),
            (r#"{{ trim .P }}"#, r#"{{ .P | trim }}"#),
            (
                r#"{{ trimPrefix "dir/" .P }}"#,
                r#"{{ .P | trimPrefix "dir/" }}"#,
            ),
            (
                r#"{{ hasSuffix ".po" .P }}"#,
                r#"{{ .P | hasSuffix ".po" }}"#,
            ),
            (r#"{{ first .MISSING }}"#, r#"{{ .MISSING | first }}"#),
            (r#"{{ last .MISSING }}"#, r#"{{ .MISSING | last }}"#),
            (
                r#"{{ first (splitList "/" .P) }}"#,
                r#"{{ .P | splitList "/" | first }}"#,
            ),
            (
                r#"{{ last (splitList "/" .P) }}"#,
                r#"{{ .P | splitList "/" | last }}"#,
            ),
            (r#"{{ join "," .P }}"#, r#"{{ .P | join "," }}"#),
        ];
        for (func_form, pipe_form) in pairs {
            let mut c = cache_with(&[("P", "dir/fr.po")]);
            let (a, b) = (c.replace(func_form), c.replace(pipe_form));
            // Checked first: once the cache records an error `replace` returns
            // its input, which would make the mismatch the misleading failure.
            assert!(
                !c.is_err(),
                "{func_form} / {pipe_form} recorded {:?}",
                c.err()
            );
            assert_eq!(a, b, "{func_form} vs {pipe_form}");
        }
    }

    // The error names the offending action and where it is, not the string it
    // came from: under `--migrate` that string is the whole Taskfile, which
    // used to be dumped into the message in full.
    #[test]
    fn unsupported_construct_points_at_the_action() {
        let src = "version: '3'\ntasks:\n  a:\n    cmds:\n      - echo {{range .LIST}}x{{end}}\n";
        let err = to_jinja(src).expect_err("range is unsupported");
        match &err {
            TemplaterError::UnsupportedConstruct {
                construct,
                action,
                line,
            } => {
                assert_eq!(construct, "range");
                assert_eq!(action, "{{range .LIST}}");
                assert_eq!(*line, Some(5));
            }
            other => panic!("expected UnsupportedConstruct, got {other:?}"),
        }
        assert_eq!(
            err.to_string(),
            "template uses unsupported Go construct \"range\" in \"{{range .LIST}}\" on line 5"
        );

        // An unmapped function is rejected deeper in, where only the action is
        // known, so the line has to be stamped on by the caller — on the `if`
        // path as much as on a plain action.
        for (src, want) in [
            (
                "version: '3'\ntasks:\n  a:\n    cmds:\n      - echo {{spew .X}}\n",
                Some(5),
            ),
            (
                "version: '3'\ntasks:\n  a:\n    cmds:\n      - |\n        echo x\n        {{if uuid}}y{{end}}\n",
                Some(7),
            ),
        ] {
            match to_jinja(src).expect_err("unmapped function") {
                TemplaterError::UnsupportedConstruct { line, .. } => assert_eq!(line, want),
                other => panic!("expected UnsupportedConstruct, got {other:?}"),
            }
        }

        // Rendering translates one field at a time, so its lines are not the
        // Taskfile's and no line is reported.
        let err = translate("echo {{spew .X}}").expect_err("spew is unmapped");
        assert_eq!(
            err.to_string(),
            "template uses unsupported Go construct \"spew\" in \"{{spew .X}}\""
        );
    }

    // A name the preflight does not know is rejected before the rewrite can
    // see it, so the call-position set has to be a subset of the mapped funcs.
    #[test]
    fn call_after_pipe_names_are_all_mapped() {
        for name in CALL_AFTER_PIPE {
            assert!(MAPPED_FUNCS.contains(name), "{name} is not a mapped func");
        }
    }

    // sprig re-cases only the leading letter of each word; minijinja's builtin
    // `title` lowercases the tail as well. Both break on ASCII punctuation, so
    // only an input with an upper-case tail tells them apart.
    #[test]
    fn title_follows_sprig_not_jinja() {
        let mut c = cache_with(&[("P", "hello-world")]);
        assert_eq!(c.replace(r#"{{ title .P }}"#), "Hello-World");
        assert_eq!(c.replace(r#"{{ title "HELLO world" }}"#), "HELLO World");
        assert_eq!(c.replace(r#"{{ title "a.b c" }}"#), "A.B C");
        assert!(!c.is_err(), "recorded {:?}", c.err());
    }

    // sprig wraps a non-list operand in a one-element list; minijinja would
    // iterate a string's characters and reject a scalar outright.
    #[test]
    fn join_treats_a_non_list_as_one_element() {
        let mut c = cache_with(&[("P", "dir/fr.po")]);
        assert_eq!(c.replace(r#"{{ join "," .P }}"#), "dir/fr.po");
        assert_eq!(c.replace(r#"{{ .P | join "," }}"#), "dir/fr.po");
        assert_eq!(
            c.replace(r#"{{ join "," (splitList "/" .P) }}"#),
            "dir,fr.po"
        );
        assert!(!c.is_err(), "recorded {:?}", c.err());
    }

    // `first`/`last` iterate rather than index, so a value they cannot iterate
    // renders empty instead of failing — and a string, which minijinja iterates
    // by character, yields a character rather than being rejected as sprig
    // rejects it.
    #[test]
    fn first_and_last_iterate() {
        let mut c = cache_with(&[("P", "dir/fr.po")]);
        assert_eq!(c.replace(r#"{{ first .MISSING }}"#), "");
        assert_eq!(c.replace(r#"{{ last .MISSING }}"#), "");
        assert_eq!(c.replace(r#"{{ first .P }}"#), "d");
        assert_eq!(c.replace(r#"{{ last .P }}"#), "o");
        assert!(!c.is_err(), "recorded {:?}", c.err());
    }

    // A list element is rendered as Go would render it, not as minijinja's
    // `Display` would: a helper stringifies inside the call, past the point
    // `set_formatter` would have corrected the casing. Nil elements are
    // dropped, the way sprig's `strslice` drops them.
    #[test]
    fn join_renders_elements_the_go_way() {
        let mut c = cache_with(&[]);
        c.set_dialect(Dialect::Jinja);
        assert_eq!(c.replace(r#"{{ join(",", [true, false]) }}"#), "true,false");
        assert_eq!(c.replace(r#"{{ join(",", [1, none, 2]) }}"#), "1,2");
        assert_eq!(c.replace(r#"{{ join(",", true) }}"#), "true");
        assert!(!c.is_err(), "recorded {:?}", c.err());
    }

    // Iterating a map yields its *keys*, in sorted order rather than insertion
    // order — indexing it would look the position up as a key, and a map has no
    // key `0`. Inserted back to front so insertion order cannot pass by luck.
    #[test]
    fn first_and_last_iterate_a_map() {
        let mut m = serde_yaml_ng::Mapping::new();
        m.insert(YamlValue::String("B".into()), YamlValue::String("y".into()));
        m.insert(YamlValue::String("A".into()), YamlValue::String("x".into()));
        let mut vars = Vars::new();
        vars.set(
            "M".to_string(),
            Var {
                value: Some(YamlValue::Mapping(m)),
                ..Default::default()
            },
        );
        let mut c = Cache::new(vars);
        assert_eq!(c.replace("{{ first .M }}"), "A");
        assert_eq!(c.replace("{{ last .M }}"), "B");
        assert!(!c.is_err(), "recorded {:?}", c.err());
    }

    // sprig's `default` substitutes for any empty value, not just an undefined
    // one; minijinja's builtin filter reaches that only with its `boolean`
    // argument set, which is what both Go spellings translate to.
    #[test]
    fn default_substitutes_for_empty_string() {
        let mut c = cache_with(&[("EMPTY", "")]);
        assert_eq!(c.replace(r#"{{ .EMPTY | default "fb" }}"#), "fb");
        assert_eq!(c.replace(r#"{{ default "fb" .EMPTY }}"#), "fb");
        assert!(!c.is_err(), "recorded {:?}", c.err());
    }

    // Every empty value sprig covers, through the filter spelling both Go forms
    // translate to — and which a Jinja Taskfile writes directly.
    #[test]
    fn default_substitutes_for_every_empty_value() {
        let mut c = cache_with(&[]);
        c.set_dialect(Dialect::Jinja);
        assert_eq!(c.replace(r#"{{ 0 | default("fb", true) }}"#), "fb");
        assert_eq!(c.replace(r#"{{ false | default("fb", true) }}"#), "fb");
        assert_eq!(c.replace(r#"{{ [] | default("fb", true) }}"#), "fb");
        assert_eq!(c.replace(r#"{{ 1 | default("fb", true) }}"#), "1");
        assert!(!c.is_err(), "recorded {:?}", c.err());
    }

    // No `default` global is registered, so the sprig-ordered call a Taskfile
    // migrated by an older release carries fails loudly instead of silently
    // taking the fallback for the value.
    #[test]
    fn sprig_ordered_default_call_is_not_a_function() {
        let mut c = cache_with(&[("EMPTY", "")]);
        c.set_dialect(Dialect::Jinja);
        c.replace(r#"{{ default("fb", EMPTY) }}"#);
        let err = c.err().expect("unknown function").to_string();
        assert!(err.contains("unknown function"), "{err}");
    }

    // Both Go spellings — piped and called — translate to the one filter form,
    // and it renders what Go rendered.
    #[test]
    fn default_translates_to_the_builtin_filter() {
        assert_eq!(
            to_jinja(r#"{{ .EMPTY | default "fb" }}"#).expect("migrates"),
            r#"{{ EMPTY | default("fb", true) }}"#
        );
        assert_eq!(
            to_jinja(r#"{{ default "fb" .EMPTY }}"#).expect("migrates"),
            r#"{{ EMPTY | default("fb", true) }}"#
        );
        // A fallback with no value at all is what sprig returns: the fallback.
        assert_eq!(
            to_jinja(r#"{{ default "fb" }}"#).expect("migrates"),
            r#"{{ "fb" }}"#
        );
        // Chained, and nested inside another call.
        assert_eq!(
            to_jinja(r#"{{ .EMPTY | default "fb" | upper }}"#).expect("migrates"),
            r#"{{ EMPTY | default("fb", true) | upper() }}"#
        );
        assert_eq!(
            to_jinja(r#"{{ printf "%s" (default "fb" .EMPTY) }}"#).expect("migrates"),
            r#"{{ printf("%s", EMPTY | default("fb", true)) }}"#
        );
        // The variadic arity stays unsupported, as it was before, not guessed at.
        assert!(to_jinja(r#"{{ default "a" .X "b" }}"#).is_err());
    }

    // minijinja takes the builtin `default` filter's arguments as a `Rest`, so
    // a keyword argument arrives as a map that its second argument reads as a
    // truthy value: `boolean=false` turns the empty-value substitution *on*.
    // The templating reference warns against the keyword form because of this;
    // if a minijinja release starts rejecting or honouring it, update that
    // warning along with this test.
    #[test]
    fn default_keyword_argument_is_read_as_on() {
        let mut c = cache_with(&[("EMPTY", "")]);
        c.set_dialect(Dialect::Jinja);
        assert_eq!(
            c.replace(r#"{{ EMPTY | default("fb", boolean=false) }}"#),
            "fb"
        );
        // Positionally it means what it says.
        assert_eq!(c.replace(r#"{{ EMPTY | default("fb", false) }}"#), "");
        assert!(!c.is_err(), "recorded {:?}", c.err());
    }

    // The four sprig helpers reached through a call are still functions in the
    // Jinja dialect, but the *filters* of those names stay minijinja's own, and
    // `default` is minijinja's filter alone: a Taskfile written natively in
    // Jinja keeps standard Jinja meaning.
    #[test]
    fn jinja_filters_are_not_overridden() {
        let mut c = cache_with(&[("EMPTY", "")]);
        c.set_dialect(Dialect::Jinja);
        // Substitutes only for an undefined value, not for every empty one.
        assert_eq!(c.replace(r#"{{ EMPTY | default("fb") }}"#), "");
        assert_eq!(c.replace(r#"{{ MISSING | default("fb") }}"#), "fb");
        assert_eq!(c.replace(r#"{{ 0 | default("fb") }}"#), "0");
        // Jinja's `title` lowercases the tail of each word; sprig's does not.
        assert_eq!(c.replace(r#"{{ "HELLO world" | title }}"#), "Hello World");
        // Jinja's `join` iterates a string by character.
        assert_eq!(c.replace(r#"{{ "ab" | join("-") }}"#), "a-b");
        // The builtin's own arities are untouched, since it *is* the builtin.
        assert_eq!(c.replace(r#"{{ MISSING | default("fb", true) }}"#), "fb");
        assert!(!c.is_err(), "recorded {:?}", c.err());
    }

    #[test]
    fn mapped_trim_prefix() {
        let mut c = cache_with(&[("P", "prefix-value")]);
        assert_eq!(c.replace("{{ .P | trimPrefix \"prefix-\" }}"), "value");
    }

    #[test]
    fn urlsafe_filter() {
        let cases = [
            ("simple", "simple"),
            ("foo:bar", "foo:bar"),
            ("ns:task@v1", "ns:task|v1"),
            ("a/b/c", "a%2Fb%2Fc"),
            ("hello world", "hello%20world"),
            ("already-safe_123", "already-safe_123"),
            ("", ""),
        ];
        for (input, want) in cases {
            let mut c = cache_with(&[("V", input)]);
            assert_eq!(c.replace("{{ .V | urlsafe }}"), want, "urlsafe({input:?})");
        }
    }

    #[test]
    fn replace_vec_applies_each() {
        let mut c = cache_with(&[("FOO", "bar")]);
        let out = c.replace_vec(&["{{.FOO}}".to_string(), "lit".to_string()]);
        assert_eq!(out, vec!["bar".to_string(), "lit".to_string()]);
    }

    #[test]
    fn replace_globs_templates_fields() {
        let mut c = cache_with(&[("DIR", "src")]);
        let globs = vec![Glob {
            glob: "{{.DIR}}/**".to_string(),
            negate: true,
            fingerprint: "{{.DIR}}/.stamp".to_string(),
            from: "deps".to_string(),
        }];
        let out = c.replace_globs(&globs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].glob, "src/**");
        assert!(out[0].negate);
        assert_eq!(out[0].fingerprint, "src/.stamp");
        assert_eq!(out[0].from, "deps");
    }

    #[test]
    fn replace_globs_empty_on_error() {
        let mut c = cache_with(&[]);
        c.replace("{{range .X}}");
        let out = c.replace_globs(&[Glob {
            glob: "x".to_string(),
            ..Default::default()
        }]);
        assert!(out.is_empty());
    }

    #[test]
    fn replace_var_static_and_sh() {
        let mut c = cache_with(&[("FOO", "bar")]);
        let var = Var {
            value: Some(YamlValue::String("{{.FOO}}".to_string())),
            sh: Some("echo {{.FOO}}".to_string()),
            ..Default::default()
        };
        let out = c.replace_var(&var);
        assert_eq!(out.value, Some(YamlValue::String("bar".to_string())));
        assert_eq!(out.sh.as_deref(), Some("echo bar"));
    }

    #[test]
    fn replace_var_nested_value() {
        let mut c = cache_with(&[("FOO", "bar")]);
        let seq = vec![YamlValue::String("{{.FOO}}".to_string())];
        let var = Var {
            value: Some(YamlValue::Sequence(seq)),
            ..Default::default()
        };
        let out = c.replace_var(&var);
        assert_eq!(
            out.value,
            Some(YamlValue::Sequence(vec![YamlValue::String(
                "bar".to_string()
            )]))
        );
    }

    #[test]
    fn resolve_ref_dot_returns_map() {
        let mut c = cache_with(&[("FOO", "bar")]);
        let v = c.resolve_ref(".");
        match v {
            YamlValue::Mapping(m) => {
                assert_eq!(
                    m.get(YamlValue::String("FOO".to_string())),
                    Some(&YamlValue::String("bar".to_string()))
                );
            }
            other => panic!("expected mapping, got {other:?}"),
        }
    }

    #[test]
    fn resolve_ref_field() {
        let mut c = cache_with(&[("FOO", "bar")]);
        assert_eq!(c.resolve_ref(".FOO"), YamlValue::String("bar".to_string()));
    }

    #[test]
    fn resolve_ref_preserves_map_type() {
        let mut vars = Vars::new();
        let mut inner = serde_yaml_ng::Mapping::new();
        inner.insert(
            YamlValue::String("K".to_string()),
            YamlValue::String("v".to_string()),
        );
        vars.set(
            "M".to_string(),
            Var {
                value: Some(YamlValue::Mapping(inner.clone())),
                ..Default::default()
            },
        );
        let mut c = Cache::new(vars);
        // A ref to a map var keeps the map type rather than stringifying it.
        assert_eq!(c.resolve_ref(".M"), YamlValue::Mapping(inner));
        // A nested ref resolves through the map.
        assert_eq!(c.resolve_ref(".M.K"), YamlValue::String("v".to_string()));
    }

    #[test]
    fn replace_vars_none_on_empty() {
        let mut c = cache_with(&[("FOO", "bar")]);
        assert!(c.replace_vars(&Vars::new()).is_none());
    }

    #[test]
    fn replace_vars_applies_each() {
        let mut c = cache_with(&[("FOO", "bar")]);
        let mut input = Vars::new();
        input.set(
            "GREETING".to_string(),
            Var {
                value: Some(YamlValue::String("hi {{.FOO}}".to_string())),
                ..Default::default()
            },
        );
        let out = c.replace_vars(&input).unwrap();
        assert_eq!(
            out.get("GREETING").unwrap().value,
            Some(YamlValue::String("hi bar".to_string()))
        );
    }

    // An escaped delimiter stays inside its literal: before this, the naive
    // scan closed the literal at the `\"` and swallowed the next one whole.
    #[test]
    fn scanners_keep_escaped_delimiters_inside_literals() {
        assert_eq!(
            tokenize(r#"replace "\"" "q""#),
            vec![r#"replace"#, r#""\"""#, r#""q""#]
        );
        // The escape has to precede the `|`: with the pipe inside the literal
        // first, the old scanner also kept the segment whole.
        assert_eq!(
            split_pipeline(r#".X | replace "\"|" "q""#),
            vec![".X", r#"replace "\"|" "q""#]
        );
        // Guarding the raw-literal path rather than the fix: a raw literal
        // takes no escapes, so the backslash does not hide the closing
        // backquote. The old scanner got this right too.
        assert_eq!(
            tokenize("trimSuffix `a\\` .X"),
            vec!["trimSuffix", "`a\\`", ".X"]
        );
        // An unterminated literal swallows the remainder instead of closing at
        // the escaped quote, the same way `rewrite_dots` treats it.
        assert_eq!(tokenize(r#"replace "a\" b"#), vec!["replace", r#""a\" b"#]);
        // A rune literal escapes too.
        assert_eq!(
            tokenize(r#"replace '\'' "q""#),
            vec!["replace", r#"'\''"#, r#""q""#]
        );
    }

    #[test]
    fn escaped_quote_renders_in_a_go_action() {
        let mut c = cache_with(&[("P", "a\"b")]);
        let out = c.replace(r#"{{ .P | replace "\"" "-" }}"#);
        assert!(!c.is_err(), "recorded {:?}", c.err());
        assert_eq!(out, "a-b");
    }

    #[test]
    fn rewrite_dots_keeps_nested() {
        assert_eq!(rewrite_dots(".FOO"), "FOO");
        assert_eq!(rewrite_dots(".FOO.BAR"), "FOO.BAR");
        assert_eq!(rewrite_dots("FOO.BAR"), "FOO.BAR");
        assert_eq!(rewrite_dots(".FOO | trim"), "FOO | trim");
        // Every Go literal form is copied verbatim, dots included.
        assert_eq!(rewrite_dots(r#"".po""#), r#"".po""#);
        assert_eq!(rewrite_dots("`.po`"), "`.po`");
        assert_eq!(rewrite_dots("'.'"), "'.'");
        // A raw literal takes no escapes, and a literal never hides the
        // rewrite of the field access that follows it.
        assert_eq!(rewrite_dots(r#"`raw " .x`.Y"#), r#"`raw " .x`Y"#);
        assert_eq!(rewrite_dots(r#""\".po" .Y"#), r#""\".po" Y"#);
        // An unterminated literal swallows the rest instead of panicking.
        assert_eq!(rewrite_dots(r#""open .po"#), r#""open .po"#);
        assert_eq!(rewrite_dots(r#""trailing \"#), r#""trailing \"#);
    }
}
