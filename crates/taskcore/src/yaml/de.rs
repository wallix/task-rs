//! Deserialization straight from parser events, so errors carry the position
//! and path of the node that failed, and plain scalars are typed by
//! serde_yaml_ng's resolution rules (`0022` and `1_000` stay strings, `0x1F`
//! is 31, and only `true`/`false` in lower, title or upper case are booleans).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::ParseIntError;
use std::sync::Arc;

use serde::de::value::StrDeserializer;
use serde::de::{self, Deserialize, DeserializeSeed, Expected, IgnoredAny, Unexpected, Visitor};

use super::error::{self, Error, ErrorImpl, Mark};
use super::loader::{Document, Event, Fixable, Loader, Scalar, ScalarStyle};
use super::path::Path;
use super::reader;
use super::reindent::Reindent;

type Result<T, E = Error> = std::result::Result<T, E>;

const TAG_NULL: &str = "tag:yaml.org,2002:null";
const TAG_BOOL: &str = "tag:yaml.org,2002:bool";
const TAG_INT: &str = "tag:yaml.org,2002:int";
const TAG_FLOAT: &str = "tag:yaml.org,2002:float";

/// Nesting depth past which deserialization gives up. The parser has its own
/// limit of 255 flow or block levels and stops the parse there, so nesting
/// past 255 reports granit's "recursion limit exceeded" at the 256th level
/// (column 256 for `[[[…`) rather than this one's at the 129th.
const RECURSION_LIMIT: u8 = 128;

/// How many parses a re-indent may take before the parser's error stands.
const REINDENT_LIMIT: usize = 64;

/// The YAML input, as text or as bytes still to be decoded.
#[derive(Clone, Copy)]
pub(crate) enum Input<'a> {
    Str(&'a str),
    Slice(&'a [u8]),
}

/// A deserializer for a single YAML document. Every request loads the
/// document, so errors found while loading (syntax, a second document)
/// reach the `Deserialize` impl like any other, as with serde_yaml_ng.
pub(crate) struct Deserializer<'a> {
    pub(crate) input: Input<'a>,
}

impl Deserializer<'_> {
    fn de<T>(
        self,
        f: impl for<'document> FnOnce(&mut DeserializerFromEvents<'document>) -> Result<T>,
    ) -> Result<T> {
        let text = match self.input {
            Input::Str(text) => reader::check_printable(text).map(|()| text),
            Input::Slice(bytes) => reader::decode(bytes),
        };
        let text = match text {
            Ok(text) => text,
            Err(e) => {
                let document = Document {
                    events: Vec::new(),
                    error: Some(error::new(ErrorImpl::Reader(e.problem, e.offset)).shared()),
                    aliases: BTreeMap::new(),
                    fixable: None,
                    error_mark: None,
                    quoted: Vec::new(),
                };
                return deserialize_document(&document, f);
            }
        };
        let mut loader = Loader::new(text);
        let Some(document) = loader.next_document() else {
            return Err(error::new(ErrorImpl::EndOfStream));
        };
        if let Some(first) = document.fixable {
            let (document, more) = reindent(text, document, first);
            return finish(&document, more, f);
        }
        let more = document.error.is_none() && loader.next_document().is_some();
        finish(&document, more, f)
    }
}

/// Re-indents what the parser refuses and libyaml did not, until the text
/// parses or fails for another reason. `original` is the document as first
/// parsed, which failed with `first`. Returns the document, with positions
/// in the original text, and whether another document follows; the original
/// document when the first refusal cannot be fixed.
fn reindent(text: &str, mut original: Document, first: Fixable) -> (Document, bool) {
    let mut fix = Reindent::new(text);
    if !apply(&mut fix, first) {
        report_unfixable(&fix, first, &mut original);
        return (original, false);
    }
    for _ in 0..REINDENT_LIMIT {
        let rendered = fix.render();
        let mut loader = Loader::new(&rendered);
        let Some(mut document) = loader.next_document() else {
            return (original, false);
        };
        let quoted: BTreeSet<usize> = document
            .quoted
            .iter()
            .map(|&index| fix.map_offset(index))
            .collect();
        // Every quote before where the parse stopped must have been read as
        // a quoted scalar; one that was not loses its edits.
        let stop = document
            .error_mark
            .map_or(text.len(), |mark| fix.map_offset(mark.index))
            .saturating_add(1);
        if fix.reject_unconfirmed(&quoted, stop) {
            continue;
        }
        for (_, mark) in &mut document.events {
            *mark = fix.map(*mark);
        }
        let fixed = match document.fixable {
            Some(Fixable::Quote { at, after }) => Some(Fixable::Quote {
                at: fix.map(at),
                after: fix.map_offset(after),
            }),
            Some(Fixable::FlowTab { at }) => Some(Fixable::FlowTab { at: fix.map(at) }),
            None => None,
        };
        if let Some(next) = fixed
            && apply(&mut fix, next)
        {
            continue;
        }
        // Done, or failed for a reason re-indenting cannot help: report it
        // where it is in the original text.
        if let (Some(error), Some(mark)) = (&document.error, document.error_mark) {
            document.error = Some(error::moved(error, fix.map(mark)));
            if let Some(next) = fixed {
                report_unfixable(&fix, next, &mut document);
            }
            return (document, false);
        }
        let more = loader.next_document().is_some();
        return (document, more);
    }
    (original, false)
}

