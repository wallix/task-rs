//! A thin wrapper around the configured output streams that writes optionally
//! colored text to stdout and stderr.

use std::io::{BufRead, Write};

use crate::env;
use crate::term;

/// A logging error. Prompt handling distinguishes cancellation and a missing
/// terminal from ordinary I/O failures.
#[derive(Debug)]
pub enum LoggerError {
    /// The user declined the prompt.
    PromptCancelled,
    /// Standard input is not connected to a terminal.
    NoTerminal,
    /// No continue values were provided to `prompt`.
    NoContinueValues,
    /// An underlying I/O operation failed.
    Io(std::io::Error),
}

impl std::fmt::Display for LoggerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoggerError::PromptCancelled => write!(f, "prompt cancelled"),
            LoggerError::NoTerminal => write!(f, "no terminal"),
            LoggerError::NoContinueValues => write!(f, "no continue values provided"),
            LoggerError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LoggerError {}

impl From<std::io::Error> for LoggerError {
    fn from(e: std::io::Error) -> Self {
        LoggerError::Io(e)
    }
}

/// A named color for log output. Each variant maps to a set of ANSI SGR codes;
/// the exact codes are configurable through `TASK_COLOR_*` environment
/// variables (see [`Color::codes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// No color: text is written verbatim.
    None,
    /// The reset sequence, restoring the terminal's default attributes.
    Default,
    Blue,
    Green,
    Cyan,
    Yellow,
    Magenta,
    Red,
    BrightBlue,
    BrightGreen,
    BrightCyan,
    BrightYellow,
    BrightMagenta,
    BrightRed,
}

impl Color {
    /// Returns the SGR parameter codes for this color, honoring the matching
    /// `TASK_COLOR_*` environment override when present.
    fn codes(self) -> Vec<u32> {
        match self {
            Color::None => Vec::new(),
            Color::Default => env_color("COLOR_RESET", &[0]),
            Color::Blue => env_color("COLOR_BLUE", &[34]),
            Color::Green => env_color("COLOR_GREEN", &[32]),
            Color::Cyan => env_color("COLOR_CYAN", &[36]),
            Color::Yellow => env_color("COLOR_YELLOW", &[33]),
            Color::Magenta => env_color("COLOR_MAGENTA", &[35]),
            Color::Red => env_color("COLOR_RED", &[31]),
            Color::BrightBlue => env_color("COLOR_BRIGHT_BLUE", &[94]),
            Color::BrightGreen => env_color("COLOR_BRIGHT_GREEN", &[92]),
            Color::BrightCyan => env_color("COLOR_BRIGHT_CYAN", &[96]),
            Color::BrightYellow => env_color("COLOR_BRIGHT_YELLOW", &[93]),
            Color::BrightMagenta => env_color("COLOR_BRIGHT_MAGENTA", &[95]),
            Color::BrightRed => env_color("COLOR_BRIGHT_RED", &[91]),
        }
    }

    /// Writes `s` to `w`, wrapping it in the SGR escape sequences for this
    /// color. `Color::None` writes the text unchanged.
    fn write(self, w: &mut dyn Write, s: &str) -> std::io::Result<()> {
        let codes = self.codes();
        if codes.is_empty() {
            return w.write_all(s.as_bytes());
        }
        let params = codes
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(";");
        write!(w, "\x1b[{params}m{s}\x1b[0m")
    }
}

/// Resolves the SGR codes for a color, applying the `TASK_<name>` override when
/// it parses. A three-value comma list is treated as a 256-color RGB shortcut
/// (prefixed with `38;2`); otherwise the value is split on semicolons. If any
/// component fails to parse the default is used unchanged.
fn env_color(name: &str, default: &[u32]) -> Vec<u32> {
    let override_val = env::get_task_env(name);
    if override_val.is_empty() {
        return default.to_vec();
    }

    let parts: Vec<&str> = override_val.split(',').collect();
    let tokens: Vec<String> = if parts.len() == 3 {
        let mut v = vec!["38".to_string(), "2".to_string()];
        v.extend(parts.into_iter().map(str::to_string));
        v
    } else {
        override_val.split(';').map(str::to_string).collect()
    };

    let mut codes = Vec::with_capacity(tokens.len());
    for token in tokens {
        match token.parse::<u32>() {
            Ok(n) => codes.push(n),
            Err(_) => return default.to_vec(),
        }
    }
    codes
}

/// The default sequence of colors used to distinguish prefixed task output.
pub const PREFIX_COLOR_SEQUENCE: [Color; 12] = [
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Green,
    Color::Red,
    Color::BrightYellow,
    Color::BrightBlue,
    Color::BrightMagenta,
    Color::BrightCyan,
    Color::BrightGreen,
    Color::BrightRed,
];

/// Prints text to the configured stdout or stderr streams, optionally colored.
///
/// The streams are boxed trait objects so tests can capture output into an
/// in-memory buffer. `stdin` is optional and only needed for [`Logger::prompt`].
pub struct Logger {
    pub stdin: Option<Box<dyn BufRead + Send>>,
    pub stdout: Box<dyn Write + Send>,
    pub stderr: Box<dyn Write + Send>,
    pub verbose: bool,
    pub color: bool,
    pub assume_yes: bool,
    /// Forces terminal detection to succeed; used only in tests.
    pub assume_term: bool,
}

