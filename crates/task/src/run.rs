//! The CLI's setup-then-run flow. Ports Go `cmd/task/task.go` and the
//! CLI-side `help.go` task listing.

use std::process::ExitCode;
use std::rc::Rc;

use clap::Parser;
use taskcore::ast::{self, Var};
use taskcore::call::Call;
use taskcore::executor::{Executor, ExecutorError, TaskSummary};
use taskcore::goext;
use taskcore::sort::Sorter;
use taskcore::version;

use crate::cli::Cli;
use crate::fuzzy;
use crate::init;
use crate::prompter::CliPrompter;

/// A top-level CLI error carrying the exit code to surface.
pub struct CliError {
    message: String,
    exit_code: u8,
}

impl CliError {
    /// Creates an error with the generic "unknown" exit code.
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: 1,
        }
    }

    /// Creates an error carrying a specific exit code.
    fn with_code(message: impl Into<String>, exit_code: u8) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }

    /// The process exit code for this error.
    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Parses the arguments and runs the requested action, returning the process
/// exit code on success.
pub fn run() -> Result<ExitCode, CliError> {
    let cli = Cli::parse();

    cli.validate().map_err(CliError::new)?;

    if cli.version {
        println!("{}", version::get_version_with_build_info());
        return Ok(ExitCode::SUCCESS);
    }

    if cli.help {
        print_usage();
        return Ok(ExitCode::SUCCESS);
    }

    if cli.init {
        return run_init(&cli);
    }

    if let Some(shell) = &cli.completion {
        return run_completion(shell);
    }

    if cli.schema {
        return run_schema();
    }

    if cli.migrate {
        return run_migrate(&cli);
    }

    // The engine is single-threaded, so drive its async API on a current-thread
    // runtime inside a LocalSet.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::new(format!("task: failed to start runtime: {e}")))?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, run_engine(cli))
}

