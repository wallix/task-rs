//! Re-indents lines whose indentation the parser refuses and libyaml did not.
//!
//! libyaml and go-yaml accept a quoted scalar whose continuation lines sit at
//! or left of its block's indentation, or start with a tab, and a line inside
//! a flow collection that starts with a tab; YAML 1.2 and the parser do not.
//! Those leading blanks are not part of any value (a continuation line's are
//! folded away, a flow line's are separation), so re-indenting them changes
//! nothing else. Line numbers are kept, and positions are mapped back to the
//! original text.
//!
//! Once the parser trips on one quoted scalar, every later quoted scalar is
//! fixed in the same pass, so a file costs a few parses rather than one per
//! scalar. After each parse, a quote that turned out not to start a quoted
//! scalar (inside a block scalar, say) has its edits undone, whether or not
//! the parse then failed.

use std::collections::{BTreeMap, BTreeSet};

use super::error::Mark;

/// Tracks the quoted scalars and lines re-indented in one input.
pub(crate) struct Reindent<'a> {
    original: &'a str,
    /// Re-indented lines (0-based): their original and new indentation, in
    /// bytes. A line belongs to one scalar or flow line at most.
    lines: BTreeMap<usize, (usize, usize)>,
    /// Opening quote offset of each re-indented scalar, with its lines.
    scalars: BTreeMap<usize, Vec<usize>>,
    /// Opening quotes found not to start a quoted scalar.
    rejected: BTreeSet<usize>,
    /// For each re-indented line of the last rendering, in order: where it
    /// starts there, where it starts in the original, and the indentation
    /// removed and added up to and including it.
    rendered: Vec<RenderedLine>,
}

struct RenderedLine {
    start: usize,
    original_start: usize,
    old: usize,
    new: usize,
    removed: usize,
    added: usize,
}

impl<'a> Reindent<'a> {
    pub(crate) fn new(original: &'a str) -> Self {
        Reindent {
            original,
            lines: BTreeMap::new(),
            scalars: BTreeMap::new(),
            rejected: BTreeSet::new(),
            rendered: Vec::new(),
        }
    }

    /// The original text with the re-indented lines.
    pub(crate) fn render(&mut self) -> String {
        let mut out = String::with_capacity(self.original.len());
        self.rendered.clear();
        let (mut original_start, mut removed, mut added) = (0usize, 0usize, 0usize);
        for (number, line) in lines(self.original).enumerate() {
            match self.lines.get(&number) {
                Some(&(old, new)) => {
                    removed = removed.saturating_add(old);
                    added = added.saturating_add(new);
                    self.rendered.push(RenderedLine {
                        start: out.len(),
                        original_start,
                        old,
                        new,
                        removed,
                        added,
                    });
                    out.extend(std::iter::repeat_n(' ', new));
                    out.push_str(line.get(old..).unwrap_or_default());
                }
                None => out.push_str(line),
            }
            original_start = original_start.saturating_add(line.len());
        }
        out
    }

    /// The original offset of byte `index` of the last rendering. An offset
    /// in added indentation maps to the end of the original one.
    pub(crate) fn map_offset(&self, index: usize) -> usize {
        let at = self.rendered.partition_point(|line| line.start <= index);
        let Some(line) = at.checked_sub(1).and_then(|at| self.rendered.get(at)) else {
            return index;
        };
        if index < line.start.saturating_add(line.new) {
            return line.original_start.saturating_add(line.old);
        }
        index
            .saturating_add(line.removed)
            .saturating_sub(line.added)
    }

    /// Maps `mark` from the last rendering to its position in the original text.
    pub(crate) fn map(&self, mark: Mark) -> Mark {
        let column = match self.lines.get(&mark.line) {
            Some(&(old, new)) => mark.column.saturating_sub(new).saturating_add(old),
            None => mark.column,
        };
        Mark {
            index: self.map_offset(mark.index),
            line: mark.line,
            column,
        }
    }

    /// Re-indents the quoted scalar rejected at `error` and all later quoted
    /// scalars. Searches for its opening quote from `after`, the previous
    /// node's end. Returns false if the scalar is unclosed, a continuation
    /// line is a document marker (as in libyaml), its lines overlap prior
    /// edits, or it still fails after re-indenting.
    pub(crate) fn fix_from(&mut self, error: Mark, after: usize) -> bool {
        let text = self.original;
        let error_line = line_start(text, error.index);
        let Some((open, _)) = enclosing_quote(text, after, error_line, &self.rejected) else {
            return false;
        };
        if self.scalars.contains_key(&open) || line_start(text, open) >= error_line {
            return false;
        }
        let mut pos = open;
        let mut first = true;
        // Line numbers, counted as the scan moves forward.
        let (mut counted, mut line) = (0usize, 0usize);
        while let Some((open, close)) = next_quote(text, pos, &self.rejected) {
            pos = close.saturating_add(1);
            if self.scalars.contains_key(&open) {
                continue;
            }
            line = text
                .get(counted..open)
                .map_or(line, |gap| line.saturating_add(line_breaks(gap)));
            counted = open;
            let edits = continuation_edits(text, open, close, line)
                .filter(|edits| edits.iter().all(|(line, _)| !self.lines.contains_key(line)));
            match edits {
                Some(edits) => {
                    let mut lines = Vec::with_capacity(edits.len());
                    for (line, edit) in edits {
                        self.lines.insert(line, edit);
                        lines.push(line);
                    }
                    self.scalars.insert(open, lines);
                }
                None if first => return false,
                None => {
                    self.rejected.insert(open);
                }
            }
            first = false;
        }
        // The refused scalar must have been found by the scan.
        !first
    }