/// Reports an indentation error inside a quoted scalar as libyaml would
/// have: at the opening quote of a scalar that never closes (the parser read
/// on past where the value was meant to end, so its own error points at an
/// unrelated line), or at a document marker cutting the scalar.
fn report_unfixable(fix: &Reindent<'_>, fixable: Fixable, document: &mut Document) {
    let Fixable::Quote { at, after } = fixable else {
        return;
    };
    let error = if let Some(open) = fix.unclosed_quote(at, after) {
        ErrorImpl::Scan("unclosed quote".to_string(), open)
    } else if let Some(marker) = fix.document_marker(at, after) {
        ErrorImpl::Scan(
            "unexpected document indicator in quoted scalar".to_string(),
            marker,
        )
    } else {
        return;
    };
    document.error = Some(error::new(error).shared());
}

fn apply(fix: &mut Reindent<'_>, fixable: Fixable) -> bool {
    match fixable {
        Fixable::Quote { at, after } => fix.fix_from(at, after),
        Fixable::FlowTab { at } => fix.fix_flow_tabs(at),
    }
}

/// Deserializes `document`, then refuses a document following it.
fn finish<T>(
    document: &Document,
    more: bool,
    f: impl for<'document> FnOnce(&mut DeserializerFromEvents<'document>) -> Result<T>,
) -> Result<T> {
    let t = deserialize_document(document, f)?;
    if more {
        Err(error::new(ErrorImpl::MoreThanOneDocument))
    } else {
        Ok(t)
    }
}

fn deserialize_document<T>(
    document: &Document,
    f: impl for<'document> FnOnce(&mut DeserializerFromEvents<'document>) -> Result<T>,
) -> Result<T> {
    let mut pos = 0;
    let mut jumpcount = 0;
    let t = f(&mut DeserializerFromEvents {
        document,
        pos: &mut pos,
        jumpcount: &mut jumpcount,
        path: Path::Root,
        remaining_depth: RECURSION_LIMIT,
        current_enum: None,
    })?;
    if let Some(parse_error) = &document.error {
        return Err(error::shared(Arc::clone(parse_error)));
    }
    Ok(t)
}

macro_rules! forward_to_document {
    ($($method:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            fn $method<V: Visitor<'de>>(self, $($arg: $ty,)* visitor: V) -> Result<V::Value> {
                self.de(|state| state.$method($($arg,)* visitor))
            }
        )*
    };
}

