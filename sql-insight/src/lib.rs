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
//! - **CRUD Table Extraction** — CRUD-bucketed table sets per
//!   statement. See [`extractor::extract_crud_tables`].
//! - **Table-level Operation Extraction** — `reads` / `writes` /
//!   `lineage` surfaces with [`extractor::StatementKind`] classification.
//!   See [`extractor::extract_table_operations`].
//! - **Column-level Operation Extraction** — the same three surfaces at
//!   column granularity, with `lineage` carrying
//!   [`extractor::ColumnLineageKind`] (`Passthrough` vs `Transformation`).
//!   The value-vs-filter distinction is structural: a value contributor is
//!   a `lineage` source, a filter-only column is in `reads` but not
//!   `lineage`. See [`extractor::extract_column_operations`].
//! - **Optional [`catalog::Catalog`]** — supply a schema provider to make
//!   resolution strict (each read's [`ResolutionKind`] records how it
//!   matched); every extractor also works catalog-free in best-effort mode.
//! - **Diagnostics** ([`diagnostic::TableLevelDiagnostic`] /
//!   [`diagnostic::ColumnLevelDiagnostic`]) — non-fatal coverage gaps
//!   surface alongside the result rather than failing the call.
//!   Per-reference resolution outcomes live on [`ColumnRead::resolution`]
//!   instead, keeping the diagnostic stream for tool-side gaps.
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
//! (`sql_insight::extractor::extract_table_operations`,
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
//! `reads` / `writes` follow a relation's **syntactic role in the
//! written SQL**, not what is physically touched at runtime: an
//! unreferenced CTE body's tables, a `SELECT COUNT(*) FROM t`, and a
//! `CREATE TABLE t LIKE src` source all read, even though no row data is
//! consumed. The actual data-flow precision lives in `lineage` — e.g.
//! `LIKE` (schema only) emits none, while `CLONE` (data copied) feeds
//! `src → t`.
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
//! - **Wildcards not expanded**: the `*` / `t.*` itself contributes
//!   nothing to `reads` / `lineage` (expanding it safely would require
//!   modelling USING / NATURAL JOIN merge, EXCLUDE / EXCEPT / RENAME, and
//!   multi-level aliases — too much rigor for a SQL-text-only library).
//!   Surfaced as
//!   [`WildcardSuppressed`](diagnostic::ColumnLevelDiagnosticKind::WildcardSuppressed)
//!   so consumers can detect incomplete projections. A `REPLACE (expr AS
//!   col)` clause *is* extracted — each replacement's `expr` contributes
//!   reads and a `col` lineage edge, exactly like a standalone `expr AS col`
//!   — but its **output position** is best-effort, since the wildcard's own
//!   columns aren't enumerated to place it among them.
//! - **Table functions are opaque**: `UNNEST` / `generate_series` /
//!   `JSON_TABLE` / `PIVOT` etc. produce dynamic columns that aren't
//!   enumerated. Their argument expressions surface as reads, but a
//!   reference *through* such a relation (`u.col`) is a synthetic
//!   lineage source named by the alias, not a cataloged real-table read.
//! - **Recursive CTEs aren't unrolled**: the recursive self-reference
//!   terminates against the anchor branch's columns (via an active-set),
//!   so lineage traces through to the anchor's real tables — it doesn't
//!   enumerate per-iteration contributions.
//! - **Column-list-less `INSERT` needs a catalog for column lineage**: an
//!   `INSERT INTO t SELECT …` (or `MERGE … INSERT VALUES …`) without an
//!   explicit column list can only pair source columns to target columns
//!   when a catalog supplies `t`'s columns. Catalog-free, the column-level
//!   `writes` / `lineage` are dropped (the table still surfaces in
//!   `table_writes`), flagged
//!   [`InsertColumnsUnresolved`](diagnostic::ColumnLevelDiagnosticKind::InsertColumnsUnresolved)
//!   so the empty surfaces read as "couldn't analyze", not "nothing written".
//! - **Lineage kind is coarse** (`Passthrough` vs `Transformation`).
//!   Aggregates, window functions, arithmetic, casts, etc. are all
//!   `Transformation` — the model deliberately does not sub-classify
//!   "changed" values (that distinction is lossy for edge cases like
//!   window aggregates and value-preserving `STRING_AGG`, and not
//!   needed for the core dependency / impact-analysis use case).
//! - **Qualifier matching is right-anchored**: a partial qualifier
//!   (`users.col`) matches a fuller registered path (`mydb.users`),
//!   and a bare name does not merge into a schema-qualified one. A
//!   table reference with more than `catalog.schema.name` segments
//!   can't be represented, so it's dropped and flagged
//!   [`TooManyTableQualifiers`](diagnostic::ColumnLevelDiagnosticKind::TooManyTableQualifiers).
//! - **No type checking**: the catalog is an enrichment input,
//!   not a validator. Type compatibility, coercion, nullability, and
//!   structural well-formedness (e.g. an `INSERT`'s column / value count
//!   matching) are out of scope — a malformed statement is analysed as
//!   written (columns and values pair positionally, extras dropped), not
//!   rejected.
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
//!   fine catalog-free. Those `None`s carry their status on
//!   [`ColumnRead::resolution`] (`Ambiguous` / `Unresolved`), not a
//!   diagnostic stream — the consumer reads it off the reference. A
//!   catalog makes resolution strict: a confirmed hit is
//!   [`ResolutionKind::Cataloged`], a denied ref [`ResolutionKind::Unresolved`],
//!   and INSERT without an explicit column list pairs source
//!   projections with the target's catalog columns. Catalog-free, every
//!   relation is open (anything could belong), so reads are best-effort
//!   [`ResolutionKind::Inferred`] / [`ResolutionKind::Ambiguous`].
//! - **Per-statement isolation (post-parse)**: every extractor returns
//!   `Vec<Result<X, Error>>` so one statement that fails to *extract*
//!   doesn't sink the rest. A *parse* error is different — it fails the
//!   whole call (the outer `Result`), since statements can't be separated
//!   before parsing.
//! - **Fatal vs non-fatal split**: a parse error or a per-statement
//!   extraction failure is an `Err`; tool-side coverage gaps (unsupported
//!   statement, suppressed wildcards, over-qualified table names) surface
//!   in the per-statement `diagnostics` list instead. Per-reference
//!   resolution outcomes (ambiguous / unresolved columns) are not
//!   diagnostics — they live on [`ColumnRead::resolution`].
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

