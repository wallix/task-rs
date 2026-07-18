//! The source of a Taskfile's bytes.
//!
//! A [`Node`] abstracts *where* a Taskfile comes from. Two concrete sources
//! exist in this fork: a file on disk ([`FileNode`]) and the standard input
//! stream ([`StdinNode`]). Each node also knows how to resolve include paths
//! (entrypoints) and working directories relative to itself, so that includes
//! are located relative to the Taskfile that declares them rather than the
//! process working directory.

use std::io::{self, Read};
use std::path::Path;

use crate::filepathext;

use super::DEFAULT_TASKFILES;
use super::error::{ReaderError, TaskfileNotFoundError};
use super::fsext::{self, SearchError};

/// A source of Taskfile bytes and the include-resolution rules attached to it.
pub trait Node: Send + Sync {
    /// Reads the raw Taskfile bytes from the source.
    fn read(&self) -> Result<Vec<u8>, ReaderError>;

    /// A stable, printable identifier for the source (a path, or `__stdin__`).
    fn location(&self) -> &str;

    /// The working directory associated with the source.
    fn dir(&self) -> &str;

    /// Resolves an include entrypoint against this node, returning an absolute
    /// path (or one relative to this node's directory).
    fn resolve_entrypoint(&self, entrypoint: &str) -> Result<String, ReaderError>;

    /// Resolves an include directory against this node.
    fn resolve_dir(&self, dir: &str) -> Result<String, ReaderError>;
}

/// Builds the root node for a run: reads from stdin when the entrypoint is
/// `-`, otherwise from a file located via [`fsext::search`].
pub fn new_root_node(entrypoint: &str, dir: &str) -> Result<Box<dyn Node>, ReaderError> {
    let dir = fsext::default_dir(entrypoint, dir);
    if entrypoint == "-" {
        return Ok(Box::new(StdinNode::new(dir)));
    }
    Ok(Box::new(FileNode::new(entrypoint, &dir)?))
}

/// Builds a file node for an include, carrying the resolved entrypoint and
/// directory.
pub fn new_node(entrypoint: &str, dir: &str) -> Result<Box<dyn Node>, ReaderError> {
    Ok(Box::new(FileNode::new(entrypoint, dir)?))
}

/// A node that reads a Taskfile from the local filesystem.
#[derive(Clone, Debug)]
pub struct FileNode {
    dir: String,
    entrypoint: String,
}

impl FileNode {
    /// Locates the entrypoint file starting from `entrypoint`/`dir`, resolving
    /// the working directory, and records both. Missing files and ownership
    /// changes are reported through [`TaskfileNotFoundError`].
    pub fn new(entrypoint: &str, dir: &str) -> Result<Self, ReaderError> {
        let resolved_entrypoint = match fsext::search(entrypoint, dir, &DEFAULT_TASKFILES) {
            Ok(p) => p,
            Err(SearchError::NotFound) => {
                return Err(ReaderError::NotFound(TaskfileNotFoundError {
                    uri: entrypoint.to_string(),
                    walk: entrypoint.is_empty(),
                    ..Default::default()
                }));
            }
            Err(SearchError::PermissionDenied) => {
                return Err(ReaderError::NotFound(TaskfileNotFoundError {
                    uri: entrypoint.to_string(),
                    walk: true,
                    owner_change: true,
                    ..Default::default()
                }));
            }
            Err(SearchError::Io) => {
                return Err(ReaderError::Io(format!(
                    "task: failed searching for Taskfile {entrypoint:?}"
                )));
            }
        };

        let resolved_dir = fsext::resolve_dir(entrypoint, &resolved_entrypoint, dir)?;

        Ok(FileNode {
            dir: resolved_dir,
            entrypoint: resolved_entrypoint,
        })
    }

    fn resolve(&self, path: &str) -> Result<String, ReaderError> {
        let expanded = expand_literal(path)?;
        if filepathext::is_abs(&expanded) {
            return Ok(expanded);
        }
        // Includes resolve relative to the directory of the entrypoint
        // (Taskfile), not the process working directory.
        let entrypoint_dir = Path::new(&self.entrypoint)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(filepathext::smart_join(&entrypoint_dir, &expanded)
            .to_string_lossy()
            .into_owned())
    }
}

impl Node for FileNode {
    fn read(&self) -> Result<Vec<u8>, ReaderError> {
        let mut f = std::fs::File::open(&self.entrypoint)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn location(&self) -> &str {
        &self.entrypoint
    }

    fn dir(&self) -> &str {
        &self.dir
    }

    fn resolve_entrypoint(&self, entrypoint: &str) -> Result<String, ReaderError> {
        self.resolve(entrypoint)
    }

    fn resolve_dir(&self, dir: &str) -> Result<String, ReaderError> {
        self.resolve(dir)
    }
}

/// A node that reads a Taskfile from the standard input stream.
#[derive(Clone, Debug)]
pub struct StdinNode {
    dir: String,
}

impl StdinNode {
    /// Creates a stdin node associated with the given working directory.
    pub fn new(dir: String) -> Self {
        StdinNode { dir }
    }

