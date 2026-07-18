//! Executor initialization: locate and read the Taskfile, run version checks,
//! and build the logger, output style, compiler, and concurrency state.
//!
//! Ports Go `setup.go`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler::Compiler;
use crate::concurrency::ConcurrencyLimiter;
use crate::env;
use crate::execext;
use crate::filepathext;
use crate::logger::{Color, Logger};
use crate::output;
use crate::reader::{self, Reader};

use super::{Executor, ExecutorError, TempDir};

/// The Taskfile schema version below which the runner refuses to operate.
const MIN_SCHEMA_VERSION: u64 = 3;

impl Executor {
    /// Locates and reads the Taskfile, then initializes the logger, output
    /// style, compiler, environment, and concurrency state. Must be called
    /// before running any task. Ports Go `Setup`.
    pub async fn setup(&mut self) -> Result<(), ExecutorError> {
        self.setup_logger();
        let node = self.get_root_node()?;
        self.setup_temp_dir()?;
        self.read_taskfile(node.as_ref())?;
        self.setup_env_precedence();
        self.setup_output()?;
        self.setup_compiler()?;
        self.read_dotenv_files().await?;
        self.do_version_checks()?;
        self.setup_defaults();
        self.setup_concurrency_state();
        Ok(())
    }

    fn setup_logger(&mut self) {
        let logger = Logger {
            stdin: None,
            stdout: Box::new(std::io::stdout()),
            stderr: Box::new(std::io::stderr()),
            verbose: self.verbose,
            color: self.color,
            assume_yes: self.assume_yes,
            assume_term: self.assume_term,
        };
        self.logger = Rc::new(RefCell::new(logger));
    }

    fn get_root_node(&mut self) -> Result<Box<dyn reader::Node>, ExecutorError> {
        let node = reader::new_root_node(&self.entrypoint, &self.dir)?;
        self.dir = node.dir().to_string();
        self.entrypoint = node.location().to_string();
        Ok(node)
    }

    fn read_taskfile(&mut self, node: &dyn reader::Node) -> Result<(), ExecutorError> {
        // The reader's debug/prompt callbacks are `Send + Sync` and so cannot
        // borrow the `!Send` `Rc<RefCell<Logger>>`. Reading is synchronous and
        // quick, so verbose reader tracing is left out here (a known minor gap
        // relative to Go, which streams reader debug lines to stderr).
        let reader = Reader::new();
        let mut graph = reader.read(node)?;
        self.warn_deprecated_go_dialect(&graph);
        let taskfile = graph.merge().map_err(ExecutorError::Merge)?;
        self.taskfile = Some(taskfile);
        Ok(())
    }

    /// Warns once per read Taskfile that still uses the deprecated Go template
    /// dialect, pointing at `--migrate`. Go rendering will be removed in a future
    /// release (migration will remain). Suppressed by `TASK_NO_GO_DEPRECATION`.
    fn warn_deprecated_go_dialect(&self, graph: &crate::ast::TaskfileGraph) {
        if env::get_task_env("NO_GO_DEPRECATION") == "1" {
            return;
        }
        for vertex in graph.vertices() {
            if vertex.taskfile.templater != crate::ast::Dialect::Go {
                continue;
            }
            let path = filepathext::try_abs_to_rel(&vertex.uri)
                .to_string_lossy()
                .into_owned();
            self.logger().borrow_mut().warnf(&format!(
                "{path} uses the deprecated Go template dialect; run `task --migrate {path}` to convert it to Jinja (Go template support will be removed in a future release)"
            ));
        }
    }

    /// Task-defined env/vars take precedence over the inherited process
    /// environment by default. Set `TASK_X_ENV_PRECEDENCE=0` to restore the old
    /// behaviour, where a value already in the process environment wins.
    fn setup_env_precedence(&mut self) {
        self.env_precedence = env::get_task_env("X_ENV_PRECEDENCE") != "0";
    }

