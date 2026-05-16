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
//! This is the entry point for the operation-facts story laid out in the
//! project roadmap; the MVP currently focuses on table-level operations.
//! `usages` enrichment and richer `table_flows` arrive in later steps.

use crate::catalog::Catalog;
use crate::error::Error;
use crate::operation::TableRole;
use crate::relation::TableReference;
use crate::resolver::RelationResolver;
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
/// use sql_insight::{extract_table_operations, StatementKind, TableRole};
///
/// let dialect = GenericDialect {};
/// let result = extract_table_operations(&dialect, "SELECT * FROM users", None).unwrap();
/// let ops = result[0].as_ref().unwrap();
/// assert_eq!(ops.statement_kind, StatementKind::Select);
/// assert_eq!(ops.table_operations.len(), 1);
/// assert_eq!(ops.table_operations[0].role, TableRole::Read);
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
    pub table_operations: Vec<TableOperation>,
    pub table_flows: Vec<TableFlow>,
    pub diagnostics: Vec<OperationDiagnostic>,
}

/// What a statement does, at a coarse level. The *verb* of the statement
/// — INSERT vs CREATE TABLE vs MERGE vs … — combined with the per-table
/// [`TableRole`] (`Read`/`Write`) recovers every distinction the project
/// needs to make at table granularity.
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

/// A single operation on a single table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableOperation {
    pub table: TableReference,
    pub role: TableRole,
    /// Contextual hints about where in the statement the table was touched.
    /// Empty in the MVP; populated in later phases.
    pub usages: Vec<TableUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableUsage {
    Target,
    From,
    Projection,
    Predicate,
    Join,
    WriteValue,
}

/// A source-to-target table flow inferred from the statement structure.
///
/// Emitted only for statements that physically move data into a target
/// (`INSERT`, `UPDATE`, `MERGE`, `CREATE TABLE AS SELECT`, `CREATE VIEW`).
/// `DELETE`, `DROP`, `TRUNCATE`, `ALTER`, and bare `SELECT` produce no
/// flows even when they reference other tables — the touched tables are
/// still visible through [`StatementTableOperations::table_operations`].
///
/// Each `TableFlow` is a single directed edge — a statement that derives
/// `t` from `a JOIN b` emits two flows (`a → t`, `b → t`), not one entry
/// with both sources. This keeps equality and aggregation across
/// statements simple (set-union over edges).
///
/// Tables referenced only inside a predicate subquery are excluded:
/// `INSERT INTO t SELECT FROM s WHERE id IN (SELECT id FROM x)` emits
/// `s → t` but not `x → t`. `x` remains visible via `table_operations`.
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

/// A non-fatal diagnostic specific to operation extraction. Distinct from
/// the resolver-level [`Diagnostic`](crate::Diagnostic) because the codes
/// here speak the operations vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationDiagnostic {
    pub code: OperationDiagnosticCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum OperationDiagnosticCode {
    UnsupportedStatement,
    UnsupportedTableFactor,
    AmbiguousColumn,
    CatalogRequired,
    DynamicSql,
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
        let resolution = RelationResolver::resolve_statement(catalog, statement)?;

        let mut table_operations = Vec::new();
        let mut diagnostics = Vec::new();

        if matches!(kind, StatementKind::Unsupported) {
            diagnostics.push(OperationDiagnostic {
                code: OperationDiagnosticCode::UnsupportedStatement,
                message: format!(
                    "Unsupported statement for operation extraction: {}",
                    statement
                ),
            });
        } else {
            // Each table binding becomes one TableOperation. When a
            // binding carries multiple roles (e.g. `DELETE t1 FROM t1`),
            // Write wins over Read — fine-grained "Write *and* From"
            // attribution belongs to the future `usages` enrichment.
            for binding in resolution.table_bindings() {
                let role = primary_role(&binding.roles);
                table_operations.push(TableOperation {
                    table: binding.table,
                    role,
                    usages: Vec::new(),
                });
            }
        }

        let table_flows = extract_table_flows(&resolution, &kind);

        Ok(StatementTableOperations {
            statement_kind: kind,
            table_operations,
            table_flows,
            diagnostics,
        })
    }
}

