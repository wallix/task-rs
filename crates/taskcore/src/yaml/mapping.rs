//! A YAML mapping: insertion-ordered keys and values.

use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::fmt::{self, Display};
use std::hash::{Hash, Hasher};
use std::mem;

use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize};

use super::Value;

/// A YAML mapping in which the keys and values are both `Value`.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct Mapping {
    map: IndexMap<Value, Value>,
}

impl Mapping {
    /// Creates an empty mapping.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a key-value pair; an existing key keeps its position and the
    /// old value is returned.
    pub fn insert(&mut self, k: Value, v: Value) -> Option<Value> {
        self.map.insert(k, v)
    }

    /// The value for `index`, if present.
    pub fn get<I: Index>(&self, index: I) -> Option<&Value> {
        index.index_into(self)
    }

    /// The entry for `k`.
    pub(crate) fn entry(&mut self, k: Value) -> Entry<'_> {
        match self.map.entry(k) {
            indexmap::map::Entry::Occupied(occupied) => Entry::Occupied(OccupiedEntry { occupied }),
            indexmap::map::Entry::Vacant(vacant) => Entry::Vacant(VacantEntry { vacant }),
        }
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the mapping has no entries.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Iterates over the entries in insertion order.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            iter: self.map.iter(),
        }
    }
}

/// A type that can be used to index into a `Mapping`: a `Value`, or a string
/// matching a `Value::String` key.
pub trait Index: private::Sealed {
    #[doc(hidden)]
    fn index_into<'a>(&self, v: &'a Mapping) -> Option<&'a Value>;
}

mod private {
    pub trait Sealed {}
    impl Sealed for super::Value {}
    impl Sealed for str {}
    impl Sealed for String {}
    impl<T: ?Sized + Sealed> Sealed for &T {}
}

struct HashLikeValue<'a>(&'a str);

impl indexmap::Equivalent<Value> for HashLikeValue<'_> {
    fn equivalent(&self, key: &Value) -> bool {
        match key {
            Value::String(string) => self.0 == string,
            _ => false,
        }
    }
}

// NOTE: This impl must be kept consistent with Value's Hash impl.
impl Hash for HashLikeValue<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        const STRING: Value = Value::String(String::new());
        mem::discriminant(&STRING).hash(state);
        self.0.hash(state);
    }
}

impl Index for Value {
    fn index_into<'a>(&self, v: &'a Mapping) -> Option<&'a Value> {
        v.map.get(self)
    }
}

impl Index for str {
    fn index_into<'a>(&self, v: &'a Mapping) -> Option<&'a Value> {
        v.map.get(&HashLikeValue(self))
    }
}

impl Index for String {
    fn index_into<'a>(&self, v: &'a Mapping) -> Option<&'a Value> {
        self.as_str().index_into(v)
    }
}

impl<T: ?Sized + Index> Index for &T {
    fn index_into<'a>(&self, v: &'a Mapping) -> Option<&'a Value> {
        (**self).index_into(v)
    }
}

// Order-insensitive, consistent with IndexMap's order-insensitive Eq.
impl Hash for Mapping {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut xor = 0;
        for (k, v) in self {
            let mut hasher = DefaultHasher::new();
            k.hash(&mut hasher);
            v.hash(&mut hasher);
            xor ^= hasher.finish();
        }
        xor.hash(state);
    }
}

impl PartialOrd for Mapping {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let mut self_entries = Vec::from_iter(self);
        let mut other_entries = Vec::from_iter(other);

        // Sort in an arbitrary order that is consistent with Value's PartialOrd
        // impl.
        fn total_cmp(a: &Value, b: &Value) -> Ordering {
            match (a, b) {
                (Value::Null, Value::Null) => Ordering::Equal,
                (Value::Null, _) => Ordering::Less,
                (_, Value::Null) => Ordering::Greater,

                (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
                (Value::Bool(_), _) => Ordering::Less,
                (_, Value::Bool(_)) => Ordering::Greater,

                (Value::Number(a), Value::Number(b)) => a.total_cmp(b),
                (Value::Number(_), _) => Ordering::Less,
                (_, Value::Number(_)) => Ordering::Greater,

                (Value::String(a), Value::String(b)) => a.cmp(b),
                (Value::String(_), _) => Ordering::Less,
                (_, Value::String(_)) => Ordering::Greater,

                (Value::Sequence(a), Value::Sequence(b)) => iter_cmp_by(a, b, total_cmp),
                (Value::Sequence(_), _) => Ordering::Less,
                (_, Value::Sequence(_)) => Ordering::Greater,

                (Value::Mapping(a), Value::Mapping(b)) => {
                    iter_cmp_by(a, b, |(ak, av), (bk, bv)| {
                        total_cmp(ak, bk).then_with(|| total_cmp(av, bv))
                    })
                }
                (Value::Mapping(_), _) => Ordering::Less,
                (_, Value::Mapping(_)) => Ordering::Greater,

                (Value::Tagged(a), Value::Tagged(b)) => a
                    .tag
                    .cmp(&b.tag)
                    .then_with(|| total_cmp(&a.value, &b.value)),
            }
        }

        fn iter_cmp_by<I, F>(this: I, other: I, mut cmp: F) -> Ordering
        where
            I: IntoIterator,
            F: FnMut(I::Item, I::Item) -> Ordering,
        {
            let mut this = this.into_iter();
            let mut other = other.into_iter();

            loop {
                let x = match this.next() {
                    None => {
                        if other.next().is_none() {
                            return Ordering::Equal;
                        } else {
                            return Ordering::Less;
                        }
                    }
                    Some(val) => val,
                };

                let y = match other.next() {
                    None => return Ordering::Greater,
                    Some(val) => val,
                };

                match cmp(x, y) {
                    Ordering::Equal => {}
                    non_eq => return non_eq,
                }
            }
        }

        // While sorting by map key, we get to assume that no two keys are
        // equal, otherwise they wouldn't both be in the map. This is not a safe
        // assumption outside of this situation.
        let total_cmp = |&(a, _): &_, &(b, _): &_| total_cmp(a, b);
        self_entries.sort_by(total_cmp);
        other_entries.sort_by(total_cmp);
        self_entries.partial_cmp(&other_entries)
    }
}

