//! YAML's `!Tag` syntax: a tag attached to a value.

use std::cmp::Ordering;
use std::fmt::{self, Debug, Display};
use std::hash::{Hash, Hasher};

use serde::de::value::StrDeserializer;
use serde::de::{DeserializeSeed, Deserializer, EnumAccess, Error as _, VariantAccess, Visitor};
use serde::forward_to_deserialize_any;
use serde::ser::{Serialize, SerializeMap, Serializer};

use super::error::Error;
use super::value::Value;
use super::value_de::{MapDeserializer, SeqDeserializer};

/// A YAML tag, compared and displayed without its leading `!`-ambiguity:
/// `Tag::new("foo")` and `Tag::new("!foo")` are equal and both display `!foo`.
#[derive(Clone)]
pub struct Tag {
    pub(crate) string: String,
}

/// A `Tag` + `Value` representing a tagged YAML scalar, sequence, or mapping.
#[derive(Clone, PartialEq, PartialOrd, Hash, Debug)]
pub struct TaggedValue {
    /// The tag.
    pub tag: Tag,
    /// The value it applies to.
    pub value: Value,
}

impl Tag {
    /// Creates a tag, with or without its leading `!`.
    pub fn new(string: impl Into<String>) -> Self {
        Tag {
            string: string.into(),
        }
    }
}

pub(crate) fn nobang(maybe_banged: &str) -> &str {
    match maybe_banged.strip_prefix('!') {
        Some("") | None => maybe_banged,
        Some(unbanged) => unbanged,
    }
}

impl Eq for Tag {}

impl PartialEq for Tag {
    fn eq(&self, other: &Tag) -> bool {
        PartialEq::eq(nobang(&self.string), nobang(&other.string))
    }
}

impl<T: ?Sized + AsRef<str>> PartialEq<T> for Tag {
    fn eq(&self, other: &T) -> bool {
        PartialEq::eq(nobang(&self.string), nobang(other.as_ref()))
    }
}

impl Ord for Tag {
    fn cmp(&self, other: &Self) -> Ordering {
        Ord::cmp(nobang(&self.string), nobang(&other.string))
    }
}

impl PartialOrd for Tag {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for Tag {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        nobang(&self.string).hash(hasher);
    }
}

impl Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "!{}", nobang(&self.string))
    }
}

impl Debug for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(self, f)
    }
}

/// Serializes as a one-entry map `{"!tag": value}`.
impl Serialize for TaggedValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct SerializeTag<'a>(&'a Tag);

        impl Serialize for SerializeTag<'_> {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self.0)
            }
        }

        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(&SerializeTag(&self.tag), &self.value)?;
        map.end()
    }
}

impl<'de> Deserializer<'de> for TaggedValue {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_enum(self)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        drop(self);
        visitor.visit_unit()
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes
        byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct
        map struct enum identifier
    }
}

impl<'de> EnumAccess<'de> for TaggedValue {
    type Error = Error;
    type Variant = Value;

    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Error> {
        let tag = StrDeserializer::<Error>::new(nobang(&self.tag.string));
        let value = seed.deserialize(tag)?;
        Ok((value, self.value))
    }
}

impl<'de> VariantAccess<'de> for Value {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        serde::Deserialize::deserialize(self)
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, Error> {
        seed.deserialize(self)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, Error> {
        if let Value::Sequence(v) = self {
            Deserializer::deserialize_any(SeqDeserializer::new(v), visitor)
        } else {
            Err(Error::invalid_type(self.unexpected(), &"tuple variant"))
        }
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        if let Value::Mapping(v) = self {
            Deserializer::deserialize_any(MapDeserializer::new(v), visitor)
        } else {
            Err(Error::invalid_type(self.unexpected(), &"struct variant"))
        }
    }
}

pub(crate) struct TagStringVisitor;

impl Visitor<'_> for TagStringVisitor {
    type Value = Tag;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a YAML tag string")
    }

    fn visit_str<E: serde::de::Error>(self, string: &str) -> Result<Self::Value, E> {
        self.visit_string(string.to_owned())
    }

    fn visit_string<E: serde::de::Error>(self, string: String) -> Result<Self::Value, E> {
        if string.is_empty() {
            return Err(E::custom("empty YAML tag is not allowed"));
        }
        Ok(Tag::new(string))
    }
}

impl<'de> DeserializeSeed<'de> for TagStringVisitor {
    type Value = Tag;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_string(self)
    }
}
