//! Turns the parser's event stream into one document's events, each marked
//! where libyaml marked it, so error positions stay as they were.

use std::collections::BTreeMap;
use std::sync::Arc;

use granit_parser::{
    ErrorKind, Event as ParserEvent, Marker, Parser, ScalarStyle as ParserStyle, Span, StrInput,
    StructureStyle,
};

use super::error::{self, ErrorImpl, Mark};

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum ScalarStyle {
    Plain,
    SingleQuoted,
    DoubleQuoted,
    Literal,
    Folded,
}

#[derive(Debug)]
pub(crate) struct Scalar {
    /// The resolved tag (`tag:yaml.org,2002:str` for `!!str`, `!foo` for `!foo`).
    pub(crate) tag: Option<String>,
    pub(crate) value: String,
    pub(crate) style: ScalarStyle,
}

#[derive(Debug)]
pub(crate) enum Event {
    /// An alias, by the anchor id it refers to.
    Alias(usize),
    Scalar(Scalar),
    SequenceStart(Option<String>),
    SequenceEnd,
    MappingStart(Option<String>),
    MappingEnd,
    /// The stream holds no document at all.
    Void,
}

/// An indentation error libyaml did not raise.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Fixable {
    /// Possibly in the continuation lines of a quoted scalar; `after` is the
    /// end of the last event before it.
    Quote { at: Mark, after: usize },
    /// A tab leading a line inside a flow collection.
    FlowTab { at: Mark },
}

pub(crate) struct Document {
    pub(crate) events: Vec<(Event, Mark)>,
    pub(crate) error: Option<Arc<ErrorImpl>>,
    /// Anchor id to the index of the anchored node's first event.
    pub(crate) aliases: BTreeMap<usize, usize>,
    /// A candidate for re-indenting: an indentation error the parser raised
    /// that libyaml would not have, if reindent.rs confirms it lies where the
    /// indentation cannot change the value.
    pub(crate) fixable: Option<Fixable>,
    /// Where the parser's error is, if it raised one.
    pub(crate) error_mark: Option<Mark>,
    /// Where each quoted scalar starts, as a byte offset.
    pub(crate) quoted: Vec<usize>,
}

/// The collection a node sits in, which decides where libyaml marks an
/// empty node.
#[derive(Copy, Clone)]
enum Frame {
    Sequence { flow: bool },
    Mapping { flow: bool, expect_key: bool },
}

pub(crate) struct Loader<'input> {
    input: &'input str,
    parser: Option<Parser<'input, StrInput<'input>>>,
    document_count: usize,
}

impl<'input> Loader<'input> {
    pub(crate) fn new(input: &'input str) -> Self {
        Loader {
            input,
            parser: Some(Parser::new_from_str(input)),
            document_count: 0,
        }
    }

