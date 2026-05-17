//! Extracts the application-level operations a SQL statement performs.
//!
//! Where [`extract_tables`](crate::extract_tables()) answers "what tables
//! does this SQL touch?" and [`extract_crud_tables`](crate::extract_crud_tables())
//! answers it in CRUD buckets, this module answers "what operations does
//! this SQL perform, on which tables, and how do those tables relate?".
//!
//! The output is per-statement: one [`StatementTableOperations`] per parsed
//! statement, since a single application call (e.g. an ORM `execute()`)
//! typically corresponds to a single statement.
//!
//! Three parallel surfaces describe the statement:
//! - `reads` — every table the statement reads from.
//! - `writes` — every table the statement writes to.
//! - `flows` — directed `source → target` edges for statements that
//!   physically move data.
//!
//! A single table can appear in both `reads` and `writes` when it plays
//! both roles (e.g. `DELETE t1 FROM t1` — t1 is the deletion target and
//! a row source).

use crate::catalog::Catalog;
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::error::Error;
use crate::relation::TableReference;
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
/// use sql_insight::{extract_table_operations, StatementKind};
///
/// let dialect = GenericDialect {};
/// let result = extract_table_operations(&dialect, "SELECT * FROM users", None).unwrap();
/// let ops = result[0].as_ref().unwrap();
/// assert_eq!(ops.statement_kind, StatementKind::Select);
/// assert_eq!(ops.reads.len(), 1);
/// assert_eq!(ops.reads[0].table.name.value, "users");
/// assert!(ops.writes.is_empty());
/// ```
pub fn extract_table_operations(
    dialect: &dyn Dialect,
    sql: &str,
    catalog: Option<&dyn Catalog>,
) -> Result<Vec<Result<StatementTableOperations, Error>>, Error> {
    TableOperationExtractor::extract(dialect, sql, catalog)
}

/// Operations performed by a single SQL statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementTableOperations {
    pub statement_kind: StatementKind,
    pub reads: Vec<TableRead>,
    pub writes: Vec<TableWrite>,
    pub flows: Vec<TableFlow>,
    pub diagnostics: Vec<Diagnostic>,
}

/// What a statement does, at a coarse level. The *verb* of the statement
/// — INSERT vs CREATE TABLE vs MERGE vs … — combined with the
/// `reads` / `writes` split recovers every distinction the project needs
/// to make at table granularity.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StatementKind {
    Select,
    Insert,
    Update,
    Delete,
    Merge,
    CreateTable,
    CreateView,
    AlterTable,
    AlterView,
    Drop,
    Truncate,
    /// Statement is outside the operation-extraction scope. The accompanying
    /// `diagnostics` list explains why.
    Unsupported,
}

/// A table referenced as a Read source.
///
/// Carried in [`StatementTableOperations::reads`]. The struct exists to
/// give future positional / usage enrichment (FROM vs Predicate vs Join)
/// a natural home; the MVP carries only `table`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableRead {
    pub table: TableReference,
}

/// A table referenced as a Write target (insert / update / delete /
/// merge / create / drop / alter / truncate target).
///
/// Carried in [`StatementTableOperations::writes`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableWrite {
    pub table: TableReference,
}

