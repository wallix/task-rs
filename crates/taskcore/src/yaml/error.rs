//! The YAML error type: a message with an optional source position.

use std::fmt::{self, Debug, Display};
use std::sync::Arc;

use serde::{de, ser};

use super::path::Path;

/// An error that happened deserializing or serializing YAML data.
pub struct Error(Box<ErrorImpl>);

/// A position in the YAML source: 0-based line and column, byte index.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mark {
    pub(crate) index: usize,
    pub(crate) line: usize,
    pub(crate) column: usize,
}

impl Display for Mark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line != 0 || self.column != 0 {
            write!(
                f,
                "line {} column {}",
                self.line.saturating_add(1),
                self.column.saturating_add(1)
            )
        } else {
            write!(f, "position {}", self.index)
        }
    }
}

#[derive(Debug)]
pub(crate) enum ErrorImpl {
    Message(String, Option<Pos>),
    /// A syntax error reported by the parser.
    Scan(String, Mark),
    /// Malformed input, at a byte offset.
    Reader(&'static str, usize),
    EndOfStream,
    MoreThanOneDocument,
    RecursionLimitExceeded(Mark),
    RepetitionLimitExceeded,
    BytesUnsupported,
    SerializeNestedEnum,
    Shared(Arc<ErrorImpl>),
}

#[derive(Debug)]
pub(crate) struct Pos {
    mark: Mark,
    path: String,
}

/// The input location an error occurred at.
#[derive(Debug)]
pub struct Location {
    line: usize,
    column: usize,
}

impl Location {
    /// The 1-based line of the error.
    pub fn line(&self) -> usize {
        self.line
    }

    /// The 1-based column of the error.
    pub fn column(&self) -> usize {
        self.column
    }

    fn from_mark(mark: Mark) -> Self {
        Location {
            line: mark.line.saturating_add(1),
            column: mark.column.saturating_add(1),
        }
    }
}

impl Error {
    /// Returns where in the input the error occurred, if known.
    pub fn location(&self) -> Option<Location> {
        self.0.mark().map(Location::from_mark)
    }

    pub(crate) fn shared(self) -> Arc<ErrorImpl> {
        if let ErrorImpl::Shared(err) = *self.0 {
            err
        } else {
            Arc::from(self.0)
        }
    }
}

pub(crate) fn new(inner: ErrorImpl) -> Error {
    Error(Box::new(inner))
}

pub(crate) fn shared(shared: Arc<ErrorImpl>) -> Error {
    Error(Box::new(ErrorImpl::Shared(shared)))
}

/// A parser error moved to `mark`; other errors are kept.
pub(crate) fn moved(error: &Arc<ErrorImpl>, mark: Mark) -> Arc<ErrorImpl> {
    match &**error {
        ErrorImpl::Scan(message, _) => new(ErrorImpl::Scan(message.clone(), mark)).shared(),
        _ => Arc::clone(error),
    }
}

/// Attaches `mark` and `path` to a message error that has no position yet.
pub(crate) fn fix_mark(mut error: Error, mark: Mark, path: Path<'_>) -> Error {
    if let ErrorImpl::Message(_, none @ None) = error.0.as_mut() {
        *none = Some(Pos {
            mark,
            path: path.to_string(),
        });
    }
    error
}

impl std::error::Error for Error {}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display(f)
    }
}

impl Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.debug(f)
    }
}

impl ser::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        new(ErrorImpl::Message(msg.to_string(), None))
    }
}

impl de::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        new(ErrorImpl::Message(msg.to_string(), None))
    }
}

impl ErrorImpl {
    pub(crate) fn mark(&self) -> Option<Mark> {
        match self {
            ErrorImpl::Message(_, Some(Pos { mark, path: _ }))
            | ErrorImpl::Scan(_, mark)
            | ErrorImpl::RecursionLimitExceeded(mark) => Some(*mark),
            ErrorImpl::Reader(..) => Some(Mark::default()),
            ErrorImpl::Shared(err) => err.mark(),
            _ => None,
        }
    }

    fn message_no_mark(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorImpl::Message(msg, None) => f.write_str(msg),
            ErrorImpl::Message(msg, Some(Pos { mark: _, path })) => {
                if path != "." {
                    write!(f, "{path}: ")?;
                }
                f.write_str(msg)
            }
            ErrorImpl::Scan(msg, _) => f.write_str(msg),
            ErrorImpl::Reader(problem, offset) => {
                f.write_str(problem)?;
                if *offset != 0 {
                    write!(f, " at position {offset}")?;
                }
                Ok(())
            }
            ErrorImpl::EndOfStream => f.write_str("EOF while parsing a value"),
            ErrorImpl::MoreThanOneDocument => f.write_str(
                "deserializing from YAML containing more than one document is not supported",
            ),
            ErrorImpl::RecursionLimitExceeded(_) => f.write_str("recursion limit exceeded"),
            ErrorImpl::RepetitionLimitExceeded => f.write_str("repetition limit exceeded"),
            ErrorImpl::BytesUnsupported => {
                f.write_str("serialization and deserialization of bytes in YAML is not implemented")
            }
            ErrorImpl::SerializeNestedEnum => {
                f.write_str("serializing nested enums in YAML is not supported yet")
            }
            ErrorImpl::Shared(err) => err.message_no_mark(f),
        }
    }

    fn display(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorImpl::Shared(err) => err.display(f),
            ErrorImpl::Reader(..) => self.message_no_mark(f),
            _ => {
                self.message_no_mark(f)?;
                if let Some(mark) = self.mark()
                    && (mark.line != 0 || mark.column != 0)
                {
                    write!(f, " at {mark}")?;
                }
                Ok(())
            }
        }
    }

    fn debug(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorImpl::Shared(err) => err.debug(f),
            _ => {
                f.write_str("Error(")?;
                struct MessageNoMark<'a>(&'a ErrorImpl);
                impl Display for MessageNoMark<'_> {
                    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        self.0.message_no_mark(f)
                    }
                }
                let msg = MessageNoMark(self).to_string();
                Debug::fmt(&msg, f)?;
                if let Some(mark) = self.mark() {
                    write!(
                        f,
                        ", line: {}, column: {}",
                        mark.line.saturating_add(1),
                        mark.column.saturating_add(1),
                    )?;
                }
                f.write_str(")")
            }
        }
    }
}
