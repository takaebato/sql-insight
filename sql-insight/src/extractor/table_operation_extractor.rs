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

use crate::catalog::Catalog;
use crate::diagnostic::{TableLevelDiagnostic, TableLevelDiagnosticKind};
use crate::error::Error;
use crate::reference::TableReference;
use crate::resolver::Resolver;
use sqlparser::ast::Statement;
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to extract table-level operations from SQL.
///
/// `catalog` is consulted opportunistically for relation-level enrichment
/// (table schema lookup, future view expansion and synonym resolution).
/// Pass `None` for the lightest path — table-level extraction works
/// purely from the AST and never requires a catalog.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
/// use sql_insight::extractor::{extract_table_operations, StatementKind};
///
/// let dialect = GenericDialect {};
/// let result = extract_table_operations(&dialect, "SELECT * FROM users", None).unwrap();
/// let ops = result[0].as_ref().unwrap();
/// assert_eq!(ops.statement_kind, StatementKind::Select);
/// assert_eq!(ops.reads.len(), 1);
/// assert_eq!(ops.reads[0].name.value, "users");
/// assert!(ops.writes.is_empty());
/// ```
pub fn extract_table_operations(
    dialect: &dyn Dialect,
    sql: &str,
    catalog: Option<&dyn Catalog>,
) -> Result<Vec<Result<TableOperation, Error>>, Error> {
    TableOperationExtractor::extract(dialect, sql, catalog)
}

/// Operations performed by a single SQL statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableOperation {
    pub statement_kind: StatementKind,
    pub reads: Vec<TableReference>,
    pub writes: Vec<TableReference>,
    pub lineage: Vec<TableLineageEdge>,
    pub diagnostics: Vec<TableLevelDiagnostic>,
}

/// What a statement does, at a coarse level. The *verb* of the statement
/// — INSERT vs CREATE TABLE vs MERGE vs … — combined with the
/// `reads` / `writes` split recovers every distinction the project needs
/// to make at table granularity.
#[derive(Debug, Clone, PartialEq, Eq)]
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
pub struct TableLineageEdge {
    pub source: TableReference,
    pub target: TableReference,
}

/// Extracts operations from SQL.
#[derive(Default, Debug)]
pub struct TableOperationExtractor;

impl TableOperationExtractor {
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
        catalog: Option<&dyn Catalog>,
    ) -> Result<Vec<Result<TableOperation, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        Ok(statements
            .iter()
            .map(|s| Self::extract_from_statement(s, catalog))
            .collect())
    }

    pub fn extract_from_statement(
        statement: &Statement,
        catalog: Option<&dyn Catalog>,
    ) -> Result<TableOperation, Error> {
        let kind = classify_statement(statement);
        let resolution = Resolver::resolve_statement(catalog, statement)?;

        let mut reads = Vec::new();
        let mut writes = Vec::new();
        // Start from resolver-level diagnostics, projected down to the
        // table granularity — column-resolution gaps and suppressed
        // wildcards don't affect table-level completeness, so they drop
        // out here (only `UnsupportedStatement` carries over). Extractor
        // adds its own only when classify_statement detects an unsupported
        // case the resolver did not already report — avoids duplicating
        // the common case where both layers agree.
        let mut diagnostics: Vec<TableLevelDiagnostic> = resolution
            .diagnostics
            .iter()
            .filter_map(|d| d.to_table_level())
            .collect();

        if matches!(kind, StatementKind::Unsupported) {
            if !diagnostics
                .iter()
                .any(|d| matches!(d.kind, TableLevelDiagnosticKind::UnsupportedStatement))
            {
                diagnostics.push(TableLevelDiagnostic {
                    kind: TableLevelDiagnosticKind::UnsupportedStatement,
                    message: format!(
                        "Unsupported statement for operation extraction: {}",
                        statement
                    ),
                    span: None,
                });
            }
        } else {
            // A multi-role table (e.g. `DELETE t1 FROM t1` — t1 is both
            // deletion target and row source) appears in both lists.
            reads = resolution.read_tables();
            writes = resolution.write_tables();
        }

        let lineage = extract_table_lineage(&resolution, &kind);

        Ok(TableOperation {
            statement_kind: kind,
            reads,
            writes,
            lineage,
            diagnostics,
        })
    }
}