/// A source-to-target table flow inferred from the statement structure.
///
/// Emitted only for statements that physically move data into a target
/// (`INSERT`, `UPDATE`, `MERGE`, `CREATE TABLE AS SELECT`, `CREATE VIEW`).
/// `DELETE`, `DROP`, `TRUNCATE`, `ALTER`, and bare `SELECT` produce no
/// flows even when they reference other tables — the touched tables are
/// still visible through [`StatementTableOperations::reads`] and
/// [`StatementTableOperations::writes`].
///
/// Each `TableFlow` is a single directed edge — a statement that derives
/// `t` from `a JOIN b` emits two flows (`a → t`, `b → t`), not one entry
/// with both sources. This keeps equality and aggregation across
/// statements simple (set-union over edges).
///
/// Tables referenced only inside a predicate subquery are excluded:
/// `INSERT INTO t SELECT FROM s WHERE id IN (SELECT id FROM x)` emits
/// `s → t` but not `x → t`. `x` remains visible via `reads`.
///
/// CTE transitivity: `WITH cte AS (SELECT ... FROM s) INSERT INTO t
/// SELECT ... FROM cte` emits `s → t` because `s` sits in a
/// data-feeding chain from the CTE body up through the INSERT target.
/// Deeper transitivity (recursive CTEs, multi-hop indirection) is
/// intentionally out of scope for the MVP.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableFlow {
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
    ) -> Result<Vec<Result<StatementTableOperations, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        Ok(statements
            .iter()
            .map(|s| Self::extract_from_statement(s, catalog))
            .collect())
    }

    pub fn extract_from_statement(
        statement: &Statement,
        catalog: Option<&dyn Catalog>,
    ) -> Result<StatementTableOperations, Error> {
        let kind = classify_statement(statement);
        let resolution = Resolver::resolve_statement(catalog, statement)?;

        let mut reads = Vec::new();
        let mut writes = Vec::new();
        // Start from resolver-level diagnostics (e.g. statements the
        // resolver explicitly flagged unsupported). Extractor adds its
        // own only when classify_statement detects an unsupported case
        // the resolver did not already report — avoids duplicating the
        // common case where both layers agree.
        let mut diagnostics = resolution.diagnostics.clone();

        if matches!(kind, StatementKind::Unsupported) {
            if !diagnostics
                .iter()
                .any(|d| matches!(d.kind, DiagnosticKind::UnsupportedStatement))
            {
                diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::UnsupportedStatement,
                    message: format!(
                        "Unsupported statement for operation extraction: {}",
                        statement
                    ),
                });
            }
        } else {
            // A multi-role table (e.g. `DELETE t1 FROM t1` — t1 is both
            // deletion target and row source) appears in both lists.
            reads = resolution
                .read_tables()
                .into_iter()
                .map(|table| TableRead { table })
                .collect();
            writes = resolution
                .write_tables()
                .into_iter()
                .map(|table| TableWrite { table })
                .collect();
        }

        let flows = extract_table_flows(&resolution, &kind);

        Ok(StatementTableOperations {
            statement_kind: kind,
            reads,
            writes,
            flows,
            diagnostics,
        })
    }
}

