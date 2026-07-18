//! Shell command execution backed by the pure-Rust `brush` bash implementation.
//!
//! A single [`run_command`] call builds a one-shot shell, applies the requested
//! POSIX (`set`) and bash (`shopt`) options, wires up stdio, and runs a command
//! string. Non-zero exit status is reported as an error; `errexit` (`set -e`) is
//! enabled by default so a failing command aborts the string.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::thread::JoinHandle;

use brush_builtins::{BuiltinSet, default_builtins};
use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::{Shell, ShellFd, ShellVariable};

/// Errors produced while preparing or running a shell command.
#[derive(Debug)]
pub enum Error {
    /// The command exited with a non-zero status.
    NonZeroExit(u8),
    /// The underlying shell reported an error while building or running.
    Shell(brush_core::Error),
    /// Stdio wiring (pipe creation or draining) failed.
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonZeroExit(code) => write!(f, "command exited with status {code}"),
            Self::Shell(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Shell(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::NonZeroExit(_) => None,
        }
    }
}

impl From<brush_core::Error> for Error {
    fn from(err: brush_core::Error) -> Self {
        Self::Shell(err)
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Destination for a shell's standard output or standard error stream.
#[derive(Default)]
pub enum Stdio {
    /// Inherit the corresponding stream of the current process.
    #[default]
    Inherit,
    /// Discard all output.
    Null,
    /// Forward every byte to the given writer, running on a helper thread.
    Capture(Box<dyn Write + Send>),
}

/// Options for [`run_command`].
pub struct RunCommandOptions {
    /// The shell command string to execute.
    pub command: String,
    /// Working directory for the shell. Created lazily by commands if missing,
    /// so a not-yet-existing path is accepted and used as-is.
    pub dir: Option<PathBuf>,
    /// Environment for the shell. When empty, the process environment is
    /// inherited; otherwise exactly these variables are exported.
    pub env: Vec<(String, String)>,
    /// POSIX `set` options to enable, as either single letters (`e`) or long
    /// names (`pipefail`). `errexit` is always enabled in addition to these.
    pub posix_opts: Vec<String>,
    /// Bash `shopt` options to enable (e.g. `globstar`).
    pub bash_opts: Vec<String>,
    /// Where standard output is sent.
    pub stdout: Stdio,
    /// Where standard error is sent.
    pub stderr: Stdio,
}

impl RunCommandOptions {
    /// Builds options for `command` with all other fields at their defaults
    /// (inherited environment and stdio, errexit on).
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            dir: None,
            env: Vec::new(),
            posix_opts: Vec::new(),
            bash_opts: Vec::new(),
            stdout: Stdio::default(),
            stderr: Stdio::default(),
        }
    }
}

/// A pipe whose read end is drained into a caller-provided writer on a helper
/// thread. Joining the thread flushes all remaining output.
struct CaptureDrain {
    handle: JoinHandle<std::io::Result<()>>,
}

impl CaptureDrain {
    /// Spawns the draining thread and returns it together with the pipe's write
    /// end, which is handed to the shell as an open file.
    fn spawn(mut sink: Box<dyn Write + Send>) -> Result<(Self, OpenFile), Error> {
        let (mut reader, writer) = std::io::pipe()?;
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                let chunk = buf.get(..n).unwrap_or(&[]);
                sink.write_all(chunk)?;
            }
        });
        Ok((Self { handle }, OpenFile::from(writer)))
    }

    /// Waits for the writer to close and all buffered bytes to be forwarded.
    fn finish(self) -> Result<(), Error> {
        match self.handle.join() {
            Ok(result) => result.map_err(Error::Io),
            Err(_) => Err(Error::Io(std::io::Error::other("capture thread panicked"))),
        }
    }
}

/// Resolves a [`Stdio`] destination into an open file for the given descriptor,
/// returning a drain to join afterwards when the output is captured.
fn open_file_for(
    fd: ShellFd,
    dest: Stdio,
) -> Result<(Option<OpenFile>, Option<CaptureDrain>), Error> {
    match dest {
        Stdio::Inherit => {
            let file = if fd == OpenFiles::STDERR_FD {
                OpenFile::from(std::io::stderr())
            } else {
                OpenFile::from(std::io::stdout())
            };
            Ok((Some(file), None))
        }
        Stdio::Null => Ok((None, None)),
        Stdio::Capture(sink) => {
            let (drain, file) = CaptureDrain::spawn(sink)?;
            Ok((Some(file), Some(drain)))
        }
    }
}

