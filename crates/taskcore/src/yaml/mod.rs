//! YAML reading and writing for Taskfiles: every call site goes through this
//! module, so the YAML backend can be replaced in one place.
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

pub use serde_yaml_ng::value::{Tag, TaggedValue};
pub use serde_yaml_ng::{
    Error, Location, Mapping, Number, Value, from_slice, from_str, from_value, to_string,
};
