//! Extraction APIs at four granularities of "what does this SQL touch?"
//!
//! Each sub-extractor is a thin wrapper around the bound-plan analysis
//! engine, projecting the resolved plan into a different surface:
//!
//! - [`extract_tables`] — flat list of `TableReference`s per
//!   statement, no read/write distinction.
//! - [`extract_crud_tables`] — tables bucketed by CRUD verb
//!   (Create / Read / Update / Delete).
//! - [`extract_table_operations`] — per-statement
//!   `TableOperation` with `reads` / `writes` / `lineage` at table
//!   granularity.
//! - [`extract_column_operations`] — same shape at column
//!   granularity, plus per-column lineage kinds
//!   (Passthrough / Transformation).
//!
//! Each extractor returns `Vec<Result<X, Error>>` so one statement that
//! fails to extract doesn't sink the rest of a multi-statement string. (A
//! *parse* error fails the whole call — the outer `Result` — since
//! statements can't be separated before parsing.) Sub-modules are private;
//! the public items reach users through the wildcard re-exports below.

mod column_operation_extractor;
mod crud_table_extractor;
mod table_extractor;
mod table_operation_extractor;

pub use column_operation_extractor::*;
pub use crud_table_extractor::*;
pub use table_extractor::*;
pub use table_operation_extractor::*;

use crate::casing::{IdentifierCasing, IdentifierStyle};
use crate::catalog::Catalog;
use sqlparser::ast::Statement;
use sqlparser::dialect::Dialect;

/// Optional inputs shared by every `*_with_options` extractor. Defaults
/// to no catalog and the dialect-derived identifier casing — i.e. the
/// plain `extract_*(dialect, sql)` behaviour.
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
/// use sql_insight::extractor::{extract_table_operations_with_options, ExtractorOptions};
/// use sql_insight::{CaseRule, IdentifierCasing};
///
/// let dialect = GenericDialect {};
/// let options = ExtractorOptions::new().with_casing(IdentifierCasing::uniform(CaseRule::Sensitive));
/// let result = extract_table_operations_with_options(&dialect, "SELECT * FROM users", options).unwrap();
/// assert_eq!(result[0].as_ref().unwrap().reads.len(), 1);
/// ```
#[derive(Default, Clone, Debug)]
pub struct ExtractorOptions<'a> {
    /// The schema to resolve against. With a catalog, matched tables are
    /// canonicalized to their registered path and column resolution is
    /// strict; without one (the default), references stay as written and
    /// resolution is inferred.
    pub catalog: Option<&'a Catalog>,
    /// Override the dialect-derived identifier casing. `None` (the
    /// default) derives it from the dialect via
    /// [`IdentifierCasing::for_dialect`] — set this to model a
    /// deployment-specific collation.
    pub casing: Option<IdentifierCasing>,
}