impl<'de> de::Deserializer<'de> for Deserializer<'_> {
    type Error = Error;

    forward_to_document! {
        deserialize_any();
        deserialize_bool();
        deserialize_i8();
        deserialize_i16();
        deserialize_i32();
        deserialize_i64();
        deserialize_i128();
        deserialize_u8();
        deserialize_u16();
        deserialize_u32();
        deserialize_u64();
        deserialize_u128();
        deserialize_f32();
        deserialize_f64();
        deserialize_char();
        deserialize_str();
        deserialize_string();
        deserialize_bytes();
        deserialize_byte_buf();
        deserialize_option();
        deserialize_unit();
        deserialize_unit_struct(name: &'static str);
        deserialize_newtype_struct(name: &'static str);
        deserialize_seq();
        deserialize_tuple(len: usize);
        deserialize_tuple_struct(name: &'static str, len: usize);
        deserialize_map();
        deserialize_struct(name: &'static str, fields: &'static [&'static str]);
        deserialize_enum(name: &'static str, variants: &'static [&'static str]);
        deserialize_identifier();
        deserialize_ignored_any();
    }
}

fn unexpected_end(what: &str) -> Error {
    de::Error::custom(format_args!("unexpected end of {what}"))
}

struct DeserializerFromEvents<'document> {
    document: &'document Document,
    pos: &'document mut usize,
    jumpcount: &'document mut usize,
    path: Path<'document>,
    remaining_depth: u8,
    current_enum: Option<CurrentEnum<'document>>,
}

#[derive(Copy, Clone)]
struct CurrentEnum<'document> {
    name: Option<&'static str>,
    tag: &'document str,
}

impl<'document> DeserializerFromEvents<'document> {
    fn peek_event(&self) -> Result<&'document Event> {
        self.peek_event_mark().map(|(event, _mark)| event)
    }

    fn peek_event_mark(&self) -> Result<(&'document Event, Mark)> {
        match self.document.events.get(*self.pos) {
            Some((event, mark)) => Ok((event, *mark)),
            None => Err(match &self.document.error {
                Some(parse_error) => error::shared(Arc::clone(parse_error)),
                None => error::new(ErrorImpl::EndOfStream),
            }),
        }
    }

    fn next_event(&mut self) -> Result<&'document Event> {
        self.next_event_mark().map(|(event, _mark)| event)
    }

    fn next_event_mark(&mut self) -> Result<(&'document Event, Mark)> {
        self.peek_event_mark().map(|(event, mark)| {
            *self.pos = self.pos.saturating_add(1);
            self.current_enum = None;
            (event, mark)
        })
    }

    /// Steps back over the event just consumed.
    fn unread(&mut self) {
        *self.pos = self.pos.saturating_sub(1);
    }

    fn jump<'anchor>(
        &'anchor mut self,
        pos: &'anchor mut usize,
    ) -> Result<DeserializerFromEvents<'anchor>> {
        *self.jumpcount = self.jumpcount.saturating_add(1);
        if *self.jumpcount > self.document.events.len().saturating_mul(100) {
            return Err(error::new(ErrorImpl::RepetitionLimitExceeded));
        }
        match self.document.aliases.get(pos) {
            Some(found) => {
                *pos = *found;
                Ok(DeserializerFromEvents {
                    document: self.document,
                    pos,
                    jumpcount: self.jumpcount,
                    path: Path::Alias { parent: &self.path },
                    remaining_depth: self.remaining_depth,
                    current_enum: None,
                })
            }
            None => Err(de::Error::custom(format_args!("unresolved alias: {pos}"))),
        }
    }

    fn ignore_any(&mut self) -> Result<()> {
        enum Nest {
            Sequence,
            Mapping,
        }

        let mut stack = Vec::new();

        loop {
            match self.next_event()? {
                Event::Alias(_) | Event::Scalar(_) | Event::Void => {}
                Event::SequenceStart(_) => stack.push(Nest::Sequence),
                Event::MappingStart(_) => stack.push(Nest::Mapping),
                Event::SequenceEnd => match stack.pop() {
                    Some(Nest::Sequence) => {}
                    None | Some(Nest::Mapping) => return Err(unexpected_end("sequence")),
                },
                Event::MappingEnd => match stack.pop() {
                    Some(Nest::Mapping) => {}
                    None | Some(Nest::Sequence) => return Err(unexpected_end("mapping")),
                },
            }
            if stack.is_empty() {
                return Ok(());
            }
        }
    }

    fn visit_sequence<'de, V: Visitor<'de>>(&mut self, visitor: V, mark: Mark) -> Result<V::Value> {
        let (value, len) = self.recursion_check(mark, |de| {
            let mut seq = SeqAccess {
                empty: false,
                de,
                len: 0,
            };
            let value = visitor.visit_seq(&mut seq)?;
            Ok((value, seq.len))
        })?;
        self.end_sequence(len)?;
        Ok(value)
    }

    fn visit_mapping<'de, V: Visitor<'de>>(&mut self, visitor: V, mark: Mark) -> Result<V::Value> {
        let (value, len) = self.recursion_check(mark, |de| {
            let mut map = MapAccess {
                empty: false,
                de,
                len: 0,
                key: None,
            };
            let value = visitor.visit_map(&mut map)?;
            Ok((value, map.len))
        })?;
        self.end_mapping(len)?;
        Ok(value)
    }

    fn end_sequence(&mut self, len: usize) -> Result<()> {
        let total = {
            let mut seq = SeqAccess {
                empty: false,
                de: self,
                len,
            };
            while de::SeqAccess::next_element::<IgnoredAny>(&mut seq)?.is_some() {}
            seq.len
        };
        match self.next_event()? {
            Event::SequenceEnd | Event::Void => {}
            _ => return Err(de::Error::custom("expected a SequenceEnd event")),
        }
        if total == len {
            Ok(())
        } else {
            struct ExpectedSeq(usize);
            impl Expected for ExpectedSeq {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    if self.0 == 1 {
                        write!(f, "sequence of 1 element")
                    } else {
                        write!(f, "sequence of {} elements", self.0)
                    }
                }
            }
            Err(de::Error::invalid_length(total, &ExpectedSeq(len)))
        }
    }

    fn end_mapping(&mut self, len: usize) -> Result<()> {
        let total = {
            let mut map = MapAccess {
                empty: false,
                de: self,
                len,
                key: None,
            };
            while de::MapAccess::next_entry::<IgnoredAny, IgnoredAny>(&mut map)?.is_some() {}
            map.len
        };
        match self.next_event()? {
            Event::MappingEnd | Event::Void => {}
            _ => return Err(de::Error::custom("expected a MappingEnd event")),
        }
        if total == len {
            Ok(())
        } else {
            struct ExpectedMap(usize);
            impl Expected for ExpectedMap {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    if self.0 == 1 {
                        write!(f, "map containing 1 entry")
                    } else {
                        write!(f, "map containing {} entries", self.0)
                    }
                }
            }
            Err(de::Error::invalid_length(total, &ExpectedMap(len)))
        }
    }

    fn recursion_check<F: FnOnce(&mut Self) -> Result<T>, T>(
        &mut self,
        mark: Mark,
        f: F,
    ) -> Result<T> {
        let previous_depth = self.remaining_depth;
        self.remaining_depth = match previous_depth.checked_sub(1) {
            Some(depth) => depth,
            None => return Err(error::new(ErrorImpl::RecursionLimitExceeded(mark))),
        };
        let result = f(self);
        self.remaining_depth = previous_depth;
        result
    }
}

