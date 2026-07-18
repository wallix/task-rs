//! Extraction of a source snippet around an error location for readable
//! diagnostics.
//!
//! A [`Snippet`] captures a window of lines centered on a 1-indexed line/column
//! and renders it with line numbers, a `>` line indicator, and a `^` column
//! caret. Line and column numbers are 1-indexed: the first character in a file
//! is `1:1`.
//!
// The Go implementation syntax-highlights the YAML with `chroma` before
// rendering. No pure-Rust highlighter is available here without pulling in a
// new dependency, so the snippet renders the raw source text. The line/column
// windowing and indicator layout are preserved exactly.

use std::fmt::Write as _;

const LINE_INDICATOR: &str = ">";
const COLUMN_INDICATOR: &str = "^";

/// A window of Taskfile source lines with optional line/column indicators.
#[derive(Clone, Debug, PartialEq)]
pub struct Snippet {
    lines: Vec<String>,
    start: usize,
    end: usize,
    line: usize,
    column: usize,
    padding: usize,
    no_indicators: bool,
}

/// Builder options for a [`Snippet`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SnippetOptions {
    /// The 1-indexed line the snippet centers on. `0` means no line is chosen.
    pub line: usize,
    /// The 1-indexed column the caret points at. `0` means no column.
    pub column: usize,
    /// Number of lines to include before and after the chosen line.
    pub padding: usize,
    /// Suppress the line and column indicators.
    pub no_indicators: bool,
}

impl Snippet {
    /// Builds a snippet from raw bytes, centered per `opts`.
    ///
    /// The window spans `[line - padding, line + padding]`, clamped to the
    /// file. With `line == 0` and `padding > 0` the window still starts at the
    /// first line, matching the Go behavior.
    pub fn new(b: &[u8], opts: SnippetOptions) -> Self {
        let text = String::from_utf8_lossy(b);
        let lines_raw: Vec<String> = text.split('\n').map(str::to_string).collect();

        let last_index = lines_raw.len().saturating_sub(1);
        let start = opts.line.saturating_sub(opts.padding).max(1);
        let end = opts.line.saturating_add(opts.padding).min(last_index);

        // `start` is 1-indexed; `end` is an exclusive count in the Go slice
        // `linesRaw[start-1:end]`, i.e. lines `start..=end`.
        let lines = if start > end {
            Vec::new()
        } else {
            lines_raw
                .get(start.saturating_sub(1)..end)
                .map(<[String]>::to_vec)
                .unwrap_or_default()
        };

        Snippet {
            lines,
            start,
            end,
            line: opts.line,
            column: opts.column,
            padding: opts.padding,
            no_indicators: opts.no_indicators,
        }
    }

    /// Renders the snippet to a string.
    pub fn render(&self) -> String {
        let mut buf = String::new();

        let max_line_number_digits = digits(self.end);
        let line_number_spacer = " ".repeat(max_line_number_digits);
        let line_indicator_spacer = " ".repeat(LINE_INDICATOR.len());
        let column_spacer = " ".repeat(self.column.saturating_sub(1));

        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                buf.push('\n');
            }

            let current_line = self.start.saturating_add(i);
            let line_number = format!("{current_line:>width$}", width = max_line_number_digits);

            if current_line != self.line || self.no_indicators {
                let _ = write!(buf, "{line_indicator_spacer} {line_number} | {line}");
                continue;
            }

            let _ = write!(buf, "{LINE_INDICATOR} {line_number} | {line}");

            // Only render the caret if the column is within the raw line.
            let raw_len = self.lines.get(i).map(String::len).unwrap_or(0);
            if self.column > 0 && self.column <= raw_len {
                let _ = write!(
                    buf,
                    "\n{line_indicator_spacer} {line_number_spacer} | {column_spacer}{COLUMN_INDICATOR}"
                );
            }
        }

        // With lines present but no selected line, render the caret beneath all
        // of them.
        if !self.lines.is_empty() && self.line == 0 && self.column > 0 {
            let _ = write!(
                buf,
                "\n{line_indicator_spacer} {line_number_spacer} | {column_spacer}{COLUMN_INDICATOR}"
            );
        }

        buf
    }
}

