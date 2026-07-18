//! Errors surfaced while locating, reading, and resolving Taskfiles.

use std::fmt;

/// Any failure that can occur while reading a Taskfile and its includes.
#[derive(Debug)]
pub enum ReaderError {
    /// No Taskfile was found at the given location.
    NotFound(TaskfileNotFoundError),
    /// The Taskfile could not be parsed. Carries the location and the wrapped
    /// parse error text (which may already include a highlighted snippet).
    Invalid { uri: String, err: String },
    /// The Taskfile has no schema version key.
    MissingVersion { uri: String },
    /// An include cycle was detected between two Taskfiles.
    Cycle { source: String, destination: String },
    /// An underlying I/O failure.
    Io(String),
    /// A templating failure while resolving an include's fields.
    Template(String),
}

/// Raised when no Taskfile is found when searching the filesystem.
#[derive(Clone, Debug, Default)]
pub struct TaskfileNotFoundError {
    pub uri: String,
    pub walk: bool,
    pub ask_init: bool,
    pub owner_change: bool,
}

impl fmt::Display for TaskfileNotFoundError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut walk_text = String::new();
        if self.owner_change {
            walk_text.push_str(" (or any of the parent directories until ownership changed).");
        } else if self.walk {
            walk_text.push_str(" (or any of the parent directories).");
        }
        if self.ask_init {
            walk_text.push_str(" Run `task --init` to create a new Taskfile.");
        }
        write!(f, "task: No Taskfile found at {:?}{}", self.uri, walk_text)
    }
}

impl fmt::Display for ReaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReaderError::NotFound(e) => write!(f, "{e}"),
            ReaderError::Invalid { uri, err } => {
                write!(f, "task: Failed to parse {uri}:\n{err}")
            }
            ReaderError::MissingVersion { uri } => {
                write!(f, "task: Missing schema version in Taskfile {uri:?}")
            }
            ReaderError::Cycle {
                source,
                destination,
            } => write!(
                f,
                "task: include cycle detected between {source} <--> {destination}"
            ),
            ReaderError::Io(e) => write!(f, "{e}"),
            ReaderError::Template(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReaderError {}

impl From<std::io::Error> for ReaderError {
    fn from(e: std::io::Error) -> Self {
        ReaderError::Io(e.to_string())
    }
}
