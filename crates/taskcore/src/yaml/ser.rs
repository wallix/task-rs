//! `Value` serialization: `Serialize` impls for handing values to other
//! formats, and `to_string`, a block-style YAML emitter reproducing libyaml's
//! output as serde_yaml_ng drove it (2-space indent, no line folding, Unicode
//! kept, sequences in mappings not indented).
//!
//! Scalar analysis, style selection and writing follow libyaml's emitter
//! (`emitter.c`, Copyright 2006-2016 Kirill Simonov, 2017-2020 Ingy döt Net,
//! MIT) as translated by unsafe-libyaml (David Tolnay, MIT), modified; see
//! `THIRD-PARTY-NOTICES`.

use serde::Serialize;

use super::de::{digits_but_not_number, visit_untagged_scalar};
use super::error::{self, Error, ErrorImpl};
use super::mapping::Mapping;
use super::tagged::nobang;
use super::value::Value;

impl Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Number(n) => n.serialize(serializer),
            Value::String(s) => serializer.serialize_str(s),
            Value::Sequence(seq) => seq.serialize(serializer),
            Value::Mapping(mapping) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(mapping.len()))?;
                for (k, v) in mapping {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
            Value::Tagged(tagged) => tagged.serialize(serializer),
        }
    }
}

/// Serializes `value` as a YAML document.
pub fn to_string(value: &Value) -> Result<String, Error> {
    let mut emitter = Emitter::default();
    emitter.node(value, Context::Root)?;
    // Document end: finish the last line.
    emitter.write_indent();
    Ok(emitter.out)
}

const BEST_INDENT: usize = 2;
/// libyaml's limit on the length of an implicit (`key: value`) key.
const MAX_SIMPLE_KEY_LEN: usize = 128;

