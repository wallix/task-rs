//! Compilation of the variable set visible to a task.
//!
//! [`Compiler`] resolves the layered variables that a task sees, in Go's order:
//! process environment, special vars (`TASK`, `ROOT_DIR`, …), taskfile env,
//! taskfile vars, include vars, and finally the task's own vars and the call
//! vars. Each layer is templated against the accumulated result so later layers
//! can reference earlier ones. Dynamic (`sh:`) variables are evaluated by
//! running the shell and caching the output, keyed by the command string.
//!
//! Ports Go `compiler.go`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_yaml_ng::Value;

use crate::ast::{Task, Var, Vars};
use crate::call::Call;
use crate::env;
use crate::execext::{self, RunCommandOptions};
use crate::filepathext;
use crate::logger::{Color, Logger};
use crate::templater::Cache;
use crate::version;

/// An error raised while compiling a task's variables.
#[derive(Debug)]
pub enum CompilerError {
    /// Templating a variable value failed.
    Template(crate::templater::TemplaterError),
    /// A dynamic (`sh:`) variable's command failed.
    DynamicVar {
        /// The failing command.
        command: String,
        /// The underlying execution error.
        source: execext::Error,
    },
}

impl std::fmt::Display for CompilerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Template(err) => write!(f, "{err}"),
            Self::DynamicVar { command, source } => {
                write!(f, "task: Command \"{command}\" failed: {source}")
            }
        }
    }
}

impl std::error::Error for CompilerError {}

/// Resolves the variables a task sees, evaluating and caching dynamic vars.
///
/// The dynamic-variable cache is shared (behind a mutex) so repeated
/// compilations reuse shell results, matching the Go `Compiler`.
#[derive(Clone)]
pub struct Compiler {
    /// The root working directory of the executor.
    pub dir: String,
    /// The path of the entrypoint Taskfile relative to `dir`.
    pub entrypoint: String,
    /// The directory the user invoked `task` from.
    pub user_working_dir: String,
    /// The taskfile-level environment variables.
    pub taskfile_env: Vars,
    /// The taskfile-level variables.
    pub taskfile_vars: Vars,
    /// Whether task env/vars override the inherited process environment.
    pub env_precedence: bool,
    /// Cache of dynamic-variable command output, keyed by the command string.
    dynamic_cache: Arc<Mutex<HashMap<String, String>>>,
}