    pub(crate) fn next_document(&mut self) -> Option<Document> {
        // Put back once the document ends; an error or the end of the stream
        // leaves the loader exhausted.
        let mut parser = self.parser.take()?;

        let first = self.document_count == 0;
        self.document_count = self.document_count.saturating_add(1);

        let mut document = Document {
            events: Vec::new(),
            error: None,
            aliases: BTreeMap::new(),
            fixable: None,
            error_mark: None,
            quoted: Vec::new(),
        };
        let mut frames: Vec<Frame> = Vec::new();
        let mut last_end = 0;

        loop {
            let (event, span) = match parser.next_event() {
                Some(Ok(next)) => next,
                Some(Err(err)) => {
                    let info = match err.kind() {
                        // libyaml's wording.
                        ErrorKind::UnknownAnchor => "unknown anchor".to_string(),
                        _ => err.info(),
                    };
                    let mark = self.mark_at(err.marker());
                    let in_flow = matches!(
                        frames.last(),
                        Some(Frame::Mapping { flow: true, .. } | Frame::Sequence { flow: true })
                    );
                    document.fixable = match err.kind() {
                        ErrorKind::InvalidQuotedScalarIndent | ErrorKind::TabInIndentation => {
                            Some(Fixable::Quote {
                                at: mark,
                                after: last_end,
                            })
                        }
                        ErrorKind::TabInBlockIndentation if in_flow => {
                            Some(Fixable::FlowTab { at: mark })
                        }
                        _ => None,
                    };
                    document.error_mark = Some(mark);
                    document.error = Some(error::new(ErrorImpl::Scan(info, mark)).shared());
                    return Some(document);
                }
                None => return self.end_of_stream(first, document, Mark::default()),
            };

            last_end = span.end.byte_offset().unwrap_or(last_end);
            let anchor_id = event.anchor_id().unwrap_or(0);
            let parent = frames.last().copied();
            let is_node = event.is_node();
            if is_node && let Some(Frame::Mapping { expect_key, .. }) = frames.last_mut() {
                *expect_key = !*expect_key;
            }

            let event = match event {
                ParserEvent::StreamStart | ParserEvent::DocumentStart(..) => continue,
                ParserEvent::StreamEnd => {
                    let mark = self.mark_at(&span.start);
                    return self.end_of_stream(first, document, mark);
                }
                ParserEvent::DocumentEnd => {
                    self.parser = Some(parser);
                    return Some(document);
                }
                ParserEvent::Alias(id) => Event::Alias(id),
                ParserEvent::Scalar(value, style, _, tag) => {
                    // An absent node reads as the empty plain scalar.
                    let absent =
                        span.start.index() == span.end.index() && style == ParserStyle::Plain;
                    let block = matches!(style, ParserStyle::Literal | ParserStyle::Folded);
                    let mark = if absent && tag.is_none() && anchor_id == 0 {
                        self.empty_node_mark(&span.start, parent)
                    } else {
                        self.node_mark(&span, anchor_id != 0, block)
                    };
                    if anchor_id != 0 {
                        document.aliases.insert(anchor_id, document.events.len());
                    }
                    if matches!(style, ParserStyle::SingleQuoted | ParserStyle::DoubleQuoted) {
                        document
                            .quoted
                            .push(span.start.byte_offset().unwrap_or(mark.index));
                    }
                    let event = Event::Scalar(Scalar {
                        tag: full_tag(tag.as_deref()),
                        value: if absent {
                            String::new()
                        } else if block && self.ends_unterminated(&span) {
                            // The last line has no line break to keep.
                            let mut value = value.into_owned();
                            if value.ends_with('\n') {
                                value.pop();
                            }
                            value
                        } else {
                            value.into_owned()
                        },
                        style: match style {
                            ParserStyle::Plain => ScalarStyle::Plain,
                            ParserStyle::SingleQuoted => ScalarStyle::SingleQuoted,
                            ParserStyle::DoubleQuoted => ScalarStyle::DoubleQuoted,
                            ParserStyle::Literal => ScalarStyle::Literal,
                            ParserStyle::Folded => ScalarStyle::Folded,
                        },
                    });
                    document.events.push((event, mark));
                    continue;
                }
                ParserEvent::SequenceStart(style, _, tag) => {
                    let flow = style == StructureStyle::Flow;
                    frames.push(Frame::Sequence { flow });
                    let mut mark = self.node_mark(&span, anchor_id != 0, false);
                    // An indentless sequence is reported at its first entry's
                    // content; libyaml marks the `-`.
                    if !flow
                        && tag.is_none()
                        && anchor_id == 0
                        && let Some(dash) = self.dash_before(mark)
                    {
                        mark = dash;
                    }
                    if anchor_id != 0 {
                        document.aliases.insert(anchor_id, document.events.len());
                    }
                    let event = Event::SequenceStart(full_tag(tag.as_deref()));
                    document.events.push((event, mark));
                    continue;
                }
                ParserEvent::MappingStart(style, _, tag) => {
                    frames.push(Frame::Mapping {
                        flow: style == StructureStyle::Flow,
                        expect_key: true,
                    });
                    Event::MappingStart(full_tag(tag.as_deref()))
                }
                ParserEvent::SequenceEnd | ParserEvent::MappingEnd => {
                    frames.pop();
                    let end = if matches!(event, ParserEvent::SequenceEnd) {
                        Event::SequenceEnd
                    } else {
                        Event::MappingEnd
                    };
                    document.events.push((end, self.mark_at(&span.start)));
                    continue;
                }
                // Comments, when the parser is built to report them.
                _ => continue,
            };
            if anchor_id != 0 {
                document.aliases.insert(anchor_id, document.events.len());
            }
            let mark = self.node_mark(&span, anchor_id != 0, false);
            document.events.push((event, mark));
        }
    }

