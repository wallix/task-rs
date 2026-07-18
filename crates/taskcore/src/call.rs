//! The parameters passed when invoking a task.

use crate::ast::Vars;

/// A task call: the target task name, the variables supplied to it, and flags
/// controlling how it is run. `indirect` is set when the call originates from
/// another task (a dep or a cmd) rather than the command line.
#[derive(Clone, Debug, Default)]
pub struct Call {
    /// The name (or alias) of the task to invoke.
    pub task: String,
    /// The variables supplied to the task, layered on top of its own vars.
    pub vars: Vars,
    /// Whether the call should be silenced.
    pub silent: bool,
    /// True when the task was called by another task rather than directly.
    pub indirect: bool,
}

impl Call {
    /// Creates a direct call to `task` with no variables.
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            ..Default::default()
        }
    }
}
