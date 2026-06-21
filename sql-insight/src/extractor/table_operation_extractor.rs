//! Extracts the application-level operations a SQL statement performs.
//!
//! Where [`extract_tables`](crate::extractor::extract_tables()) answers "what tables
//! does this SQL touch?" and [`extract_crud_tables`](crate::extractor::extract_crud_tables())
//! answers it in CRUD buckets, this module answers "what operations does
//! this SQL perform, on which tables, and how do those tables relate?".
//!
//! The output is per-statement: one [`TableOperation`] per parsed
//! statement, since a single application call (e.g. an ORM `execute()`)
//! typically corresponds to a single statement.
//!
//! Three parallel surfaces describe the statement:
//! - `reads` — every table the statement reads from.
//! - `writes` — every table the statement writes to.
//! - `lineage` — directed `source → target` edges for statements that
//!   physically move data.
//!
//! A single table can appear in both `reads` and `writes` when it plays
//! both roles (e.g. `DELETE t1 FROM t1` — t1 is the deletion target and
//! a row source).

use crate::casing::IdentifierStyle;
use crate::catalog::Catalog;
use crate::diagnostic::{TableLevelDiagnostic, TableLevelDiagnosticKind};
use crate::error::Error;
use crate::extractor::{classify_statement, ExtractorOptions, StatementKind};
use crate::reference::{TableRead, TableReference};
use sqlparser::ast::Statement;
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to extract table-level operations from SQL using
/// the dialect defaults (no catalog, dialect-derived casing). For a
/// catalog or a casing override, use
/// [`extract_table_operations_with_options`].
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
/// use sql_insight::extractor::{extract_table_operations, StatementKind};
///
/// let dialect = GenericDialect {};
/// let result = extract_table_operations(&dialect, "SELECT * FROM users").unwrap();
/// let ops = result[0].as_ref().unwrap();
/// assert_eq!(ops.statement_kind, StatementKind::Select);
/// assert_eq!(ops.reads.len(), 1);
/// assert_eq!(ops.reads[0].reference.name.value, "users");
/// assert!(ops.writes.is_empty());
/// ```
pub fn extract_table_operations(
    dialect: &dyn Dialect,
    sql: &str,
) -> Result<Vec<Result<TableOperation, Error>>, Error> {
    TableOperationExtractor::extract(dialect, sql)
}

/// Like [`extract_table_operations`] but with [`ExtractorOptions`] — a
/// catalog and/or an identifier-casing override. `dialect` still drives
/// parsing; the options govern only the analysis.
pub fn extract_table_operations_with_options(
    dialect: &dyn Dialect,
    sql: &str,
    options: ExtractorOptions,
) -> Result<Vec<Result<TableOperation, Error>>, Error> {
    TableOperationExtractor::extract_with_options(dialect, sql, options)
}

/// Operations performed by a single SQL statement.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TableOperation {
    /// What the statement does at a coarse level (Insert / Update /
    /// Merge / CTAS / …).
    pub statement_kind: StatementKind,
    /// Tables read by the statement. Occurrence-based: a table referenced
    /// more than once appears more than once. Each [`TableRead`] pairs the
    /// identity with the catalog-match
    /// [`ResolutionKind`](crate::ResolutionKind). **In source order** — by
    /// each read's written token span (`reference.name.span`), a deterministic
    /// function of the SQL rather than the internal traversal. For the distinct
    /// identity set, dedup `reads.iter().map(|r| &r.reference)` via a `HashSet`
    /// (or, catalog-free, by
    /// [`TableReference::identity_key`](crate::TableReference::identity_key)
    /// to fold case-equivalent spellings).
    pub reads: Vec<TableRead>,
    /// Tables written by the statement, in source order. Occurrence-based
    /// like `reads`. Bare [`TableReference`] — write targets are trivially
    /// resolved by construction.
    pub writes: Vec<TableReference>,
    /// Lineage edges, only for statements that physically move data
    /// (`INSERT`, `UPDATE`, `MERGE` with an Insert / Update WHEN
    /// clause, CTAS, `CREATE VIEW`, `ALTER VIEW`). **In source order** of the
    /// feeding source table (by its written token span); occurrence /
    /// multiplicity is preserved.
    pub lineage: Vec<TableLineageEdge>,
    /// Non-fatal diagnostics from the walk; only
    /// `UnsupportedStatement` arises at this granularity.
    pub diagnostics: Vec<TableLevelDiagnostic>,
}

/// A source-to-target table lineage edge inferred from the statement
/// structure.
///
/// Emitted only for statements that physically move data into a target
/// (`INSERT`, `UPDATE`, `MERGE`, `CREATE TABLE AS SELECT`, `CREATE VIEW`).
/// `DELETE`, `DROP`, `TRUNCATE`, `ALTER`, and bare `SELECT` produce no
/// lineage even when they reference other tables — the touched tables are
/// still visible through [`TableOperation::reads`] and
/// [`TableOperation::writes`].
///
/// Each `TableLineageEdge` is a single directed edge — a statement that derives
/// `t` from `a JOIN b` emits two edges (`a → t`, `b → t`), not one entry
/// with both sources.
///
/// **Occurrence-based**: a statement using the same source more than
/// once (`FROM s AS x JOIN s AS y`, repeated `FROM cte` across UNION
/// branches) emits one entry per use, not one deduped entry. Matches
/// [`ColumnLineageEdge`](crate::extractor::ColumnLineageEdge)
/// on multiplicity. Consumers wanting set-union semantics dedup
/// explicitly via `HashSet::from_iter`.
///
/// Tables referenced only inside a predicate subquery are excluded:
/// `INSERT INTO t SELECT FROM s WHERE id IN (SELECT id FROM x)` emits
/// `s → t` but not `x → t`. `x` remains visible via `reads`.
///
/// CTE transitivity: `WITH cte AS (SELECT ... FROM s) INSERT INTO t
/// SELECT ... FROM cte` emits `s → t` because `s` sits in a
/// data-feeding chain from the CTE body up through the INSERT target.
/// An unreferenced CTE contributes nothing — `WITH cte AS (SELECT a
/// FROM s) INSERT INTO t SELECT 1` emits no edge (the `cte` is bound
/// but never `FROM`-used, so `s` doesn't feed `t`).
///
/// Recursive CTEs collapse the same way: the anchor branch's real
/// tables feed the target, and the self-reference terminates against
/// the pre-bind stub without re-emitting the cycle.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TableLineageEdge {
    /// The feeding source table, paired with its catalog-match
    /// [`ResolutionKind`](crate::ResolutionKind).
    pub source: TableRead,
    /// The write target. Bare [`TableReference`] — trivially resolved
    /// by construction.
    pub target: TableReference,
}

