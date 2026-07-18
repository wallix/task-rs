//! Errors surfaced by the execution engine.

use crate::cache::CacheError;
use crate::compiler::CompilerError;
use crate::logger::LoggerError;
use crate::output::BuildError;
use crate::precondition::PreconditionError;
use crate::reader::ReaderError;
use crate::requires::RequiresError;
use crate::variables::CompileError;

/// An error raised while setting up or running the executor.
#[derive(Debug)]
pub enum ExecutorError {
    /// Locating or reading the Taskfile failed.
    Reader(Box<ReaderError>),
    /// Merging the Taskfile graph failed.
    Merge(String),
    /// The Taskfile schema version is outside the supported range.
    VersionCheck {
        /// The Taskfile URI.
        uri: String,
        /// The schema version found.
        version: String,
        /// A human-readable reason.
        message: String,
    },
    /// Building the output style failed.
    Output(BuildError),
    /// Resolving a task's variables failed.
    Compiler(Box<CompilerError>),
    /// Compiling a task failed.
    Compile(Box<CompileError>),
    /// A required-variable check failed.
    Requires(Box<RequiresError>),
    /// A precondition was not met or failed to run.
    Precondition(Box<PreconditionError>),
    /// A command failed while running a task.
    Exec(crate::execext::Error),
    /// A cache operation failed.
    Cache(Box<CacheError>),
    /// A logger/prompt operation failed.
    Logger(LoggerError),
    /// The requested task does not exist.
    TaskNotFound {
        /// The requested task name.
        task_name: String,
        /// A fuzzy "did you mean" suggestion, when available.
        did_you_mean: String,
    },
    /// The requested task is internal and cannot be called directly.
    TaskInternal {
        /// The task name.
        task_name: String,
    },
    /// Multiple tasks share the alias used in the call.
    TaskNameConflict {
        /// The alias used in the call.
        call: String,
        /// The task names that share it.
        task_names: Vec<String>,
    },
    /// A task was called more times than [`super::MAXIMUM_TASK_CALL`], indicating
    /// a dependency cycle.
    TaskCalledTooManyTimes {
        /// The offending task name.
        task_name: String,
    },
    /// A task run failed; wraps the underlying error with the task name.
    TaskRun {
        /// The task name.
        task_name: String,
        /// The underlying error.
        source: Box<ExecutorError>,
    },
    /// The run was cancelled by a signal.
    Cancelled,
    /// An I/O error occurred.
    Io(String),
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reader(e) => write!(f, "{e}"),
            Self::Merge(e) => write!(f, "{e}"),
            Self::VersionCheck {
                uri,
                version,
                message,
            } => write!(
                f,
                "task: Taskfile schema version {version:?} in {uri:?} {message}"
            ),
            Self::Output(e) => write!(f, "{e}"),
            Self::Compiler(e) => write!(f, "{e}"),
            Self::Compile(e) => write!(f, "{e}"),
            Self::Requires(e) => write!(f, "{e}"),
            Self::Precondition(e) => write!(f, "{e}"),
            Self::Exec(e) => write!(f, "{e}"),
            Self::Cache(e) => write!(f, "{e}"),
            Self::Logger(e) => write!(f, "{e}"),
            Self::TaskNotFound {
                task_name,
                did_you_mean,
            } => {
                if did_you_mean.is_empty() {
                    write!(f, "task: Task {task_name:?} does not exist")
                } else {
                    write!(
                        f,
                        "task: Task {task_name:?} does not exist. Did you mean {did_you_mean:?}?"
                    )
                }
            }
            Self::TaskInternal { task_name } => {
                write!(f, "task: Task {task_name:?} is internal")
            }
            Self::TaskNameConflict { call, task_names } => write!(
                f,
                "task: Multiple tasks ({}) match the alias {call:?}",
                task_names.join(", ")
            ),
            Self::TaskCalledTooManyTimes { task_name } => write!(
                f,
                "task: Task {task_name:?} was called too many times (probably a cyclic dependency)"
            ),
            Self::TaskRun { task_name, source } => {
                write!(f, "task: Failed to run task {task_name:?}: {source}")
            }
            Self::Cancelled => write!(f, "task: run cancelled"),
            Self::Io(e) => write!(f, "task: {e}"),
        }
    }
}

