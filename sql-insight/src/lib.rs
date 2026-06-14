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
//!   [`extractor::extract_tables`].
//! - **CRUD Table Extraction** — CRUD-bucketed table sets per
//!   statement. See [`extractor::extract_crud_tables`].
//! - **Table-level Operation Extraction** — `reads` / `writes` /
//!   `lineage` surfaces with [`extractor::StatementKind`] classification.
//!   See [`extractor::extract_table_operations`].
//! - **Column-level Operation Extraction** — the same three
//!   surfaces at column granularity. `reads` is a list of
//!   [`ColumnRead`]s (occurrence-based, each pairing a
//!   [`ColumnReference`] identity with the resolver's
//!   [`ResolutionKind`]); `writes` is a plain occurrence list of
//!   [`ColumnReference`]s; `lineage` forms a source → target graph
//!   carrying [`extractor::ColumnLineageKind`] (`Passthrough` vs
//!   `Transformation`). The value-vs-filter distinction is
//!   structural: a value contributor is a `lineage` source, a
//!   filter-only column is in `reads` but not `lineage`. See
//!   [`extractor::extract_column_operations`].
//! - **Optional [`catalog::Catalog`]** — supply a schema provider to
//!   make resolution strict. Catalog-confirmed placements surface as
//!   [`ResolutionKind::Cataloged`] reads; refs the catalog actively
//!   denies surface as [`ResolutionKind::Unresolved`]. INSERT without an
//!   explicit column list also uses the catalog to pair positional
//!   values with target columns. Every extractor works catalog-free
//!   in best-effort mode (catalog-less reads surface as
//!   [`ResolutionKind::Inferred`] / [`ResolutionKind::Ambiguous`] /
//!   [`ResolutionKind::Unresolved`]).
//! - **Diagnostics** ([`diagnostic::TableLevelDiagnostic`] /
//!   [`diagnostic::ColumnLevelDiagnostic`]) — non-fatal issues surface
//!   alongside the extraction result rather than failing the whole call.
//!   Two kinds remain: unsupported statements and suppressed wildcards.
//!   Per-reference resolution outcomes live on
//!   [`ColumnRead::resolution`] instead, so the diagnostic stream is
//!   reserved for tool-side coverage gaps.
//!
//! ## Quick Start
//!
//! Table-level operation extraction — get `reads` / `writes` /
//! `lineage` and the statement kind from a single call:
//!
//! ```rust
//! use sql_insight::sqlparser::dialect::GenericDialect;
//! use sql_insight::extractor::{extract_table_operations, StatementKind};
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
//! let formatted = sql_insight::formatter::format(
//!     &dialect, "SELECT * \n from users   WHERE id = 1"
//! ).unwrap();
//! assert_eq!(formatted, ["SELECT * FROM users WHERE id = 1"]);
//! ```
//!
//! ## API Layout
//!
//! Public types live in domain-named modules ([`catalog`],
//! [`diagnostic`], [`error`], [`extractor`], [`formatter`],
//! [`normalizer`]); access them via their module path
//! (`sql_insight::extractor::extract_tables`,
//! `sql_insight::formatter::format`, etc.). The two identity types
//! [`TableReference`] / [`ColumnReference`] are re-exported at the
//! crate root because they show up across modules; their containing
//! module is internal and may be reshaped without an API change.
//! [`sqlparser`] is re-exported so consumers can name `Dialect` /
//! `Ident` / etc. without depending on the crate directly.
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
//! For column-level lineage, [`extractor::ColumnLineageKind`] makes one
//! clean distinction: `Passthrough` (the value is forwarded unchanged; a
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
//!   [`WildcardSuppressed`](diagnostic::ColumnLevelDiagnosticKind::WildcardSuppressed)
//!   so consumers can detect incomplete projections.
//! - **TableFunction schemas stay `Unknown`** (`UNNEST`,
//!   `generate_series`, `JSON_TABLE`, etc.) — catalog enrichment
//!   doesn't reach them yet.
//! - **Recursive CTE bodies** are pre-bound under a stub for
//!   self-reference. Table-level lineage traces the anchor branch's
//!   real tables; column-level projection collapse through the
//!   recursive body is deferred, so a column lineage edge surfaces
//!   with the CTE binding as the source rather than tracing into the
//!   underlying table.
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
//!   a variant to [`extractor::StatementKind`] /
//!   [`extractor::ColumnLineageKind`] / [`extractor::ColumnTarget`] /
//!   the diagnostic-kind enums is therefore a visible breaking change —
//!   deliberate, so consumers re-acknowledge each new case rather than
//!   silently routing it to a wildcard arm. They will likely gain
//!   `#[non_exhaustive]` at the 1.0 freeze, once the variant sets
//!   stabilize.

// Incubating "bound plan" (design "B") — a materialized full-stack
// operator tree built alongside the current `resolver` (strangler).
// Not yet wired into any extractor; kept private while it grows.
mod bind;
pub mod catalog;
pub mod diagnostic;
pub mod error;
pub mod extractor;
pub mod formatter;
pub mod normalizer;
pub(crate) mod resolver;

// `reference` is intentionally private: the module name itself is not
// stable enough to commit to as part of the public API. The two
// identity types it contains (`TableReference` / `ColumnReference`)
// are re-exported at the crate root because they thread through
// every other module's public surface.
mod reference;
pub use reference::{ColumnRead, ColumnReference, ResolutionKind, TableRead, TableReference};

// `sqlparser` is re-exported so consumers can name `Dialect` /
// `Ident` / etc. via `sql_insight::sqlparser::...` without taking a
// direct dependency (and risking a version mismatch).
pub use sqlparser;

#[doc(hidden)]
// Internal module for testing. Made public for use in integration tests.
pub mod test_utils;
