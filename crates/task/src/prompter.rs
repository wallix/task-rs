//! A line-based [`Prompter`] reading from stdin and writing to stderr.
//!
//! A richer TUI prompter (matching the Go bubbletea UI) is future work; a
//! simple line reader is sufficient to drive confirmations and variable
//! prompts.

use std::io::{self, BufRead, Write};

use taskcore::executor::{PromptError, Prompter};

/// Prompts on the terminal using plain line input.
pub struct CliPrompter;

impl Prompter for CliPrompter {
    fn confirm(&self, message: &str) -> Result<bool, PromptError> {
        let mut stderr = io::stderr();
        write!(stderr, "{message} [y/N] ")
            .and_then(|()| stderr.flush())
            .map_err(|e| PromptError::Unavailable(e.to_string()))?;
        let answer = read_line()?;
        let answer = answer.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }

    fn prompt(&self, name: &str, enum_values: &[String]) -> Result<String, PromptError> {
        let mut stderr = io::stderr();
        if enum_values.is_empty() {
            write!(stderr, "{name}: ")
        } else {
            writeln!(stderr, "{name}:").and_then(|()| {
                for (i, value) in enum_values.iter().enumerate() {
                    writeln!(stderr, "  {}) {value}", i.saturating_add(1))?;
                }
                write!(stderr, "> ")
            })
        }
        .and_then(|()| stderr.flush())
        .map_err(|e| PromptError::Unavailable(e.to_string()))?;

        let answer = read_line()?;
        let answer = answer.trim().to_string();

        if enum_values.is_empty() {
            return Ok(answer);
        }
        // Accept either the choice text or its 1-based index.
        if let Ok(index) = answer.parse::<usize>()
            && index >= 1
            && let Some(choice) = enum_values.get(index.saturating_sub(1))
        {
            return Ok(choice.clone());
        }
        if enum_values.iter().any(|v| v == &answer) {
            return Ok(answer);
        }
        Err(PromptError::Unavailable(format!(
            "{answer:?} is not one of the allowed values"
        )))
    }
}

/// Reads a single line from stdin, mapping EOF to a cancellation.
fn read_line() -> Result<String, PromptError> {
    let mut line = String::new();
    let read = io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| PromptError::Unavailable(e.to_string()))?;
    if read == 0 {
        return Err(PromptError::Cancelled);
    }
    Ok(line)
}