    fn end_of_stream(&self, first: bool, mut document: Document, mark: Mark) -> Option<Document> {
        if !first {
            return None;
        }
        if document.events.is_empty() {
            document.events.push((Event::Void, mark));
        }
        Some(document)
    }

    fn mark_at(&self, marker: &Marker) -> Mark {
        let index = marker.byte_offset().unwrap_or(marker.index());
        let line = marker.line().saturating_sub(1);
        Mark {
            index,
            line,
            // libyaml counts a byte order mark as a column.
            column: if line == 0 && self.input.starts_with('\u{feff}') {
                marker.col().saturating_add(1)
            } else {
                marker.col()
            },
        }
    }

    /// Where a node starts, as libyaml marks it: at its first property
    /// (`!tag`, `&anchor`) when it has any, at the `|`/`>` indicator of a
    /// block scalar.
    fn node_mark(&self, span: &Span, anchored: bool, block_scalar: bool) -> Mark {
        let mut mark = self.mark_at(&span.start);
        if block_scalar && let Some(indicator) = self.block_indicator(mark) {
            mark = indicator;
        }
        if let Some(tag) = span.tag_start
            && tag.index() < span.start.index()
        {
            mark = self.mark_at(&tag);
        }
        if anchored && let Some(anchor) = self.anchor_mark(mark) {
            mark = anchor;
        }
        mark
    }

    /// The `&anchor` token right before the node at `node`; the parser does
    /// not report its position.
    fn anchor_mark(&self, node: Mark) -> Option<Mark> {
        let before = self.input.get(..node.index)?;
        let token = before.trim_end_matches([' ', '\t', '\r', '\n']);
        let amp = token.rfind('&')?;
        let name = token.get(amp.checked_add(1)?..)?;
        if name.is_empty()
            || name
                .contains(|c: char| c.is_whitespace() || matches!(c, ',' | '[' | ']' | '{' | '}'))
        {
            return None;
        }
        self.mark_before(node, amp)
    }