impl std::error::Error for ExecutorError {}

impl ExecutorError {
    /// The process exit code for this error, mirroring the Go `errors` package
    /// codes so callers can distinguish failure classes.
    pub fn code(&self) -> i32 {
        match self {
            Self::Reader(_) => 100,
            Self::Merge(_) => 107,
            Self::VersionCheck { .. } => 105,
            Self::TaskNotFound { .. } => 200,
            Self::TaskRun { .. } | Self::Exec(_) => 201,
            Self::TaskInternal { .. } => 202,
            Self::TaskNameConflict { .. } => 203,
            Self::TaskCalledTooManyTimes { .. } => 204,
            Self::Cancelled => 205,
            Self::Requires(_) => 206,
            // No other-appropriate code: matches Go `CodeUnknown`.
            _ => 1,
        }
    }

    /// The exit code to surface under `--exit-code`: the underlying command's
    /// status when this wraps a non-zero shell exit, otherwise [`Self::code`].
    /// Ports Go `TaskRunError.TaskExitCode`.
    pub fn task_exit_code(&self) -> i32 {
        match self {
            Self::Exec(crate::execext::Error::NonZeroExit(code)) => i32::from(*code),
            Self::TaskRun { source, .. } => source.task_exit_code(),
            _ => self.code(),
        }
    }

    /// Reports whether this error stems from the run being cancelled, unwrapping
    /// a [`ExecutorError::TaskRun`] wrapper. Ports Go `isContextError`.
    pub fn is_context_error(&self) -> bool {
        match self {
            Self::Cancelled => true,
            Self::TaskRun { source, .. } => source.is_context_error(),
            _ => false,
        }
    }
}

impl From<ReaderError> for ExecutorError {
    fn from(e: ReaderError) -> Self {
        Self::Reader(Box::new(e))
    }
}
impl From<BuildError> for ExecutorError {
    fn from(e: BuildError) -> Self {
        Self::Output(e)
    }
}
impl From<CompilerError> for ExecutorError {
    fn from(e: CompilerError) -> Self {
        Self::Compiler(Box::new(e))
    }
}
impl From<CompileError> for ExecutorError {
    fn from(e: CompileError) -> Self {
        Self::Compile(Box::new(e))
    }
}
impl From<RequiresError> for ExecutorError {
    fn from(e: RequiresError) -> Self {
        Self::Requires(Box::new(e))
    }
}
impl From<PreconditionError> for ExecutorError {
    fn from(e: PreconditionError) -> Self {
        Self::Precondition(Box::new(e))
    }
}
impl From<crate::execext::Error> for ExecutorError {
    fn from(e: crate::execext::Error) -> Self {
        Self::Exec(e)
    }
}
impl From<CacheError> for ExecutorError {
    fn from(e: CacheError) -> Self {
        Self::Cache(Box::new(e))
    }
}
impl From<LoggerError> for ExecutorError {
    fn from(e: LoggerError) -> Self {
        Self::Logger(e)
    }
}
impl From<std::io::Error> for ExecutorError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_go() {
        assert_eq!(
            ExecutorError::TaskNotFound {
                task_name: "x".to_string(),
                did_you_mean: String::new(),
            }
            .code(),
            200
        );
        assert_eq!(ExecutorError::Cancelled.code(), 205);
    }

    #[test]
    fn task_exit_code_uses_command_status() {
        let err = ExecutorError::TaskRun {
            task_name: "build".to_string(),
            source: Box::new(ExecutorError::Exec(crate::execext::Error::NonZeroExit(42))),
        };
        assert_eq!(err.task_exit_code(), 42);
        // Without --exit-code semantics, the class code is used instead.
        assert_eq!(err.code(), 201);
    }
}
