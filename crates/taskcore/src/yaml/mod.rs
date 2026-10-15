//! YAML reading and writing for Taskfiles.
//!
//! A port of serde_yaml_ng 0.10 (itself a fork of dtolnay's serde_yaml) onto
//! granit-parser, a safe-Rust YAML parser, replacing libyaml (`unsafe-libyaml`,
//! unmaintained). The value model, the typing of plain scalars, decode errors
//! and their positions, and the emitter output are kept as serde_yaml_ng had
//! them: the decoded values feed fingerprint checksums, so they must not
//! drift. YAML syntax errors come from granit-parser, so they are worded and
//! sometimes positioned differently: an unclosed `[` or `{` is reported at
//! the bracket rather than where libyaml gave up, for one.
//!
//! The code is derived from serde_yaml_ng 0.10.0 (MIT,
//! <https://github.com/acatton/serde-yaml-ng>), Copyright 2016-2024 David
//! Tolnay and 2024 Antoine Catton, and modified; the emitter and input checks
//! follow libyaml (MIT), Copyright 2006-2016 Kirill Simonov and 2017-2020
//! Ingy döt Net, through its Rust translation unsafe-libyaml (MIT, David
//! Tolnay). Their licence notices are in `THIRD-PARTY-NOTICES`.
//!
//! Callers rely on this contract, pinned by `tests/yaml_pins.rs` and the
//! fingerprint tests in `hash.rs`:
//!
//! - [`Value`] has the variants `Null`, `Bool`, `Number`, `String`,
//!   `Sequence`, `Mapping` and `Tagged` (a boxed [`TaggedValue`] whose
//!   [`Tag`] displays with its leading `!`), and implements `Deserialize`,
//!   `Serialize`, `Hash` and `Eq`. Plain scalars resolve to the same variant
//!   and the same [`Number`] as serde_yaml_ng 0.10 gives them.
//! - [`Number`] converts `From<i32>`, and its `Display` text (`1.0`, `.inf`,
//!   `-0.0`, …) is what fingerprints hash. Templates render its `Serialize`
//!   form instead (`inf`, `NaN`).
//! - [`Mapping`] keeps insertion order, `insert` replaces a key in place,
//!   `get` takes a `&str` key, and any `Value` can be a key.
//! - [`from_value`] decodes a `Value` into the AST types; its errors carry
//!   neither the path nor the position of the offending value.
//! - [`Error::location`] gives a 1-based line and column.
//! - [`to_string`] stringifies non-string scalars and keys for `for:` loops
//!   and summaries.
//! - Deserializing borrows nothing from the input: strings reach visitors
//!   through `visit_str`, never `visit_borrowed_str` as with serde_yaml_ng,
//!   so a `&'de str` field fails to decode.

mod de;
mod error;
mod loader;
mod mapping;
mod number;
mod path;
mod reader;
mod reindent;
mod ser;
mod tagged;
mod value;
mod value_de;

use serde::de::{Deserialize, DeserializeOwned};

pub use error::{Error, Location};
pub use mapping::Mapping;
pub use number::Number;
pub use ser::to_string;
pub use tagged::{Tag, TaggedValue};
pub use value::{Sequence, Value};

/// Deserializes an instance of `T` from a single YAML document.
pub fn from_str<'de, T: Deserialize<'de>>(s: &'de str) -> Result<T, Error> {
    T::deserialize(de::Deserializer {
        input: de::Input::Str(s),
    })
}

/// Deserializes an instance of `T` from a single YAML document, which must
/// be UTF-8.
pub fn from_slice<'de, T: Deserialize<'de>>(v: &'de [u8]) -> Result<T, Error> {
    T::deserialize(de::Deserializer {
        input: de::Input::Slice(v),
    })
}

/// Interprets a `Value` as an instance of `T`.
pub fn from_value<T: DeserializeOwned>(value: Value) -> Result<T, Error> {
    T::deserialize(value)
}

#[cfg(test)]
mod tests;