    /// The header (`|`, `>-`, `|2+`) of the block scalar whose content starts
    /// at `content`.
    fn block_indicator(&self, content: Mark) -> Option<Mark> {
        let before = self.input.get(..content.index)?;
        let text = before.trim_end_matches([' ', '\t', '\r', '\n']);
        let line_start = line_start(text);
        let line = text.get(line_start..)?;
        let header_end = |line: &str| -> Option<usize> {
            let header =
                line.trim_end_matches(['+', '-', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9']);
            header
                .ends_with(['|', '>'])
                .then(|| header.len().saturating_sub(1))
        };
        let at = header_end(line).or_else(|| {
            // A comment may follow the header.
            let comment = line.find(" #").or_else(|| line.find("\t#"))?;
            header_end(line.get(..comment)?.trim_end())
        })?;
        self.mark_before(content, line_start.checked_add(at)?)
    }

    /// Where libyaml marks an absent node: after the `:`, `-` or `?` that
    /// introduces it in block context, at the next token otherwise.
    fn empty_node_mark(&self, at: &Marker, parent: Option<Frame>) -> Mark {
        let mark = self.mark_at(at);
        let next = self
            .input
            .get(mark.index..)
            .and_then(|rest| rest.chars().next());
        let found = match parent {
            Some(Frame::Mapping {
                flow: false,
                expect_key: false,
            }) if next == Some(':') => self.mark_after(mark, mark.index.saturating_add(1)),
            Some(Frame::Mapping { flow: true, .. } | Frame::Sequence { flow: true }) => {
                if next == Some(':') {
                    self.next_token(mark, mark.index.saturating_add(1))
                } else {
                    None
                }
            }
            Some(Frame::Sequence { flow: false }) => self.indicator_before(mark, '-'),
            Some(Frame::Mapping {
                flow: false,
                expect_key: true,
            }) => self.indicator_before(mark, '?'),
            None => self.next_token(mark, mark.index),
            _ => None,
        };
        found.unwrap_or(mark)
    }

    /// Whether the block scalar spanning `span` runs to the end of input on a
    /// last line with content but no line break. The parser then still adds
    /// a final line break; libyaml and go-yaml did not.
    fn ends_unterminated(&self, span: &Span) -> bool {
        let end = span.end.byte_offset().unwrap_or(span.end.index());
        // Only a scalar at the end of input pays for the backward search, so
        // a file of many block scalars stays linear.
        end >= self.input.len()
            && !self
                .input
                .get(line_start(self.input)..)
                .unwrap_or_default()
                .trim_matches([' ', '\t'])
                .is_empty()
    }

    /// The `-` before the first entry starting at `entry`, on the same line.
    fn dash_before(&self, entry: Mark) -> Option<Mark> {
        let rest = self.input.get(entry.index..)?;
        if rest.starts_with('-') {
            return None;
        }
        let before = self.input.get(..entry.index)?;
        let line = before.get(line_start(before)..)?;
        let dash = line.trim_end_matches([' ', '\t']);
        if !dash.ends_with('-') || dash.len() == line.len() {
            return None;
        }
        let at = line_start(before).checked_add(dash.len())?.checked_sub(1)?;
        self.mark_before(entry, at)
    }

    /// Just past the `indicator` (`-`, `?`) that opens an absent node on the
    /// same line, before `node`.
    fn indicator_before(&self, node: Mark, indicator: char) -> Option<Mark> {
        let before = self.input.get(..node.index)?;
        let line_start = line_start(before);
        let line = before.get(line_start..)?;
        let at = line.char_indices().rev().find_map(|(i, c)| {
            let rest = line.get(i.saturating_add(1)..)?;
            let blank = rest.trim_start_matches([' ', '\t']);
            (c == indicator
                && (rest.is_empty() || rest.starts_with([' ', '\t']))
                && (blank.is_empty() || blank.starts_with('#')))
            .then_some(i)
        })?;
        self.mark_before(node, line_start.checked_add(at)?.checked_add(1)?)
    }

    /// The next token at or after byte `from`, skipping blanks, line breaks
    /// and comments; the end of input if there is none.
    fn next_token(&self, base: Mark, from: usize) -> Option<Mark> {
        let rest = self.input.get(from..)?;
        let mut offset = rest.len();
        let mut in_comment = false;
        let mut prev_blank = true;
        for (i, c) in rest.char_indices() {
            if in_comment {
                if matches!(c, '\n' | '\r') {
                    in_comment = false;
                    prev_blank = true;
                }
                continue;
            }
            match c {
                ' ' | '\t' | '\r' | '\n' | '\u{feff}' => prev_blank = true,
                '#' if prev_blank => in_comment = true,
                _ => {
                    offset = i;
                    break;
                }
            }
        }
        self.mark_after(base, from.checked_add(offset)?)
    }

    /// The mark of byte `index`, which lies before `node`.
    fn mark_before(&self, node: Mark, index: usize) -> Option<Mark> {
        let gap = self.input.get(index..node.index)?;
        let breaks = line_breaks(gap);
        let (line, column) = if breaks == 0 {
            (node.line, node.column.checked_sub(gap.chars().count())?)
        } else {
            (node.line.checked_sub(breaks)?, self.column_of(index)?)
        };
        Some(Mark {
            index,
            line,
            column,
        })
    }

    /// The mark of byte `index`, which lies after `node`.
    fn mark_after(&self, node: Mark, index: usize) -> Option<Mark> {
        let gap = self.input.get(node.index..index)?;
        let breaks = line_breaks(gap);
        let (line, column) = if breaks == 0 {
            (node.line, node.column.checked_add(gap.chars().count())?)
        } else {
            (node.line.checked_add(breaks)?, self.column_of(index)?)
        };
        Some(Mark {
            index,
            line,
            column,
        })
    }

    /// The column of byte `index`, on a line after the first.
    fn column_of(&self, index: usize) -> Option<usize> {
        let before = self.input.get(..index)?;
        let line_start = line_start(before);
        Some(before.get(line_start..)?.chars().count())
    }
}

fn full_tag(tag: Option<&granit_parser::Tag>) -> Option<String> {
    tag.map(|tag| format!("{}{}", tag.handle(), tag.suffix()))
}

/// The number of line breaks in `text`: `\n`, `\r\n` or a lone `\r`.
fn line_breaks(text: &str) -> usize {
    let crlf = text.matches("\r\n").count();
    text.matches(['\n', '\r']).count().saturating_sub(crlf)
}

/// The offset in `text` where its last line starts.
fn line_start(text: &str) -> usize {
    text.rfind(['\n', '\r']).map_or(0, |i| i.saturating_add(1))
}