fn digits(mut number: usize) -> usize {
    let mut count = 0usize;
    while number != 0 {
        number /= 10;
        count = count.saturating_add(1);
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "version: 3\n\ntasks:\n  default:\n    vars:\n      FOO: foo\n      BAR: bar\n    cmds:\n      - echo \"{{.FOO}}\"\n      - echo \"{{.BAR}}\"\n";

    fn opts(line: usize, column: usize, padding: usize) -> SnippetOptions {
        SnippetOptions {
            line,
            column,
            padding,
            no_indicators: false,
        }
    }

    #[test]
    fn first_line_first_column_window() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(1, 1, 0));
        assert_eq!(s.lines, vec!["version: 3".to_string()]);
        assert_eq!(s.start, 1);
        assert_eq!(s.end, 1);
    }

    #[test]
    fn first_line_first_column_padding2_window() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(1, 1, 2));
        assert_eq!(
            s.lines,
            vec![
                "version: 3".to_string(),
                String::new(),
                "tasks:".to_string()
            ]
        );
        assert_eq!(s.start, 1);
        assert_eq!(s.end, 3);
    }

    #[test]
    fn empty_input_renders_empty() {
        let s = Snippet::new(&[], opts(1, 1, 0));
        assert_eq!(s.render(), "");
    }

    #[test]
    fn zeroth_line_zeroth_column_renders_empty() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(0, 0, 0));
        assert_eq!(s.render(), "");
    }

    #[test]
    fn line_indicator_only() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(1, 0, 0));
        assert_eq!(s.render(), "> 1 | version: 3");
    }

    #[test]
    fn zeroth_line_first_column_no_window_renders_empty() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(0, 1, 0));
        assert_eq!(s.render(), "");
    }

    #[test]
    fn zeroth_line_first_column_padding2_caret_under_all() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(0, 1, 2));
        assert_eq!(s.render(), "  1 | version: 3\n  2 | \n    | ^");
    }

    #[test]
    fn first_line_first_column() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(1, 1, 0));
        assert_eq!(s.render(), "> 1 | version: 3\n    | ^");
    }

    #[test]
    fn first_line_tenth_column() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(1, 10, 0));
        assert_eq!(s.render(), "> 1 | version: 3\n    |          ^");
    }

    #[test]
    fn first_line_first_column_padding2() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(1, 1, 2));
        assert_eq!(
            s.render(),
            "> 1 | version: 3\n    | ^\n  2 | \n  3 | tasks:"
        );
    }

    #[test]
    fn fifth_line_fifth_column() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(5, 5, 0));
        assert_eq!(s.render(), "> 5 |     vars:\n    |     ^");
    }

    #[test]
    fn fifth_line_fifth_column_padding2_no_indicators() {
        let mut o = opts(5, 5, 2);
        o.no_indicators = true;
        let s = Snippet::new(SAMPLE.as_bytes(), o);
        assert_eq!(
            s.render(),
            "  3 | tasks:\n  4 |   default:\n  5 |     vars:\n  6 |       FOO: foo\n  7 |       BAR: bar"
        );
    }

    #[test]
    fn tenth_line_column_out_of_bounds_has_no_caret() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(10, 24, 0));
        assert_eq!(s.render(), "> 10 |       - echo \"{{.BAR}}\"");
    }

    #[test]
    fn tenth_line_last_in_bounds_column_has_caret() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(10, 23, 0));
        assert_eq!(
            s.render(),
            "> 10 |       - echo \"{{.BAR}}\"\n     |                       ^"
        );
    }

    #[test]
    fn line_out_of_bounds_renders_empty() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(11, 1, 0));
        assert_eq!(s.render(), "");
    }

    #[test]
    fn line_out_of_bounds_padding2_shows_tail() {
        let s = Snippet::new(SAMPLE.as_bytes(), opts(11, 1, 2));
        assert_eq!(
            s.render(),
            "   9 |       - echo \"{{.FOO}}\"\n  10 |       - echo \"{{.BAR}}\""
        );
    }
}