impl<'a> ExtractorOptions<'a> {
    /// Default options: no catalog, dialect-derived casing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve against `catalog`.
    pub fn with_catalog(mut self, catalog: &'a Catalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// Override the identifier casing (otherwise derived from the dialect).
    pub fn with_casing(mut self, casing: IdentifierCasing) -> Self {
        self.casing = Some(casing);
        self
    }

    /// The effective casing: the override if set, else the dialect default.
    fn casing_for(&self, dialect: &dyn Dialect) -> IdentifierCasing {
        self.casing
            .unwrap_or_else(|| IdentifierCasing::for_dialect(dialect))
    }

    /// The full identifier style for the binder: the effective casing plus
    /// the dialect's canonical quote (always dialect-derived — quoting a
    /// catalog-confirmed identity is a surface concern, not a user knob).
    pub(crate) fn identifier_style(&self, dialect: &dyn Dialect) -> IdentifierStyle {
        IdentifierStyle {
            casing: self.casing_for(dialect),
            quote: crate::casing::canonical_quote(dialect),
        }
    }
}

/// What a statement does, at a coarse level. The *verb* of the statement
/// — INSERT vs CREATE TABLE vs MERGE vs … — combined with the
/// `reads` / `writes` split recovers every distinction the project needs
/// to make at table granularity. Shared by every extractor (each surfaces
/// it as `statement_kind`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum StatementKind {
    /// `SELECT ...` (and other read-only queries: `TABLE foo`, `VALUES`,
    /// `WITH ... SELECT ...`). Reads only — no writes, no lineage.
    Select,
    /// `INSERT INTO ...`. Writes to one target table; reads from the
    /// `VALUES` / `SELECT` source. Emits source → target lineage.
    Insert,
    /// `UPDATE ... SET ...`. Reads and writes the same target table;
    /// reads from any joined / sub-query sources. Emits lineage from
    /// SET right-hand-side sources into the target columns.
    Update,
    /// `DELETE FROM ...`. The target table appears in both `reads`
    /// (row source) and `writes` (deletion target). No lineage.
    Delete,
    /// `MERGE INTO ... USING ...`. The target appears in both `reads`
    /// and `writes`; each `WHEN` clause may emit lineage from the
    /// source into the target's update / insert columns.
    Merge,
    /// `CREATE TABLE ...`. The new table is a write target. CREATE
    /// TABLE AS (CTAS) also reads from its SELECT and emits per-column
    /// lineage into the new table's columns.
    CreateTable,
    /// `CREATE VIEW ... AS SELECT ...`. The new view is a write
    /// target; reads come from the SELECT body. Per-column lineage
    /// pairs the SELECT projections with the view's columns.
    CreateView,
    /// `ALTER TABLE ...`. The altered table is a write target.
    /// Column-level changes are not modelled in detail.
    AlterTable,
    /// `ALTER VIEW ... AS SELECT ...`. Treated like CREATE VIEW for
    /// extraction purposes — the view is a write target, the new
    /// SELECT body supplies reads and per-column lineage.
    AlterView,
    /// `DROP TABLE` / `DROP VIEW` / `DROP MATERIALIZED VIEW`. The
    /// dropped relation is a write target. Other DROP variants
    /// (functions, schemas, indexes, etc.) classify as
    /// [`Unsupported`](StatementKind::Unsupported).
    Drop,
    /// `TRUNCATE TABLE ...`. The truncated table is a write target.
    Truncate,
    /// Statement is outside the operation-extraction scope. The
    /// accompanying `diagnostics` list explains why.
    Unsupported,
}

/// Classify a parsed statement into its [`StatementKind`]. Shared by the
/// column / flat / CRUD extractors to pick the verb before assembling
/// their surfaces.
pub(crate) fn classify_statement(statement: &Statement) -> StatementKind {
    use sqlparser::ast::{ObjectType, SetExpr};
    match statement {
        // `WITH cte AS (...) INSERT/UPDATE/DELETE/MERGE ...` is parsed
        // by sqlparser as a top-level Query whose body is a
        // `SetExpr::Insert/Update/Delete/Merge` wrapping the actual
        // DML statement. Reclassify against the inner statement so
        // the public StatementKind matches the verb the user wrote,
        // not the parser-level wrapper.
        Statement::Query(query) => match query.body.as_ref() {
            SetExpr::Insert(stmt)
            | SetExpr::Update(stmt)
            | SetExpr::Delete(stmt)
            | SetExpr::Merge(stmt) => classify_statement(stmt),
            _ => StatementKind::Select,
        },
        Statement::Insert(_) => StatementKind::Insert,
        Statement::Update(_) => StatementKind::Update,
        Statement::Delete(_) => StatementKind::Delete,
        Statement::Merge(_) => StatementKind::Merge,
        Statement::CreateTable(_) | Statement::CreateVirtualTable { .. } => {
            StatementKind::CreateTable
        }
        Statement::CreateView(_) => StatementKind::CreateView,
        Statement::AlterTable(_) => StatementKind::AlterTable,
        Statement::AlterView { .. } => StatementKind::AlterView,
        Statement::Drop {
            object_type: ObjectType::Table | ObjectType::View | ObjectType::MaterializedView,
            ..
        } => StatementKind::Drop,
        Statement::Truncate(_) => StatementKind::Truncate,
        // Drop variants that don't target relations (DROP FUNCTION,
        // DROP SCHEMA, etc.) — and every other unsupported variant —
        // fall through to Unsupported so the caller still gets a clear
        // diagnostic.
        _ => StatementKind::Unsupported,
    }
}
