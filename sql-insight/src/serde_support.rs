//! `serialize_with` helpers for the sqlparser `Ident` embedded in the
//! public result types. It's a foreign type, so we can't derive
//! `Serialize` on it; each field that holds one points at a helper here
//! via `#[serde(serialize_with = "...")]`.
//!
//! The shape is *owned* by this crate (not sqlparser's `serde` feature),
//! so the JSON contract is stable under our SemVer regardless of
//! sqlparser's internal representation: an `Ident` →
//! `{ "value": "users", "quote": "\"" }` (`quote` is the quote char or
//! `null`). Source spans are deliberately not serialized — they're an
//! internal source-ordering detail, not part of the identity surface.
#![cfg(feature = "serde")]

use serde::ser::{SerializeStruct, Serializer};
use sqlparser::ast::Ident;

/// Serialize an [`Ident`] as `{ value, quote }`.
pub(crate) fn ident<S: Serializer>(id: &Ident, serializer: S) -> Result<S::Ok, S::Error> {
    let mut st = serializer.serialize_struct("Ident", 2)?;
    st.serialize_field("value", &id.value)?;
    st.serialize_field("quote", &id.quote_style)?;
    st.end()
}

/// Serialize an optional [`Ident`] — the [`ident`] shape, or `null`.
pub(crate) fn opt_ident<S: Serializer>(
    id: &Option<Ident>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match id {
        Some(id) => ident(id, serializer),
        None => serializer.serialize_none(),
    }
}
