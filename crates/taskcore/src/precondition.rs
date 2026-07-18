//! Evaluation of a task's preconditions.
//!
//! Each precondition is a shell command that must exit zero. The first failing
//! precondition aborts the check: its message is logged (in magenta, matching
//! Go) and [`PreconditionError::NotMet`] is returned. A precondition runs in the
//! task's computed directory with the task's environment.

use crate::ast::Task;
use crate::env;
use crate::execext::{self, RunCommandOptions};
use crate::logger::{Color, Logger};

/// The outcome of running a task's preconditions.
#[derive(Debug)]
pub enum PreconditionError {
    /// A precondition command exited non-zero; the task must not run.
    NotMet,
    /// A precondition command could not be executed.
    Exec(execext::Error),
}

impl std::fmt::Display for PreconditionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotMet => write!(f, "task: precondition not met"),
            Self::Exec(err) => write!(f, "task: precondition failed to run: {err}"),
        }
    }
}

impl std::error::Error for PreconditionError {}

/// Runs every precondition of `task` in order, returning `Ok(true)` when all
/// pass. On the first failure the precondition's message is written to the
/// logger's stderr and `Err(PreconditionError::NotMet)` is returned. Ports Go
/// `areTaskPreconditionsMet`.
///
/// `env_precedence` controls whether task env vars override the inherited
/// process environment (default true; `TASK_X_ENV_PRECEDENCE=0` to disable).
pub async fn check(
    task: &Task,
    logger: &mut Logger,
    env_precedence: bool,
) -> Result<bool, PreconditionError> {
    let dir = task.compute_dir();
    let task_env = split_env(env::get(task, env_precedence));

    for precondition in &task.preconditions {
        let opts = RunCommandOptions {
            command: precondition.sh.clone(),
            dir: Some(dir.clone()),
            env: task_env.clone(),
            posix_opts: Vec::new(),
            bash_opts: Vec::new(),
            stdout: execext::Stdio::Inherit,
            stderr: execext::Stdio::Inherit,
        };
        if execext::run_command(opts).await.is_err() {
            logger.errf(Color::Magenta, &format!("task: {}\n", precondition.msg));
            return Err(PreconditionError::NotMet);
        }
    }

    Ok(true)
}

/// Converts the `KEY=VALUE` environment list produced by [`env::get`] into the
/// `(name, value)` pairs expected by [`execext::run_command`]. An entry without
/// `=` is treated as a name with an empty value.
fn split_env(list: Option<Vec<String>>) -> Vec<(String, String)> {
    let Some(list) = list else {
        return Vec::new();
    };
    list.into_iter()
        .map(|entry| match entry.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (entry, String::new()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Precondition;

    fn silent_logger() -> Logger {
        Logger {
            stdout: Box::new(Vec::new()),
            stderr: Box::new(Vec::new()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn all_preconditions_pass() {
        let task = Task {
            preconditions: vec![
                Precondition {
                    sh: "true".to_string(),
                    msg: "should not show".to_string(),
                },
                Precondition {
                    sh: "exit 0".to_string(),
                    msg: "nor this".to_string(),
                },
            ],
            ..Default::default()
        };
        let mut logger = silent_logger();
        let result = check(&task, &mut logger, false).await;
        assert!(matches!(result, Ok(true)));
    }

    #[tokio::test]
    async fn failing_precondition_is_not_met() {
        let task = Task {
            preconditions: vec![Precondition {
                sh: "false".to_string(),
                msg: "boom".to_string(),
            }],
            ..Default::default()
        };
        let mut logger = silent_logger();
        let result = check(&task, &mut logger, false).await;
        assert!(matches!(result, Err(PreconditionError::NotMet)));
    }

    #[tokio::test]
    async fn no_preconditions_pass() {
        let task = Task::default();
        let mut logger = silent_logger();
        assert!(matches!(check(&task, &mut logger, false).await, Ok(true)));
    }

    #[test]
    fn split_env_parses_pairs() {
        let list = Some(vec!["A=1".to_string(), "B=".to_string(), "C".to_string()]);
        let pairs = split_env(list);
        assert_eq!(pairs[0], ("A".to_string(), "1".to_string()));
        assert_eq!(pairs[1], ("B".to_string(), String::new()));
        assert_eq!(pairs[2], ("C".to_string(), String::new()));
    }
}