    fn resolve(&self, path: &str) -> Result<String, ReaderError> {
        let expanded = expand_literal(path)?;
        if filepathext::is_abs(&expanded) {
            return Ok(expanded);
        }
        Ok(filepathext::smart_join(&self.dir, &expanded)
            .to_string_lossy()
            .into_owned())
    }
}

impl Node for StdinNode {
    fn read(&self) -> Result<Vec<u8>, ReaderError> {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        // Match the Go reader, which scans lines and re-appends a trailing
        // newline to each.
        let mut out = String::new();
        for line in buf.lines() {
            out.push_str(line);
            out.push('\n');
        }
        Ok(out.into_bytes())
    }

    fn location(&self) -> &str {
        "__stdin__"
    }

    fn dir(&self) -> &str {
        &self.dir
    }

    fn resolve_entrypoint(&self, entrypoint: &str) -> Result<String, ReaderError> {
        self.resolve(entrypoint)
    }

    fn resolve_dir(&self, dir: &str) -> Result<String, ReaderError> {
        self.resolve(dir)
    }
}

/// Expands `$VAR` and `${VAR}` environment-variable references in `s`, leaving
/// literal text untouched. An unset variable expands to the empty string. A `$`
/// not followed by a name is kept verbatim.
///
// TODO(port): the Go `execext.ExpandLiteral` also performs shell glob-star
// expansion via `mvdan.cc/sh`. That path is not exercised by include
// resolution in practice and depends on brush shell-expansion internals that
// are not yet exposed by the execext module.
fn expand_literal(s: &str) -> Result<String, ReaderError> {
    if s.is_empty() {
        return Ok(String::new());
    }

    let mut out = String::new();
    let mut chars = s.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some((_, '{')) => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                for (_, nc) in chars.by_ref() {
                    if nc == '}' {
                        closed = true;
                        break;
                    }
                    name.push(nc);
                }
                if !closed {
                    return Err(ReaderError::Template(format!(
                        "task: unterminated variable reference in {s:?}"
                    )));
                }
                out.push_str(&std::env::var(&name).unwrap_or_default());
            }
            Some((_, nc)) if nc == '_' || nc.is_ascii_alphabetic() => {
                let mut name = String::new();
                while let Some(&(_, nc)) = chars.peek() {
                    if nc == '_' || nc.is_ascii_alphanumeric() {
                        name.push(nc);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(&std::env::var(&name).unwrap_or_default());
            }
            _ => out.push('$'),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn expand_literal_plain_text() {
        assert_eq!(expand_literal("foo/bar.yml").unwrap(), "foo/bar.yml");
    }

    #[test]
    fn expand_literal_braced_env() {
        // SAFETY: single-threaded test setting a scoped variable.
        unsafe { std::env::set_var("TASKCORE_TEST_DIR", "sub") };
        assert_eq!(
            expand_literal("${TASKCORE_TEST_DIR}/Taskfile.yml").unwrap(),
            "sub/Taskfile.yml"
        );
        unsafe { std::env::remove_var("TASKCORE_TEST_DIR") };
    }

    #[test]
    fn expand_literal_bare_env() {
        // SAFETY: single-threaded test setting a scoped variable.
        unsafe { std::env::set_var("TASKCORE_TEST_NAME", "child") };
        assert_eq!(
            expand_literal("$TASKCORE_TEST_NAME.yml").unwrap(),
            "child.yml"
        );
        unsafe { std::env::remove_var("TASKCORE_TEST_NAME") };
    }

    #[test]
    fn expand_literal_lone_dollar_is_literal() {
        assert_eq!(expand_literal("a$-b").unwrap(), "a$-b");
    }

    #[test]
    fn file_node_reads_and_resolves() {
        let mut d = std::env::temp_dir();
        d.push(format!("taskcore-node-{}", std::process::id()));
        fs::create_dir_all(&d).unwrap();
        let tf = d.join("Taskfile.yml");
        fs::write(&tf, "version: '3'\n").unwrap();

        let node = FileNode::new(&tf.to_string_lossy(), "").unwrap();
        assert_eq!(node.read().unwrap(), b"version: '3'\n");

        let resolved = node.resolve_entrypoint("other.yml").unwrap();
        assert!(resolved.ends_with("other.yml"));
        assert!(Path::new(&resolved).is_absolute());

        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn stdin_node_location() {
        let node = StdinNode::new("/tmp".to_string());
        assert_eq!(node.location(), "__stdin__");
        assert_eq!(node.dir(), "/tmp");
    }
}