/// Builds and drives the executor for a normal (non-init) invocation.
async fn run_engine(cli: Cli) -> Result<ExitCode, CliError> {
    let sorter = match cli.sort.as_deref() {
        Some("none") => Sorter::None,
        Some("alphanumeric") => Sorter::AlphaNumeric,
        _ => Sorter::AlphaNumericWithRootTasksFirst,
    };

    let dir = if cli.global {
        std::env::var("HOME").ok().unwrap_or_default()
    } else {
        cli.dir.clone().unwrap_or_default()
    };

    let interval_ms = match &cli.interval {
        Some(s) => goext::parse_duration(s)
            .map_err(|e| CliError::new(format!("task: invalid --interval: {e}")))?
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
        None => 0,
    };

    let mut output_style = ast::Output {
        name: cli.output.clone().unwrap_or_default(),
        group: ast::OutputGroup::default(),
    };
    output_style.group.begin = cli.output_group_begin.clone().unwrap_or_default();
    output_style.group.end = cli.output_group_end.clone().unwrap_or_default();
    output_style.group.error_only = cli.output_group_error_only;

    let mut executor = Executor::new()
        .with_dir(dir)
        .with_entrypoint(cli.taskfile.clone().unwrap_or_default())
        .with_force(cli.force)
        .with_force_all(cli.force_all)
        .with_watch(cli.watch)
        .with_verbose(cli.verbose)
        .with_silent(cli.silent)
        .with_disable_fuzzy(cli.disable_fuzzy)
        .with_assume_yes(cli.yes)
        .with_interactive(cli.interactive)
        .with_dry(cli.dry)
        .with_summary(cli.summary)
        .with_parallel(cli.parallel)
        .with_color(cli.color_enabled())
        .with_concurrency(cli.concurrency)
        .with_interval_ms(interval_ms)
        .with_output_style(output_style)
        .with_task_sorter(sorter)
        .with_failfast(cli.failfast)
        .with_version_check(true)
        .with_prompter(Box::new(CliPrompter));

    executor.setup().await.map_err(executor_error_to_cli)?;

    // Task listing short-circuits before running anything. `--json` implies a
    // listing even without `--list`/`--list-all` (matching Go), except with
    // `--status`, where `--json` selects the JSON status format instead.
    if cli.list || cli.list_all || (cli.json && !cli.status) {
        if cli.json {
            let namespace = executor.editor_output(cli.list_all, cli.nested).await;
            let json = serde_json::to_string_pretty(&namespace)
                .map_err(|e| CliError::new(format!("task: encoding task list JSON: {e}")))?;
            println!("{json}");
            return Ok(ExitCode::SUCCESS);
        }
        return list_tasks(&executor, &cli);
    }

    // Split positional args into calls and `VAR=value` globals.
    let (mut calls, globals) = parse_args(&cli.args);
    if calls.is_empty() {
        calls.push(Call::new("default"));
    }

    // Merge CLI variables so they take priority over Taskfile defaults, then
    // expose the special CLI_* variables.
    let cli_args_joined = quote_join(&cli.cli_args);

    let mut special = ast::Vars::new();
    special.set("CLI_ARGS".to_string(), Var::from_string(&cli_args_joined));
    special.set(
        "CLI_ARGS_LIST".to_string(),
        Var::from_string_list(cli.cli_args.iter().cloned()),
    );
    special.set(
        "CLI_FORCE".to_string(),
        Var::from_bool(cli.force || cli.force_all),
    );
    special.set("CLI_SILENT".to_string(), Var::from_bool(cli.silent));
    special.set("CLI_VERBOSE".to_string(), Var::from_bool(cli.verbose));
    special.set("CLI_ASSUME_YES".to_string(), Var::from_bool(cli.yes));

    executor
        .merge_cli_vars(&globals, &special)
        .map_err(executor_error_to_cli)?;

    let executor = Rc::new(executor);

    // `--status` reports fingerprint state without running the tasks.
    if cli.status {
        return executor
            .status(&calls, cli.json)
            .await
            .map(|()| ExitCode::SUCCESS)
            .map_err(executor_error_to_cli);
    }

    if let Some(path) = &cli.import_cache {
        return executor
            .import_cache(std::path::Path::new(path), &calls)
            .await
            .map(|()| ExitCode::SUCCESS)
            .map_err(executor_error_to_cli);
    }

    if let Some(path) = &cli.export_cache {
        return executor
            .export_cache(std::path::Path::new(path), &calls)
            .await
            .map(|()| ExitCode::SUCCESS)
            .map_err(executor_error_to_cli);
    }

    match executor.run(&calls).await {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(err) => {
            // Rewrite a "not found" error with a fuzzy suggestion when possible.
            let err = enrich_not_found(&executor, err, cli.disable_fuzzy);
            let code = if cli.exit_code {
                err.task_exit_code()
            } else {
                err.code()
            };
            Err(CliError::with_code(err.to_string(), clamp_code(code)))
        }
    }
}

/// Runs `--init`, writing a starter Taskfile. Ports the init branch of Go
/// `run`.
fn run_init(cli: &Cli) -> Result<ExitCode, CliError> {
    let wd = std::env::current_dir()
        .map_err(|e| CliError::new(format!("task: {e}")))?
        .to_string_lossy()
        .into_owned();

    let path = match cli.args.first() {
        Some(name) => taskcore::filepathext::smart_join(&wd, name)
            .to_string_lossy()
            .into_owned(),
        None => wd,
    };

    let final_path = init::init_taskfile(&path).map_err(|e| CliError::with_code(e, 101))?;
    if !cli.silent {
        if cli.verbose {
            println!("{}", init::DEFAULT_TASKFILE);
        }
        println!("Taskfile created: {}", final_path.display());
    }
    Ok(ExitCode::SUCCESS)
}