/// Emit one `TableFlow` edge per (feeding source × write target) pair
/// for statements that physically move data. Statements without a write
/// target or without any data-feeding source produce no flows.
fn extract_table_flows(
    resolution: &crate::resolver::RelationResolution,
    kind: &StatementKind,
) -> Vec<TableFlow> {
    if !is_data_moving(kind) {
        return Vec::new();
    }
    // Data-moving statements all carry exactly one write target. If
    // somehow zero or many appear (parser oddity, unsupported variant)
    // we conservatively emit no flows rather than guessing.
    let mut targets = resolution.write_target_tables().into_iter();
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

fn classify_statement(statement: &Statement) -> StatementKind {
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

fn primary_role(roles: &[TableRole]) -> TableRole {
    if roles.contains(&TableRole::Write) {
        TableRole::Write
    } else {
        TableRole::Read
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
            alias: None,
        }
    }

    fn table_alias(name: &str, alias: &str) -> TableReference {
        TableReference {
            alias: Some(alias.into()),
            ..table(name)
        }
    }

    fn op(table: TableReference, role: TableRole) -> TableOperation {
        TableOperation {
            table,
            role,
            usages: vec![],
        }
    }

    #[test]
    fn select_emits_source_operations() {
        let ops = extract("SELECT * FROM users");
        assert_eq!(ops.statement_kind, StatementKind::Select);
        assert_eq!(
            ops.table_operations,
            vec![op(table("users"), TableRole::Read)]
        );
        assert!(ops.table_flows.is_empty());
        assert!(ops.diagnostics.is_empty());
    }

    #[test]
    fn select_with_join_emits_one_source_per_table() {
        let ops = extract("SELECT * FROM t1 JOIN t2 ON t1.id = t2.id");
        assert_eq!(ops.statement_kind, StatementKind::Select);
        let tables: Vec<_> = ops.table_operations.iter().map(|op| &op.table).collect();
        assert_eq!(tables, vec![&table("t1"), &table("t2")]);
        assert!(ops
            .table_operations
            .iter()
            .all(|op| op.role == TableRole::Read));
    }

    #[test]
    fn select_with_subquery_emits_source_for_every_table() {
        let ops = extract("SELECT * FROM t1 WHERE id IN (SELECT id FROM t2)");
        assert_eq!(ops.statement_kind, StatementKind::Select);
        let tables: Vec<_> = ops.table_operations.iter().map(|op| &op.table).collect();
        assert_eq!(tables, vec![&table("t1"), &table("t2")]);
    }

    #[test]
    fn cte_body_tables_emit_sources_but_cte_name_does_not() {
        let ops = extract("WITH t2 AS (SELECT id FROM t1) SELECT * FROM t2");
        assert_eq!(ops.statement_kind, StatementKind::Select);
        // Only t1 is a table reference; t2 is the CTE binding and stays out.
        let tables: Vec<_> = ops.table_operations.iter().map(|op| &op.table).collect();
        assert_eq!(tables, vec![&table("t1")]);
    }

    #[test]
    fn unsupported_statement_reports_diagnostic() {
        // `CREATE INDEX` doesn't fit the operation vocabulary — no Table-level
        // operation, just an index attached to a table — so it still falls
        // through to Unsupported.
        let ops = extract("CREATE INDEX idx ON t1 (a)");
        assert_eq!(ops.statement_kind, StatementKind::Unsupported);
        assert!(ops.table_operations.is_empty());
        assert_eq!(ops.diagnostics.len(), 1);
        assert_eq!(
            ops.diagnostics[0].code,
            OperationDiagnosticCode::UnsupportedStatement
        );
    }

    #[test]
    fn multiple_statements_produce_multiple_results() {
        let dialect = GenericDialect {};
        let result =
            extract_table_operations(&dialect, "SELECT * FROM t1; SELECT * FROM t2", None).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0].as_ref().unwrap().table_operations[0].table,
            table("t1")
        );
        assert_eq!(
            result[1].as_ref().unwrap().table_operations[0].table,
            table("t2")
        );
    }

    #[test]
    fn insert_values_emits_target_only() {
        let ops = extract("INSERT INTO t1 (a, b) VALUES (1, 2)");
        assert_eq!(ops.statement_kind, StatementKind::Insert);
        assert_eq!(
            ops.table_operations,
            vec![op(table("t1"), TableRole::Write)]
        );
    }

    #[test]
    fn insert_select_emits_target_then_source() {
        let ops = extract("INSERT INTO t1 SELECT * FROM t2");
        assert_eq!(ops.statement_kind, StatementKind::Insert);
        assert_eq!(
            ops.table_operations,
            vec![
                op(table("t1"), TableRole::Write),
                op(table("t2"), TableRole::Read),
            ]
        );
    }

    #[test]
    fn update_basic_emits_target_only() {
        let ops = extract("UPDATE t1 SET a = 1");
        assert_eq!(ops.statement_kind, StatementKind::Update);
        assert_eq!(
            ops.table_operations,
            vec![op(table("t1"), TableRole::Write)]
        );
    }

    #[test]
    fn update_with_subquery_predicate_emits_target_plus_source() {
        let ops = extract("UPDATE t1 SET a = 1 WHERE id IN (SELECT id FROM t2)");
        assert_eq!(ops.statement_kind, StatementKind::Update);
        assert_eq!(
            ops.table_operations,
            vec![
                op(table("t1"), TableRole::Write),
                op(table("t2"), TableRole::Read),
            ]
        );
    }

    #[test]
    fn update_with_from_clause_treats_from_as_source() {
        let ops = extract_with(
            "UPDATE t1 SET a = (SELECT b FROM t3) FROM t2 WHERE t1.id IN (SELECT id FROM t4)",
            &PostgreSqlDialect {},
        );
        assert_eq!(ops.statement_kind, StatementKind::Update);
        let roles: Vec<_> = ops
            .table_operations
            .iter()
            .map(|op| (op.table.name.value.as_str(), op.role.clone()))
            .collect();
        assert_eq!(roles[0], ("t1", TableRole::Write));
        let source_names: std::collections::HashSet<_> =
            roles[1..].iter().map(|(n, _)| *n).collect();
        assert_eq!(
            source_names,
            ["t2", "t3", "t4"]
                .into_iter()
                .collect::<std::collections::HashSet<_>>(),
        );
    }

    #[test]
    fn delete_from_emits_target_only() {
        let ops = extract("DELETE FROM t1");
        assert_eq!(ops.statement_kind, StatementKind::Delete);
        assert_eq!(
            ops.table_operations,
            vec![op(table("t1"), TableRole::Write)]
        );
    }

    #[test]
    fn delete_from_with_subquery_predicate_emits_target_plus_source() {
        let ops = extract("DELETE FROM t1 WHERE id IN (SELECT id FROM t2)");
        assert_eq!(ops.statement_kind, StatementKind::Delete);
        assert_eq!(
            ops.table_operations,
            vec![
                op(table("t1"), TableRole::Write),
                op(table("t2"), TableRole::Read),
            ]
        );
    }

    #[test]
    fn delete_with_target_list_separates_targets_from_sources() {
        let ops = extract_with(
            "DELETE t1, t2 FROM t1 INNER JOIN t2 INNER JOIN t3",
            &MySqlDialect {},
        );
        assert_eq!(ops.statement_kind, StatementKind::Delete);
        assert_eq!(
            ops.table_operations,
            vec![
                op(table("t1"), TableRole::Write),
                op(table("t2"), TableRole::Write),
                op(table("t3"), TableRole::Read),
            ]
        );
    }

    #[test]
    fn delete_with_using_classifies_from_as_targets_and_using_as_sources() {
        let ops = extract("DELETE FROM t1, t2 USING t1 INNER JOIN t2 INNER JOIN t3");
        assert_eq!(ops.statement_kind, StatementKind::Delete);
        let roles: Vec<_> = ops
            .table_operations
            .iter()
            .map(|op| (op.table.name.value.as_str(), op.role.clone()))
            .collect();
        let targets: Vec<_> = roles
            .iter()
            .filter(|(_, r)| *r == TableRole::Write)
            .map(|(n, _)| *n)
            .collect();
        let sources: Vec<_> = roles
            .iter()
            .filter(|(_, r)| *r == TableRole::Read)
            .map(|(n, _)| *n)
            .collect();
        assert_eq!(targets, vec!["t1", "t2"]);
        assert_eq!(sources, vec!["t3"]);
    }

    #[test]
    fn delete_resolves_target_alias_to_base_table() {
        let ops = extract_with(
            "DELETE t1_alias FROM t1 AS t1_alias JOIN t2 ON t1_alias.a = t2.a",
            &MySqlDialect {},
        );
        assert_eq!(ops.statement_kind, StatementKind::Delete);
        assert_eq!(
            ops.table_operations,
            vec![
                op(table_alias("t1", "t1_alias"), TableRole::Write),
                op(table("t2"), TableRole::Read),
            ]
        );
    }

    #[test]
    fn merge_emits_target_and_source() {
        let ops = extract(
            "MERGE INTO t1 USING t2 ON t1.id = t2.id \
             WHEN MATCHED THEN UPDATE SET t1.b = t2.b",
        );
        assert_eq!(ops.statement_kind, StatementKind::Merge);
        assert_eq!(
            ops.table_operations,
            vec![
                op(table("t1"), TableRole::Write),
                op(table("t2"), TableRole::Read),
            ]
        );
    }

    #[test]
    fn create_table_emits_target_only() {
        let ops = extract("CREATE TABLE t1 (a INT)");
        assert_eq!(ops.statement_kind, StatementKind::CreateTable);
        assert_eq!(
            ops.table_operations,
            vec![op(table("t1"), TableRole::Write)]
        );
    }

    #[test]
    fn create_table_as_select_emits_target_then_source() {
        let ops = extract("CREATE TABLE t1 AS SELECT * FROM t2");
        assert_eq!(ops.statement_kind, StatementKind::CreateTable);
        assert_eq!(
            ops.table_operations,
            vec![
                op(table("t1"), TableRole::Write),
                op(table("t2"), TableRole::Read),
            ]
        );
    }

    #[test]
    fn create_view_emits_target_then_source() {
        let ops = extract("CREATE VIEW v1 AS SELECT * FROM t1");
        assert_eq!(ops.statement_kind, StatementKind::CreateView);
        assert_eq!(
            ops.table_operations,
            vec![
                op(table("v1"), TableRole::Write),
                op(table("t1"), TableRole::Read),
            ]
        );
    }

    #[test]
    fn alter_table_emits_target_only() {
        let ops = extract("ALTER TABLE t1 ADD COLUMN a INT");
        assert_eq!(ops.statement_kind, StatementKind::AlterTable);
        assert_eq!(
            ops.table_operations,
            vec![op(table("t1"), TableRole::Write)]
        );
    }

    #[test]
    fn drop_table_emits_target_per_name() {
        let ops = extract("DROP TABLE t1, t2");
        assert_eq!(ops.statement_kind, StatementKind::Drop);
        assert_eq!(
            ops.table_operations,
            vec![
                op(table("t1"), TableRole::Write),
                op(table("t2"), TableRole::Write),
            ]
        );
    }

    #[test]
    fn truncate_emits_target_per_name() {
        let ops = extract("TRUNCATE TABLE t1, t2");
        assert_eq!(ops.statement_kind, StatementKind::Truncate);
        assert_eq!(
            ops.table_operations,
            vec![
                op(table("t1"), TableRole::Write),
                op(table("t2"), TableRole::Write),
            ]
        );
    }

    #[test]
    fn drop_function_still_unsupported() {
        // DROP variants that target non-relation objects (functions,
        // schemas, etc.) don't carry a meaningful Table-level operation.
        let ops = extract("DROP FUNCTION my_fn");
        assert_eq!(ops.statement_kind, StatementKind::Unsupported);
    }

    // ─────────────────────── table_flows ───────────────────────

    fn flow(source: &str, target: &str) -> TableFlow {
        TableFlow {
            source: table(source),
            target: table(target),
        }
    }

    #[test]
    fn insert_select_emits_flow_from_source_to_target() {
        let ops = extract("INSERT INTO t1 SELECT * FROM t2");
        assert_eq!(ops.table_flows, vec![flow("t2", "t1")]);
    }

    #[test]
    fn insert_select_join_emits_one_flow_per_source() {
        let ops = extract("INSERT INTO t1 SELECT * FROM t2 JOIN t3 ON t2.id = t3.id");
        assert_eq!(ops.table_flows, vec![flow("t2", "t1"), flow("t3", "t1")]);
    }

    #[test]
    fn predicate_subquery_does_not_feed_flow() {
        // t3 is referenced only inside `WHERE id IN (SELECT id FROM t3)`,
        // so it must not appear as a flow source even though it does
        // appear in `table_operations`.
        let ops = extract("INSERT INTO t1 SELECT * FROM t2 WHERE id IN (SELECT id FROM t3)");
        assert_eq!(ops.table_flows, vec![flow("t2", "t1")]);
        // ...but t3 is still visible as a touched table.
        let touched: Vec<_> = ops
            .table_operations
            .iter()
            .map(|op| op.table.name.value.as_str())
            .collect();
        assert!(touched.contains(&"t3"));
    }

    #[test]
    fn join_on_predicate_does_not_promote_to_flow() {
        // The ON-clause subquery's t3 is a predicate dependency, not a
        // data source. Only t2 should appear in flows.
        let ops = extract(
            "INSERT INTO t1 SELECT * FROM t2 JOIN t3 ON t2.id = t3.id \
             AND t2.id IN (SELECT id FROM t4)",
        );
        let flows: std::collections::HashSet<_> = ops.table_flows.into_iter().collect();
        assert!(flows.contains(&flow("t2", "t1")));
        assert!(flows.contains(&flow("t3", "t1")));
        assert!(!flows.contains(&flow("t4", "t1")));
    }

    #[test]
    fn update_scalar_subquery_in_set_feeds_flow() {
        let ops = extract("UPDATE t1 SET col = (SELECT v FROM t2)");
        assert_eq!(ops.table_flows, vec![flow("t2", "t1")]);
    }

    #[test]
    fn update_predicate_subquery_does_not_feed_flow() {
        let ops = extract("UPDATE t1 SET col = 1 WHERE id IN (SELECT id FROM t2)");
        assert!(ops.table_flows.is_empty());
    }

    #[test]
    fn create_table_as_select_emits_flow() {
        let ops = extract("CREATE TABLE t1 AS SELECT * FROM t2");
        assert_eq!(ops.table_flows, vec![flow("t2", "t1")]);
    }

    #[test]
    fn create_view_emits_flow() {
        let ops = extract("CREATE VIEW v1 AS SELECT * FROM t1");
        assert_eq!(ops.table_flows, vec![flow("t1", "v1")]);
    }

    #[test]
    fn merge_emits_flow_from_source_to_target() {
        let ops = extract(
            "MERGE INTO t1 USING t2 ON t1.id = t2.id \
             WHEN MATCHED THEN UPDATE SET t1.b = t2.b",
        );
        assert_eq!(ops.table_flows, vec![flow("t2", "t1")]);
    }

    #[test]
    fn cte_data_flows_through_to_write_target() {
        // CTE name itself is not a physical table, but its body's source
        // (s) sits in a Body-chain from CTE → outer SELECT → INSERT
        // target, so the flow s → t1 should be emitted.
        let ops = extract("INSERT INTO t1 WITH cte AS (SELECT * FROM s) SELECT * FROM cte");
        assert!(ops.table_flows.contains(&flow("s", "t1")));
    }

    #[test]
    fn cte_predicate_subquery_does_not_leak_into_flow() {
        // Inside the CTE body, x sits in a Predicate scope; it must not
        // feed t even though the CTE itself feeds t.
        let ops = extract(
            "INSERT INTO t1 WITH cte AS (\
                 SELECT * FROM s WHERE id IN (SELECT id FROM x)\
             ) SELECT * FROM cte",
        );
        assert!(ops.table_flows.contains(&flow("s", "t1")));
        assert!(!ops.table_flows.contains(&flow("x", "t1")));
    }

    #[test]
    fn select_only_statement_emits_no_flows() {
        let ops = extract("SELECT * FROM t1 JOIN t2 ON t1.id = t2.id");
        assert!(ops.table_flows.is_empty());
    }

    #[test]
    fn insert_values_emits_no_flow() {
        let ops = extract("INSERT INTO t1 VALUES (1, 2)");
        assert!(ops.table_flows.is_empty());
    }

    #[test]
    fn delete_with_subquery_predicate_emits_no_flow() {
        // DELETE doesn't move data — no flow, even when a subquery
        // references another table.
        let ops = extract("DELETE FROM t1 WHERE id IN (SELECT id FROM t2)");
        assert!(ops.table_flows.is_empty());
    }

    #[test]
    fn truncate_emits_no_flow() {
        let ops = extract("TRUNCATE TABLE t1");
        assert!(ops.table_flows.is_empty());
    }
}
