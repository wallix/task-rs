//! The `Value` enum, a loosely typed way of representing any YAML value.

use std::fmt::{self, Debug, Display};
use std::hash::{Hash, Hasher};
use std::mem;

use super::mapping::Mapping;
use super::number::Number;
use super::tagged::TaggedValue;

/// Represents any valid YAML value.
#[derive(Clone, PartialEq, PartialOrd, Default)]
pub enum Value {
    /// Represents a YAML null value.
    #[default]
    Null,
    /// Represents a YAML boolean.
    Bool(bool),
    /// Represents a YAML numerical value, whether integer or floating point.
    Number(Number),
    /// Represents a YAML string.
    String(String),
    /// Represents a YAML sequence in which the elements are `Value`.
    Sequence(Sequence),
    /// Represents a YAML mapping in which the keys and values are both `Value`.
    Mapping(Mapping),
    /// A representation of YAML's `!Tag` syntax, used for enums.
    Tagged(Box<TaggedValue>),
}

/// A YAML sequence in which the elements are `Value`.
pub type Sequence = Vec<Value>;

impl Value {
    /// The string, if the value is one (seeing through tags).
    pub fn as_str(&self) -> Option<&str> {
        match self.untag_ref() {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn untag(self) -> Self {
        let mut cur = self;
        while let Value::Tagged(tagged) = cur {
            cur = tagged.value;
        }
        cur
    }

    pub(crate) fn untag_ref(&self) -> &Self {
        let mut cur = self;
        while let Value::Tagged(tagged) = cur {
            cur = &tagged.value;
        }
        cur
    }
}

impl Eq for Value {}

// NOTE: This impl must be kept consistent with HashLikeValue's Hash impl in
// mapping.rs in order for value[str] indexing to work.
impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        mem::discriminant(self).hash(state);
        match self {
            Value::Null => {}
            Value::Bool(v) => v.hash(state),
            Value::Number(v) => v.hash(state),
            Value::String(v) => v.hash(state),
            Value::Sequence(v) => v.hash(state),
            Value::Mapping(v) => v.hash(state),
            Value::Tagged(v) => v.hash(state),
        }
    }
}

impl Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("Null"),
            Value::Bool(boolean) => write!(f, "Bool({boolean})"),
            Value::Number(number) => write!(f, "Number({number})"),
            Value::String(string) => write!(f, "String({string:?})"),
            Value::Sequence(sequence) => {
                f.write_str("Sequence ")?;
                f.debug_list().entries(sequence).finish()
            }
            Value::Mapping(mapping) => Debug::fmt(mapping, f),
            Value::Tagged(tagged) => Debug::fmt(tagged, f),
        }
    }
}

struct DisplayNumber<'a>(&'a Number);

impl Debug for DisplayNumber<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(self.0, f)
    }
}

impl Debug for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Number({self})")
    }
}

impl Debug for Mapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Mapping ")?;
        let mut debug = f.debug_map();
        for (k, v) in self {
            let tmp;
            debug.entry(
                match k {
                    Value::Bool(boolean) => boolean,
                    Value::Number(number) => {
                        tmp = DisplayNumber(number);
                        &tmp
                    }
                    Value::String(string) => string,
                    _ => k,
                },
                v,
            );
        }
        debug.finish()
    }
}

macro_rules! from_number {
    ($($ty:ident)*) => {
        $(
            impl From<$ty> for Value {
                fn from(n: $ty) -> Self {
                    Value::Number(n.into())
                }
            }
        )*
    };
}

from_number! {
    i8 i16 i32 i64
    u8 u16 u32 u64 usize
    f64
}