    fn setup_temp_dir(&mut self) -> Result<(), ExecutorError> {
        if self.temp_dir != TempDir::default() {
            return Ok(());
        }
        let temp_dir = env::get_task_env("TEMP_DIR");
        if temp_dir.is_empty() {
            self.temp_dir = TempDir {
                fingerprint: filepathext::smart_join(&self.dir, ".task")
                    .to_string_lossy()
                    .into_owned(),
            };
        } else if filepathext::is_abs(&temp_dir) || temp_dir.starts_with('~') {
            let expanded = execext::expand_literal(&temp_dir);
            let project_dir = std::fs::canonicalize(&self.dir)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| self.dir.clone());
            let project_name = std::path::Path::new(&project_dir)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.temp_dir = TempDir {
                fingerprint: filepathext::smart_join(&expanded, &project_name)
                    .to_string_lossy()
                    .into_owned(),
            };
        } else {
            self.temp_dir = TempDir {
                fingerprint: filepathext::smart_join(&self.dir, &temp_dir)
                    .to_string_lossy()
                    .into_owned(),
            };
        }
        Ok(())
    }

    fn setup_output(&mut self) -> Result<(), ExecutorError> {
        if !self.output_style.is_set()
            && let Some(tf) = &self.taskfile
        {
            self.output_style = tf.output.clone();
        }
        let logger = self.logger();
        let out = output::build_for(&self.output_style, logger)?;
        self.output = Rc::from(out);
        Ok(())
    }

    /// Merges CLI variables into the loaded Taskfile and rebuilds the compiler
    /// so the new variables are visible during templating. `globals` are merged
    /// with normal priority (overriding Taskfile defaults); `special` are
    /// reverse-merged so they are available but do not shadow user values.
    /// Ports the CLI-side `Taskfile.Vars.Merge`/`ReverseMerge` block of Go
    /// `cmd/task/task.go`, where the compiler shares the same variable map.
    pub fn merge_cli_vars(
        &mut self,
        globals: &crate::ast::Vars,
        special: &crate::ast::Vars,
    ) -> Result<(), ExecutorError> {
        if let Some(tf) = self.taskfile.as_mut() {
            tf.vars.merge(globals, None);
            tf.vars.reverse_merge(special, None);
        }
        self.setup_compiler()
    }

    fn setup_compiler(&mut self) -> Result<(), ExecutorError> {
        if self.user_working_dir.is_empty() {
            self.user_working_dir = std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        let Some(tf) = self.taskfile.as_ref() else {
            return Ok(());
        };
        let compiler = Compiler::new(
            self.dir.clone(),
            self.entrypoint.clone(),
            self.user_working_dir.clone(),
            tf.env.clone(),
            tf.vars.clone(),
            self.env_precedence,
        );
        self.compiler = Rc::new(compiler);
        Ok(())
    }

    async fn read_dotenv_files(&mut self) -> Result<(), ExecutorError> {
        let (dotenv_empty, below_v3) = match &self.taskfile {
            Some(tf) => (tf.dotenv.is_empty(), schema_below_v3(tf.version.as_deref())),
            None => (true, true),
        };
        if dotenv_empty || below_v3 {
            return Ok(());
        }

        let compiler = self.compiler();
        let vars = {
            let (mut scratch, sink) = self.scratch_logger();
            let r = compiler.get_taskfile_variables(&mut scratch).await;
            self.flush_scratch(&sink);
            r?
        };

        let dir = self.dir.clone();
        let Some(tf) = self.taskfile.as_mut() else {
            return Ok(());
        };
        let env = reader::dotenv(&vars, tf, &dir)?;
        for (k, v) in env.all() {
            if tf.env.get(k).is_none() {
                tf.env.set(k.clone(), v.clone());
            }
        }
        Ok(())
    }

    fn setup_defaults(&mut self) {
        if let Some(tf) = self.taskfile.as_mut()
            && tf.run.is_empty()
        {
            tf.run = "always".to_string();
        }
    }

    fn setup_concurrency_state(&mut self) {
        self.limiter = ConcurrencyLimiter::new(self.concurrency);
        if let Some(tf) = &self.taskfile {
            let mut counts = self.task_call_count.borrow_mut();
            for key in tf.tasks.keys(crate::sort::Sorter::None) {
                counts.insert(key, 0);
            }
        }
    }

    fn do_version_checks(&mut self) -> Result<(), ExecutorError> {
        if !self.enable_version_check {
            return Ok(());
        }
        let Some(tf) = &self.taskfile else {
            return Ok(());
        };
        let raw = tf.version.clone().unwrap_or_default();
        let schema_version = parse_lenient(&raw);

        if let Some(schema) = &schema_version
            && schema.major < MIN_SCHEMA_VERSION
        {
            return Err(ExecutorError::VersionCheck {
                uri: tf.location.clone(),
                version: raw.clone(),
                message: "no longer supported. Please use v3 or above".to_string(),
            });
        }
        // A raw string that does not parse to at least a major version is
        // treated as below v3, matching the Go semver parse of "3".
        if schema_version.is_none() {
            return Err(ExecutorError::VersionCheck {
                uri: tf.location.clone(),
                version: raw.clone(),
                message: "no longer supported. Please use v3 or above".to_string(),
            });
        }

        // If we cannot parse the current runner version (e.g. "devel"), skip
        // the upper-bound check, matching Go.
        let Ok(current) = semver::Version::parse(crate::version::get_version()) else {
            return Ok(());
        };
        if let Some(schema) = &schema_version
            && schema > &current
        {
            return Err(ExecutorError::VersionCheck {
                uri: tf.location.clone(),
                version: raw,
                message: format!("is greater than the current version of Task ({current})"),
            });
        }
        Ok(())
    }
}

/// Reports whether a raw schema-version string is below v3. A blank or
/// unparseable version is treated as below v3.
fn schema_below_v3(raw: Option<&str>) -> bool {
    match raw.map(parse_lenient) {
        Some(Some(v)) => v.major < MIN_SCHEMA_VERSION,
        _ => true,
    }
}

/// Parses a Taskfile `version:` string leniently: a bare major (`3`) or
/// major.minor (`3.1`) is accepted by padding it to a full semver, matching the
/// Go `semver.NewVersion` behavior for short versions.
fn parse_lenient(raw: &str) -> Option<semver::Version> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Ok(v) = semver::Version::parse(raw) {
        return Some(v);
    }
    let dots = raw.matches('.').count();
    let padded = match dots {
        0 => format!("{raw}.0.0"),
        1 => format!("{raw}.0"),
        _ => return None,
    };
    semver::Version::parse(&padded).ok()
}

impl Executor {
    /// Prints a warning through the logger; used by helpers that only hold a
    /// logger reference.
    #[allow(dead_code)]
    pub(crate) fn warn(&self, color: Color, msg: &str) {
        self.logger().borrow_mut().errf(color, msg);
    }
}