impl Compiler {
    /// Creates a compiler for the given taskfile layers.
    pub fn new(
        dir: String,
        entrypoint: String,
        user_working_dir: String,
        taskfile_env: Vars,
        taskfile_vars: Vars,
        env_precedence: bool,
    ) -> Self {
        Self {
            dir,
            entrypoint,
            user_working_dir,
            taskfile_env,
            taskfile_vars,
            env_precedence,
            dynamic_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Clears the dynamic-variable cache. Ports Go `ResetCache`.
    pub fn reset_cache(&self) {
        if let Ok(mut cache) = self.dynamic_cache.lock() {
            cache.clear();
        }
    }

    /// Resolves the taskfile-level variables (no task or call context), with
    /// dynamic vars evaluated. Ports Go `GetTaskfileVariables`.
    pub async fn get_taskfile_variables(&self, logger: &mut Logger) -> Result<Vars, CompilerError> {
        self.get_variables(None, None, true, logger).await
    }

    /// Resolves the variables visible to `task` when invoked via `call`, with
    /// dynamic vars evaluated. Ports Go `GetVariables`.
    pub async fn get_variables(
        &self,
        task: Option<&Task>,
        call: Option<&Call>,
        evaluate_sh_vars: bool,
        logger: &mut Logger,
    ) -> Result<Vars, CompilerError> {
        self.compile(task, call, evaluate_sh_vars, logger).await
    }

    /// Resolves the variables without evaluating dynamic vars, so it never runs
    /// a shell. Ports Go `FastGetVariables`.
    pub async fn fast_get_variables(
        &self,
        task: Option<&Task>,
        call: Option<&Call>,
        logger: &mut Logger,
    ) -> Result<Vars, CompilerError> {
        self.compile(task, call, false, logger).await
    }

    /// The shared implementation behind the public `*get_variables` methods.
    async fn compile(
        &self,
        task: Option<&Task>,
        call: Option<&Call>,
        evaluate_sh_vars: bool,
        logger: &mut Logger,
    ) -> Result<Vars, CompilerError> {
        let mut result = env::get_environ();
        for (k, v) in self.special_vars(task, call) {
            result.set(
                k,
                Var {
                    value: Some(Value::String(v)),
                    ..Default::default()
                },
            );
        }

        // The task's directory, used when resolving that task's own vars so
        // dynamic vars run in the right place. Only needed with a task.
        let task_dir = match task {
            Some(t) => {
                let mut cache = Cache::new(result.clone());
                cache.set_dialect(t.dialect);
                let dirs = cache.replace_vec(&t.dirs);
                if let Some(err) = cache.err() {
                    return Err(CompilerError::Template(err.clone()));
                }
                let mut stack = Vec::with_capacity(dirs.len().saturating_add(1));
                stack.push(self.dir.clone());
                stack.extend(dirs);
                Some(
                    filepathext::join_dirs(&stack)
                        .to_string_lossy()
                        .into_owned(),
                )
            }
            None => None,
        };

        // Apply the taskfile env then vars, resolved against the root dir.
        let taskfile_env = self.taskfile_env.clone();
        for (k, v) in taskfile_env.all() {
            self.range_var(&mut result, k, v, &self.dir, evaluate_sh_vars, logger)
                .await?;
        }
        let taskfile_vars = self.taskfile_vars.clone();
        for (k, v) in taskfile_vars.all() {
            self.range_var(&mut result, k, v, &self.dir, evaluate_sh_vars, logger)
                .await?;
        }

        if let Some(t) = task {
            if let Some(include_vars) = &t.include_vars {
                for (k, v) in include_vars.all() {
                    self.range_var(&mut result, k, v, &self.dir, evaluate_sh_vars, logger)
                        .await?;
                }
            }
            if let Some(included_vars) = &t.included_taskfile_vars {
                let dir = task_dir.as_deref().unwrap_or(&self.dir);
                for (k, v) in included_vars.all() {
                    self.range_var(&mut result, k, v, dir, evaluate_sh_vars, logger)
                        .await?;
                }
            }
        }

        let (Some(t), Some(call)) = (task, call) else {
            return Ok(result);
        };

        for (k, v) in call.vars.all() {
            self.range_var(&mut result, k, v, &self.dir, evaluate_sh_vars, logger)
                .await?;
        }
        if let Some(task_vars) = &t.vars {
            let dir = task_dir.as_deref().unwrap_or(&self.dir);
            for (k, v) in task_vars.all() {
                self.range_var(&mut result, k, v, dir, evaluate_sh_vars, logger)
                    .await?;
            }
        }

        Ok(result)
    }

    /// Resolves a single variable and stores it in `result`. Templating happens
    /// against the current `result`, so a var may reference earlier layers.
    /// Ports the closure returned by Go `getRangeFunc`.
    async fn range_var(
        &self,
        result: &mut Vars,
        key: &str,
        var: &Var,
        dir: &str,
        evaluate_sh_vars: bool,
        logger: &mut Logger,
    ) -> Result<(), CompilerError> {
        // The variable renders in its own file's dialect, stamped at read time.
        let mut cache = Cache::new(result.clone());
        cache.set_dialect(var.dialect);
        let new_var = cache.replace_var(var);

        // When not evaluating dynamic vars, store the static value (or an empty
        // string when nil) while preserving `sh` so summaries can show it.
        if !evaluate_sh_vars {
            let value = new_var.value.clone().or(Some(Value::String(String::new())));
            result.set(
                key.to_string(),
                Var {
                    value,
                    sh: new_var.sh.clone(),
                    ..Default::default()
                },
            );
            return Ok(());
        }

        if let Some(err) = cache.err() {
            return Err(CompilerError::Template(err.clone()));
        }

        // A static value (or a non-dynamic var) is stored directly.
        if new_var.value.is_some() || new_var.sh.is_none() {
            result.set(
                key.to_string(),
                Var {
                    value: new_var.value.clone(),
                    ..Default::default()
                },
            );
            return Ok(());
        }

        // The var is dynamic: run the shell (or reuse the cached output).
        let env_list = env::get_from_vars(result, self.env_precedence);
        let static_value = self
            .handle_dynamic_var(&new_var, dir, env_list, logger)
            .await?;
        result.set(
            key.to_string(),
            Var {
                value: Some(Value::String(static_value)),
                ..Default::default()
            },
        );
        Ok(())
    }

    /// Resolves a dynamic (`sh:`) variable to its command output, caching by the
    /// command string. A per-var `dir` overrides the supplied `dir`. A single
    /// trailing newline is trimmed. Ports Go `HandleDynamicVar`.
    pub async fn handle_dynamic_var(
        &self,
        var: &Var,
        dir: &str,
        env_list: Vec<String>,
        logger: &mut Logger,
    ) -> Result<String, CompilerError> {
        let Some(command) = var.sh.as_ref().filter(|s| !s.is_empty()) else {
            return Ok(String::new());
        };

        if let Ok(cache) = self.dynamic_cache.lock()
            && let Some(result) = cache.get(command)
        {
            return Ok(result.clone());
        }

        let run_dir = if var.dir.is_empty() {
            dir.to_string()
        } else {
            var.dir.clone()
        };

        let stdout = SharedBuf::default();
        let opts = RunCommandOptions {
            command: command.clone(),
            dir: Some(std::path::PathBuf::from(run_dir)),
            env: split_env(env_list),
            posix_opts: Vec::new(),
            bash_opts: Vec::new(),
            stdout: execext::Stdio::Capture(Box::new(stdout.clone())),
            stderr: execext::Stdio::Inherit,
        };
        execext::run_command(opts)
            .await
            .map_err(|source| CompilerError::DynamicVar {
                command: command.clone(),
                source,
            })?;

        // Trim a single trailing newline (CRLF or LF) so command output is
        // easy to embed in later shell commands.
        let raw = stdout.into_string();
        let trimmed = raw
            .strip_suffix("\r\n")
            .or_else(|| raw.strip_suffix('\n'))
            .unwrap_or(&raw)
            .to_string();

        if let Ok(mut cache) = self.dynamic_cache.lock() {
            cache.insert(command.clone(), trimmed.clone());
        }
        logger.verbose_errf(
            Color::Magenta,
            &format!("task: dynamic variable: {command:?} result: {trimmed:?}\n"),
        );

        Ok(trimmed)
    }

    /// Builds the special variables injected into every compilation
    /// (`TASK_EXE`, `ROOT_DIR`, `TASK`, …). Ports Go `getSpecialVars`.
    fn special_vars(&self, task: Option<&Task>, call: Option<&Call>) -> Vec<(String, String)> {
        let root_taskfile = filepathext::smart_join(&self.dir, &self.entrypoint)
            .to_string_lossy()
            .into_owned();
        let task_exe = std::env::args()
            .next()
            .unwrap_or_default()
            .replace('\\', "/");

        let mut vars = vec![
            ("TASK_EXE".to_string(), task_exe),
            ("ROOT_TASKFILE".to_string(), root_taskfile),
            ("ROOT_DIR".to_string(), self.dir.clone()),
            (
                "USER_WORKING_DIR".to_string(),
                self.user_working_dir.clone(),
            ),
            (
                "TASK_VERSION".to_string(),
                version::get_version().to_string(),
            ),
        ];

        match task {
            Some(t) => {
                let mut stack = Vec::with_capacity(t.dirs.len().saturating_add(1));
                stack.push(self.dir.clone());
                stack.extend(t.dirs.iter().cloned());
                let task_dir = filepathext::join_dirs(&stack)
                    .to_string_lossy()
                    .into_owned();
                let taskfile = t
                    .location
                    .as_ref()
                    .map(|l| l.taskfile.clone())
                    .unwrap_or_default();
                let taskfile_dir = parent_dir(&taskfile);
                vars.push(("TASK".to_string(), t.task.clone()));
                vars.push(("TASK_DIR".to_string(), task_dir));
                vars.push(("TASKFILE".to_string(), taskfile));
                vars.push(("TASKFILE_DIR".to_string(), taskfile_dir));
            }
            None => {
                vars.push(("TASK".to_string(), String::new()));
                vars.push(("TASK_DIR".to_string(), String::new()));
                vars.push(("TASKFILE".to_string(), String::new()));
                vars.push(("TASKFILE_DIR".to_string(), String::new()));
            }
        }

        let alias = call.map(|c| c.task.clone()).unwrap_or_default();
        vars.push(("ALIAS".to_string(), alias));
        vars
    }
}

/// Splits a `KEY=VALUE` list into `(name, value)` pairs for the shell.
fn split_env(list: Vec<String>) -> Vec<(String, String)> {
    list.into_iter()
        .map(|entry| match entry.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (entry, String::new()),
        })
        .collect()
}

/// Returns the parent directory of `path` as a string, mirroring Go's
/// `filepath.Dir`. An empty path yields `.`.
fn parent_dir(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".to_string())
}

/// A thread-safe byte sink used to capture dynamic-variable command output.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    /// Consumes the buffer and returns its contents as a lossy UTF-8 string.
    fn into_string(self) -> String {
        match self.0.lock() {
            Ok(guard) => String::from_utf8_lossy(&guard).into_owned(),
            Err(poisoned) => String::from_utf8_lossy(&poisoned.into_inner()).into_owned(),
        }
    }
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut guard) = self.0.lock() {
            guard.extend_from_slice(buf);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Location, Var, VarElement, Vars};

    fn silent_logger() -> Logger {
        Logger {
            stdout: Box::new(Vec::new()),
            stderr: Box::new(Vec::new()),
            ..Default::default()
        }
    }

    fn static_var(v: &str) -> Var {
        Var {
            value: Some(Value::String(v.to_string())),
            ..Default::default()
        }
    }

    fn sh_var(cmd: &str) -> Var {
        Var {
            sh: Some(cmd.to_string()),
            ..Default::default()
        }
    }

    fn compiler(env: Vars, vars: Vars) -> Compiler {
        Compiler::new(
            "/tmp".to_string(),
            "Taskfile.yml".to_string(),
            "/tmp".to_string(),
            env,
            vars,
            false,
        )
    }

    #[tokio::test]
    async fn special_vars_are_present() {
        let c = compiler(Vars::new(), Vars::new());
        let mut logger = silent_logger();
        let result = c.get_taskfile_variables(&mut logger).await.unwrap();
        assert_eq!(
            result.get("ROOT_DIR").and_then(|v| v.value.clone()),
            Some(Value::String("/tmp".to_string()))
        );
        assert_eq!(
            result.get("TASK").and_then(|v| v.value.clone()),
            Some(Value::String(String::new()))
        );
    }

    #[tokio::test]
    async fn later_layers_reference_earlier() {
        let vars = Vars::from_elements([
            VarElement {
                key: "A".to_string(),
                value: static_var("hello"),
            },
            VarElement {
                key: "B".to_string(),
                value: static_var("{{.A}} world"),
            },
        ]);
        let c = compiler(Vars::new(), vars);
        let mut logger = silent_logger();
        let result = c.get_taskfile_variables(&mut logger).await.unwrap();
        assert_eq!(
            result.get("B").and_then(|v| v.value.clone()),
            Some(Value::String("hello world".to_string()))
        );
    }

    #[tokio::test]
    async fn call_vars_and_task_vars_merge() {
        let taskfile_vars = Vars::from_elements([VarElement {
            key: "BASE".to_string(),
            value: static_var("base"),
        }]);
        let c = compiler(Vars::new(), taskfile_vars);
        let task = Task {
            task: "build".to_string(),
            vars: Some(Vars::from_elements([VarElement {
                key: "FROM_TASK".to_string(),
                value: static_var("{{.BASE}}-task"),
            }])),
            location: Some(Location {
                taskfile: "/tmp/Taskfile.yml".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut call = Call::new("build");
        call.vars = Vars::from_elements([VarElement {
            key: "FROM_CALL".to_string(),
            value: static_var("called"),
        }]);
        let mut logger = silent_logger();
        let result = c
            .get_variables(Some(&task), Some(&call), true, &mut logger)
            .await
            .unwrap();
        assert_eq!(
            result.get("FROM_CALL").and_then(|v| v.value.clone()),
            Some(Value::String("called".to_string()))
        );
        assert_eq!(
            result.get("FROM_TASK").and_then(|v| v.value.clone()),
            Some(Value::String("base-task".to_string()))
        );
    }

    #[tokio::test]
    async fn dynamic_var_runs_and_caches() {
        let vars = Vars::from_elements([VarElement {
            key: "DYN".to_string(),
            value: sh_var("echo dynamic-result"),
        }]);
        let c = compiler(Vars::new(), vars);
        let mut logger = silent_logger();
        let result = c.get_taskfile_variables(&mut logger).await.unwrap();
        assert_eq!(
            result.get("DYN").and_then(|v| v.value.clone()),
            Some(Value::String("dynamic-result".to_string()))
        );
        // The command output is cached by command string.
        assert_eq!(
            c.dynamic_cache.lock().unwrap().get("echo dynamic-result"),
            Some(&"dynamic-result".to_string())
        );
    }

    #[tokio::test]
    async fn fast_variables_skip_dynamic() {
        let vars = Vars::from_elements([VarElement {
            key: "DYN".to_string(),
            value: sh_var("echo should-not-run"),
        }]);
        let c = compiler(Vars::new(), vars);
        let mut logger = silent_logger();
        let result = c.fast_get_variables(None, None, &mut logger).await.unwrap();
        // Fast mode leaves the value empty and preserves `sh` for display.
        let dyn_var = result.get("DYN").unwrap();
        assert_eq!(dyn_var.value, Some(Value::String(String::new())));
        assert_eq!(dyn_var.sh.as_deref(), Some("echo should-not-run"));
        // Nothing was executed, so the cache stays empty.
        assert!(c.dynamic_cache.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dynamic_var_failure_is_reported() {
        let vars = Vars::from_elements([VarElement {
            key: "DYN".to_string(),
            value: sh_var("exit 3"),
        }]);
        let c = compiler(Vars::new(), vars);
        let mut logger = silent_logger();
        let err = c.get_taskfile_variables(&mut logger).await.unwrap_err();
        assert!(matches!(err, CompilerError::DynamicVar { .. }));
        assert!(err.to_string().contains("failed"));
    }
}