/// Emit one `TableFlow` edge per (feeding source × write target) pair
/// for statements that physically move data. Statements without a write
/// target or without any data-feeding source produce no flows.
fn extract_table_flows(
    resolution: &crate::resolver::Resolution,
    kind: &StatementKind,
) -> Vec<TableFlow> {
    if !is_data_moving(kind) {
        return Vec::new();
    }
    // Data-moving statements all carry exactly one write target. If
    // somehow zero or many appear (parser oddity, unsupported variant)
    // we conservatively emit no flows rather than guessing.
    let mut targets = resolution.write_tables().into_iter();
    let Some(target) = targets.next() else {
        return Vec::new();
    };
    resolution
        .feeding_read_tables()
        .into_iter()
        .map(|source| TableFlow {
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
    use sqlparser::ast::ObjectType;
    match statement {
        Statement::Query(_) => StatementKind::Select,
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

    fn extract(sql: &str) -> StatementTableOperations {
        extract_with(sql, &GenericDialect {})
    }

    fn extract_with(sql: &str, dialect: &dyn Dialect) -> StatementTableOperations {
        extract_with_catalog(sql, dialect, None)
    }

    fn extract_with_catalog(
        sql: &str,
        dialect: &dyn Dialect,
        catalog: Option<&dyn Catalog>,
    ) -> StatementTableOperations {
        let mut result = extract_table_operations(dialect, sql, catalog).unwrap();
        result.remove(0).unwrap()
    }

    fn table(name: &str) -> TableReference {
        TableReference {
            catalog: None,
            schema: None,
            name: name.into(),
        }
    }

    fn read(name: &str) -> TableRead {
        TableRead { table: table(name) }
    }
    fn write(name: &str) -> TableWrite {
        TableWrite { table: table(name) }
    }
    fn flow(source: &str, target: &str) -> TableFlow {
        TableFlow {
            source: table(source),
            target: table(target),
        }
    }

    #[test]
    fn select_emits_reads_only() {
        let ops = extract("SELECT id FROM users");
        assert_eq!(ops.statement_kind, StatementKind::Select);
        assert_eq!(ops.reads, vec![read("users")]);
        assert!(ops.writes.is_empty());
        assert!(ops.flows.is_empty());
        assert!(ops.diagnostics.is_empty());
    }

    #[test]
    fn select_with_join_emits_one_read_per_table() {
        let ops = extract("SELECT * FROM t1 JOIN t2 ON t1.id = t2.id");
        assert_eq!(ops.statement_kind, StatementKind::Select);
        assert_eq!(ops.reads, vec![read("t1"), read("t2")]);
        assert!(ops.writes.is_empty());
    }

    #[test]
    fn select_with_subquery_emits_read_for_every_table() {
        let ops = extract("SELECT * FROM t1 WHERE id IN (SELECT id FROM t2)");
        assert_eq!(ops.statement_kind, StatementKind::Select);
        assert_eq!(ops.reads, vec![read("t1"), read("t2")]);
    }

    #[test]
    fn cte_body_tables_emit_reads_but_cte_name_does_not() {
        let ops = extract("WITH t2 AS (SELECT id FROM t1) SELECT * FROM t2");
        assert_eq!(ops.statement_kind, StatementKind::Select);
        // Only t1 is a table reference; t2 is the CTE binding and stays out.
        assert_eq!(ops.reads, vec![read("t1")]);
    }

    #[test]
    fn unsupported_statement_reports_diagnostic() {
        let ops = extract("CREATE INDEX idx ON t1 (a)");
        assert_eq!(ops.statement_kind, StatementKind::Unsupported);
        assert!(ops.reads.is_empty());
        assert!(ops.writes.is_empty());
        assert_eq!(ops.diagnostics.len(), 1);
        assert_eq!(
            ops.diagnostics[0].kind,
            DiagnosticKind::UnsupportedStatement
        );
    }

    #[test]
    fn multiple_statements_produce_multiple_results() {
        let dialect = GenericDialect {};
        let result =
            extract_table_operations(&dialect, "SELECT * FROM t1; SELECT * FROM t2", None).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].as_ref().unwrap().reads, vec![read("t1")]);
        assert_eq!(result[1].as_ref().unwrap().reads, vec![read("t2")]);
    }

    #[test]
    fn insert_values_emits_write_only() {
        let ops = extract("INSERT INTO t1 (a, b) VALUES (1, 2)");
        assert_eq!(ops.statement_kind, StatementKind::Insert);
        assert_eq!(ops.writes, vec![write("t1")]);
        assert!(ops.reads.is_empty());
    }

    #[test]
    fn insert_select_emits_write_and_read() {
        let ops = extract("INSERT INTO t1 SELECT * FROM t2");
        assert_eq!(ops.statement_kind, StatementKind::Insert);
        assert_eq!(ops.writes, vec![write("t1")]);
        assert_eq!(ops.reads, vec![read("t2")]);
    }

    #[test]
    fn update_basic_emits_write_only() {
        let ops = extract("UPDATE t1 SET a = 1");
        assert_eq!(ops.statement_kind, StatementKind::Update);
        assert_eq!(ops.writes, vec![write("t1")]);
        assert!(ops.reads.is_empty());
    }

    #[test]
    fn update_with_subquery_predicate_emits_write_plus_read() {
        let ops = extract("UPDATE t1 SET a = 1 WHERE id IN (SELECT id FROM t2)");
        assert_eq!(ops.statement_kind, StatementKind::Update);
        assert_eq!(ops.writes, vec![write("t1")]);
        assert_eq!(ops.reads, vec![read("t2")]);
    }

    #[test]
    fn update_with_from_clause_treats_from_as_read() {
        let ops = extract_with(
            "UPDATE t1 SET a = (SELECT b FROM t3) FROM t2 WHERE t1.id IN (SELECT id FROM t4)",
            &PostgreSqlDialect {},
        );
        assert_eq!(ops.statement_kind, StatementKind::Update);
        assert_eq!(ops.writes, vec![write("t1")]);
        let read_names: std::collections::HashSet<_> = ops
            .reads
            .iter()
            .map(|r| r.table.name.value.as_str())
            .collect();
        assert_eq!(
            read_names,
            ["t2", "t3", "t4"]
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
        );
    }

    #[test]
    fn delete_from_emits_write_only() {
        let ops = extract("DELETE FROM t1");
        assert_eq!(ops.statement_kind, StatementKind::Delete);
        assert_eq!(ops.writes, vec![write("t1")]);
        assert!(ops.reads.is_empty());
    }

    #[test]
    fn delete_from_with_subquery_predicate_emits_write_plus_read() {
        let ops = extract("DELETE FROM t1 WHERE id IN (SELECT id FROM t2)");
        assert_eq!(ops.statement_kind, StatementKind::Delete);
        assert_eq!(ops.writes, vec![write("t1")]);
        assert_eq!(ops.reads, vec![read("t2")]);
    }

    #[test]
    fn delete_with_target_list_overlaps_writes_and_reads() {
        // `DELETE t1, t2 FROM t1 JOIN t2 JOIN t3` — t1 and t2 are both
        // deletion targets (writes) AND row sources (reads via FROM).
        let ops = extract_with(
            "DELETE t1, t2 FROM t1 INNER JOIN t2 INNER JOIN t3",
            &MySqlDialect {},
        );
        assert_eq!(ops.statement_kind, StatementKind::Delete);
        assert_eq!(ops.writes, vec![write("t1"), write("t2")]);
        assert_eq!(ops.reads, vec![read("t1"), read("t2"), read("t3")]);
    }

    #[test]
    fn delete_with_using_lists_target_in_writes_and_source_in_reads() {
        let ops = extract("DELETE FROM t1, t2 USING t1 INNER JOIN t2 INNER JOIN t3");
        assert_eq!(ops.statement_kind, StatementKind::Delete);
        assert_eq!(ops.writes, vec![write("t1"), write("t2")]);
        assert_eq!(ops.reads, vec![read("t1"), read("t2"), read("t3")]);
    }

    #[test]
    fn delete_resolves_target_alias_to_base_table() {
        let ops = extract_with(
            "DELETE t1_alias FROM t1 AS t1_alias JOIN t2 ON t1_alias.a = t2.a",
            &MySqlDialect {},
        );
        assert_eq!(ops.statement_kind, StatementKind::Delete);
        assert_eq!(ops.writes, vec![write("t1")]);
        assert_eq!(ops.reads, vec![read("t1"), read("t2")]);
    }

    #[test]
    fn merge_emits_write_target_and_read_source() {
        let ops = extract(
            "MERGE INTO t1 USING t2 ON t1.id = t2.id \
             WHEN MATCHED THEN UPDATE SET t1.b = t2.b",
        );
        assert_eq!(ops.statement_kind, StatementKind::Merge);
        assert_eq!(ops.writes, vec![write("t1")]);
        assert_eq!(ops.reads, vec![read("t2")]);
    }

    #[test]
    fn create_table_emits_write_only() {
        let ops = extract("CREATE TABLE t1 (a INT)");
        assert_eq!(ops.statement_kind, StatementKind::CreateTable);
        assert_eq!(ops.writes, vec![write("t1")]);
        assert!(ops.reads.is_empty());
    }

    #[test]
    fn create_table_as_select_emits_write_and_read() {
        let ops = extract("CREATE TABLE t1 AS SELECT * FROM t2");
        assert_eq!(ops.statement_kind, StatementKind::CreateTable);
        assert_eq!(ops.writes, vec![write("t1")]);
        assert_eq!(ops.reads, vec![read("t2")]);
    }

    #[test]
    fn create_view_emits_write_and_read() {
        let ops = extract("CREATE VIEW v1 AS SELECT * FROM t1");
        assert_eq!(ops.statement_kind, StatementKind::CreateView);
        assert_eq!(ops.writes, vec![write("v1")]);
        assert_eq!(ops.reads, vec![read("t1")]);
    }

    #[test]
    fn alter_table_emits_write_only() {
        let ops = extract("ALTER TABLE t1 ADD COLUMN a INT");
        assert_eq!(ops.statement_kind, StatementKind::AlterTable);
        assert_eq!(ops.writes, vec![write("t1")]);
        assert!(ops.reads.is_empty());
    }

    #[test]
    fn drop_table_emits_one_write_per_name() {
        let ops = extract("DROP TABLE t1, t2");
        assert_eq!(ops.statement_kind, StatementKind::Drop);
        assert_eq!(ops.writes, vec![write("t1"), write("t2")]);
    }

    #[test]
    fn truncate_emits_one_write_per_name() {
        let ops = extract("TRUNCATE TABLE t1, t2");
        assert_eq!(ops.statement_kind, StatementKind::Truncate);
        assert_eq!(ops.writes, vec![write("t1"), write("t2")]);
    }

    #[test]
    fn drop_function_still_unsupported() {
        // DROP variants that target non-relation objects don't carry a
        // meaningful table-level operation.
        let ops = extract("DROP FUNCTION my_fn");
        assert_eq!(ops.statement_kind, StatementKind::Unsupported);
    }

    // ─────────────────────── flows ───────────────────────

    #[test]
    fn insert_select_emits_flow_from_source_to_target() {
        let ops = extract("INSERT INTO t1 SELECT * FROM t2");
        assert_eq!(ops.flows, vec![flow("t2", "t1")]);
    }

    #[test]
    fn insert_select_join_emits_one_flow_per_source() {
        let ops = extract("INSERT INTO t1 SELECT * FROM t2 JOIN t3 ON t2.id = t3.id");
        assert_eq!(ops.flows, vec![flow("t2", "t1"), flow("t3", "t1")]);
    }

    #[test]
    fn predicate_subquery_does_not_feed_flow() {
        // t3 is referenced only inside `WHERE id IN (SELECT id FROM t3)`,
        // so it must not appear as a flow source even though it does
        // appear in `reads`.
        let ops = extract("INSERT INTO t1 SELECT * FROM t2 WHERE id IN (SELECT id FROM t3)");
        assert_eq!(ops.flows, vec![flow("t2", "t1")]);
        // ...but t3 is still visible as a touched table.
        let read_names: Vec<_> = ops
            .reads
            .iter()
            .map(|r| r.table.name.value.as_str())
            .collect();
        assert!(read_names.contains(&"t3"));
    }

    #[test]
    fn join_on_predicate_does_not_promote_to_flow() {
        let ops = extract(
            "INSERT INTO t1 SELECT * FROM t2 JOIN t3 ON t2.id = t3.id \
             AND t2.id IN (SELECT id FROM t4)",
        );
        let flows: std::collections::HashSet<_> = ops.flows.into_iter().collect();
        assert!(flows.contains(&flow("t2", "t1")));
        assert!(flows.contains(&flow("t3", "t1")));
        assert!(!flows.contains(&flow("t4", "t1")));
    }

    #[test]
    fn update_scalar_subquery_in_set_feeds_flow() {
        let ops = extract("UPDATE t1 SET col = (SELECT v FROM t2)");
        assert_eq!(ops.flows, vec![flow("t2", "t1")]);
    }

    #[test]
    fn update_predicate_subquery_does_not_feed_flow() {
        let ops = extract("UPDATE t1 SET col = 1 WHERE id IN (SELECT id FROM t2)");
        assert!(ops.flows.is_empty());
    }

    #[test]
    fn create_table_as_select_emits_flow() {
        let ops = extract("CREATE TABLE t1 AS SELECT * FROM t2");
        assert_eq!(ops.flows, vec![flow("t2", "t1")]);
    }

    #[test]
    fn create_view_emits_flow() {
        let ops = extract("CREATE VIEW v1 AS SELECT * FROM t1");
        assert_eq!(ops.flows, vec![flow("t1", "v1")]);
    }

    #[test]
    fn merge_emits_flow_from_source_to_target() {
        let ops = extract(
            "MERGE INTO t1 USING t2 ON t1.id = t2.id \
             WHEN MATCHED THEN UPDATE SET t1.b = t2.b",
        );
        assert_eq!(ops.flows, vec![flow("t2", "t1")]);
    }

    #[test]
    fn cte_data_flows_through_to_write_target() {
        let ops = extract("INSERT INTO t1 WITH cte AS (SELECT * FROM s) SELECT * FROM cte");
        assert!(ops.flows.contains(&flow("s", "t1")));
    }

    #[test]
    fn cte_predicate_subquery_does_not_leak_into_flow() {
        let ops = extract(
            "INSERT INTO t1 WITH cte AS (\
                 SELECT * FROM s WHERE id IN (SELECT id FROM x)\
             ) SELECT * FROM cte",
        );
        assert!(ops.flows.contains(&flow("s", "t1")));
        assert!(!ops.flows.contains(&flow("x", "t1")));
    }

    #[test]
    fn select_only_statement_emits_no_flows() {
        let ops = extract("SELECT * FROM t1 JOIN t2 ON t1.id = t2.id");
        assert!(ops.flows.is_empty());
    }

    #[test]
    fn insert_values_emits_no_flow() {
        let ops = extract("INSERT INTO t1 VALUES (1, 2)");
        assert!(ops.flows.is_empty());
    }

    #[test]
    fn delete_with_subquery_predicate_emits_no_flow() {
        // DELETE doesn't move data — no flow, even when a subquery
        // references another table.
        let ops = extract("DELETE FROM t1 WHERE id IN (SELECT id FROM t2)");
        assert!(ops.flows.is_empty());
    }

    #[test]
    fn truncate_emits_no_flow() {
        let ops = extract("TRUNCATE TABLE t1");
        assert!(ops.flows.is_empty());
    }
}