    /// Turns the tabs leading line `at.line`, inside a flow collection, into
    /// spaces. False when that line was re-indented already or has none.
    pub(crate) fn fix_flow_tabs(&mut self, at: Mark) -> bool {
        if self.lines.contains_key(&at.line) {
            return false;
        }
        let start = line_start(self.original, at.index);
        let Some(line) = self.original.get(start..) else {
            return false;
        };
        let blanks = line
            .len()
            .saturating_sub(line.trim_start_matches([' ', '\t']).len());
        if !line
            .get(..blanks)
            .is_some_and(|indent| indent.contains('\t'))
        {
            return false;
        }
        self.lines.insert(at.line, (blanks, blanks));
        true
    }

    /// Undoes edits to scalars whose opening quotes precede `stop` but were
    /// not confirmed by the parser. `stop` is an original offset the parser
    /// passed. Returns true if any edits were undone.
    pub(crate) fn reject_unconfirmed(&mut self, quoted: &BTreeSet<usize>, stop: usize) -> bool {
        let wrong: Vec<usize> = self
            .scalars
            .range(..stop)
            .map(|(&open, _)| open)
            .filter(|open| !quoted.contains(open))
            .collect();
        for open in &wrong {
            if let Some(lines) = self.scalars.remove(open) {
                for line in lines {
                    self.lines.remove(&line);
                }
            }
            self.rejected.insert(*open);
        }
        !wrong.is_empty()
    }

    /// The opening quote of a scalar left unclosed, which the parser read on
    /// past the end of its value until it refused the indentation at `error`.
    /// Searches from `after`, the previous node's end, like `fix_from`.
    pub(crate) fn unclosed_quote(&self, error: Mark, after: usize) -> Option<Mark> {
        let text = self.original;
        let open = quotes(text, after, &self.rejected)
            .take_while(|&(open, _)| open < error.index)
            .find_map(|(open, close)| close.is_none().then_some(open))?;
        mark_of(text, open)
    }

    /// The document marker (`---` or `...` at column 0) starting the line of
    /// `error`, inside the quoted scalar opened after `after`. libyaml ended
    /// the scalar there with an error, so re-indenting must not fix it.
    pub(crate) fn document_marker(&self, error: Mark, after: usize) -> Option<Mark> {
        let text = self.original;
        let error_line = line_start(text, error.index);
        // Quotes rejected for holding a marker are what is looked for here.
        enclosing_quote(text, after, error_line, &BTreeSet::new())?;
        is_document_marker(text.get(error_line..)?)
            .then(|| mark_of(text, error_line))
            .flatten()
    }
}

/// The mark of byte `index` of `text`.
fn mark_of(text: &str, index: usize) -> Option<Mark> {
    let start = line_start(text, index);
    Some(Mark {
        index,
        line: line_breaks(text.get(..index)?),
        column: text.get(start..index)?.chars().count(),
    })
}

/// The indentation changes, by 0-based line, that let the parser read the
/// scalar quoted from `open` to `close`; `None` when a continuation line is a
/// document marker. `first_number` is the line of `open`.
fn continuation_edits(
    text: &str,
    open: usize,
    close: usize,
    first_number: usize,
) -> Option<Vec<(usize, (usize, usize))>> {
    let open_line = line_start(text, open);
    let target = text.get(open_line..open)?.chars().count();
    let mut edits = Vec::new();
    let mut offset = open_line;
    for (i, line) in lines(text.get(open_line..)?).enumerate() {
        let start = offset;
        offset = offset.checked_add(line.len())?;
        if i == 0 {
            continue;
        }
        if start > close {
            break;
        }
        let body = line.trim_start_matches([' ', '\t']);
        let blanks = line.len().saturating_sub(body.len());
        let tabbed = line.get(..blanks)?.contains('\t');
        if blanks == 0 && is_document_marker(body) {
            return None;
        }
        let blank_line = body.is_empty() || body.starts_with(['\n', '\r']);
        let new = if blank_line {
            if tabbed { 0 } else { blanks }
        } else if tabbed || blanks < target {
            target.max(blanks)
        } else {
            blanks
        };
        if new != blanks || tabbed {
            edits.push((first_number.checked_add(i)?, (blanks, new)));
        }
    }
    Some(edits)
}

