//! Terminal detection for the standard streams.

use std::io::IsTerminal;

/// Reports whether both standard input and standard output are connected to
/// a terminal.
pub fn is_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_panic() {
        // Under the test harness stdin/stdout are usually redirected, so the
        // result is typically false; we only assert the call is well-formed.
        let _ = is_terminal();
    }
}