#[derive(Copy, Clone, PartialEq, Eq)]
enum Context {
    Root,
    Sequence,
    Mapping,
    SimpleKey,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Style {
    Any,
    Plain,
    SingleQuoted,
    DoubleQuoted,
    Literal,
}

/// The output state libyaml tracks, for a document in block style; flow
/// style only appears for empty collections.
struct Emitter {
    out: String,
    column: usize,
    /// The last character written is whitespace (or nothing was written).
    whitespace: bool,
    /// Only indentation has been written on the current line.
    indention: bool,
    indent: Option<usize>,
    indents: Vec<Option<usize>>,
}

impl Default for Emitter {
    fn default() -> Self {
        Emitter {
            out: String::new(),
            column: 0,
            whitespace: true,
            indention: true,
            indent: None,
            indents: Vec::new(),
        }
    }
}

/// libyaml's view of a scalar's content, deciding which styles may hold it.
struct Analysis {
    multiline: bool,
    block_plain_allowed: bool,
    single_quoted_allowed: bool,
    block_allowed: bool,
}

fn is_break(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

fn is_blankz(c: Option<char>) -> bool {
    match c {
        None | Some(' ' | '\t' | '\0') => true,
        Some(c) => is_break(c),
    }
}

/// libyaml's IS_PRINTABLE.
fn is_printable(c: char) -> bool {
    matches!(
        u32::from(c),
        0x0A | 0x20..=0x7E | 0xA0..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF
    ) && c != '\u{feff}'
}

fn analyze(value: &str) -> Analysis {
    if value.is_empty() {
        return Analysis {
            multiline: false,
            block_plain_allowed: true,
            single_quoted_allowed: true,
            block_allowed: false,
        };
    }

    let mut block_indicators = value.starts_with("---") || value.starts_with("...");
    let mut line_breaks = false;
    let mut special_characters = false;
    let mut leading_space = false;
    let mut leading_break = false;
    let mut trailing_space = false;
    let mut trailing_break = false;
    let mut break_space = false;
    let mut space_break = false;
    let mut previous_space = false;
    let mut previous_break = false;

    let mut preceded_by_whitespace = true;
    let mut chars = value.chars().peekable();
    let mut first = true;
    while let Some(c) = chars.next() {
        let next = chars.peek().copied();
        let followed_by_whitespace = is_blankz(next);
        let last = next.is_none();
        if first {
            if matches!(
                c,
                '#' | ','
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '&'
                    | '*'
                    | '!'
                    | '|'
                    | '>'
                    | '\''
                    | '"'
                    | '%'
                    | '@'
                    | '`'
            ) {
                block_indicators = true;
            }
            if matches!(c, '?' | ':') && followed_by_whitespace {
                block_indicators = true;
            }
            if c == '-' && followed_by_whitespace {
                block_indicators = true;
            }
        } else {
            if c == ':' && followed_by_whitespace {
                block_indicators = true;
            }
            if c == '#' && preceded_by_whitespace {
                block_indicators = true;
            }
        }

        if !is_printable(c) {
            special_characters = true;
        }
        if is_break(c) {
            line_breaks = true;
        }

        if c == ' ' {
            if first {
                leading_space = true;
            }
            if last {
                trailing_space = true;
            }
            if previous_break {
                break_space = true;
            }
            previous_space = true;
            previous_break = false;
        } else if is_break(c) {
            if first {
                leading_break = true;
            }
            if last {
                trailing_break = true;
            }
            if previous_space {
                space_break = true;
            }
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }

        preceded_by_whitespace = is_blankz(Some(c));
        first = false;
    }

    let mut analysis = Analysis {
        multiline: line_breaks,
        block_plain_allowed: true,
        single_quoted_allowed: true,
        block_allowed: true,
    };
    if leading_space || leading_break || trailing_space || trailing_break {
        analysis.block_plain_allowed = false;
    }
    if trailing_space {
        analysis.block_allowed = false;
    }
    if break_space {
        analysis.block_plain_allowed = false;
        analysis.single_quoted_allowed = false;
    }
    if space_break || special_characters {
        analysis.block_plain_allowed = false;
        analysis.single_quoted_allowed = false;
        analysis.block_allowed = false;
    }
    if line_breaks || block_indicators {
        analysis.block_plain_allowed = false;
    }
    analysis
}

/// The style serde_yaml_ng requests for a string: quoted when it would read
/// back as something else, literal when it spans lines.
fn requested_style(value: &str) -> Style {
    struct InferScalarStyle;

    impl serde::de::Visitor<'_> for InferScalarStyle {
        type Value = Style;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("I wonder")
        }

        fn visit_bool<E>(self, _v: bool) -> Result<Self::Value, E> {
            Ok(Style::SingleQuoted)
        }

        fn visit_i64<E>(self, _v: i64) -> Result<Self::Value, E> {
            Ok(Style::SingleQuoted)
        }

        fn visit_i128<E>(self, _v: i128) -> Result<Self::Value, E> {
            Ok(Style::SingleQuoted)
        }

        fn visit_u64<E>(self, _v: u64) -> Result<Self::Value, E> {
            Ok(Style::SingleQuoted)
        }

        fn visit_u128<E>(self, _v: u128) -> Result<Self::Value, E> {
            Ok(Style::SingleQuoted)
        }

        fn visit_f64<E>(self, _v: f64) -> Result<Self::Value, E> {
            Ok(Style::SingleQuoted)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
            Ok(if digits_but_not_number(v) {
                Style::SingleQuoted
            } else {
                Style::Any
            })
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Style::SingleQuoted)
        }
    }

    if value.contains('\n') {
        Style::Literal
    } else {
        visit_untagged_scalar(InferScalarStyle, value).unwrap_or(Style::Any)
    }
}

/// The scalar text and requested style of a non-collection value.
fn scalar_text(value: &Value) -> Option<(String, Style)> {
    match value {
        Value::Null => Some(("null".to_string(), Style::Plain)),
        Value::Bool(b) => Some((b.to_string(), Style::Plain)),
        Value::Number(n) => Some((n.to_string(), Style::Plain)),
        Value::String(s) => Some((s.clone(), requested_style(s))),
        Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => None,
    }
}

/// A node with its tag split off, the tag spelled as serde_yaml_ng handed it
/// to libyaml (`!foo`); nested tags are not representable.
fn untag(value: &Value) -> Result<(Option<String>, &Value), Error> {
    match value {
        Value::Tagged(tagged) => {
            if let Value::Tagged(_) = tagged.value {
                return Err(error::new(ErrorImpl::SerializeNestedEnum));
            }
            let tag = nobang(&tagged.tag.string);
            let tag = if tag.starts_with('!') {
                tag.to_string()
            } else {
                format!("!{tag}")
            };
            Ok((Some(tag), &tagged.value))
        }
        other => Ok((None, other)),
    }
}

/// Whether `value` fits as an implicit key: a single-line scalar or an empty
/// collection, at most 128 bytes with its tag.
fn is_simple_key(value: &Value) -> Result<bool, Error> {
    let (tag, value) = untag(value)?;
    let mut length = tag.map_or(0, |t| t.len());
    match value {
        Value::Sequence(seq) if !seq.is_empty() => return Ok(false),
        Value::Mapping(map) if !map.is_empty() => return Ok(false),
        Value::Sequence(_) | Value::Mapping(_) => {}
        other => {
            if let Some((text, _)) = scalar_text(other) {
                if analyze(&text).multiline {
                    return Ok(false);
                }
                length = length.saturating_add(text.len());
            }
        }
    }
    Ok(length <= MAX_SIMPLE_KEY_LEN)
}

