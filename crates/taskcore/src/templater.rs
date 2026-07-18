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
//! `exeExt`, `default`, `trim`, `trimAll`, `trimPrefix`, `trimSuffix`, `lower`,
//! `upper`, `title`, `contains`, `hasPrefix`, `hasSuffix`, `replace`, `quote`,
//! `squote`, `urlsafe`, `splitList`, `join`, `first`, `last`, `base`, `dir`,
//! `ext`, and `isAbs`. Every other sprig helper (`range`-style list
//! builders, YAML/UUID/spew helpers, shell quoting, `merge`, …) is intentionally
//! left unmapped so it hits the preflight error rather than being dropped.
//!
//! [slim-sprig]: https://github.com/go-task/slim-sprig
//! [`minijinja`]: https://docs.rs/minijinja

use std::fmt;
use std::rc::Rc;

use indexmap::IndexMap;
use minijinja::value::{Rest, Value as JinjaValue};
use minijinja::{Environment, context};
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
        /// The template string that contained it.
        template: String,
    },
    /// The translated template failed to render.
    Render {
        /// The (translated) template string.
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
                template,
            } => write!(
                f,
                "template uses unsupported Go construct {construct:?} in {template:?}"
            ),
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
}

/// The style used when rendering: sentinel delimiters, comments dropped.
const RENDER_STYLE: TranslateStyle = TranslateStyle {
    block: (GO_BLOCK_START, GO_BLOCK_END),
    comment: None,
};

/// The style used when migrating a Taskfile to Jinja: readable delimiters,
/// comments preserved as Jinja comments.
const MIGRATE_STYLE: TranslateStyle = TranslateStyle {
    block: ("{%", "%}"),
    comment: Some(("{#", "#}")),
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
                    let c = translate_action(cond, tmpl)?;
                    out.push_str(&format!("{block_start} if {} {block_end}", c.trim()));
                }
                Some(Control::ElseIf(cond)) => {
                    let c = translate_action(cond, tmpl)?;
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
                        template: tmpl.to_string(),
                    });
                }
                None => {
                    let translated_body = translate_action(body, tmpl)?;
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

/// Translates a single action body, rejecting unsupported constructs.
///
/// The body is split into `|`-delimited pipeline segments. The head segment is
/// a variable reference or a function call; each following segment is a filter.
/// Go's space-separated call syntax (`f a b`) is rewritten to minijinja's call
/// syntax (`f(a, b)`), and the sprig value-last convention is honored by
/// registering multi-argument filters with the piped value as their first
/// parameter (which is what minijinja supplies).
fn translate_action(body: &str, tmpl: &str) -> Result<String, TemplaterError> {
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
            template: tmpl.to_string(),
        });
    }

    let segments = split_pipeline(trimmed);
    let mut parts: Vec<String> = Vec::with_capacity(segments.len());
    for (idx, segment) in segments.iter().enumerate() {
        let tokens = tokenize(segment);
        let Some(first) = tokens.first() else {
            return Err(TemplaterError::Render {
                template: tmpl.to_string(),
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
                .map(|a| translate_arg(a, tmpl))
                .collect::<Result<_, _>>()?;
            let expr = match first.as_str() {
                "not" => format!("(not {})", rendered_args.join(" ")),
                op => format!("({})", rendered_args.join(&format!(" {op} "))),
            };
            parts.push(expr);
            continue;
        }
        if is_bare_identifier(first) {
            // A bare identifier in either position is a function/filter call.
            if !MAPPED_FUNCS.contains(&first.as_str()) {
                return Err(TemplaterError::UnsupportedConstruct {
                    construct: first.clone(),
                    template: tmpl.to_string(),
                });
            }
            let rendered_args: Vec<String> = args
                .iter()
                .map(|a| translate_arg(a, tmpl))
                .collect::<Result<_, _>>()?;
            parts.push(format!("{first}({})", rendered_args.join(", ")));
        } else {
            // The head is a value expression (`.Foo`, a literal, a parenthesized
            // sub-call, …). Values are only valid as the first segment; a value
            // after a pipe is invalid.
            if idx != 0 || !args.is_empty() {
                return Err(TemplaterError::Render {
                    template: tmpl.to_string(),
                    message: format!("cannot translate pipeline segment {segment:?}"),
                });
            }
            parts.push(translate_arg(first, tmpl)?);
        }
    }
    Ok(parts.join(" | "))
}

/// Translates a single argument token into a minijinja expression. A
/// parenthesized token is a Go sub-expression (`(trunc 48 .TASK)`) and is
/// translated recursively into a call (`trunc(48, TASK)`); anything else is a
/// value with Go dotted-field access rewritten.
fn translate_arg(token: &str, tmpl: &str) -> Result<String, TemplaterError> {
    if let Some(inner) = token.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
        return translate_action(inner, tmpl);
    }
    Ok(rewrite_dots(token))
}