/// Struct-style entry point. Equivalent to the free
/// [`extract_table_operations`] function.
#[derive(Default, Debug)]
pub struct TableOperationExtractor;

impl TableOperationExtractor {
    /// Same as the free [`extract_table_operations`] function — kept
    /// for users who prefer the struct-style API.
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
    ) -> Result<Vec<Result<TableOperation, Error>>, Error> {
        Self::extract_with_options(dialect, sql, ExtractorOptions::new())
    }

    /// Like [`extract`](Self::extract) but with [`ExtractorOptions`] — a
    /// catalog and/or an identifier-casing override. `dialect` still
    /// drives parsing; the options govern only the analysis.
    pub fn extract_with_options(
        dialect: &dyn Dialect,
        sql: &str,
        options: ExtractorOptions,
    ) -> Result<Vec<Result<TableOperation, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        let style = options.identifier_style(dialect);
        Ok(statements
            .iter()
            .map(|s| Self::extract_from_statement(s, options.catalog, style))
            .collect())
    }

    /// Assemble the table operation from the bound plan: classify the verb,
    /// bind the statement, walk the plan for `reads` / `writes`, and (for
    /// data-moving statements only) `lineage`. Column-level diagnostics
    /// project down to the table level. A kind the binder can't model
    /// yields an empty operation with an `UnsupportedStatement` diagnostic.
    pub(crate) fn extract_from_statement(
        statement: &Statement,
        catalog: Option<&Catalog>,
        style: IdentifierStyle,
    ) -> Result<TableOperation, Error> {
        let statement_kind = classify_statement(statement);
        if statement_kind == StatementKind::Unsupported {
            return Ok(unsupported_table_operation(statement_kind, statement));
        }
        let (plan, column_diagnostics) = crate::resolver::build(statement, catalog, style);
        // Lineage is only for statements that move data into a target. A
        // column-less INSERT and a DELETE both bind to a `Write`, so the
        // structural walk can't tell them apart — gate on the kind. A MERGE
        // whose WHEN clauses are only DELETEs uses its source solely to pick
        // target rows, so it moves no data even though the source is a
        // feeding input — gate it out via `merge_moves_data`.
        let lineage = if moves_data(&statement_kind) && merge_moves_data(statement) {
            crate::resolver::table_lineage(&plan)
        } else {
            Vec::new()
        };
        Ok(TableOperation {
            statement_kind,
            reads: crate::resolver::table_reads(&plan),
            writes: crate::resolver::table_writes(&plan),
            lineage,
            // Table-level diagnostics are the column-level ones projected
            // down (only `UnsupportedStatement` / `TooManyTableQualifiers`
            // survive the projection).
            diagnostics: column_diagnostics
                .iter()
                .filter_map(|d| d.to_table_level())
                .collect(),
        })
    }
}

/// Whether a statement physically moves data into its target (so it emits
/// table lineage). `DELETE` / `DROP` / `TRUNCATE` / `ALTER TABLE` touch a
/// target but feed it nothing; a bare `SELECT` has no target.
fn moves_data(kind: &StatementKind) -> bool {
    matches!(
        kind,
        StatementKind::Insert
            | StatementKind::Update
            | StatementKind::Merge
            | StatementKind::CreateTable
            | StatementKind::CreateView
            | StatementKind::AlterView
    )
}

fn unsupported_table_operation(
    statement_kind: StatementKind,
    statement: &Statement,
) -> TableOperation {
    TableOperation {
        statement_kind,
        reads: Vec::new(),
        writes: Vec::new(),
        lineage: Vec::new(),
        diagnostics: vec![TableLevelDiagnostic {
            kind: TableLevelDiagnosticKind::UnsupportedStatement,
            message: format!("Unsupported statement for plan-based extraction: {statement}"),
            span: None,
        }],
    }
}

/// `MERGE` is `is_data_moving` whenever it appears, but a MERGE
/// whose WHEN clauses are only `DELETE` actions doesn't actually
/// move data from the source into the target — the source is just
/// used to pick which rows of the target to delete. Inspect the
/// statement and report whether at least one WHEN clause is an
/// INSERT or UPDATE, so the lineage path can short-circuit for the
/// DELETE-only case.
fn merge_moves_data(statement: &Statement) -> bool {
    use sqlparser::ast::MergeAction;
    let Statement::Merge(merge) = statement else {
        return true;
    };
    merge.clauses.iter().any(|clause| {
        matches!(
            clause.action,
            MergeAction::Insert(_) | MergeAction::Update { .. }
        )
    })
}
