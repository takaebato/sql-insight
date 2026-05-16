//! Optional schema provider plugged into the resolver.
//!
//! The resolver uses [`Catalog`] purely as an *enrichment* input: structural
//! resolution (CTE / derived table schemas, FROM alias bindings) works
//! catalog-free, and a catalog only fills in the columns of tables
//! that the resolver could not derive from the SQL alone. When no catalog is
//! provided, those holes stay `RelationSchema::Unknown` and surface as diagnostics
//! once consumers (e.g. column-level operations) start reading them.
//!
//! Implementations typically wrap an `information_schema` query, an ORM
//! model registry, or a static map produced from `CREATE TABLE` statements.

use std::fmt;

use sqlparser::ast::Ident;

use crate::relation::TableReference;

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
    fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>>;
}

/// A column entry returned by a [`Catalog`]. Intentionally minimal: starts
/// with `name` only and grows along the project roadmap (see the resolver
/// memory note). Type/nullability/comment fields are deliberately deferred
/// until a downstream consumer needs them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSchema {
    pub name: Ident,
}