/// Splits an action body into `|`-delimited pipeline segments, keeping quoted
/// strings and parenthesized sub-expressions intact so a pipe inside a literal
/// or a nested call is not treated as a delimiter.
fn split_pipeline(body: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut cur = String::new();
    let mut in_quote: Option<char> = None;
    let mut depth = 0usize;
    for c in body.chars() {
        match in_quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    in_quote = None;
                }
            }
            None => match c {
                '"' | '\'' | '`' => {
                    in_quote = Some(c);
                    cur.push(c);
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
            },
        }
    }
    segments.push(cur.trim().to_string());
    segments
}

/// Splits a pipeline segment into whitespace-separated tokens, keeping quoted
/// strings and parenthesized sub-expressions (`(f a b)`) intact as one token.
fn tokenize(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut in_quote: Option<char> = None;
    let mut depth = 0usize;
    for c in segment.chars() {
        match in_quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    in_quote = None;
                }
            }
            None => match c {
                '"' | '\'' | '`' => {
                    in_quote = Some(c);
                    cur.push(c);
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
                        tokens.push(cur.clone());
                        cur.clear();
                    }
                }
                _ => cur.push(c),
            },
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

/// Rewrites Go dotted field access into minijinja variable access by dropping a
/// dot that immediately precedes an identifier character. `.Foo.Bar` becomes
/// `Foo.Bar`; interior dots (`Foo.Bar`) are left untouched.
fn rewrite_dots(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut prev: Option<char> = None;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
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
    env.add_function("OS", func_os);
    env.add_function("ARCH", func_arch);
    env.add_function("numCPU", func_num_cpu);
    env.add_function("exeExt", func_exe_ext);

    // The single-argument sprig helpers are registered as functions as well as
    // filters: Go Taskfiles call them in function position (`{{urlsafe .TASK}}`)
    // as often as in a pipeline (`{{.TASK | urlsafe}}`).
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
    env.add_function("splitList", filter_split_list);

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

    // Task `joinPath` (filepath.Join) and the sprig helpers `trunc`,
    // `regexReplaceAll`, and `env`, all called in function position.
    env.add_function("joinPath", func_join_path);
    env.add_function("trunc", func_trunc);
    env.add_function("regexReplaceAll", func_regex_replace_all);
    env.add_function("env", func_env);

    env.add_filter("catLines", filter_cat_lines);
    env.add_filter("splitLines", filter_split_lines);
    env.add_filter("fromSlash", filter_from_slash);
    env.add_filter("toSlash", filter_to_slash);
    env.add_filter("urlsafe", filter_urlsafe);
    env.add_filter("splitList", filter_split_list);

    // These sprig-compatible helpers are also usable as filters. minijinja
    // ships its own `default`, `trim`, `lower`, `upper`, `title`, `join`,
    // `first`, `last`, and `replace`; register the remaining ones.
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
    fn mapped_default_filter() {
        let mut c = cache_with(&[]);
        // Go pipeline syntax (`| default "x"`) is rewritten to minijinja's call
        // form, and minijinja's builtin default filter handles the value.
        assert_eq!(
            c.replace("{{ .MISSING | default \"fallback\" }}"),
            "fallback"
        );
        assert!(!c.is_err());
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

    #[test]
    fn rewrite_dots_keeps_nested() {
        assert_eq!(rewrite_dots(".FOO"), "FOO");
        assert_eq!(rewrite_dots(".FOO.BAR"), "FOO.BAR");
        assert_eq!(rewrite_dots("FOO.BAR"), "FOO.BAR");
        assert_eq!(rewrite_dots(".FOO | trim"), "FOO | trim");
    }
}