impl Default for Logger {
    fn default() -> Self {
        Logger {
            stdin: None,
            stdout: Box::new(std::io::stdout()),
            stderr: Box::new(std::io::stderr()),
            verbose: false,
            color: false,
            assume_yes: false,
            assume_term: false,
        }
    }
}

impl Logger {
    /// Prints `s` to stdout in the given color.
    pub fn outf(&mut self, color: Color, s: &str) {
        let color = if self.color { color } else { Color::None };
        let _ = color.write(self.stdout.as_mut(), s);
    }

    /// Prints `s` to `w` in the given color, honoring the logger's color flag.
    pub fn f_outf(&self, w: &mut dyn Write, color: Color, s: &str) {
        let color = if self.color { color } else { Color::None };
        let _ = color.write(w, s);
    }

    /// Prints `s` to stdout only when verbose mode is enabled.
    pub fn verbose_outf(&mut self, color: Color, s: &str) {
        if self.verbose {
            self.outf(color, s);
        }
    }

    /// Prints `s` to stderr in the given color.
    pub fn errf(&mut self, color: Color, s: &str) {
        let color = if self.color { color } else { Color::None };
        let _ = color.write(self.stderr.as_mut(), s);
    }

    /// Prints `s` to stderr only when verbose mode is enabled.
    pub fn verbose_errf(&mut self, color: Color, s: &str) {
        if self.verbose {
            self.errf(color, s);
        }
    }

    /// Prints a warning to stderr in yellow.
    pub fn warnf(&mut self, message: &str) {
        self.errf(Color::Yellow, message);
    }

    /// Prompts the user for confirmation, reading a line from stdin. Returns
    /// `Ok(())` when the input matches one of `continue_values`. When
    /// `assume_yes` is set the prompt is auto-confirmed.
    pub fn prompt(
        &mut self,
        color: Color,
        prompt: &str,
        default_value: &str,
        continue_values: &[&str],
    ) -> Result<(), LoggerError> {
        if self.assume_yes {
            self.outf(color, &format!("{prompt} [assuming yes]\n"));
            return Ok(());
        }

        if !self.assume_term && !term::is_terminal() {
            return Err(LoggerError::NoTerminal);
        }

        let Some(first) = continue_values.first() else {
            return Err(LoggerError::NoContinueValues);
        };

        self.outf(
            color,
            &format!(
                "{prompt} [{}/{}]: ",
                first.to_lowercase(),
                default_value.to_uppercase()
            ),
        );

        let mut input = String::new();
        match self.stdin.as_mut() {
            Some(stdin) => {
                stdin.read_line(&mut input)?;
            }
            None => return Err(LoggerError::NoTerminal),
        }

        let input = input.trim().to_lowercase();
        if !continue_values.contains(&input.as_str()) {
            return Err(LoggerError::PromptCancelled);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
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

    fn logger_with(color: bool) -> (SharedBuf, Logger) {
        let buf = SharedBuf::default();
        let l = Logger {
            stdout: Box::new(buf.clone()),
            stderr: Box::new(buf.clone()),
            color,
            ..Default::default()
        };
        (buf, l)
    }

    #[test]
    fn outf_without_color_is_verbatim() {
        let (buf, mut l) = logger_with(false);
        l.outf(Color::Green, "hello\n");
        assert_eq!(buf.contents(), "hello\n");
    }

    #[test]
    fn outf_with_color_wraps_in_sgr() {
        let (buf, mut l) = logger_with(true);
        l.outf(Color::Green, "hello");
        assert_eq!(buf.contents(), "\x1b[32mhello\x1b[0m");
    }

    #[test]
    fn verbose_outf_respects_flag() {
        let (buf, mut l) = logger_with(false);
        l.verbose_outf(Color::Default, "nope");
        assert_eq!(buf.contents(), "");
        l.verbose = true;
        l.verbose_outf(Color::Default, "yes");
        assert_eq!(buf.contents(), "yes");
    }

    #[test]
    fn warnf_writes_to_stderr() {
        let (buf, mut l) = logger_with(false);
        l.warnf("careful\n");
        assert_eq!(buf.contents(), "careful\n");
    }

    #[test]
    fn prompt_assume_yes() {
        let (buf, mut l) = logger_with(false);
        l.assume_yes = true;
        l.prompt(Color::Default, "continue?", "n", &["y"]).unwrap();
        assert_eq!(buf.contents(), "continue? [assuming yes]\n");
    }

    #[test]
    fn prompt_accepts_continue_value() {
        let (_buf, mut l) = logger_with(false);
        l.assume_term = true;
        l.stdin = Some(Box::new(std::io::Cursor::new(b"y\n".to_vec())));
        l.prompt(Color::Default, "continue?", "n", &["y"]).unwrap();
    }

    #[test]
    fn prompt_rejects_other_value() {
        let (_buf, mut l) = logger_with(false);
        l.assume_term = true;
        l.stdin = Some(Box::new(std::io::Cursor::new(b"n\n".to_vec())));
        let err = l
            .prompt(Color::Default, "continue?", "n", &["y"])
            .unwrap_err();
        assert!(matches!(err, LoggerError::PromptCancelled));
    }

    #[test]
    fn prompt_no_continue_values() {
        let (_buf, mut l) = logger_with(false);
        l.assume_term = true;
        let err = l.prompt(Color::Default, "continue?", "n", &[]).unwrap_err();
        assert!(matches!(err, LoggerError::NoContinueValues));
    }
}
