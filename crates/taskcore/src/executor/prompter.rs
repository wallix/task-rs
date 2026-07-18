//! Interactive prompting, abstracted so the TUI stays out of the engine.
//!
//! The Go implementation prompts for missing required variables and yes/no
//! confirmations using a bubbletea-based `input` package. Porting a terminal UI
//! into the engine would drag in unrelated dependencies, so the engine instead
//! holds an optional [`Prompter`]. The CLI supplies a concrete implementation;
//! when none is set the engine behaves as a non-interactive `--yes=false`
//! session: confirmations are declined and variable prompts are unavailable.

/// An error raised by a prompt.
#[derive(Debug)]
pub enum PromptError {
    /// The user cancelled the prompt (e.g. Ctrl-C).
    Cancelled,
    /// The prompt could not run (no terminal, I/O failure, …).
    Unavailable(String),
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "prompt cancelled"),
            Self::Unavailable(msg) => write!(f, "prompt unavailable: {msg}"),
        }
    }
}

impl std::error::Error for PromptError {}

/// A source of interactive answers. Implementations drive whatever UI is
/// appropriate (a real TUI in the CLI, a scripted stub in tests).
pub trait Prompter {
    /// Asks the user to confirm `message`, returning whether they accepted.
    fn confirm(&self, message: &str) -> Result<bool, PromptError>;

    /// Asks the user for the value of variable `name`. When `enum_values` is
    /// non-empty the answer is constrained to one of those choices.
    fn prompt(&self, name: &str, enum_values: &[String]) -> Result<String, PromptError>;
}