struct SeqAccess<'document, 'seq> {
    empty: bool,
    de: &'seq mut DeserializerFromEvents<'document>,
    len: usize,
}

impl<'de> de::SeqAccess<'de> for SeqAccess<'_, '_> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        if self.empty {
            return Ok(None);
        }
        match self.de.peek_event()? {
            Event::SequenceEnd | Event::Void => Ok(None),
            _ => {
                let mut element_de = DeserializerFromEvents {
                    document: self.de.document,
                    pos: self.de.pos,
                    jumpcount: self.de.jumpcount,
                    path: Path::Seq {
                        parent: &self.de.path,
                        index: self.len,
                    },
                    remaining_depth: self.de.remaining_depth,
                    current_enum: None,
                };
                self.len = self.len.saturating_add(1);
                seed.deserialize(&mut element_de).map(Some)
            }
        }
    }
}

struct MapAccess<'document, 'map> {
    empty: bool,
    de: &'map mut DeserializerFromEvents<'document>,
    len: usize,
    key: Option<&'document str>,
}

impl<'de> de::MapAccess<'de> for MapAccess<'_, '_> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        if self.empty {
            return Ok(None);
        }
        match self.de.peek_event()? {
            Event::MappingEnd | Event::Void => Ok(None),
            Event::Scalar(scalar) => {
                self.len = self.len.saturating_add(1);
                self.key = Some(&scalar.value);
                seed.deserialize(&mut *self.de).map(Some)
            }
            _ => {
                self.len = self.len.saturating_add(1);
                self.key = None;
                seed.deserialize(&mut *self.de).map(Some)
            }
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        let mut value_de = DeserializerFromEvents {
            document: self.de.document,
            pos: self.de.pos,
            jumpcount: self.de.jumpcount,
            path: if let Some(key) = self.key {
                Path::Map {
                    parent: &self.de.path,
                    key,
                }
            } else {
                Path::Unknown {
                    parent: &self.de.path,
                }
            },
            remaining_depth: self.de.remaining_depth,
            current_enum: None,
        };
        seed.deserialize(&mut value_de)
    }
}

struct EnumAccess<'document, 'variant> {
    de: &'variant mut DeserializerFromEvents<'document>,
    name: Option<&'static str>,
    tag: &'document str,
}

impl<'de, 'document, 'variant> de::EnumAccess<'de> for EnumAccess<'document, 'variant> {
    type Error = Error;
    type Variant = DeserializerFromEvents<'variant>;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant)> {
        let str_de = StrDeserializer::<Error>::new(self.tag);
        let variant = seed.deserialize(str_de)?;
        let visitor = DeserializerFromEvents {
            document: self.de.document,
            pos: self.de.pos,
            jumpcount: self.de.jumpcount,
            path: self.de.path,
            remaining_depth: self.de.remaining_depth,
            current_enum: Some(CurrentEnum {
                name: self.name,
                tag: self.tag,
            }),
        };
        Ok((variant, visitor))
    }
}

impl<'de> de::VariantAccess<'de> for DeserializerFromEvents<'_> {
    type Error = Error;

    fn unit_variant(mut self) -> Result<()> {
        Deserialize::deserialize(&mut self)
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(mut self, seed: T) -> Result<T::Value> {
        seed.deserialize(&mut self)
    }

    fn tuple_variant<V: Visitor<'de>>(mut self, _len: usize, visitor: V) -> Result<V::Value> {
        de::Deserializer::deserialize_seq(&mut self, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        mut self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        de::Deserializer::deserialize_struct(&mut self, "", fields, visitor)
    }
}

struct UnitVariantAccess<'document, 'variant> {
    de: &'variant mut DeserializerFromEvents<'document>,
}

