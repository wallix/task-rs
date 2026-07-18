//! Converting a Go-dialect Taskfile to the native Jinja dialect.
//!
//! The migration is a textual pass over the Taskfile source: every
//! `{{ ... }}` action is rewritten to native minijinja via
//! [`templater::to_jinja`], while all surrounding YAML — keys, quoting,
//! comments, blank lines — is preserved byte-for-byte. A `templater: jinja`
//! marker is then inserted so the engine renders the file in Jinja mode.

use crate::templater::{self, TemplaterError};

/// The outcome of migrating a single Taskfile source.
pub enum Migration {
    /// The file was converted; the string is the new source.
    Converted(String),
    /// The file already declares a `templater:` dialect, so it was left as-is.
    AlreadyDeclared,
}

/// Converts a Taskfile's Go template strings to Jinja and inserts the
/// `templater: jinja` marker. Returns [`Migration::AlreadyDeclared`] when the
/// file already has a `templater:` field (idempotent — never double-migrates).
///
/// Errors if any template action uses a construct that cannot be converted
/// automatically (`range`, `with`, or an unmapped function), naming it so the
/// author can rewrite it by hand.
pub fn migrate_source(src: &str) -> Result<Migration, TemplaterError> {
    if has_templater_field(src) {
        return Ok(Migration::AlreadyDeclared);
    }
    let converted = templater::to_jinja(src)?;
    Ok(Migration::Converted(insert_marker(&converted)))
}

/// Reports whether the source already has a top-level `templater:` key.
/// Top-level keys have no indentation, which is enough to avoid matching a
/// `templater:` nested under some task.
fn has_templater_field(src: &str) -> bool {
    src.lines()
        .any(|line| !line.starts_with([' ', '\t']) && line.starts_with("templater:"))
}

/// Inserts the `templater: jinja` marker as a top-level key. It is placed
/// immediately after the `version:` line when present (the conventional first
/// key), otherwise after a leading `---` document marker, otherwise at the top.
/// The rest of the source is preserved exactly.
fn insert_marker(src: &str) -> String {
    let marker = "templater: jinja\n";

    if let Some(pos) = top_level_line_end(src, "version:") {
        let mut out = String::with_capacity(src.len().saturating_add(marker.len()));
        out.push_str(&src[..pos]);
        out.push_str(marker);
        out.push_str(&src[pos..]);
        return out;
    }
    if let Some(pos) = leading_doc_marker_end(src) {
        let mut out = String::with_capacity(src.len().saturating_add(marker.len()));
        out.push_str(&src[..pos]);
        out.push_str(marker);
        out.push_str(&src[pos..]);
        return out;
    }
    format!("{marker}{src}")
}

/// Returns the byte offset just past the newline of the first top-level line
/// whose trimmed text starts with `prefix`, or `None`.
fn top_level_line_end(src: &str, prefix: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in src.split_inclusive('\n') {
        let bare = line.trim_start_matches([' ', '\t']);
        let is_top_level = !line.starts_with([' ', '\t']);
        if is_top_level && bare.starts_with(prefix) {
            return Some(offset.saturating_add(line.len()));
        }
        offset = offset.saturating_add(line.len());
    }
    None
}

/// Returns the byte offset just past a leading `---` document marker line, or
/// `None` when the source does not start with one.
fn leading_doc_marker_end(src: &str) -> Option<usize> {
    let first = src.split_inclusive('\n').next()?;
    if first.trim_end() == "---" {
        return Some(first.len());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn converted(src: &str) -> String {
        match migrate_source(src).unwrap() {
            Migration::Converted(s) => s,
            Migration::AlreadyDeclared => panic!("expected conversion"),
        }
    }

    #[test]
    fn rewrites_actions_and_inserts_marker_after_version() {
        let src = "version: '3'\nvars:\n  DIR: '{{ joinPath .ROOT \"x\" }}'\ntasks:\n  b:\n    cmds:\n      - 'echo {{.DIR}} {{if .CI}}ci{{end}}'\n";
        let out = converted(src);
        assert!(out.contains("version: '3'\ntemplater: jinja\n"));
        assert!(out.contains(r#"{{ joinPath(ROOT, "x") }}"#));
        assert!(out.contains("{{ DIR }}"));
        assert!(out.contains("{% if CI %}ci{% endif %}"));
    }

    #[test]
    fn preserves_comments_and_structure() {
        let src = "# header comment\nversion: '3'\ntasks:\n  b:\n    # keep me\n    cmds: ['echo {{.X}}']\n";
        let out = converted(src);
        assert!(out.contains("# header comment\n"));
        assert!(out.contains("    # keep me\n"));
        assert!(out.contains("{{ X }}"));
    }

    #[test]
    fn go_comment_becomes_jinja_comment() {
        let src = "version: '3'\nvars:\n  X: 'a{{/* note */}}b'\n";
        let out = converted(src);
        assert!(out.contains("a{# note #}b"), "got: {out}");
    }

    #[test]
    fn idempotent_when_already_declared() {
        let src = "version: '3'\ntemplater: go\ntasks: {}\n";
        assert!(matches!(
            migrate_source(src).unwrap(),
            Migration::AlreadyDeclared
        ));
    }

    #[test]
    fn unsupported_construct_errors() {
        let src = "version: '3'\nvars:\n  X: '{{range .Items}}{{.}}{{end}}'\n";
        assert!(migrate_source(src).is_err());
    }
}
