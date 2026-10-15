//! A YAML number: an unsigned, negative or floating-point value.

use std::cmp::Ordering;
use std::fmt::{self, Display};
use std::hash::{Hash, Hasher};

use serde::de::{Unexpected, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer, forward_to_deserialize_any};

use super::error::Error;

/// Represents a YAML number, whether integer or floating point.
#[derive(Clone, PartialEq, PartialOrd)]
pub struct Number {
    n: N,
}

#[derive(Copy, Clone)]
enum N {
    PosInt(u64),
    /// Always less than zero.
    NegInt(i64),
    /// May be infinite or NaN.
    Float(f64),
}

impl Number {
    /// The number as an `i64`, if it is an integer that fits.
    pub fn as_i64(&self) -> Option<i64> {
        match self.n {
            N::PosInt(n) => i64::try_from(n).ok(),
            N::NegInt(n) => Some(n),
            N::Float(_) => None,
        }
    }

    pub(crate) fn total_cmp(&self, other: &Self) -> Ordering {
        self.n.total_cmp(&other.n)
    }

    pub(crate) fn unexpected(&self) -> Unexpected<'_> {
        match self.n {
            N::PosInt(u) => Unexpected::Unsigned(u),
            N::NegInt(i) => Unexpected::Signed(i),
            N::Float(f) => Unexpected::Float(f),
        }
    }
}

impl Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.n {
            N::PosInt(i) => f.write_str(itoa::Buffer::new().format(i)),
            N::NegInt(i) => f.write_str(itoa::Buffer::new().format(i)),
            N::Float(x) if x.is_nan() => f.write_str(".nan"),
            N::Float(x) if x.is_infinite() => {
                if x.is_sign_negative() {
                    f.write_str("-.inf")
                } else {
                    f.write_str(".inf")
                }
            }
            N::Float(x) => f.write_str(ryu::Buffer::new().format_finite(x)),
        }
    }
}

impl PartialEq for N {
    fn eq(&self, other: &N) -> bool {
        match (*self, *other) {
            (N::PosInt(a), N::PosInt(b)) => a == b,
            (N::NegInt(a), N::NegInt(b)) => a == b,
            (N::Float(a), N::Float(b)) => {
                if a.is_nan() && b.is_nan() {
                    // YAML only has one NaN; the bit representation isn't
                    // relevant.
                    true
                } else {
                    a == b
                }
            }
            _ => false,
        }
    }
}

impl PartialOrd for N {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (*self, *other) {
            (N::Float(a), N::Float(b)) => {
                if a.is_nan() && b.is_nan() {
                    Some(Ordering::Equal)
                } else {
                    a.partial_cmp(&b)
                }
            }
            _ => Some(self.total_cmp(other)),
        }
    }
}

impl N {
    fn total_cmp(&self, other: &Self) -> Ordering {
        match (*self, *other) {
            (N::PosInt(a), N::PosInt(b)) => a.cmp(&b),
            (N::NegInt(a), N::NegInt(b)) => a.cmp(&b),
            (N::NegInt(_), N::PosInt(_)) => Ordering::Less,
            (N::PosInt(_), N::NegInt(_)) => Ordering::Greater,
            (N::Float(a), N::Float(b)) => a.partial_cmp(&b).unwrap_or_else(|| {
                // arbitrarily sort the NaN last
                if !a.is_nan() {
                    Ordering::Less
                } else if !b.is_nan() {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            }),
            // arbitrarily sort integers below floats
            (_, N::Float(_)) => Ordering::Less,
            (N::Float(_), _) => Ordering::Greater,
        }
    }
}

// Every float hashes alike, keeping Hash consistent with the NaN-aware Eq.
impl Hash for Number {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self.n {
            N::Float(_) => 3.hash(state),
            N::PosInt(u) => u.hash(state),
            N::NegInt(i) => i.hash(state),
        }
    }
}

impl Serialize for Number {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.n {
            N::PosInt(i) => serializer.serialize_u64(i),
            N::NegInt(i) => serializer.serialize_i64(i),
            N::Float(f) => serializer.serialize_f64(f),
        }
    }
}

struct NumberVisitor;

impl Visitor<'_> for NumberVisitor {
    type Value = Number;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a number")
    }

    fn visit_i64<E>(self, value: i64) -> Result<Number, E> {
        Ok(value.into())
    }

    fn visit_u64<E>(self, value: u64) -> Result<Number, E> {
        Ok(value.into())
    }

    fn visit_f64<E>(self, value: f64) -> Result<Number, E> {
        Ok(value.into())
    }
}

impl<'de> Deserialize<'de> for Number {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Number, D::Error> {
        deserializer.deserialize_any(NumberVisitor)
    }
}

impl<'de> Deserializer<'de> for &Number {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.n {
            N::PosInt(i) => visitor.visit_u64(i),
            N::NegInt(i) => visitor.visit_i64(i),
            N::Float(f) => visitor.visit_f64(f),
        }
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

macro_rules! from_signed {
    ($($signed_ty:ident)*) => {
        $(
            impl From<$signed_ty> for Number {
                fn from(i: $signed_ty) -> Self {
                    match u64::try_from(i) {
                        Ok(u) => Number { n: N::PosInt(u) },
                        Err(_) => Number { n: N::NegInt(i64::from(i)) },
                    }
                }
            }
        )*
    };
}

macro_rules! from_unsigned {
    ($($unsigned_ty:ident)*) => {
        $(
            impl From<$unsigned_ty> for Number {
                fn from(u: $unsigned_ty) -> Self {
                    Number { n: N::PosInt(u64::from(u)) }
                }
            }
        )*
    };
}

from_signed!(i8 i16 i32 i64);
from_unsigned!(u8 u16 u32 u64);

impl From<usize> for Number {
    fn from(u: usize) -> Self {
        // usize is at most 64 bits on every supported target.
        Number {
            n: N::PosInt(u64::try_from(u).unwrap_or(u64::MAX)),
        }
    }
}

impl From<f64> for Number {
    fn from(mut f: f64) -> Self {
        if f.is_nan() {
            // Destroy NaN sign, signaling, and payload. YAML only has one NaN.
            f = f64::NAN.copysign(1.0);
        }
        Number { n: N::Float(f) }
    }
}