impl<'de> de::EnumAccess<'de> for UnitVariantAccess<'_, '_> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self::Variant)> {
        Ok((seed.deserialize(&mut *self.de)?, self))
    }
}

impl<'de> de::VariantAccess<'de> for UnitVariantAccess<'_, '_> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, _seed: T) -> Result<T::Value> {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"newtype variant",
        ))
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, _visitor: V) -> Result<V::Value> {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"tuple variant",
        ))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value> {
        Err(de::Error::invalid_type(
            Unexpected::UnitVariant,
            &"struct variant",
        ))
    }
}

fn visit_scalar<'de, V: Visitor<'de>>(
    visitor: V,
    scalar: &Scalar,
    tagged_already: bool,
) -> Result<V::Value> {
    let v = scalar.value.as_str();
    if let (Some(tag), false) = (&scalar.tag, tagged_already) {
        if tag == TAG_BOOL {
            return match parse_bool(v) {
                Some(v) => visitor.visit_bool(v),
                None => Err(de::Error::invalid_value(Unexpected::Str(v), &"a boolean")),
            };
        } else if tag == TAG_INT {
            return match visit_int(visitor, v) {
                Ok(result) => result,
                Err(_) => Err(de::Error::invalid_value(Unexpected::Str(v), &"an integer")),
            };
        } else if tag == TAG_FLOAT {
            return match parse_f64(v) {
                Some(v) => visitor.visit_f64(v),
                None => Err(de::Error::invalid_value(Unexpected::Str(v), &"a float")),
            };
        } else if tag == TAG_NULL {
            return match parse_null(v) {
                Some(()) => visitor.visit_unit(),
                None => Err(de::Error::invalid_value(Unexpected::Str(v), &"null")),
            };
        } else if tag.starts_with('!') && scalar.style == ScalarStyle::Plain {
            return visit_untagged_scalar(visitor, v);
        }
    } else if scalar.style == ScalarStyle::Plain {
        return visit_untagged_scalar(visitor, v);
    }
    visitor.visit_str(v)
}

fn parse_null(scalar: &str) -> Option<()> {
    match scalar {
        "null" | "Null" | "NULL" | "~" => Some(()),
        _ => None,
    }
}

fn parse_bool(scalar: &str) -> Option<bool> {
    match scalar {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    }
}

type FromStrRadix<T> = fn(&str, u32) -> Result<T, ParseIntError>;

/// Parses `rest` in `radix` unless it carries its own sign.
fn unsigned_radix<T>(rest: &str, radix: u32, from_str_radix: FromStrRadix<T>) -> Option<Option<T>> {
    if rest.starts_with(['+', '-']) {
        return Some(None);
    }
    from_str_radix(rest, radix).ok().map(Some)
}

fn parse_unsigned_int<T>(scalar: &str, from_str_radix: FromStrRadix<T>) -> Option<T> {
    let unpositive = scalar.strip_prefix('+').unwrap_or(scalar);
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = unpositive.strip_prefix(prefix)
            && let Some(int) = unsigned_radix(rest, radix, from_str_radix)
        {
            return int;
        }
    }
    if unpositive.starts_with(['+', '-']) {
        return None;
    }
    if digits_but_not_number(scalar) {
        return None;
    }
    from_str_radix(unpositive, 10).ok()
}

fn parse_signed_int<T>(scalar: &str, from_str_radix: FromStrRadix<T>) -> Option<T> {
    let unpositive = if let Some(unpositive) = scalar.strip_prefix('+') {
        if unpositive.starts_with(['+', '-']) {
            return None;
        }
        unpositive
    } else {
        scalar
    };
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(rest) = unpositive.strip_prefix(prefix)
            && let Some(int) = unsigned_radix(rest, radix, from_str_radix)
        {
            return int;
        }
        if let Some(rest) = scalar
            .strip_prefix('-')
            .and_then(|s| s.strip_prefix(prefix))
            && let Ok(int) = from_str_radix(&format!("-{rest}"), radix)
        {
            return Some(int);
        }
    }
    if digits_but_not_number(scalar) {
        return None;
    }
    from_str_radix(unpositive, 10).ok()
}

fn parse_negative_int<T>(scalar: &str, from_str_radix: FromStrRadix<T>) -> Option<T> {
    for (prefix, radix) in [("-0x", 16), ("-0o", 8), ("-0b", 2)] {
        if let Some(rest) = scalar.strip_prefix(prefix)
            && let Ok(int) = from_str_radix(&format!("-{rest}"), radix)
        {
            return Some(int);
        }
    }
    if digits_but_not_number(scalar) {
        return None;
    }
    from_str_radix(scalar, 10).ok()
}