/// Renders a `set`/`shopt` prelude that enables the requested options, or an
/// empty string when there is nothing to enable.
fn options_prelude(posix_opts: &[String], bash_opts: &[String]) -> String {
    let mut prelude = String::new();
    // errexit is always on; single letters take `-x`, long names take `-o name`.
    let mut set_args = vec![String::from("-e")];
    for opt in posix_opts {
        if opt.chars().count() == 1 {
            set_args.push(format!("-{opt}"));
        } else {
            set_args.push(String::from("-o"));
            set_args.push(opt.clone());
        }
    }
    prelude.push_str("set ");
    prelude.push_str(&set_args.join(" "));
    prelude.push('\n');

    if !bash_opts.is_empty() {
        prelude.push_str("shopt -s ");
        prelude.push_str(&bash_opts.join(" "));
        prelude.push('\n');
    }
    prelude
}

/// Runs a shell command string, returning an error on non-zero exit.
///
/// The command runs under a fresh non-interactive bash-compatible shell with
/// `errexit` enabled, the requested POSIX/bash options applied, and stdio wired
/// per [`RunCommandOptions`].
pub async fn run_command(opts: RunCommandOptions) -> Result<(), Error> {
    let (stdout_file, stdout_drain) = open_file_for(OpenFiles::STDOUT_FD, opts.stdout)?;
    let (stderr_file, stderr_drain) = open_file_for(OpenFiles::STDERR_FD, opts.stderr)?;

    let mut fds: HashMap<ShellFd, OpenFile> = HashMap::new();
    if let Some(file) = stdout_file {
        fds.insert(OpenFiles::STDOUT_FD, file);
    }
    if let Some(file) = stderr_file {
        fds.insert(OpenFiles::STDERR_FD, file);
    }

    let have_env = !opts.env.is_empty();
    let mut shell = Shell::builder()
        .builtins(default_builtins::<
            brush_core::extensions::DefaultShellExtensions,
        >(BuiltinSet::BashMode))
        .no_editing(true)
        .fds(fds)
        // errexit is applied via the `set -e` prelude so both single-letter and
        // long-form POSIX options share one code path.
        .do_not_inherit_env(have_env)
        .maybe_working_dir(opts.dir)
        .build()
        .await?;

    for (name, value) in opts.env {
        let mut var = ShellVariable::new(value);
        var.export();
        shell.set_env_global(&name, var)?;
    }

    let prelude = options_prelude(&opts.posix_opts, &opts.bash_opts);
    let source_info = brush_core::SourceInfo::from("task");
    let params = shell.default_exec_params();
    let prelude_result = shell.run_string(prelude, &source_info, &params).await?;
    if !prelude_result.is_success() {
        return finish_with(
            stdout_drain,
            stderr_drain,
            Err(Error::Shell(brush_core::Error::from(
                std::io::Error::other("failed to apply shell options"),
            ))),
        );
    }

    let result = shell.run_string(opts.command, &source_info, &params).await;

    // Close the shell's write ends before joining the drains so the reader
    // threads observe end-of-file.
    drop(shell);

    let run_result = match result {
        Ok(execution) if execution.is_success() => Ok(()),
        Ok(execution) => Err(Error::NonZeroExit(execution.exit_code.into())),
        Err(err) => Err(Error::Shell(err)),
    };
    finish_with(stdout_drain, stderr_drain, run_result)
}

/// Expands shell symbols in a literal string: a leading `~` becomes the home
/// directory, and `$VAR` / `${VAR}` references are resolved from the process
/// environment. An unset variable expands to the empty string, matching the
/// shell (and the Go `expand.Literal` wrapper). An empty input yields an empty
/// string.
///
/// Unlike [`expand_fields`], no word-splitting, brace expansion, or globbing is
/// performed and the result is always a single string.
pub fn expand_literal(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let expanded = expand_tilde(s);
    expand_env(&expanded)
}

/// Expands a string into fields the way the shell does for pathname arguments:
/// spaces (and `&`, `(`, `)`) are treated as literal characters, then `~` and
/// environment variables are expanded, the string is split into words, and each
/// word is glob-expanded against the filesystem (with `globstar` on). Words that
/// match no files are dropped (bash `nullglob`), and a word with no glob
/// metacharacters yields itself.
///
/// This mirrors the Go `expand.Fields` wrapper, whose only caller is glob
/// expansion of `sources:`/`generates:`; the filesystem walk is shared with
/// [`crate::fingerprint::glob`].
pub fn expand_fields(s: &str) -> Vec<String> {
    // Escape the characters the Go wrapper escapes so they survive
    // word-splitting as literals.
    let escaped = escape_fields(s);
    let expanded = expand_env(&expand_tilde(&escaped));

    let mut out = Vec::new();
    for word in split_fields(&expanded) {
        let unescaped = unescape_fields(&word);
        if has_glob_meta(&unescaped) {
            let matches = crate::fingerprint::glob("", &unescaped).unwrap_or_default();
            // nullglob: a pattern that matches nothing contributes no field.
            out.extend(matches);
        } else {
            out.push(unescaped);
        }
    }
    out
}

