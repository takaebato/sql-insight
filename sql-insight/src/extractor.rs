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
use crate::error::Error;
use sqlparser::ast::Statement;
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

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
    /// `SELECT ...` (and other read-only queries: `VALUES (...)`,
    /// `WITH ... SELECT ...`; a bare `TABLE foo` is read-only too but only
    /// parses as a set-operation branch, not a standalone statement). Reads
    /// only — no writes, no lineage.
    Select,
    /// `INSERT INTO ...`. Writes to one target table; reads from the
    /// `VALUES` / `SELECT` source. Emits source → target lineage.
    Insert,
    /// `UPDATE ... SET ...`. The target is a write, not a scan, so at
    /// *table* granularity it is in `writes` only (not `reads`); joined /
    /// sub-query sources are reads. At *column* granularity a target column
    /// read by a SET right-hand side or `WHERE` (e.g. `SET a = a + 1`)
    /// surfaces in `reads`. Emits lineage from SET sources into the target
    /// columns.
    Update,
    /// `DELETE FROM ...`. Removes whole rows: the target is in `writes`
    /// (table granularity) but has no column-level writes and no lineage —
    /// nothing is written into a column. At column granularity, a target
    /// column referenced in `WHERE` surfaces in `reads`.
    Delete,
    /// `MERGE INTO ... USING ...`. The target is a write (in `writes`, not a
    /// scan); at column granularity a target column referenced in `ON` / a
    /// `WHEN` predicate or `SET` surfaces in `reads`. Each `WHEN` clause may
    /// emit lineage from the source into the target's update / insert columns.
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

/// The shared "Unsupported statement: …" diagnostic message — embeds the
/// statement's `Display`. Both granularity-specific extractors use this so
/// the wording stays in step (the kinds themselves split table- vs
/// column-level via the [`diagnostic`](crate::diagnostic) types).
pub(crate) fn unsupported_message(statement: &Statement) -> String {
    format!("Unsupported statement: {statement}")
}

/// The shared `*_with_options` driver: parse `sql` once, then run
/// `extract_from` on each parsed statement. A *parse* failure fails the whole
/// call (`Err` on the outer `Result`) because statements can't be separated
/// without the parser; a per-statement failure stays inside that statement's
/// inner `Result` so the rest of a multi-statement batch still surfaces. Each
/// sub-extractor's public `extract*_with_options` is a one-liner over this.
pub(crate) fn extract_each<T, F>(
    dialect: &dyn Dialect,
    sql: &str,
    options: ExtractorOptions,
    extract_from: F,
) -> Result<Vec<Result<T, Error>>, Error>
where
    F: Fn(&Statement, Option<&Catalog>, IdentifierStyle) -> Result<T, Error>,
{
    let statements = Parser::parse_sql(dialect, sql)?;
    let style = options.identifier_style(dialect);
    Ok(statements
        .iter()
        .map(|s| extract_from(s, options.catalog, style))
        .collect())
}

/// Classify a parsed statement into its [`StatementKind`]. Shared by the
/// column / flat / CRUD extractors to pick the verb before assembling
/// their surfaces.
pub(crate) fn classify_statement(statement: &Statement) -> StatementKind {
    use sqlparser::ast::ObjectType;
    match statement {
        // `WITH cte AS (...) INSERT/UPDATE/DELETE/MERGE ...` and a
        // parenthesised DML `(DELETE FROM t)` are both parsed by sqlparser as a
        // top-level Query whose body wraps the actual DML. Reclassify against
        // the inner statement so the public StatementKind matches the verb the
        // user wrote, not the parser-level wrapper.
        Statement::Query(query) => classify_query_body(query.body.as_ref()),
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

/// Classify a top-level `Query`'s body to the verb the user wrote. A DML body
/// (`WITH … <DML>`) reclassifies against the inner statement; a parenthesised
/// body (`(DELETE …)`, even nested) recurses through the wrapper to find the
/// DML / `SELECT … INTO` verb — mirroring [`has_leading_select_into`], which
/// already recurses `SetExpr::Query`, so the two stay in step. Anything else is
/// read-only / row-producing → `Select`. The match is exhaustive so a new
/// write-bearing `SetExpr` variant is a compile error rather than a silent
/// `Select`.
fn classify_query_body(body: &sqlparser::ast::SetExpr) -> StatementKind {
    use sqlparser::ast::SetExpr;
    match body {
        SetExpr::Insert(stmt)
        | SetExpr::Update(stmt)
        | SetExpr::Delete(stmt)
        | SetExpr::Merge(stmt) => classify_statement(stmt),
        // A parenthesised query: peel the wrapper (nested parens included) and
        // classify the inner body, so `(DELETE …)` keeps its verb.
        SetExpr::Query(inner) => classify_query_body(inner.body.as_ref()),
        // A leading `SELECT … INTO t` lowers to `CreateTableAs` in the binder;
        // keep the verb (and the table-lineage gate that keys on it) in step.
        body if has_leading_select_into(body) => StatementKind::CreateTable,
        SetExpr::Select(_)
        | SetExpr::SetOperation { .. }
        | SetExpr::Values(_)
        | SetExpr::Table(_) => StatementKind::Select,
    }
}

/// Whether a query body carries a leading `SELECT … INTO t` — the table-creating
/// `SELECT INTO` (T-SQL / Postgres), which the binder lowers to `CreateTableAs`.
/// `INTO` rides the left spine (through a parenthesised query and the left
/// branch of a set operation, where it targets the combined result), so this
/// mirrors the binder's `leading_select_into`; the two must stay in step. The
/// match is exhaustive so a new `SetExpr` variant forces a decision here too.
fn has_leading_select_into(body: &sqlparser::ast::SetExpr) -> bool {
    use sqlparser::ast::SetExpr;
    match body {
        SetExpr::Select(select) => select.into.is_some(),
        SetExpr::Query(query) => has_leading_select_into(&query.body),
        SetExpr::SetOperation { left, .. } => has_leading_select_into(left),
        SetExpr::Values(_)
        | SetExpr::Insert(_)
        | SetExpr::Update(_)
        | SetExpr::Delete(_)
        | SetExpr::Merge(_)
        | SetExpr::Table(_) => false,
    }
}