pub(crate) fn parse_f64(scalar: &str) -> Option<f64> {
    let unpositive = if let Some(unpositive) = scalar.strip_prefix('+') {
        if unpositive.starts_with(['+', '-']) {
            return None;
        }
        unpositive
    } else {
        scalar
    };
    if let ".inf" | ".Inf" | ".INF" = unpositive {
        return Some(f64::INFINITY);
    }
    if let "-.inf" | "-.Inf" | "-.INF" = scalar {
        return Some(f64::NEG_INFINITY);
    }
    if let ".nan" | ".NaN" | ".NAN" = scalar {
        return Some(f64::NAN.copysign(1.0));
    }
    if let Ok(float) = unpositive.parse::<f64>()
        && float.is_finite()
    {
        return Some(float);
    }
    None
}

/// Leading zero(s) followed by digits is a string in YAML 1.2
/// (https://yaml.org/spec/1.2/spec.html#id2761292).
pub(crate) fn digits_but_not_number(scalar: &str) -> bool {
    let scalar = scalar.strip_prefix(['-', '+']).unwrap_or(scalar);
    scalar.len() > 1
        && scalar.starts_with('0')
        && scalar.bytes().skip(1).all(|b| b.is_ascii_digit())
}

pub(crate) fn visit_int<'de, V: Visitor<'de>>(visitor: V, v: &str) -> Result<Result<V::Value>, V> {
    if let Some(int) = parse_unsigned_int(v, u64::from_str_radix) {
        return Ok(visitor.visit_u64(int));
    }
    if let Some(int) = parse_negative_int(v, i64::from_str_radix) {
        return Ok(visitor.visit_i64(int));
    }
    if let Some(int) = parse_unsigned_int(v, u128::from_str_radix) {
        return Ok(visitor.visit_u128(int));
    }
    if let Some(int) = parse_negative_int(v, i128::from_str_radix) {
        return Ok(visitor.visit_i128(int));
    }
    Err(visitor)
}

/// Types a plain scalar: null, boolean, integer, float, else string.
pub(crate) fn visit_untagged_scalar<'de, V: Visitor<'de>>(visitor: V, v: &str) -> Result<V::Value> {
    if v.is_empty() || parse_null(v) == Some(()) {
        return visitor.visit_unit();
    }
    if let Some(boolean) = parse_bool(v) {
        return visitor.visit_bool(boolean);
    }
    let visitor = match visit_int(visitor, v) {
        Ok(result) => return result,
        Err(visitor) => visitor,
    };
    if !digits_but_not_number(v)
        && let Some(float) = parse_f64(v)
    {
        return visitor.visit_f64(float);
    }
    visitor.visit_str(v)
}

fn is_plain_or_tagged_literal_scalar(
    expected: &str,
    scalar: &Scalar,
    tagged_already: bool,
) -> bool {
    match (scalar.style, &scalar.tag, tagged_already) {
        (ScalarStyle::Plain, _, _) => true,
        (ScalarStyle::Literal, Some(tag), false) => tag == expected,
        _ => false,
    }
}

fn invalid_type(event: &Event, exp: &dyn Expected) -> Error {
    enum Void {}

    struct InvalidType<'a> {
        exp: &'a dyn Expected,
    }

    impl Visitor<'_> for InvalidType<'_> {
        type Value = Void;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.exp.fmt(f)
        }
    }

    match event {
        Event::Alias(_) => de::Error::custom("unexpected alias"),
        Event::Scalar(scalar) => {
            let get_type = InvalidType { exp };
            match visit_scalar(get_type, scalar, false) {
                Ok(void) => match void {},
                Err(invalid_type) => invalid_type,
            }
        }
        Event::SequenceStart(_) => de::Error::invalid_type(Unexpected::Seq, exp),
        Event::MappingStart(_) => de::Error::invalid_type(Unexpected::Map, exp),
        Event::SequenceEnd => unexpected_end("sequence"),
        Event::MappingEnd => unexpected_end("mapping"),
        Event::Void => error::new(ErrorImpl::EndOfStream),
    }
}

/// The variant name of a node tagged `!name`; `None` for untagged nodes and
/// global tags (`!!str`, `!<tag:…>`).
fn parse_tag(tag: &Option<String>) -> Option<&str> {
    let tag = tag.as_deref()?;
    let rest = tag.strip_prefix('!')?;
    Some(if rest.is_empty() { tag } else { rest })
}