/// Escapes the characters the Go `escape` helper escapes before field splitting:
/// path separators are normalized to `/` and spaces, `&`, `(`, `)` are
/// backslash-escaped so they are not treated as word boundaries or operators.
fn escape_fields(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push('/'),
            ' ' | '&' | '(' | ')' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

/// Splits on unescaped whitespace, honoring backslash escapes produced by
/// [`escape_fields`]. Escape sequences are preserved for later unescaping.
fn split_fields(s: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                current.push('\\');
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    fields.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        fields.push(current);
    }
    fields
}

/// Removes the backslash escapes added by [`escape_fields`], restoring the
/// literal characters.
fn unescape_fields(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Expands a leading `~` (alone or followed by `/`) into the current user's
/// home directory. Other tildes are left untouched.
fn expand_tilde(s: &str) -> String {
    let Some(rest) = s.strip_prefix('~') else {
        return s.to_string();
    };
    if !rest.is_empty() && !rest.starts_with('/') {
        return s.to_string();
    }
    let Some(home) = home_dir() else {
        return s.to_string();
    };
    format!("{home}{rest}")
}

/// Resolves the home directory from `HOME` (or `USERPROFILE` on Windows),
/// returning `None` when unset.
fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|h| !h.is_empty())
}

/// Substitutes `$VAR` and `${VAR}` references from the process environment. A
/// `$$` escapes a literal `$`; an unset variable resolves to the empty string.
fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('$') => {
                chars.next();
                out.push('$');
            }
            Some('{') => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                if closed {
                    out.push_str(&std::env::var(&name).unwrap_or_default());
                } else {
                    // No closing brace: emit the text verbatim.
                    out.push_str("${");
                    out.push_str(&name);
                }
            }
            Some(c) if is_var_start(*c) => {
                let mut name = String::new();
                while let Some(&c) = chars.peek() {
                    if is_var_char(c) {
                        name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(&std::env::var(&name).unwrap_or_default());
            }
            _ => out.push('$'),
        }
    }
    out
}

/// Reports whether `c` can begin a shell variable name.
fn is_var_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

/// Reports whether `c` can continue a shell variable name.
fn is_var_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

