//! Loading of `.env` files listed under a Taskfile's `dotenv:` key.
//!
//! Each path is templated against the given variables, joined onto `dir`, and
//! parsed. The first definition of a key wins across all files (later files do
//! not override earlier ones). Missing files are skipped silently.

use std::collections::HashMap;

use crate::ast;
use crate::filepathext;
use crate::templater;

use super::error::ReaderError;

/// Reads every `.env` file referenced by `tf.dotenv`, resolving templated
/// paths with `vars`, and returns the collected variables. Earlier files and
/// earlier keys take precedence.
pub fn dotenv(vars: &ast::Vars, tf: &ast::Taskfile, dir: &str) -> Result<ast::Vars, ReaderError> {
    let mut env = ast::Vars::new();
    let mut cache = templater::Cache::new(vars.clone());
    cache.set_dialect(tf.templater);

    for raw_path in &tf.dotenv {
        let path = cache.replace(raw_path);
        if let Some(e) = cache.err() {
            return Err(ReaderError::Template(e.to_string()));
        }
        if path.is_empty() {
            continue;
        }
        let full = filepathext::smart_join(dir, &path);
        if !full.exists() {
            continue;
        }

        let contents = std::fs::read_to_string(&full).map_err(|e| {
            ReaderError::Io(format!("error reading env file {}: {e}", full.display()))
        })?;

        let envs = parse(&contents).map_err(|e| {
            ReaderError::Io(format!("error reading env file {}: {e}", full.display()))
        })?;

        for (key, value) in envs {
            if env.get(&key).is_none() {
                env.set(
                    key,
                    ast::Var {
                        value: Some(serde_yaml_ng::Value::String(value)),
                        ..Default::default()
                    },
                );
            }
        }
    }

    Ok(env)
}

/// Parses the contents of a `.env` file into an ordered key/value list.
///
/// Recognized syntax:
/// - `KEY=VALUE` and `export KEY=VALUE` assignments.
/// - `#` comments (full-line, or trailing on unquoted values).
/// - Blank lines.
/// - Single- or double-quoted values; double quotes honor `\n`, `\r`, `\t`,
///   `\\` and `\"` escapes, single quotes are literal.
/// - Surrounding whitespace trimmed from keys and unquoted values.
///
/// Later occurrences of a key overwrite earlier ones, matching `godotenv`.
pub fn parse(contents: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();

    for raw in contents.lines() {
        let line = strip_leading_bom(raw).trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let mut line = line;
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim_start();
        }

        let Some(eq) = line.find('=') else {
            return Err(format!("line without '=' separator: {raw:?}"));
        };

        let key = line.get(..eq).unwrap_or_default().trim();
        if key.is_empty() {
            return Err(format!("empty key: {raw:?}"));
        }
        let rest = line.get(eq.saturating_add(1)..).unwrap_or_default();
        let value = parse_value(rest)?;

        if let Some(&i) = index.get(key) {
            if let Some(entry) = out.get_mut(i) {
                entry.1 = value;
            }
        } else {
            index.insert(key.to_string(), out.len());
            out.push((key.to_string(), value));
        }
    }

    Ok(out)
}

fn strip_leading_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

fn parse_value(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim_start();
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(q @ ('"' | '\'')) => parse_quoted(trimmed, q),
        _ => {
            // Unquoted: cut any trailing comment introduced by a whitespace-
            // preceded '#', then trim surrounding whitespace.
            Ok(strip_unquoted_comment(trimmed).trim().to_string())
        }
    }
}

fn parse_quoted(s: &str, quote: char) -> Result<String, String> {
    let mut out = String::new();
    // Skip the opening quote.
    let mut chars = s.chars();
    chars.next();
    let double = quote == '"';
    let mut closed = false;
    while let Some(c) = chars.next() {
        if double && c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
            continue;
        }
        if c == quote {
            closed = true;
            break;
        }
        out.push(c);
    }
    if !closed {
        return Err(format!("unterminated quoted value: {s:?}"));
    }
    Ok(out)
}

fn strip_unquoted_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes.get(i).copied().unwrap_or(0);
        if b == b'#' {
            // A comment starts at '#' if it is at the start or preceded by
            // whitespace.
            let prev = i.checked_sub(1).and_then(|p| bytes.get(p)).copied();
            if i == 0 || matches!(prev, Some(b' ') | Some(b'\t')) {
                return s.get(..i).unwrap_or(s);
            }
        }
        i = i.saturating_add(1);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: Vec<(String, String)>) -> HashMap<String, String> {
        pairs.into_iter().collect()
    }

    #[test]
    fn simple_pairs() {
        let m = map(parse("FOO=bar\nBAZ=qux\n").unwrap());
        assert_eq!(m.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(m.get("BAZ").map(String::as_str), Some("qux"));
    }

    #[test]
    fn comments_and_blanks() {
        let m = map(parse("# comment\n\nFOO=bar\n  # indented\n").unwrap());
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn export_prefix() {
        let m = map(parse("export FOO=bar\n").unwrap());
        assert_eq!(m.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn trailing_comment_on_unquoted() {
        let m = map(parse("FOO=bar # trailing\n").unwrap());
        assert_eq!(m.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn hash_inside_unquoted_value_kept() {
        let m = map(parse("URL=http://host/#frag\n").unwrap());
        assert_eq!(m.get("URL").map(String::as_str), Some("http://host/#frag"));
    }

    #[test]
    fn double_quotes_with_escapes() {
        let m = map(parse("FOO=\"line1\\nline2\"\n").unwrap());
        assert_eq!(m.get("FOO").map(String::as_str), Some("line1\nline2"));
    }

    #[test]
    fn single_quotes_are_literal() {
        let m = map(parse("FOO='a\\nb # not comment'\n").unwrap());
        assert_eq!(
            m.get("FOO").map(String::as_str),
            Some("a\\nb # not comment")
        );
    }

    #[test]
    fn whitespace_trimmed_around_key_and_value() {
        let m = map(parse("  FOO = bar  \n").unwrap());
        assert_eq!(m.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn later_key_overrides() {
        let pairs = parse("FOO=1\nFOO=2\n").unwrap();
        assert_eq!(pairs, vec![("FOO".to_string(), "2".to_string())]);
    }

    #[test]
    fn insertion_order_preserved() {
        let pairs = parse("B=1\nA=2\nC=3\n").unwrap();
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["B", "A", "C"]);
    }

    #[test]
    fn line_without_equals_errors() {
        assert!(parse("NOTANASSIGNMENT\n").is_err());
    }
}