impl<'de> de::Deserializer<'de> for &mut DeserializerFromEvents<'_> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        let enum_tag =
            |tag: &'_ Option<String>| -> bool { !tagged_already && parse_tag(tag).is_some() };
        let result = match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_any(visitor)
            }
            Event::Scalar(Scalar { tag, .. })
            | Event::SequenceStart(tag)
            | Event::MappingStart(tag)
                if enum_tag(tag) =>
            {
                let tag = parse_tag(tag).unwrap_or_default();
                self.unread();
                visitor.visit_enum(EnumAccess {
                    de: self,
                    name: None,
                    tag,
                })
            }
            Event::Scalar(scalar) => visit_scalar(visitor, scalar, tagged_already),
            Event::SequenceStart(_) => self.visit_sequence(visitor, mark),
            Event::MappingStart(_) => self.visit_mapping(visitor, mark),
            Event::SequenceEnd => Err(unexpected_end("sequence")),
            Event::MappingEnd => Err(unexpected_end("mapping")),
            Event::Void => visitor.visit_none(),
        };
        // The de::Error impl creates errors with unknown line and column. Fill
        // in the position here by looking at the current index in the input.
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        let result = match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_bool(visitor)
            }
            Event::Scalar(scalar)
                if is_plain_or_tagged_literal_scalar(TAG_BOOL, scalar, tagged_already) =>
            {
                match parse_bool(&scalar.value) {
                    Some(boolean) => visitor.visit_bool(boolean),
                    None => Err(invalid_type(next, &visitor)),
                }
            }
            _ => Err(invalid_type(next, &visitor)),
        };
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        let result = match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_i64(visitor)
            }
            Event::Scalar(scalar)
                if is_plain_or_tagged_literal_scalar(TAG_INT, scalar, tagged_already) =>
            {
                match parse_signed_int(&scalar.value, i64::from_str_radix) {
                    Some(int) => visitor.visit_i64(int),
                    None => Err(invalid_type(next, &visitor)),
                }
            }
            _ => Err(invalid_type(next, &visitor)),
        };
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_i128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        let result = match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_i128(visitor)
            }
            Event::Scalar(scalar)
                if is_plain_or_tagged_literal_scalar(TAG_INT, scalar, tagged_already) =>
            {
                match parse_signed_int(&scalar.value, i128::from_str_radix) {
                    Some(int) => visitor.visit_i128(int),
                    None => Err(invalid_type(next, &visitor)),
                }
            }
            _ => Err(invalid_type(next, &visitor)),
        };
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_u64(visitor)
    }

    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_u64(visitor)
    }

    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_u64(visitor)
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        let result = match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_u64(visitor)
            }
            Event::Scalar(scalar)
                if is_plain_or_tagged_literal_scalar(TAG_INT, scalar, tagged_already) =>
            {
                match parse_unsigned_int(&scalar.value, u64::from_str_radix) {
                    Some(int) => visitor.visit_u64(int),
                    None => Err(invalid_type(next, &visitor)),
                }
            }
            _ => Err(invalid_type(next, &visitor)),
        };
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_u128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        let result = match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_u128(visitor)
            }
            Event::Scalar(scalar)
                if is_plain_or_tagged_literal_scalar(TAG_INT, scalar, tagged_already) =>
            {
                match parse_unsigned_int(&scalar.value, u128::from_str_radix) {
                    Some(int) => visitor.visit_u128(int),
                    None => Err(invalid_type(next, &visitor)),
                }
            }
            _ => Err(invalid_type(next, &visitor)),
        };
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_f64(visitor)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        let result = match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_f64(visitor)
            }
            Event::Scalar(scalar)
                if is_plain_or_tagged_literal_scalar(TAG_FLOAT, scalar, tagged_already) =>
            {
                match parse_f64(&scalar.value) {
                    Some(float) => visitor.visit_f64(float),
                    None => Err(invalid_type(next, &visitor)),
                }
            }
            _ => Err(invalid_type(next, &visitor)),
        };
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let (next, mark) = self.next_event_mark()?;
        let result = match next {
            Event::Scalar(scalar) => visitor.visit_str(&scalar.value),
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_str(visitor)
            }
            other => Err(invalid_type(other, &visitor)),
        };
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(error::new(ErrorImpl::BytesUnsupported))
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(error::new(ErrorImpl::BytesUnsupported))
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let is_some = match self.peek_event()? {
            Event::Alias(pos) => {
                let mut pos = *pos;
                *self.pos = self.pos.saturating_add(1);
                return self.jump(&mut pos)?.deserialize_option(visitor);
            }
            Event::Scalar(scalar) => {
                let tagged_already = self.current_enum.is_some();
                if scalar.style != ScalarStyle::Plain {
                    true
                } else if let (Some(tag), false) = (&scalar.tag, tagged_already) {
                    if tag == TAG_NULL {
                        if let Some(()) = parse_null(&scalar.value) {
                            false
                        } else {
                            return Err(de::Error::invalid_value(
                                Unexpected::Str(&scalar.value),
                                &"null",
                            ));
                        }
                    } else {
                        true
                    }
                } else {
                    !scalar.value.is_empty() && parse_null(&scalar.value).is_none()
                }
            }
            Event::SequenceStart(_) | Event::MappingStart(_) => true,
            Event::SequenceEnd => return Err(unexpected_end("sequence")),
            Event::MappingEnd => return Err(unexpected_end("mapping")),
            Event::Void => false,
        };
        if is_some {
            visitor.visit_some(self)
        } else {
            *self.pos = self.pos.saturating_add(1);
            self.current_enum = None;
            visitor.visit_none()
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let tagged_already = self.current_enum.is_some();
        let (next, mark) = self.next_event_mark()?;
        let result = match next {
            Event::Scalar(scalar) => {
                let is_null = if scalar.style != ScalarStyle::Plain {
                    false
                } else if let (Some(tag), false) = (&scalar.tag, tagged_already) {
                    tag == TAG_NULL && parse_null(&scalar.value).is_some()
                } else {
                    scalar.value.is_empty() || parse_null(&scalar.value).is_some()
                };
                if is_null {
                    visitor.visit_unit()
                } else {
                    Err(de::Error::invalid_value(
                        Unexpected::Str(&scalar.value),
                        &"null",
                    ))
                }
            }
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_unit(visitor)
            }
            Event::Void => visitor.visit_unit(),
            other => Err(invalid_type(other, &visitor)),
        };
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        let (_event, mark) = self.peek_event_mark()?;
        self.recursion_check(mark, |de| visitor.visit_newtype_struct(de))
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let (next, mark) = self.next_event_mark()?;
        let result = match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_seq(visitor)
            }
            Event::SequenceStart(_) => self.visit_sequence(visitor, mark),
            other => {
                if is_empty_node(other) {
                    visitor.visit_seq(SeqAccess {
                        empty: true,
                        de: self,
                        len: 0,
                    })
                } else {
                    Err(invalid_type(other, &visitor))
                }
            }
        };
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let (next, mark) = self.next_event_mark()?;
        let result = match next {
            Event::Alias(pos) => {
                let mut pos = *pos;
                self.jump(&mut pos)?.deserialize_map(visitor)
            }
            Event::MappingStart(_) => self.visit_mapping(visitor, mark),
            other => {
                if is_empty_node(other) {
                    visitor.visit_map(MapAccess {
                        empty: true,
                        de: self,
                        len: 0,
                        key: None,
                    })
                } else {
                    Err(invalid_type(other, &visitor))
                }
            }
        };
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let (next, mark) = self.peek_event_mark()?;
        let result = if let Some(current_enum) = self.current_enum {
            if let Event::Scalar(scalar) = next
                && !scalar.value.is_empty()
            {
                visitor.visit_enum(UnitVariantAccess { de: self })
            } else {
                let message = if let Some(name) = current_enum.name {
                    format!(
                        "deserializing nested enum in {name}::{} from YAML is not supported yet",
                        current_enum.tag,
                    )
                } else {
                    format!(
                        "deserializing nested enum in !{} from YAML is not supported yet",
                        current_enum.tag,
                    )
                };
                Err(error::new(ErrorImpl::Message(message, None)))
            }
        } else {
            match next {
                Event::Alias(pos) => {
                    let mut pos = *pos;
                    *self.pos = self.pos.saturating_add(1);
                    self.jump(&mut pos)?
                        .deserialize_enum(name, variants, visitor)
                }
                Event::Scalar(scalar) => {
                    if let Some(tag) = parse_tag(&scalar.tag) {
                        return visitor.visit_enum(EnumAccess {
                            de: self,
                            name: Some(name),
                            tag,
                        });
                    }
                    visitor.visit_enum(UnitVariantAccess { de: self })
                }
                Event::MappingStart(tag) | Event::SequenceStart(tag) => {
                    if let Some(tag) = parse_tag(tag) {
                        return visitor.visit_enum(EnumAccess {
                            de: self,
                            name: Some(name),
                            tag,
                        });
                    }
                    let unexpected = if matches!(next, Event::MappingStart(_)) {
                        Unexpected::Map
                    } else {
                        Unexpected::Seq
                    };
                    let err = de::Error::invalid_type(unexpected, &"a YAML tag starting with '!'");
                    Err(error::fix_mark(err, mark, self.path))
                }
                Event::SequenceEnd => Err(unexpected_end("sequence")),
                Event::MappingEnd => Err(unexpected_end("mapping")),
                Event::Void => Err(error::new(ErrorImpl::EndOfStream)),
            }
        };
        result.map_err(|err| error::fix_mark(err, mark, self.path))
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.ignore_any()?;
        visitor.visit_unit()
    }
}

/// An absent value or an empty plain scalar, which reads as an empty
/// sequence or mapping.
fn is_empty_node(event: &Event) -> bool {
    match event {
        Event::Void => true,
        Event::Scalar(scalar) => scalar.value.is_empty() && scalar.style == ScalarStyle::Plain,
        _ => false,
    }
}