impl Emitter {
    fn put(&mut self, c: char) {
        self.out.push(c);
        self.column = self.column.saturating_add(1);
    }

    fn put_str(&mut self, s: &str) {
        for c in s.chars() {
            self.put(c);
        }
    }

    fn put_break(&mut self) {
        self.out.push('\n');
        self.column = 0;
    }

    /// libyaml's WRITE_BREAK: a line feed, or another break character kept as is.
    fn write_break(&mut self, c: char) {
        if c == '\n' {
            self.put_break();
        } else {
            self.out.push(c);
            self.column = 0;
        }
    }

    fn write_indicator(
        &mut self,
        indicator: &str,
        need_whitespace: bool,
        is_whitespace: bool,
        is_indention: bool,
    ) {
        if need_whitespace && !self.whitespace {
            self.put(' ');
        }
        self.put_str(indicator);
        self.whitespace = is_whitespace;
        self.indention = self.indention && is_indention;
    }

    fn write_indent(&mut self) {
        let indent = self.indent.unwrap_or(0);
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.put_break();
        }
        while self.column < indent {
            self.put(' ');
        }
        self.whitespace = true;
        self.indention = true;
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        self.indent = match self.indent {
            None => Some(if flow { BEST_INDENT } else { 0 }),
            Some(indent) if !indentless => Some(indent.saturating_add(BEST_INDENT)),
            Some(indent) => Some(indent),
        };
    }

    fn decrease_indent(&mut self) {
        self.indent = self.indents.pop().flatten();
    }

    fn write_tag(&mut self, tag: Option<&str>) {
        let Some(tag) = tag else { return };
        if !self.whitespace {
            self.put(' ');
        }
        // The `!` handle applies to a tag with a suffix after it; any other
        // tag is written verbatim.
        let verbatim = tag.len() <= 1;
        let content = if verbatim {
            self.put_str("!<");
            tag
        } else {
            self.put('!');
            tag.get(1..).unwrap_or_default()
        };
        for c in content.chars() {
            if c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    '-' | '_'
                        | ';'
                        | '/'
                        | '?'
                        | ':'
                        | '@'
                        | '&'
                        | '='
                        | '+'
                        | '$'
                        | ','
                        | '.'
                        | '~'
                        | '*'
                        | '\''
                        | '('
                        | ')'
                        | '['
                        | ']'
                )
            {
                self.put(c);
            } else {
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).bytes() {
                    self.put_str(&format!("%{byte:02X}"));
                }
            }
        }
        if verbatim {
            self.put('>');
        }
        self.whitespace = false;
        self.indention = false;
    }

    fn node(&mut self, value: &Value, context: Context) -> Result<(), Error> {
        let (tag, value) = untag(value)?;
        match value {
            Value::Sequence(seq) => {
                self.write_tag(tag.as_deref());
                if seq.is_empty() {
                    self.empty_flow("[", "]");
                    return Ok(());
                }
                let indentless = context == Context::Mapping && !self.indention;
                self.increase_indent(false, indentless);
                for item in seq {
                    self.write_indent();
                    self.write_indicator("-", true, false, true);
                    self.node(item, Context::Sequence)?;
                }
                self.decrease_indent();
            }
            Value::Mapping(map) => {
                self.write_tag(tag.as_deref());
                if map.is_empty() {
                    self.empty_flow("{", "}");
                    return Ok(());
                }
                self.mapping(map)?;
            }
            scalar => {
                let Some((text, style)) = scalar_text(scalar) else {
                    return Ok(());
                };
                let style = select_style(&text, style, context == Context::SimpleKey);
                self.write_tag(tag.as_deref());
                self.increase_indent(true, false);
                self.scalar(&text, style);
                self.decrease_indent();
            }
        }
        Ok(())
    }

    fn empty_flow(&mut self, open: &str, close: &str) {
        self.write_indicator(open, true, true, false);
        self.write_indicator(close, false, false, false);
    }

    fn mapping(&mut self, map: &Mapping) -> Result<(), Error> {
        self.increase_indent(false, false);
        for (key, value) in map {
            self.write_indent();
            if is_simple_key(key)? {
                self.node(key, Context::SimpleKey)?;
                self.write_indicator(":", false, false, false);
            } else {
                self.write_indicator("?", true, false, true);
                self.node(key, Context::Mapping)?;
                self.write_indent();
                self.write_indicator(":", true, false, true);
            }
            self.node(value, Context::Mapping)?;
        }
        self.decrease_indent();
        Ok(())
    }

    fn scalar(&mut self, text: &str, style: Style) {
        match style {
            Style::Any | Style::Plain => {
                if !self.whitespace && !text.is_empty() {
                    self.put(' ');
                }
                self.put_str(text);
                self.whitespace = false;
                self.indention = false;
            }
            Style::SingleQuoted => self.single_quoted(text),
            Style::DoubleQuoted => self.double_quoted(text),
            Style::Literal => self.literal(text),
        }
    }

    fn single_quoted(&mut self, text: &str) {
        self.write_indicator("'", true, false, false);
        let mut breaks = false;
        for c in text.chars() {
            if is_break(c) {
                if !breaks && c == '\n' {
                    self.put_break();
                }
                self.write_break(c);
                self.indention = true;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                if c == '\'' {
                    self.put_str("''");
                } else {
                    self.put(c);
                }
                self.indention = false;
                breaks = false;
            }
        }
        if breaks {
            self.write_indent();
        }
        self.write_indicator("'", false, false, false);
    }

    fn double_quoted(&mut self, text: &str) {
        self.write_indicator("\"", true, false, false);
        for c in text.chars() {
            if !is_printable(c) || c == '\u{feff}' || is_break(c) || c == '"' || c == '\\' {
                self.put('\\');
                match c {
                    '\0' => self.put('0'),
                    '\u{7}' => self.put('a'),
                    '\u{8}' => self.put('b'),
                    '\t' => self.put('t'),
                    '\n' => self.put('n'),
                    '\u{b}' => self.put('v'),
                    '\u{c}' => self.put('f'),
                    '\r' => self.put('r'),
                    '\u{1b}' => self.put('e'),
                    '"' => self.put('"'),
                    '\\' => self.put('\\'),
                    '\u{85}' => self.put('N'),
                    '\u{a0}' => self.put('_'),
                    '\u{2028}' => self.put('L'),
                    '\u{2029}' => self.put('P'),
                    _ => {
                        let code = u32::from(c);
                        let escaped = if code <= 0xFF {
                            format!("x{code:02X}")
                        } else if code <= 0xFFFF {
                            format!("u{code:04X}")
                        } else {
                            format!("U{code:08X}")
                        };
                        self.put_str(&escaped);
                    }
                }
            } else {
                self.put(c);
            }
        }
        self.write_indicator("\"", false, false, false);
    }

    fn literal(&mut self, text: &str) {
        self.write_indicator("|", true, false, false);
        // Block scalar hints: an explicit indent when the content starts with
        // whitespace, and the chomping that keeps the trailing breaks exact.
        if text.starts_with(|c: char| c == ' ' || is_break(c)) {
            self.write_indicator(&BEST_INDENT.to_string(), false, false, false);
        }
        let mut rev = text.chars().rev();
        let chomp = match (rev.next(), rev.next()) {
            (None, _) => Some("-"),
            (Some(last), _) if !is_break(last) => Some("-"),
            (Some(_), None) => Some("+"),
            (Some(_), Some(prev)) if is_break(prev) => Some("+"),
            _ => None,
        };
        if let Some(chomp) = chomp {
            self.write_indicator(chomp, false, false, false);
        }
        self.put_break();
        self.indention = true;
        self.whitespace = true;
        let mut breaks = true;
        for c in text.chars() {
            if is_break(c) {
                self.write_break(c);
                self.indention = true;
                breaks = true;
            } else {
                if breaks {
                    self.write_indent();
                }
                self.put(c);
                self.indention = false;
                breaks = false;
            }
        }
    }
}

/// libyaml's choice among the requested style and those the content allows.
fn select_style(text: &str, requested: Style, simple_key: bool) -> Style {
    let analysis = analyze(text);
    let mut style = if requested == Style::Any {
        Style::Plain
    } else {
        requested
    };
    if simple_key && analysis.multiline {
        style = Style::DoubleQuoted;
    }
    if style == Style::Plain {
        if !analysis.block_plain_allowed {
            style = Style::SingleQuoted;
        }
        if text.is_empty() && simple_key {
            style = Style::SingleQuoted;
        }
    }
    if style == Style::SingleQuoted && !analysis.single_quoted_allowed {
        style = Style::DoubleQuoted;
    }
    if style == Style::Literal && (!analysis.block_allowed || simple_key) {
        style = Style::DoubleQuoted;
    }
    style
}
