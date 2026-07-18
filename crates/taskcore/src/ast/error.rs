//! The decode error raised when a Taskfile node has an unexpected shape.

use std::fmt;

/// Raised when a YAML node cannot be decoded into the expected AST type.
///
/// It carries either a wrapped underlying error, a type name (for the
/// "must be a ..." message), or a custom message.
#[derive(Debug)]
pub struct TaskfileDecodeError {
    message: String,
}

impl TaskfileDecodeError {
    /// Wraps an underlying deserialization error.
    pub fn wrap<E: fmt::Display>(err: E) -> Self {
        Self {
            message: err.to_string(),
        }
    }

    /// Builds an error stating the node must be of the given type.
    pub fn type_message(type_name: &str) -> Self {
        Self {
            message: format!("cannot unmarshal into {type_name}"),
        }
    }

    /// Builds an error with a custom message.
    pub fn message(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl fmt::Display for TaskfileDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "task: Failed to decode Taskfile: {}", self.message)
    }
}

impl std::error::Error for TaskfileDecodeError {}

impl serde::de::Error for TaskfileDecodeError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::wrap(msg)
    }
}