/// Handles `--completion <shell>` by printing the embedded completion script for
/// the requested shell. The scripts are the same hand-written ones the Go build
/// ships (they call `task --list-all` for dynamic task-name completion).
fn run_completion(shell: &str) -> Result<ExitCode, CliError> {
    let script = match shell {
        "bash" => include_str!("../completion/task.bash"),
        "zsh" => include_str!("../completion/task.zsh"),
        "fish" => include_str!("../completion/task.fish"),
        "powershell" => include_str!("../completion/task.ps1"),
        other => {
            return Err(CliError::new(format!(
                "task: unknown shell: {other} (expected bash, zsh, fish, or powershell)"
            )));
        }
    };
    print!("{script}");
    Ok(ExitCode::SUCCESS)
}

/// Handles `--schema` by printing the embedded Taskfile JSON Schema. The schema
/// is the repo-root `schema.json`, baked into the binary at build time so it
/// always matches the shipped version.
fn run_schema() -> Result<ExitCode, CliError> {
    print!("{}", include_str!("../../../schema.json"));
    Ok(ExitCode::SUCCESS)
}

/// Handles `--migrate`: converts the target Taskfile(s) from Go template syntax
/// to native Jinja. Prints to stdout by default; `--write` applies in place.
fn run_migrate(cli: &Cli) -> Result<ExitCode, CliError> {
    let targets = migrate_targets(cli)?;
    if targets.is_empty() {
        return Err(CliError::new(
            "task: no Taskfile found to migrate (pass a path or use --taskfile)",
        ));
    }
    let multiple = targets.len() > 1;
    for path in &targets {
        let src = std::fs::read_to_string(path)
            .map_err(|e| CliError::new(format!("task: cannot read {path}: {e}")))?;
        let migration = taskcore::migrate::migrate_source(&src)
            .map_err(|e| CliError::with_code(format!("task: cannot migrate {path}: {e}"), 1))?;
        match migration {
            taskcore::migrate::Migration::AlreadyDeclared => {
                eprintln!("task: {path}: already declares a `templater:` dialect; skipping");
            }
            taskcore::migrate::Migration::Converted(out) => {
                if cli.write {
                    std::fs::write(path, &out)
                        .map_err(|e| CliError::new(format!("task: cannot write {path}: {e}")))?;
                    eprintln!("task: migrated {path}");
                } else {
                    if multiple {
                        println!("# ---- {path} ----");
                    }
                    print!("{out}");
                }
            }
        }
    }
    if !cli.write {
        eprintln!("task: preview only; re-run with --write to apply");
    }
    Ok(ExitCode::SUCCESS)
}

/// Resolves the files `--migrate` should operate on: the positional paths if
/// given, else the `--taskfile` entrypoint, else the default Taskfile in the
/// working directory.
fn migrate_targets(cli: &Cli) -> Result<Vec<String>, CliError> {
    if !cli.args.is_empty() {
        return Ok(cli.args.clone());
    }
    if let Some(tf) = &cli.taskfile
        && !tf.is_empty()
    {
        return Ok(vec![tf.clone()]);
    }
    let dir = cli.dir.clone().unwrap_or_else(|| ".".to_string());
    for name in taskcore::reader::DEFAULT_TASKFILES {
        let candidate = std::path::Path::new(&dir).join(name);
        if candidate.is_file() {
            return Ok(vec![candidate.to_string_lossy().into_owned()]);
        }
    }
    Ok(Vec::new())
}