fn line_start(text: &str, index: usize) -> usize {
    text.get(..index)
        .and_then(|before| before.rfind(['\n', '\r']))
        .map_or(0, |i| i.saturating_add(1))
}

/// `---` or `...` followed by a blank, a line break or the end of input.
fn is_document_marker(line: &str) -> bool {
    ["---", "..."].iter().any(|marker| {
        line.strip_prefix(marker)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t', '\r', '\n']))
    })
}

/// The quoted scalar starting at or after `from` that is still open at byte
/// `at`, as offsets of its opening and closing quotes.
fn enclosing_quote(
    text: &str,
    from: usize,
    at: usize,
    rejected: &BTreeSet<usize>,
) -> Option<(usize, usize)> {
    let mut pos = from;
    while let Some((open, close)) = next_quote(text, pos, rejected) {
        if open >= at {
            return None;
        }
        if close >= at {
            return Some((open, close));
        }
        pos = close.saturating_add(1);
    }
    None
}

/// The next quoted scalar opening at or after `from` that spans lines, as
/// offsets of its opening and closing quotes; `None` past an unclosed one.
fn next_quote(text: &str, from: usize, rejected: &BTreeSet<usize>) -> Option<(usize, usize)> {
    quotes(text, from, rejected)
        .map_while(|(open, close)| close.map(|close| (open, close)))
        .find(|&(open, close)| {
            text.get(open..close)
                .is_some_and(|scalar| scalar.contains(['\n', '\r']))
        })
}

/// The quoted scalars opening at or after `from` (outside comments, at the
/// start of a node, not rejected), as offsets of their opening and closing
/// quotes, up to and including the first one left unclosed.
fn quotes<'t>(
    text: &'t str,
    from: usize,
    rejected: &'t BTreeSet<usize>,
) -> impl Iterator<Item = (usize, Option<usize>)> + 't {
    let mut prev = text.get(..from).and_then(|t| t.chars().next_back());
    let mut in_comment = false;
    let mut next_pos = Some(from);
    std::iter::from_fn(move || {
        let mut pos = next_pos.take()?;
        while let Some(c) = text.get(pos..)?.chars().next() {
            if in_comment {
                in_comment = !matches!(c, '\n' | '\r');
            } else if c == '#' && prev.is_none_or(char::is_whitespace) {
                in_comment = true;
            } else if matches!(c, '\'' | '"') && starts_node(text, pos) && !rejected.contains(&pos)
            {
                let close = closing_quote(text, pos, c);
                prev = Some(c);
                next_pos = close.and_then(|close| close.checked_add(1));
                return Some((pos, close));
            }
            prev = Some(c);
            pos = pos.checked_add(c.len_utf8())?;
        }
        None
    })
}

/// Whether the quote at `pos` starts a node: first on its line, after a
/// flow indicator (`[`, `{`, `,`), after an indicator (`:`, `-`, `?`) and a
/// blank, or after a node property (`!tag`, `&anchor`). Looks back over the
/// blanks and one word only, so a scan stays linear.
fn starts_node(text: &str, pos: usize) -> bool {
    let mut before = text.get(..pos).unwrap_or_default().chars().rev().peekable();
    let mut blanks = false;
    while before.next_if(|c| matches!(c, ' ' | '\t')).is_some() {
        blanks = true;
    }
    match before.next() {
        None | Some('\n' | '\r' | '[' | '{' | ',') => true,
        Some(':' | '-' | '?') => blanks,
        Some(_) if !blanks => false,
        Some(last) => {
            let first = before
                .take_while(|c| !c.is_whitespace())
                .last()
                .unwrap_or(last);
            matches!(first, '!' | '&')
        }
    }
}

/// The offset of the quote closing the scalar opened at `open`.
fn closing_quote(text: &str, open: usize, quote: char) -> Option<usize> {
    let start = open.checked_add(1)?;
    let mut chars = text.get(start..)?.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if quote == '"' && c == '\\' {
            chars.next();
        } else if c == quote {
            if quote == '\'' && chars.peek().is_some_and(|&(_, next)| next == '\'') {
                chars.next();
            } else {
                return start.checked_add(i);
            }
        }
    }
    None
}

/// The lines of `text` with their line breaks (`\n`, `\r\n` or `\r`).
fn lines(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = match rest.find(['\n', '\r']) {
            Some(i) if rest.get(i..)?.starts_with("\r\n") => i.checked_add(2)?,
            Some(i) => i.checked_add(1)?,
            None => rest.len(),
        };
        let (line, tail) = rest.split_at_checked(end)?;
        rest = tail;
        Some(line)
    })
}

/// The number of line breaks in `text`.
fn line_breaks(text: &str) -> usize {
    let crlf = text.matches("\r\n").count();
    text.matches(['\n', '\r']).count().saturating_sub(crlf)
}