pub mod catalog;
pub mod diagnostic;
pub mod error;
pub mod extractor;
pub mod formatter;
pub mod normalizer;

// The analysis engine: binds a `Statement` into a standard bound logical
// plan (`LogicalPlan`) and walks it with a column-origin traversal for the
// extraction surfaces. Backs every public extractor.
mod resolver;

// `serde::Serialize` helpers for the sqlparser types (`Ident` / `Span`)
// embedded in the public result types. Gated on the `serde` feature.
#[cfg(feature = "serde")]
mod serde_support;

// Dialect-aware identifier casing (case folding for table / alias /
// column matching). Threaded into the binder and the extractors. The
// module stays private; the two configuration types are re-exported at
// the crate root so consumers can override the dialect default via the
// `*_with_options` extractors (through `ExtractorOptions::with_casing`).
pub(crate) mod casing;
pub use casing::{CaseRule, IdentifierCasing};

// `reference` is intentionally private: the module name itself is not
// stable enough to commit to as part of the public API. The two
// identity types it contains (`TableReference` / `ColumnReference`)
// are re-exported at the crate root because they thread through
// every other module's public surface.
mod reference;
pub use reference::{
    ColumnIdentityKey, ColumnRead, ColumnReference, ColumnWrite, ResolutionKind, TableIdentityKey,
    TableRead, TableReference, TableWrite,
};

// `sqlparser` is re-exported so consumers can name `Dialect` /
// `Ident` / etc. via `sql_insight::sqlparser::...` without taking a
// direct dependency (and risking a version mismatch).
pub use sqlparser;

#[doc(hidden)]
// Internal module for testing. Made public for use in integration tests.
pub mod test_utils;