impl Extend<(Value, Value)> for Mapping {
    fn extend<I: IntoIterator<Item = (Value, Value)>>(&mut self, iter: I) {
        self.map.extend(iter);
    }
}

impl FromIterator<(Value, Value)> for Mapping {
    fn from_iter<I: IntoIterator<Item = (Value, Value)>>(iter: I) -> Self {
        Mapping {
            map: IndexMap::from_iter(iter),
        }
    }
}

macro_rules! delegate_iterator {
    (($name:ident $($generics:tt)*) => $item:ty) => {
        impl $($generics)* Iterator for $name $($generics)* {
            type Item = $item;
            fn next(&mut self) -> Option<Self::Item> {
                self.iter.next()
            }
            fn size_hint(&self) -> (usize, Option<usize>) {
                self.iter.size_hint()
            }
        }

        impl $($generics)* ExactSizeIterator for $name $($generics)* {
            fn len(&self) -> usize {
                self.iter.len()
            }
        }

        impl $($generics)* DoubleEndedIterator for $name $($generics)* {
            fn next_back(&mut self) -> Option<Self::Item> {
                self.iter.next_back()
            }
        }
    }
}

/// Iterator over `&Mapping`.
pub struct Iter<'a> {
    iter: indexmap::map::Iter<'a, Value, Value>,
}

delegate_iterator!((Iter<'a>) => (&'a Value, &'a Value));

impl<'a> IntoIterator for &'a Mapping {
    type Item = (&'a Value, &'a Value);
    type IntoIter = Iter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        Iter {
            iter: self.map.iter(),
        }
    }
}

/// Iterator over `Mapping` by value.
pub struct IntoIter {
    iter: indexmap::map::IntoIter<Value, Value>,
}

delegate_iterator!((IntoIter) => (Value, Value));

impl IntoIterator for Mapping {
    type Item = (Value, Value);
    type IntoIter = IntoIter;
    fn into_iter(self) -> Self::IntoIter {
        IntoIter {
            iter: self.map.into_iter(),
        }
    }
}

/// Entry for an existing key-value pair or a vacant location to insert one.
pub(crate) enum Entry<'a> {
    /// Existing slot with equivalent key.
    Occupied(OccupiedEntry<'a>),
    /// Vacant slot (no equivalent key in the map).
    Vacant(VacantEntry<'a>),
}

/// A view into an occupied entry in a [`Mapping`].
pub(crate) struct OccupiedEntry<'a> {
    occupied: indexmap::map::OccupiedEntry<'a, Value, Value>,
}

/// A view into a vacant entry in a [`Mapping`].
pub(crate) struct VacantEntry<'a> {
    vacant: indexmap::map::VacantEntry<'a, Value, Value>,
}

impl OccupiedEntry<'_> {
    /// The key in the entry.
    pub fn key(&self) -> &Value {
        self.occupied.key()
    }
}

impl<'a> VacantEntry<'a> {
    /// Sets the value of the entry, returning a mutable reference to it.
    pub fn insert(self, value: Value) -> &'a mut Value {
        self.vacant.insert(value)
    }
}

impl Serialize for Mapping {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map_serializer = serializer.serialize_map(Some(self.len()))?;
        for (k, v) in self {
            map_serializer.serialize_entry(k, v)?;
        }
        map_serializer.end()
    }
}

impl<'de> Deserialize<'de> for Mapping {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Mapping;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a YAML mapping")
            }

            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(Mapping::new())
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut data: A,
            ) -> Result<Self::Value, A::Error> {
                let mut mapping = Mapping::new();
                while let Some(key) = data.next_key()? {
                    match mapping.entry(key) {
                        Entry::Occupied(entry) => {
                            return Err(serde::de::Error::custom(DuplicateKeyError { entry }));
                        }
                        Entry::Vacant(entry) => {
                            let value = data.next_value()?;
                            entry.insert(value);
                        }
                    }
                }
                Ok(mapping)
            }
        }

        deserializer.deserialize_map(Visitor)
    }
}

struct DuplicateKeyError<'a> {
    entry: OccupiedEntry<'a>,
}

impl Display for DuplicateKeyError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("duplicate entry ")?;
        match self.entry.key() {
            Value::Null => f.write_str("with null key"),
            Value::Bool(boolean) => write!(f, "with key `{boolean}`"),
            Value::Number(number) => write!(f, "with key {number}"),
            Value::String(string) => write!(f, "with key {string:?}"),
            Value::Sequence(_) | Value::Mapping(_) | Value::Tagged(_) => f.write_str("in YAML map"),
        }
    }
}