/// Emit one `TableLineageEdge` per (feeding source × write target) pair
/// for statements that physically move data. Statements without a write
/// target or without any data-feeding source produce no lineage.
fn extract_table_lineage(
    resolution: &crate::resolver::Resolution,
    kind: &StatementKind,
) -> Vec<TableLineageEdge> {
    if !is_data_moving(kind) {
        return Vec::new();
    }
    // Data-moving statements all carry exactly one write target. If
    // somehow zero or many appear (parser oddity, unsupported variant)
    // we conservatively emit no lineage rather than guessing.
    let mut targets = resolution.write_tables().into_iter();
    let Some(target) = targets.next() else {
        return Vec::new();
    };
    resolution
        .collapsed_feeding_table_sources()
        .into_iter()
        .map(|source| TableLineageEdge {
            source,
            target: target.clone(),
        })
        .collect()
}

fn is_data_moving(kind: &StatementKind) -> bool {
    matches!(
        kind,
        StatementKind::Insert
            | StatementKind::Update
            | StatementKind::Merge
            | StatementKind::CreateTable
            | StatementKind::CreateView
    )
}

pub(super) fn classify_statement(statement: &Statement) -> StatementKind {
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::{Dialect, GenericDialect, MySqlDialect, PostgreSqlDialect};

    fn table(name: &str) -> TableReference {
        TableReference {
            catalog: None,
            schema: None,
            name: name.into(),
        }
    }

    fn edge(source: &str, target: &str) -> TableLineageEdge {
        TableLineageEdge {
            source: table(source),
            target: table(target),
        }
    }

    /// Whole-value-ish assertion: pin down the full
    /// `TableOperation` for `sql`, but compare diagnostics
    /// by **kind sequence only** — message text and span coordinates
    /// are ignored. This lets tests focus on "what was extracted"
    /// without coupling to diagnostic wording or column offsets that
    /// shift when SQL is reformatted.
    ///
    /// Tests that genuinely care about the message / span shape
    /// should fall back to per-field `assert_eq!`.
    fn assert_ops(sql: &str, expected: TableOperation) {
        assert_nth_ops_with(sql, 0, &GenericDialect {}, expected);
    }

    fn assert_ops_with(sql: &str, dialect: &dyn Dialect, expected: TableOperation) {
        assert_nth_ops_with(sql, 0, dialect, expected);
    }

    /// Like `assert_ops`, but for multi-statement SQL — pins down the
    /// statement at `index` in the parsed batch. Compose calls to pin
    /// down every statement in a batch separately.
    fn assert_nth_ops(sql: &str, index: usize, expected: TableOperation) {
        assert_nth_ops_with(sql, index, &GenericDialect {}, expected);
    }

    fn assert_nth_ops_with(
        sql: &str,
        index: usize,
        dialect: &dyn Dialect,
        expected: TableOperation,
    ) {
        let result = extract_table_operations(dialect, sql, None).unwrap();
        let actual = result
            .into_iter()
            .nth(index)
            .unwrap_or_else(|| panic!("statement {index} missing in result for SQL: {sql}"))
            .unwrap();
        let TableOperation {
            statement_kind,
            reads,
            writes,
            lineage,
            diagnostics,
        } = expected;
        assert_eq!(
            actual.statement_kind, statement_kind,
            "kind for SQL: {sql} (statement {index})"
        );
        assert_eq!(
            actual.reads, reads,
            "reads for SQL: {sql} (statement {index})"
        );
        assert_eq!(
            actual.writes, writes,
            "writes for SQL: {sql} (statement {index})"
        );
        assert_eq!(
            actual.lineage, lineage,
            "lineage for SQL: {sql} (statement {index})"
        );
        let actual_kinds: Vec<_> = actual.diagnostics.iter().map(|d| d.kind.clone()).collect();
        let expected_kinds: Vec<_> = diagnostics.iter().map(|d| d.kind.clone()).collect();
        assert_eq!(
            actual_kinds, expected_kinds,
            "diagnostic kinds for SQL: {sql} (statement {index})"
        );
    }

    /// Construct a placeholder `TableLevelDiagnostic` for the
    /// `expected.diagnostics` list in `assert_ops`. Only the kind is
    /// compared; the message and span are placeholders.
    fn diag(kind: TableLevelDiagnosticKind) -> TableLevelDiagnostic {
        TableLevelDiagnostic {
            kind,
            message: String::new(),
            span: None,
        }
    }

    mod select {
        use super::*;

        #[test]
        fn select_emits_reads_only() {
            assert_ops(
                "SELECT id FROM users",
                TableOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![table("users")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn select_with_join_emits_one_read_per_table() {
            // The `*` does not surface a diagnostic at table granularity —
            // WildcardSuppressed is a column-level concern and is filtered
            // out of table-level output (the table set is complete
            // regardless of wildcard expansion).
            assert_ops(
                "SELECT * FROM t1 JOIN t2 ON t1.id = t2.id",
                TableOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![table("t1"), table("t2")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn select_with_subquery_emits_read_for_every_table() {
            assert_ops(
                "SELECT t1.a FROM t1 WHERE id IN (SELECT id FROM t2)",
                TableOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![table("t1"), table("t2")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_body_tables_emit_reads_but_cte_name_does_not() {
            // Only t1 is a table reference; t2 is the CTE binding and stays out.
            assert_ops(
                "WITH t2 AS (SELECT id FROM t1) SELECT t2.id FROM t2",
                TableOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![table("t1")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod set_operations {
        use super::*;

        #[test]
        fn union_emits_read_for_each_branch_table() {
            // Each UNION branch walks its own FROM, so both tables
            // surface in reads. No lineage: bare SELECT statements
            // never produce table-level data movement.
            assert_ops(
                "SELECT a FROM t1 UNION SELECT b FROM t2",
                TableOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![table("t1"), table("t2")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn intersect_and_except_match_union_shape() {
            // SetOperator variant doesn't influence table-level
            // surfacing — INTERSECT and EXCEPT both walk both branches.
            for op in ["INTERSECT", "EXCEPT"] {
                let sql = format!("SELECT a FROM t1 {op} SELECT b FROM t2");
                assert_ops(
                    &sql,
                    TableOperation {
                        statement_kind: StatementKind::Select,
                        reads: vec![table("t1"), table("t2")],
                        writes: vec![],
                        lineage: vec![],
                        diagnostics: vec![],
                    },
                );
            }
        }

        #[test]
        fn insert_select_union_emits_one_lineage_edge_per_branch() {
            // INSERT-SELECT-UNION moves data from each branch into the
            // target, so both source tables surface as lineage sources.
            assert_ops(
                "INSERT INTO dst SELECT a FROM t1 UNION SELECT b FROM t2",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![table("t1"), table("t2")],
                    writes: vec![table("dst")],
                    lineage: vec![edge("t1", "dst"), edge("t2", "dst")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn ctas_with_union_body_emits_lineage_per_branch() {
            assert_ops(
                "CREATE TABLE dst AS SELECT a FROM t1 UNION SELECT b FROM t2",
                TableOperation {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![table("t1"), table("t2")],
                    writes: vec![table("dst")],
                    lineage: vec![edge("t1", "dst"), edge("t2", "dst")],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod diagnostics {
        use super::*;

        #[test]
        fn unsupported_statement_reports_diagnostic() {
            assert_ops(
                "CREATE INDEX idx ON t1 (a)",
                TableOperation {
                    statement_kind: StatementKind::Unsupported,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![diag(TableLevelDiagnosticKind::UnsupportedStatement)],
                },
            );
        }

        #[test]
        fn multiple_statements_produce_multiple_results() {
            let sql = "SELECT * FROM t1; SELECT * FROM t2";
            assert_nth_ops(
                sql,
                0,
                TableOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![table("t1")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
            assert_nth_ops(
                sql,
                1,
                TableOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![table("t2")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod insert {
        use super::*;

        #[test]
        fn insert_values_emits_write_only() {
            assert_ops(
                "INSERT INTO t1 (a, b) VALUES (1, 2)",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_emits_write_and_read() {
            assert_ops(
                "INSERT INTO t1 SELECT * FROM t2",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("t2", "t1")],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod update {
        use super::*;

        #[test]
        fn update_basic_emits_write_only() {
            assert_ops(
                "UPDATE t1 SET a = 1",
                TableOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_with_subquery_predicate_emits_write_plus_read() {
            assert_ops(
                "UPDATE t1 SET a = 1 WHERE id IN (SELECT id FROM t2)",
                TableOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_with_from_clause_treats_from_as_read() {
            // FROM t2 contributes rows to the UPDATE target → t2 → t1
            // lineage edge. SET RHS scalar subquery from t3 feeds the new
            // value → t3 → t1 lineage edge. WHERE predicate subquery from
            // t4 is predicate-only → no lineage.
            assert_ops_with(
                "UPDATE t1 SET a = (SELECT b FROM t3) FROM t2 WHERE t1.id IN (SELECT id FROM t4)",
                &PostgreSqlDialect {},
                TableOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![table("t2"), table("t3"), table("t4")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("t2", "t1"), edge("t3", "t1")],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod delete {
        use super::*;

        #[test]
        fn delete_from_emits_write_only() {
            assert_ops(
                "DELETE FROM t1",
                TableOperation {
                    statement_kind: StatementKind::Delete,
                    reads: vec![],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn delete_from_with_subquery_predicate_emits_write_plus_read() {
            assert_ops(
                "DELETE FROM t1 WHERE id IN (SELECT id FROM t2)",
                TableOperation {
                    statement_kind: StatementKind::Delete,
                    reads: vec![table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn delete_with_target_list_overlaps_writes_and_reads() {
            // `DELETE t1, t2 FROM t1 JOIN t2 JOIN t3` — t1 and t2 are both
            // deletion targets (writes) AND row sources (reads via FROM).
            assert_ops_with(
                "DELETE t1, t2 FROM t1 INNER JOIN t2 INNER JOIN t3",
                &MySqlDialect {},
                TableOperation {
                    statement_kind: StatementKind::Delete,
                    reads: vec![table("t1"), table("t2"), table("t3")],
                    writes: vec![table("t1"), table("t2")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn delete_with_using_lists_target_in_writes_and_source_in_reads() {
            assert_ops(
                "DELETE FROM t1, t2 USING t1 INNER JOIN t2 INNER JOIN t3",
                TableOperation {
                    statement_kind: StatementKind::Delete,
                    reads: vec![table("t1"), table("t2"), table("t3")],
                    writes: vec![table("t1"), table("t2")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn delete_resolves_target_alias_to_real_table() {
            assert_ops_with(
                "DELETE t1_alias FROM t1 AS t1_alias JOIN t2 ON t1_alias.a = t2.a",
                &MySqlDialect {},
                TableOperation {
                    statement_kind: StatementKind::Delete,
                    reads: vec![table("t1"), table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod merge {
        use super::*;

        #[test]
        fn merge_emits_write_target_and_read_source() {
            assert_ops(
                "MERGE INTO t1 USING t2 ON t1.id = t2.id \
                 WHEN MATCHED THEN UPDATE SET t1.b = t2.b",
                TableOperation {
                    statement_kind: StatementKind::Merge,
                    reads: vec![table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("t2", "t1")],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod ddl {
        use super::*;

        #[test]
        fn create_table_emits_write_only() {
            assert_ops(
                "CREATE TABLE t1 (a INT)",
                TableOperation {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn create_table_as_select_emits_write_and_read() {
            assert_ops(
                "CREATE TABLE t1 AS SELECT * FROM t2",
                TableOperation {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("t2", "t1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn create_view_emits_write_and_read() {
            assert_ops(
                "CREATE VIEW v1 AS SELECT * FROM t1",
                TableOperation {
                    statement_kind: StatementKind::CreateView,
                    reads: vec![table("t1")],
                    writes: vec![table("v1")],
                    lineage: vec![edge("t1", "v1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn alter_table_emits_write_only() {
            assert_ops(
                "ALTER TABLE t1 ADD COLUMN a INT",
                TableOperation {
                    statement_kind: StatementKind::AlterTable,
                    reads: vec![],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn drop_table_emits_one_write_per_name() {
            assert_ops(
                "DROP TABLE t1, t2",
                TableOperation {
                    statement_kind: StatementKind::Drop,
                    reads: vec![],
                    writes: vec![table("t1"), table("t2")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn truncate_emits_one_write_per_name() {
            assert_ops(
                "TRUNCATE TABLE t1, t2",
                TableOperation {
                    statement_kind: StatementKind::Truncate,
                    reads: vec![],
                    writes: vec![table("t1"), table("t2")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn drop_function_still_unsupported() {
            // DROP variants that target non-relation objects don't carry a
            // meaningful table-level operation.
            assert_ops(
                "DROP FUNCTION my_fn",
                TableOperation {
                    statement_kind: StatementKind::Unsupported,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![diag(TableLevelDiagnosticKind::UnsupportedStatement)],
                },
            );
        }
    }

    mod lineage {
        use super::*;

        #[test]
        fn insert_select_emits_lineage_from_source_to_target() {
            assert_ops(
                "INSERT INTO t1 SELECT * FROM t2",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("t2", "t1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_join_emits_one_lineage_edge_per_source() {
            assert_ops(
                "INSERT INTO t1 SELECT * FROM t2 JOIN t3 ON t2.id = t3.id",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![table("t2"), table("t3")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("t2", "t1"), edge("t3", "t1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn predicate_subquery_does_not_feed_lineage() {
            // t3 is referenced only inside `WHERE id IN (SELECT id FROM t3)`,
            // so it must not appear as a lineage source even though it does
            // appear in `reads`.
            assert_ops(
                "INSERT INTO t1 SELECT * FROM t2 WHERE id IN (SELECT id FROM t3)",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![table("t2"), table("t3")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("t2", "t1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn join_on_predicate_does_not_promote_to_lineage() {
            // t4 is in JOIN ON's predicate subquery — touches as read
            // but doesn't promote to a lineage edge (predicate position excluded
            // from data-feeding chain).
            assert_ops(
                "INSERT INTO t1 SELECT * FROM t2 JOIN t3 ON t2.id = t3.id \
                 AND t2.id IN (SELECT id FROM t4)",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![table("t2"), table("t3"), table("t4")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("t2", "t1"), edge("t3", "t1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_scalar_subquery_in_set_feeds_lineage() {
            assert_ops(
                "UPDATE t1 SET col = (SELECT v FROM t2)",
                TableOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("t2", "t1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_predicate_subquery_does_not_feed_lineage() {
            assert_ops(
                "UPDATE t1 SET col = 1 WHERE id IN (SELECT id FROM t2)",
                TableOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn create_table_as_select_emits_lineage() {
            assert_ops(
                "CREATE TABLE t1 AS SELECT * FROM t2",
                TableOperation {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("t2", "t1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn create_view_emits_lineage() {
            assert_ops(
                "CREATE VIEW v1 AS SELECT * FROM t1",
                TableOperation {
                    statement_kind: StatementKind::CreateView,
                    reads: vec![table("t1")],
                    writes: vec![table("v1")],
                    lineage: vec![edge("t1", "v1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn merge_emits_lineage_from_source_to_target() {
            assert_ops(
                "MERGE INTO t1 USING t2 ON t1.id = t2.id \
                 WHEN MATCHED THEN UPDATE SET t1.b = t2.b",
                TableOperation {
                    statement_kind: StatementKind::Merge,
                    reads: vec![table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("t2", "t1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_data_reaches_write_target() {
            assert_ops(
                "INSERT INTO t1 WITH cte AS (SELECT * FROM s) SELECT * FROM cte",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![table("s")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("s", "t1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_predicate_subquery_does_not_leak_into_lineage() {
            // x is in the CTE body's WHERE predicate subquery — touches
            // as read but doesn't promote to a lineage edge.
            assert_ops(
                "INSERT INTO t1 WITH cte AS (\
                 SELECT * FROM s WHERE id IN (SELECT id FROM x)\
             ) SELECT * FROM cte",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![table("s"), table("x")],
                    writes: vec![table("t1")],
                    lineage: vec![edge("s", "t1")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unreferenced_cte_body_tables_do_not_leak_into_lineage() {
            // `cte` is defined but never referenced — the INSERT pulls
            // its value from a constant projection. No data actually
            // flows from `s` to `t1` (an optimizer would prune the CTE
            // entirely). `s` should still appear in `reads` since the
            // SQL text touches it, but it must not produce a lineage
            // edge into `t1`.
            //
            // This pins down the boundary between "table is referenced
            // in the SQL" (reads) and "data actually moves from this
            // table to the target" (lineage). Driven by
            // `collapsed_feeding_table_sources`: real-table captures
            // sitting inside an unreferenced synthetic body sit under
            // that body's scope, get filtered by the synthetic-body
            // mask in the top loop, and never get reached by a
            // top-level `FROM cte` recursion either.
            assert_ops(
                "WITH cte AS (SELECT a FROM s) INSERT INTO t1 SELECT 1",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![table("s")],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn recursive_cte_emits_lineage_from_anchor_real_table() {
            // The recursive CTE's anchor reads `x`; the recursive
            // branch's self-reference contributes no new real source.
            // External `FROM cte` use feeds the INSERT target, so
            // collapse should reach through the body to `x` and emit
            // a single `x → t` edge.
            assert_ops(
                "WITH RECURSIVE cte AS (\
                 SELECT id FROM x UNION ALL \
                 SELECT id + 1 FROM cte WHERE id < 10\
             ) INSERT INTO t SELECT id FROM cte",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![table("x")],
                    writes: vec![table("t")],
                    lineage: vec![edge("x", "t")],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn recursive_cte_self_only_emits_no_lineage() {
            // Pathological body — only a self-reference, no real
            // anchor source. The CTE has no real feeder, so no edge
            // is emitted even though the INSERT has a target. Pins
            // that the self-reference collapse terminates at the
            // pre-bind stub without traversing into the body scope
            // and re-emitting the self-cycle.
            assert_ops(
                "WITH RECURSIVE cte AS (SELECT id FROM cte) \
             INSERT INTO t SELECT id FROM cte",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![table("t")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn select_only_statement_emits_no_lineage() {
            assert_ops(
                "SELECT * FROM t1 JOIN t2 ON t1.id = t2.id",
                TableOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![table("t1"), table("t2")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_values_emits_no_lineage() {
            assert_ops(
                "INSERT INTO t1 VALUES (1, 2)",
                TableOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn delete_with_subquery_predicate_emits_no_lineage() {
            // DELETE doesn't move data — no lineage, even when a subquery
            // references another table.
            assert_ops(
                "DELETE FROM t1 WHERE id IN (SELECT id FROM t2)",
                TableOperation {
                    statement_kind: StatementKind::Delete,
                    reads: vec![table("t2")],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn truncate_emits_no_lineage() {
            assert_ops(
                "TRUNCATE TABLE t1",
                TableOperation {
                    statement_kind: StatementKind::Truncate,
                    reads: vec![],
                    writes: vec![table("t1")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }
    }
}
