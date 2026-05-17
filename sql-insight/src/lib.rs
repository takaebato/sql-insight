//! # sql-insight
//!
//! Operation extraction for SQL, built on
//! [`sqlparser-rs`](https://crates.io/crates/sqlparser). Turn a SQL
//! string into structured facts about what a statement does —
//! which tables and columns it reads, which it writes, and how data
//! moves from sources to targets — alongside utilities for
//! formatting and normalization.
//!
//! ## Main Functionalities
//!
//! - **SQL Formatting** — pretty-print SQL with a standardized
//!   layout. See [`formatter`].
//! - **SQL Normalization** — abstract literals into placeholders so
//!   structurally identical queries hash to the same shape. See
//!   [`normalizer`].
//! - **Table Extraction** — flat list of
//!   [`TableReference`]s touched by a statement. See
//!   [`extract_tables`].
//! - **CRUD Table Extraction** — CRUD-bucketed table sets per
//!   statement. See [`extract_crud_tables`].
//! - **Table-level Operation Extraction** — `reads` / `writes` /
//!   `flows` surfaces with [`StatementKind`] classification. See
//!   [`extract_table_operations`].
//! - **Column-level Operation Extraction** — the same three
//!   surfaces at column granularity, with clause-role
//!   ([`ReadKind`]) and flow-kind ([`ColumnFlowKind`]) metadata.
//!   Column flows form a source → target graph suitable for
//!   lineage-style analyses. See [`extract_column_operations`].
//! - **Optional [`Catalog`]** — supply a schema provider to make
//!   resolution strict (catch typos as
//!   [`UnresolvedColumn`](DiagnosticKind::UnresolvedColumn),
//!   pair INSERT positional values with target columns, etc.).
//!   Every extractor works catalog-free in best-effort mode.
//! - **[`Diagnostic`]** — non-fatal issues surface alongside the
//!   extraction result rather than failing the whole call:
//!   unsupported statements, suppressed wildcards, ambiguous /
//!   unresolved columns.
//!
//! ## Quick Start
//!
//! Table-level operation extraction — get `reads` / `writes` /
//! `flows` and the statement kind from a single call:
//!
//! ```rust
//! use sql_insight::sqlparser::dialect::GenericDialect;
//! use sql_insight::{extract_table_operations, StatementKind};
//!
//! let dialect = GenericDialect {};
//! let result = extract_table_operations(
//!     &dialect,
//!     "INSERT INTO orders (id) SELECT id FROM staging",
//!     None,
//! ).unwrap();
//! let ops = result[0].as_ref().unwrap();
//! assert_eq!(ops.statement_kind, StatementKind::Insert);
//! assert_eq!(ops.reads.len(), 1);   // staging
//! assert_eq!(ops.writes.len(), 1);  // orders
//! assert_eq!(ops.flows.len(), 1);   // staging → orders
//! ```
//!
//! SQL formatting:
//!
//! ```rust
//! use sql_insight::sqlparser::dialect::GenericDialect;
//!
//! let dialect = GenericDialect {};
//! let formatted = sql_insight::format(
//!     &dialect, "SELECT * \n from users   WHERE id = 1"
//! ).unwrap();
//! assert_eq!(formatted, ["SELECT * FROM users WHERE id = 1"]);
//! ```
//!
//! ## Vocabulary
//!
//! Operation extraction returns three parallel surfaces per
//! statement:
//!
//! - `reads` — every table (or column) the statement reads from.
//! - `writes` — every table (or column) the statement writes to. A
//!   table that plays both roles (e.g. `DELETE t1 FROM t1`) appears
//!   in both.
//! - `flows` — directed `source → target` edges, emitted only for
//!   statements that physically move data (`INSERT` / `UPDATE` /
//!   `MERGE` / `CREATE TABLE AS` / `CREATE VIEW`).
//!
//! For column-level flows, [`ColumnFlowKind`] distinguishes
//! `Passthrough` (raw move), `Aggregation` (through `SUM` / `COUNT`
//! / etc.) and `Computed` (through expressions). Reads carry a
//! [`Vec<ReadKind>`](ReadKind) describing where in the statement
//! they appeared (`Projection` / `Filter` / `GroupBy` / `Sort` /
//! `Window`, plus a `Conditional` modifier for `CASE WHEN`).
//!
//! ## Limitations
//!
//! Intentional non-support and known gaps — set expectations before
//! relying on a given output:
//!
//! - **Wildcards not expanded**: `SELECT *` / `t.*` contribute
//!   nothing to `reads` / `flows`. Expanding them safely would
//!   require modelling USING / NATURAL JOIN merge, EXCLUDE / REPLACE
//!   clauses, and multi-level aliases — too much rigor for a
//!   SQL-text-only library. Surfaced as
//!   [`WildcardSuppressed`](DiagnosticKind::WildcardSuppressed) so
//!   consumers can detect incomplete projections.
//! - **TableFunction schemas stay `Unknown`** (`UNNEST`,
//!   `generate_series`, `JSON_TABLE`, etc.) — catalog enrichment
//!   doesn't reach them yet.
//! - **Recursive CTE bodies** are pre-bound under a stub for
//!   self-reference; their projection composition is deferred, so
//!   `flows` won't trace through them end-to-end.
//! - **Aggregate detection** combines structural markers
//!   (`FILTER (WHERE ...)`, `WITHIN GROUP (...)`, `DISTINCT` in
//!   args — all aggregate-only per SQL standard) with a built-in
//!   union of common aggregate names across major dialects.
//!   Dialect-specific UDAFs outside that list are misclassified as
//!   `Computed`. Window-only functions (`ROW_NUMBER`, `RANK`,
//!   `LAG`, `LEAD`, …) are intentionally excluded.
//! - **Multi-segment qualifiers** (`s.t.col`): only the head `s`
//!   is matched against in-scope bindings for synthetic-vs-real
//!   classification — schema- / catalog-qualified shapes resolve
//!   loosely.
//! - **No type checking**: the catalog is an enrichment input,
//!   not a validator. Type compatibility, coercion, and nullability
//!   are out of scope.
//!
//! ## Behavior notes
//!
//! - **Catalog is optional, and shapes resolver strictness**.
//!   Without a catalog the resolver runs best-effort: table schemas
//!   stay `Unknown`, ambiguous and unresolved column diagnostics are
//!   suppressed (every `Unknown` schema could contain anything).
//!   With a catalog, those diagnostics fire and INSERT positional
//!   pairing pairs source projections with target columns.
//! - **Per-statement isolation**: every extractor returns
//!   `Vec<Result<X, Error>>` so a bad statement in a multi-statement
//!   batch doesn't take the rest down.
//! - **Fatal vs non-fatal split**: parser failures and structural
//!   problems short-circuit as `Err`; semantic issues (unsupported
//!   statement, ambiguity, suppressed wildcards) surface in the
//!   per-statement `diagnostics: Vec<Diagnostic>` instead.
//! - **[`TableReference`] / [`ColumnReference`] are identity-only**.
//!   No `alias` field — alias is use-site decoration. `HashSet`
//!   dedup behaves intuitively across statements.
//! - **Set operations follow the left side**: the result schema of
//!   `UNION` / `INTERSECT` / `EXCEPT` takes its column names from
//!   the left branch, mirroring SQL's conventional behaviour.
//! - **Public enums are `#[non_exhaustive]`** so future variants
//!   stay SemVer-minor — consumers must include a wildcard arm when
//!   matching on [`DiagnosticKind`] / [`StatementKind`] /
//!   [`ReadKind`] / [`ColumnFlowKind`] / [`ColumnTarget`].

pub mod catalog;
pub mod diagnostic;
pub mod error;
pub mod extractor;
pub mod formatter;
pub mod normalizer;
pub mod relation;
pub(crate) mod resolver;

pub use catalog::{Catalog, ColumnSchema};
pub use diagnostic::*;
pub use extractor::*;
pub use formatter::*;
pub use normalizer::*;
pub use relation::*;
pub use sqlparser;

#[doc(hidden)]
// Internal module for testing. Made public for use in integration tests.
pub mod test_utils;
