//! The template dialect a Taskfile is authored in.

use serde::Deserialize;

/// Which template syntax a Taskfile's string fields use.
///
/// Selected per file by the top-level `templater:` field (default [`Dialect::Go`]).
/// `Go` strings are translated from Go `text/template` into minijinja; `Jinja`
/// strings are rendered as native minijinja with no translation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    /// Go `text/template` (`{{.VAR}}`, pipelines, slim-sprig helpers). The default,
    /// preserving drop-in compatibility with upstream go-task Taskfiles.
    #[default]
    Go,
    /// Native minijinja / Jinja2 syntax, rendered directly.
    Jinja,
}
