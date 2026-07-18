//! The source position of a task within its Taskfile.

/// The line, column, and originating Taskfile of a task definition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Location {
    pub line: usize,
    pub column: usize,
    pub taskfile: String,
}
