//! The command-line flag set, parsed with `clap`. Mirrors the pflag set
//! defined in the Go `internal/flags` package.

use clap::Parser;

/// The help header shown before the option list.
const ABOUT: &str = "Runs the specified task(s). Falls back to the \"default\" task if no task name \
was specified, or lists all tasks if an unknown task name was specified.";

/// The parsed command-line arguments.
///
/// Positional arguments carry task names and `VAR=value` overrides; everything
/// after a `--` is collected separately as pass-through CLI args.
#[derive(Debug, Parser)]
#[command(
    name = "task",
    about = ABOUT,
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Cli {
    /// Show Task version.
    #[arg(long)]
    pub version: bool,

    /// Shows Task usage.
    #[arg(short = 'h', long)]
    pub help: bool,

    /// Creates a new Taskfile.yml in the current folder.
    #[arg(long)]
    pub init: bool,

    /// Generates shell completion script.
    #[arg(long, value_name = "SHELL")]
    pub completion: Option<String>,

    /// Prints the JSON Schema for Taskfiles to stdout.
    #[arg(long)]
    pub schema: bool,

    /// Lists tasks with description of current Taskfile.
    #[arg(short = 'l', long)]
    pub list: bool,

    /// Lists tasks with or without a description.
    #[arg(short = 'a', long = "list-all")]
    pub list_all: bool,

    /// Formats task list as JSON.
    #[arg(short = 'j', long)]
    pub json: bool,

    /// Changes the order of the tasks when listed. [default|alphanumeric|none].
    #[arg(long)]
    pub sort: Option<String>,

    /// Nest namespaces when listing tasks as JSON.
    #[arg(long)]
    pub nested: bool,

    /// Enables watch of the given task.
    #[arg(short = 'w', long)]
    pub watch: bool,

    /// Enables verbose mode.
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Disables echoing.
    #[arg(short = 's', long)]
    pub silent: bool,

    /// Disables fuzzy matching for task names.
    #[arg(long = "disable-fuzzy")]
    pub disable_fuzzy: bool,

    /// Assume "yes" as answer to all prompts.
    #[arg(short = 'y', long)]
    pub yes: bool,

    /// Prompt for missing required variables.
    #[arg(long)]
    pub interactive: bool,

    /// Executes tasks provided on command line in parallel.
    #[arg(short = 'p', long)]
    pub parallel: bool,

    /// Compiles and prints tasks in the order that they would be run, without
    /// executing them.
    #[arg(short = 'n', long)]
    pub dry: bool,

    /// Show fingerprint status of the given task(s).
    #[arg(long)]
    pub status: bool,

    /// Show summary about a task.
    #[arg(long)]
    pub summary: bool,

    /// Pass-through the exit code of the task command.
    #[arg(short = 'x', long = "exit-code")]
    pub exit_code: bool,

    /// Sets the directory in which Task will execute and look for a Taskfile.
    #[arg(short = 'd', long)]
    pub dir: Option<String>,

    /// Choose which Taskfile to run. Defaults to "Taskfile.yml".
    #[arg(short = 't', long = "taskfile")]
    pub taskfile: Option<String>,

    /// Sets output style: [interleaved|group|prefixed].
    #[arg(short = 'o', long)]
    pub output: Option<String>,

    /// Message template to print before a task's grouped output.
    #[arg(long = "output-group-begin")]
    pub output_group_begin: Option<String>,

    /// Message template to print after a task's grouped output.
    #[arg(long = "output-group-end")]
    pub output_group_end: Option<String>,

    /// Swallow output from successful tasks.
    #[arg(long = "output-group-error-only")]
    pub output_group_error_only: bool,

    /// Colored output. Enabled by default. Set --no-color or NO_COLOR=1 to
    /// disable.
    #[arg(short = 'c', long, overrides_with = "no_color", default_value_t = true)]
    pub color: bool,

    /// Disables colored output.
    #[arg(long = "no-color")]
    pub no_color: bool,

    /// Limit number of tasks to run concurrently.
    #[arg(short = 'C', long, default_value_t = 0)]
    pub concurrency: usize,

    /// Interval to watch for changes (e.g. 500ms, 1s).
    #[arg(short = 'I', long)]
    pub interval: Option<String>,

    /// When running tasks in parallel, stop all tasks if one fails.
    #[arg(short = 'F', long)]
    pub failfast: bool,

    /// Runs global Taskfile, from $HOME/{T,t}askfile.{yml,yaml}.
    #[arg(short = 'g', long)]
    pub global: bool,

    /// Forces execution of the directly called task.
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Forces execution of the called task and all its dependent tasks.
    #[arg(long = "force-all")]
    pub force_all: bool,

    /// Exports generated files and fingerprint state to a zip file.
    #[arg(long = "export-cache", value_name = "PATH")]
    pub export_cache: Option<String>,

    /// Imports generated files and fingerprint state from a zip file.
    #[arg(long = "import-cache", value_name = "PATH")]
    pub import_cache: Option<String>,

    /// Converts the Taskfile(s) from Go template syntax to native Jinja and adds
    /// a `templater: jinja` marker. Prints the result by default; use --write to
    /// apply in place. Targets the given file paths, or the resolved Taskfile.
    #[arg(long)]
    pub migrate: bool,

    /// With --migrate, rewrites the file(s) in place instead of printing.
    #[arg(long)]
    pub write: bool,

    /// Task names and `VAR=value` overrides. Arguments after `--` are collected
    /// separately as pass-through CLI args.
    #[arg(trailing_var_arg = false)]
    pub args: Vec<String>,

    /// Pass-through arguments given after `--`, exposed as `CLI_ARGS`.
    #[arg(last = true)]
    pub cli_args: Vec<String>,
}

impl Cli {
    /// Whether colored output is enabled. Honors `--color`/`--no-color`, the
    /// `NO_COLOR` convention, and disables color when stdout is not a terminal
    /// (a pipe or file), matching Go's behavior.
    pub fn color_enabled(&self) -> bool {
        use std::io::IsTerminal;
        self.color
            && !self.no_color
            && std::env::var_os("NO_COLOR").is_none()
            && std::io::stdout().is_terminal()
    }

    /// Validates flag combinations, mirroring Go `flags.Validate`.
    pub fn validate(&self) -> Result<(), String> {
        if self.global && self.dir.is_some() {
            return Err("task: You can't set both --global and --dir".to_string());
        }
        let is_group = self.output.as_deref() == Some("group");
        if !is_group {
            if self.output_group_begin.is_some() {
                return Err(
                    "task: You can't set --output-group-begin without --output=group".to_string(),
                );
            }
            if self.output_group_end.is_some() {
                return Err(
                    "task: You can't set --output-group-end without --output=group".to_string(),
                );
            }
            if self.output_group_error_only {
                return Err(
                    "task: You can't set --output-group-error-only without --output=group"
                        .to_string(),
                );
            }
        }
        if self.list && self.list_all {
            return Err("task: cannot use --list and --list-all at the same time".to_string());
        }
        if self.json && !self.list && !self.list_all && !self.status {
            return Err("task: --json only applies to --list, --list-all, or --status".to_string());
        }
        if self.nested && !self.json {
            return Err(
                "task: --nested only applies to --json with --list or --list-all".to_string(),
            );
        }
        Ok(())
    }
}
