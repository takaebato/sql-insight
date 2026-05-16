//! Shared operation vocabulary used across the resolver and the
//! operation extractor.
//!
//! The two-variant [`TableRole`] encodes only the *role* a table plays
//! within a single statement — whether it is being modified (`Write`) or
//! merely read (`Read`). The *verb* of the statement (INSERT / UPDATE /
//! CREATE TABLE / …) lives separately in `StatementKind`, and the
//! combination of statement kind and per-table role recovers every
//! distinction the older granular enum carried, while letting one table
//! appear with multiple roles (e.g. `DELETE t1 FROM t1` — both `Write`
//! and `Read`).

/// The role a table plays in a single statement.
///
/// Kept intentionally coarse:
/// - `Write` covers every "mutating" role (insert target, update target,
///   delete target, merge target, create/alter/drop/truncate object).
/// - `Read` covers every "reading" role (FROM, USING, predicate
///   subquery, scalar subquery, join, etc.).
///
/// The finer "where exactly was this table used" classification (predicate
/// vs. projection vs. join etc.) belongs to the future `TableUsage`
/// enrichment, not to this enum.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TableRole {
    Read,
    Write,
}
