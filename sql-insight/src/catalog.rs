//! Optional schema provider plugged into the resolver.
//!
//! The resolver uses [`Catalog`] purely as an *enrichment* input: structural
//! resolution (CTE / derived table schemas, FROM alias bindings) works
//! catalog-free, and a catalog only fills in the columns of tables
//! that the resolver could not derive from the SQL alone. When no catalog is
//! provided, those holes stay `RelationSchema::Unknown` and surface as diagnostics
//! once consumers (e.g. column-level operations) start reading them.
//!
//! The catalog is treated as **open-world**: a table it returns no columns
//! for is taken as *schema unknown*, not *nonexistent*. A misspelled or
//! unknown table name is therefore never flagged — it surfaces as an
//! ordinary read / write carrying an unknown schema. Strictness is
//! column-level and local: `UnresolvedColumn` / `AmbiguousColumn` only fire
//! where a known schema is in scope. (Treating absence as nonexistence
//! would require promising the catalog is exhaustive, which most providers
//! cannot, so it is not the default.)
//!
//! Implementations typically wrap an `information_schema` query, an ORM
//! model registry, or a static map produced from `CREATE TABLE` statements.

use std::fmt;

use crate::reference::TableReference;

/// Provides the column list of a table.
///
/// Implementations return `None` when the table is unknown to the catalog;
/// the resolver treats this the same as "no catalog" for that table and may
/// emit a diagnostic instead of failing the whole resolution.
///
/// The trait is object-safe so it can be passed as `&dyn Catalog`. `Debug`
/// is a supertrait so that resolver state containing `&dyn Catalog` can
/// derive `Debug` — implementations are expected to `#[derive(Debug)]` or
/// provide a manual implementation.
pub trait Catalog: fmt::Debug {
    /// Resolve a table to its column list. The `table` argument may
    /// carry an alias, but implementations should treat the catalog/schema/
    /// name triplet as the identity — the alias is callsite-only metadata.
    ///
    /// Identifier case-folding *for this table lookup* is the
    /// implementation's responsibility: the resolver passes the table name
    /// as written in the SQL and does not normalize it, so an
    /// implementation wanting case-insensitive lookup (most dialects) must
    /// fold both its stored keys and the incoming `table` name.
    ///
    /// That is the only matching the implementation governs. The returned
    /// column names are then matched against the SQL's column references
    /// by the resolver's own fixed normalization rule (unquoted folds to
    /// lowercase, quoted is exact) — independent of this implementation
    /// and of the dialect. So supplying a catalog changes *which columns
    /// exist*, never *how a column name compares*.
    fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>>;
}

/// A column entry returned by a [`Catalog`]. Intentionally minimal: starts
/// with `name` only and grows along the project roadmap (see the resolver
/// memory note). Type/nullability/comment fields are deliberately deferred
/// until a downstream consumer needs them.
///
/// `name` is a plain `String`: a catalog provides column identities, and
/// matching against SQL refs is case-insensitive by default (quoting /
/// case-sensitivity is not modelled per-column — see `BindingKey`), so
/// there is no need to carry `sqlparser`'s `Ident` (quote style / span).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSchema {
    pub name: String,
}
