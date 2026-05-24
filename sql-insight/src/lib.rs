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
//!   `lineage` surfaces with [`StatementKind`] classification. See
//!   [`extract_table_operations`].
//! - **Column-level Operation Extraction** — the same three
//!   surfaces at column granularity. `reads` / `writes` are plain
//!   occurrence lists of [`ColumnReference`]s; `lineage` form a
//!   source → target graph carrying [`ColumnLineageKind`]
//!   (`Passthrough` vs `Transformation`). The value-vs-filter
//!   distinction is structural: a value contributor is a `lineage`
//!   source, a filter-only column is in `reads` but not `lineage`.
//!   See [`extract_column_operations`].
//! - **Optional [`Catalog`]** — supply a schema provider to make
//!   resolution strict (catch typos as
//!   [`UnresolvedColumn`](ColumnLevelDiagnosticKind::UnresolvedColumn),
//!   pair INSERT positional values with target columns, etc.).
//!   Every extractor works catalog-free in best-effort mode.
//! - **Diagnostics** ([`TableLevelDiagnostic`] / [`ColumnLevelDiagnostic`])
//!   — non-fatal issues surface alongside the extraction result rather
//!   than failing the whole call: unsupported statements, suppressed
//!   wildcards, ambiguous / unresolved columns. Split by granularity so a
//!   table-level result can't carry a column-only condition.
//!
//! ## Quick Start
//!
//! Table-level operation extraction — get `reads` / `writes` /
//! `lineage` and the statement kind from a single call:
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
//! assert_eq!(ops.lineage.len(), 1);   // staging → orders
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
//! - `lineage` — directed `source → target` edges, emitted only for
//!   statements that physically move data (`INSERT` / `UPDATE` /
//!   `MERGE` / `CREATE TABLE AS` / `CREATE VIEW`).
//!
//! For column-level lineage, [`ColumnLineageKind`] makes one clean
//! distinction: `Passthrough` (the value is forwarded unchanged; a
//! rename still counts) vs `Transformation` (any expression that
//! changes the value — arithmetic, function calls, aggregates,
//! window functions, CASE, casts, …). `reads` / `writes` are plain
//! occurrence lists of column references with no clause tag; whether
//! a column contributes a value or merely influences the result
//! (e.g. a `WHERE` predicate) is recovered structurally — value
//! contributors appear as `lineage` sources, filter-only columns do
//! not.
//!
//! ## Limitations
//!
//! Intentional non-support and known gaps — set expectations before
//! relying on a given output:
//!
//! - **Wildcards not expanded**: `SELECT *` / `t.*` contribute
//!   nothing to `reads` / `lineage`. Expanding them safely would
//!   require modelling USING / NATURAL JOIN merge, EXCLUDE / REPLACE
//!   clauses, and multi-level aliases — too much rigor for a
//!   SQL-text-only library. Surfaced as
//!   [`WildcardSuppressed`](ColumnLevelDiagnosticKind::WildcardSuppressed) so
//!   consumers can detect incomplete projections.
//! - **TableFunction schemas stay `Unknown`** (`UNNEST`,
//!   `generate_series`, `JSON_TABLE`, etc.) — catalog enrichment
//!   doesn't reach them yet.
//! - **Recursive CTE bodies** are pre-bound under a stub for
//!   self-reference; their projection composition is deferred, so
//!   `lineage` won't trace through them end-to-end.
//! - **Lineage kind is coarse** (`Passthrough` vs `Transformation`).
//!   Aggregates, window functions, arithmetic, casts, etc. are all
//!   `Transformation` — the model deliberately does not sub-classify
//!   "changed" values (that distinction is lossy for edge cases like
//!   window aggregates and value-preserving `STRING_AGG`, and not
//!   needed for the core dependency / impact-analysis use case).
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
//! - **Catalog is optional, but load-bearing for column lineage**.
//!   Table-level extraction is robust catalog-free — a table's
//!   identity comes straight from the FROM clause. Column-level
//!   extraction degrades without one: an unqualified column across
//!   multiple in-scope tables (`SELECT x FROM a JOIN b`) is not
//!   determinable from the SQL text alone, so it resolves to
//!   `table: None`. Qualified (`t.col`) and single-table refs resolve
//!   fine catalog-free. The ambiguous / unresolved-column diagnostics
//!   that explain those `None`s fire only *with* a catalog; without
//!   one they are suppressed (every `Unknown` schema could contain
//!   anything, so flagging would flood the output with noise). With a
//!   catalog, those diagnostics fire and INSERT positional pairing
//!   pairs source projections with target columns.
//! - **Per-statement isolation**: every extractor returns
//!   `Vec<Result<X, Error>>` so a bad statement in a multi-statement
//!   batch doesn't take the rest down.
//! - **Fatal vs non-fatal split**: parser failures and structural
//!   problems short-circuit as `Err`; semantic issues (unsupported
//!   statement, ambiguity, suppressed wildcards) surface in the
//!   per-statement `diagnostics` list instead.
//! - **[`TableReference`] / [`ColumnReference`] are identity-only**.
//!   No `alias` field — alias is use-site decoration. `HashSet`
//!   dedup behaves intuitively across statements.
//! - **Set operations follow the left side**: the result schema of
//!   `UNION` / `INTERSECT` / `EXCEPT` takes its column names from
//!   the left branch, mirroring SQL's conventional behaviour.
//! - **Public enums are exhaustive while the crate is pre-1.0.** Adding
//!   a variant to [`StatementKind`] / [`ColumnLineageKind`] /
//!   [`ColumnTarget`] / the diagnostic-kind enums is therefore a visible
//!   breaking change — deliberate, so consumers re-acknowledge each new
//!   case rather than silently routing it to a wildcard arm. They will
//!   likely gain `#[non_exhaustive]` at the 1.0 freeze, once the variant
//!   sets stabilize.

pub mod catalog;
pub mod diagnostic;
pub mod error;
pub mod extractor;
pub mod formatter;
pub mod normalizer;
pub mod reference;
pub(crate) mod resolver;

pub use catalog::{Catalog, ColumnSchema};
pub use diagnostic::*;
pub use extractor::*;
pub use formatter::*;
pub use normalizer::*;
pub use reference::*;
pub use sqlparser;

#[doc(hidden)]
// Internal module for testing. Made public for use in integration tests.
pub mod test_utils;