/// Reports whether `s` contains any pathname-expansion metacharacter.
fn has_glob_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Joins any capture drains, then returns the command result (surfacing a drain
/// error only when the command itself succeeded).
fn finish_with(
    stdout_drain: Option<CaptureDrain>,
    stderr_drain: Option<CaptureDrain>,
    run_result: Result<(), Error>,
) -> Result<(), Error> {
    let mut drain_result = Ok(());
    for drain in [stdout_drain, stderr_drain].into_iter().flatten() {
        if let Err(err) = drain.finish() {
            drain_result = Err(err);
        }
    }
    run_result.and(drain_result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A thread-safe byte sink usable as a capture target and readable after.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Runs `command` capturing stdout, asserting success, returning stdout.
    async fn run_capture(mut opts: RunCommandOptions) -> (Result<(), Error>, String) {
        let buf = SharedBuf::default();
        opts.stdout = Stdio::Capture(Box::new(buf.clone()));
        opts.stderr = Stdio::Null;
        let result = run_command(opts).await;
        (result, buf.contents())
    }

    #[tokio::test]
    async fn echo_writes_stdout() {
        let (result, out) = run_capture(RunCommandOptions::new("echo hello")).await;
        assert!(result.is_ok());
        assert_eq!(out, "hello\n");
    }

    #[tokio::test]
    async fn false_under_errexit_errors() {
        let (result, _) = run_capture(RunCommandOptions::new("false")).await;
        match result {
            Err(Error::NonZeroExit(code)) => assert_ne!(code, 0),
            other => panic!("expected non-zero exit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn errexit_aborts_after_failure() {
        // Without errexit `echo after` would print; with it the string aborts.
        let (result, out) = run_capture(RunCommandOptions::new("false\necho after")).await;
        assert!(result.is_err());
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn multiline_and_and_or() {
        let (result, out) = run_capture(RunCommandOptions::new(
            "echo one\ntrue && echo two\nfalse || echo three",
        ))
        .await;
        assert!(result.is_ok());
        assert_eq!(out, "one\ntwo\nthree\n");
    }

    #[tokio::test]
    async fn env_var_expansion() {
        let mut opts = RunCommandOptions::new("echo $GREETING");
        opts.env = vec![(String::from("GREETING"), String::from("hi there"))];
        let (result, out) = run_capture(opts).await;
        assert!(result.is_ok());
        assert_eq!(out, "hi there\n");
    }

    #[tokio::test]
    async fn provided_env_is_isolated_from_process_env() {
        // A process-inherited variable must not leak when an explicit env is set.
        // SAFETY: single-threaded test setup before the shell runs.
        unsafe { std::env::set_var("EXECEXT_LEAK_CHECK", "leaked") };
        let mut opts = RunCommandOptions::new("echo \"[$EXECEXT_LEAK_CHECK]\"");
        opts.env = vec![(String::from("OTHER"), String::from("x"))];
        let (result, out) = run_capture(opts).await;
        assert!(result.is_ok());
        assert_eq!(out, "[]\n");
    }

    #[tokio::test]
    async fn dir_sets_working_directory() {
        let tmp = std::env::temp_dir();
        let mut opts = RunCommandOptions::new("pwd");
        opts.dir = Some(tmp.clone());
        let (result, out) = run_capture(opts).await;
        assert!(result.is_ok());
        // pwd may resolve symlinks (e.g. /tmp); compare canonical forms.
        let want = std::fs::canonicalize(&tmp).unwrap();
        let got = std::fs::canonicalize(out.trim_end()).unwrap();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn pipeline_transforms_output() {
        let (result, out) = run_capture(RunCommandOptions::new("echo hi | tr a-z A-Z")).await;
        assert!(result.is_ok());
        assert_eq!(out, "HI\n");
    }

    #[test]
    fn expand_literal_resolves_env_and_tilde() {
        // SAFETY: these env names are unique to this test.
        unsafe {
            std::env::set_var("EXECEXT_LIT_A", "alpha");
            std::env::set_var("HOME", "/home/tester");
        }
        assert_eq!(expand_literal(""), "");
        assert_eq!(expand_literal("$EXECEXT_LIT_A/x"), "alpha/x");
        assert_eq!(expand_literal("${EXECEXT_LIT_A}b"), "alphab");
        assert_eq!(expand_literal("~/sub"), "/home/tester/sub");
        assert_eq!(expand_literal("no-vars"), "no-vars");
        assert_eq!(expand_literal("$EXECEXT_LIT_UNSET/x"), "/x");
        assert_eq!(expand_literal("a$$b"), "a$b");
        unsafe {
            std::env::remove_var("EXECEXT_LIT_A");
        }
    }

    #[test]
    fn expand_fields_keeps_spaces_literal() {
        // The Go wrapper escapes spaces before splitting, so a value with a
        // space stays a single field (paths with spaces are not split).
        let fields = expand_fields("foo bar");
        assert_eq!(fields, vec!["foo bar".to_string()]);
        let single = expand_fields("just-one");
        assert_eq!(single, vec!["just-one".to_string()]);
    }

    #[test]
    fn expand_fields_globs_files() {
        let dir = std::env::temp_dir().join(format!("execext-fields-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::fs::write(dir.join("b.txt"), "y").unwrap();
        let pattern = format!("{}/*.txt", dir.to_string_lossy());
        let mut fields = expand_fields(&pattern);
        fields.sort();
        assert_eq!(fields.len(), 2);
        assert!(fields[0].ends_with("a.txt"));
        assert!(fields[1].ends_with("b.txt"));
        // nullglob: a non-matching pattern contributes nothing.
        let none = expand_fields(&format!("{}/nope-*.zzz", dir.to_string_lossy()));
        assert!(none.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stderr_capture_is_separate() {
        let out_buf = SharedBuf::default();
        let err_buf = SharedBuf::default();
        let mut opts = RunCommandOptions::new("echo out\necho err 1>&2");
        opts.stdout = Stdio::Capture(Box::new(out_buf.clone()));
        opts.stderr = Stdio::Capture(Box::new(err_buf.clone()));
        let result = run_command(opts).await;
        assert!(result.is_ok());
        assert_eq!(out_buf.contents(), "out\n");
        assert_eq!(err_buf.contents(), "err\n");
    }
}