/// Prints the tasks for `--list`/`--list-all`. Ports Go `ListTasks`/
/// `ListTaskNames`.
fn list_tasks(executor: &Executor, cli: &Cli) -> Result<ExitCode, CliError> {
    let tasks = executor.list_tasks(cli.list_all);

    // `--silent` prints only names (Go `ListTaskNames`).
    if cli.silent {
        for task in &tasks {
            println!("{}", task.name.trim_end_matches(':'));
            for alias in &task.aliases {
                println!("{}", alias.trim_end_matches(':'));
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    if tasks.is_empty() {
        if cli.list {
            eprintln!(
                "task: No tasks with description available. Try --list-all to list all tasks"
            );
        } else {
            eprintln!("task: No tasks available");
        }
        return Ok(ExitCode::from(1));
    }

    println!("task: Available tasks for this project:");
    print_task_table(&tasks);
    Ok(ExitCode::SUCCESS)
}

/// Prints the task list aligned in two columns (name, description + aliases).
fn print_task_table(tasks: &[TaskSummary]) {
    let width = tasks
        .iter()
        .map(|t| t.name.chars().count())
        .max()
        .unwrap_or(0);
    for task in tasks {
        let desc = task.desc.replace('\n', " ");
        let pad = width.saturating_sub(task.name.chars().count());
        let spaces = " ".repeat(pad);
        if task.aliases.is_empty() {
            println!("* {}:{}   {}", task.name, spaces, desc);
        } else {
            println!(
                "* {}:{}   {}   (aliases: {})",
                task.name,
                spaces,
                desc,
                task.aliases.join(", ")
            );
        }
    }
}

/// Splits positional arguments into task calls and `VAR=value` globals. Ports
/// Go `args.Parse`.
fn parse_args(args: &[String]) -> (Vec<Call>, ast::Vars) {
    let mut calls = Vec::new();
    let mut globals = ast::Vars::new();
    for arg in args {
        match arg.split_once('=') {
            Some((name, value)) => {
                globals.set(name.to_string(), Var::from_string(value));
            }
            None => calls.push(Call::new(arg.clone())),
        }
    }
    (calls, globals)
}

/// Rewrites a bare "task not found" error into one carrying a fuzzy suggestion.
fn enrich_not_found(executor: &Executor, err: ExecutorError, disable_fuzzy: bool) -> ExecutorError {
    let ExecutorError::TaskNotFound {
        task_name,
        did_you_mean,
    } = &err
    else {
        return err;
    };
    if disable_fuzzy || !did_you_mean.is_empty() {
        return err;
    }
    let Some(tf) = executor.taskfile() else {
        return err;
    };
    let names: Vec<String> = tf
        .tasks
        .values(Sorter::None)
        .into_iter()
        .filter(|t| !t.internal)
        .flat_map(|t| std::iter::once(t.name().to_string()).chain(t.aliases.iter().cloned()))
        .collect();
    match fuzzy::suggest(task_name, names.iter().map(String::as_str)) {
        Some(suggestion) => ExecutorError::TaskNotFound {
            task_name: task_name.clone(),
            did_you_mean: suggestion.to_string(),
        },
        None => err,
    }
}

/// Maps an [`ExecutorError`] to a [`CliError`] using the engine's exit codes.
fn executor_error_to_cli(err: ExecutorError) -> CliError {
    CliError::with_code(err.to_string(), clamp_code(err.code()))
}

/// Clamps an exit code into the `u8` range a process can return.
fn clamp_code(code: i32) -> u8 {
    code.clamp(0, 255) as u8
}

/// Joins pass-through args into a single shell-quotable string. A minimal
/// quoting suffices for the common no-whitespace case; arguments containing
/// whitespace or quotes are wrapped in single quotes.
fn quote_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.is_empty()
                || a.chars()
                    .any(|c| c.is_whitespace() || c == '\'' || c == '"' || c == '$')
            {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Prints the top-level usage text. Ports the Go `usage` string header plus a
/// hint to run `--list-all`.
fn print_usage() {
    println!(
        "Usage: task [flags...] [task...]\n\n\
Runs the specified task(s). Falls back to the \"default\" task if no task name\n\
was specified, or lists all tasks if an unknown task name was specified.\n\n\
Run 'task --list-all' to see the available tasks.\n\
Run 'task --help' via clap for the full flag list."
    );
}
