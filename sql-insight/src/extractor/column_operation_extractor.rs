//! Extracts the column-level operations a SQL statement performs.
//!
//! Where [`extract_table_operations`](crate::extract_table_operations)
//! answers "what tables does this statement touch / write / flow", this
//! module answers the same questions at column granularity.
//!
//! The output mirrors `StatementTableOperations` — three parallel
//! surfaces (`reads`, `writes`, `flows`) — plus a small enrichment on
//! flow edges to distinguish passthrough projections from computed
//! expressions.
//!
//! **Current coverage** (column tracking is rolling in incrementally):
//! - `reads`: qualified column references (`t1.a`, `schema.t1.a`,
//!   `catalog.schema.t1.a`) collected from anywhere in the statement.
//!   Unqualified references (`a`) are dropped here; their scope-chain
//!   resolution lands in a later phase.
//! - `writes`: INSERT explicit column lists scoped to the INSERT
//!   target, and UPDATE SET targets scoped to the UPDATE table.
//!   Projection-derived writes (CTAS / CREATE VIEW / MERGE actions)
//!   and column-list-less INSERT SELECT are deferred.
//! - `flows`: always empty in this slice; column flow construction
//!   needs `reads` / `writes` completeness first.

use crate::catalog::Catalog;
use crate::error::Error;
use crate::extractor::operation_extractor::{
    OperationDiagnostic, OperationDiagnosticCode, StatementKind,
};
use crate::relation::TableReference;
use crate::resolver::{RawColumnRef, RelationResolver};
use sqlparser::ast::{AssignmentTarget, Ident, Statement, TableFactor};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to extract column-level operations from SQL.
///
/// `catalog` is consulted for relation-level enrichment as well as
/// future column-level needs (`SELECT *` expansion, ambiguous
/// unqualified column resolution). Pass `None` for the lightest path —
/// the MVP does not consult the catalog yet, but the signature is fixed
/// so callers don't have to migrate when it does.
pub fn extract_column_operations(
    dialect: &dyn Dialect,
    sql: &str,
    catalog: Option<&dyn Catalog>,
) -> Result<Vec<Result<StatementColumnOperations, Error>>, Error> {
    ColumnOperationExtractor::extract(dialect, sql, catalog)
}

/// Column-level operations performed by a single SQL statement.
///
/// Mirrors [`StatementTableOperations`](crate::StatementTableOperations)
/// with the same three surfaces — `reads`, `writes`, `flows` — at
/// column granularity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementColumnOperations {
    pub statement_kind: StatementKind,
    pub reads: Vec<ColumnRead>,
    pub writes: Vec<ColumnWrite>,
    pub flows: Vec<ColumnFlow>,
    pub diagnostics: Vec<OperationDiagnostic>,
}

/// A column-level identity reference: an optional owning table plus the
/// column name.
///
/// `table` is `Option` because some column references cannot be
/// resolved structurally (ambiguous unqualified columns, references to
/// derived tables we do not yet expand, etc.) — in that case a
/// diagnostic accompanies the operation. Identity is name-based: two
/// `ColumnReference`s with the same `table` and `name` compare equal,
/// independent of where they appeared in the SQL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColumnReference {
    pub table: Option<TableReference>,
    pub name: Ident,
}

/// A column referenced as a Read source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnRead {
    pub column: ColumnReference,
}

/// A column that the statement writes to — an INSERT target column,
/// an UPDATE SET target, a MERGE WHEN clause target, or a column of
/// the new relation produced by CTAS / CREATE VIEW.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnWrite {
    pub column: ColumnReference,
}

/// A column-level flow edge: data from `source` contributes to
/// `target`. Emitted for both persisted-target statements (INSERT /
/// UPDATE / MERGE / CTAS / CREATE VIEW) and bare SELECT (where target
/// is a `ColumnTarget::QueryOutput`).
///
/// One edge per (source, target) pair: `SELECT a + b FROM t1` emits two
/// flows, both from `t1.a` and `t1.b` to the same query-output target,
/// each tagged `Computed`.
///
/// Statements that physically move data emit composed end-to-end flows
/// — `INSERT INTO t1 (col) SELECT b FROM t2` emits `t2.b → t1.col`
/// directly, with no intermediate query-output entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnFlow {
    pub source: ColumnReference,
    pub target: ColumnTarget,
    pub kind: ColumnFlowKind,
}

/// The target endpoint of a [`ColumnFlow`].
///
/// `Persisted` covers columns that live in a real relation (table or
/// view) and receive a value from the statement (INSERT target,
/// UPDATE SET target, MERGE INSERT/UPDATE target, CTAS / CREATE VIEW
/// output column).
///
/// `QueryOutput` covers transient columns produced by a SELECT
/// projection that is not piped into a persisted relation. `name`
/// follows the projection: the alias if explicit, the bare column name
/// if the projection is a single column, otherwise `None`. `position`
/// is always set so anonymous outputs can be identified.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ColumnTarget {
    Persisted(ColumnReference),
    QueryOutput {
        name: Option<Ident>,
        position: usize,
    },
}

/// How a source column contributes to its target.
///
/// MVP carries two variants:
/// - `Passthrough` — the source value is forwarded unchanged
///   (`SELECT a FROM t1`, `INSERT INTO t1 (a) SELECT b FROM t2`).
/// - `Computed` — the source feeds an expression that produces the
///   target (`SELECT a + b FROM t1`, both `a` and `b` are `Computed`).
///
/// More variants (`Aggregation`, plus predicate-influence kinds like
/// `Filter` / `Join` / `GroupBy` / `Sort` / `Window` / `Conditional`)
/// will be added incrementally as later phases tighten the
/// classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColumnFlowKind {
    Passthrough,
    Computed,
}

/// Extracts column-level operations from SQL.
#[derive(Default, Debug)]
pub struct ColumnOperationExtractor;

impl ColumnOperationExtractor {
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
        catalog: Option<&dyn Catalog>,
    ) -> Result<Vec<Result<StatementColumnOperations, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        Ok(statements
            .iter()
            .map(|s| Self::extract_from_statement(s, catalog))
            .collect())
    }

    pub fn extract_from_statement(
        statement: &Statement,
        catalog: Option<&dyn Catalog>,
    ) -> Result<StatementColumnOperations, Error> {
        let kind = super::operation_extractor::classify_statement(statement);
        let mut diagnostics = Vec::new();

        if matches!(kind, StatementKind::Unsupported) {
            diagnostics.push(OperationDiagnostic {
                code: OperationDiagnosticCode::UnsupportedStatement,
                message: format!(
                    "Unsupported statement for column operation extraction: {}",
                    statement
                ),
            });
            return Ok(StatementColumnOperations {
                statement_kind: kind,
                reads: Vec::new(),
                writes: Vec::new(),
                flows: Vec::new(),
                diagnostics,
            });
        }

        let resolution = RelationResolver::resolve_statement(catalog, statement)?;
        let reads = collect_qualified_reads(&resolution.column_refs);
        let writes = collect_writes(statement)?;

        Ok(StatementColumnOperations {
            statement_kind: kind,
            reads,
            writes,
            flows: Vec::new(),
            diagnostics,
        })
    }
}

/// Filter the resolver's raw column refs down to qualified ones and
/// convert them into [`ColumnRead`]. Unqualified refs need scope-chain
/// resolution and are dropped here.
fn collect_qualified_reads(column_refs: &[RawColumnRef]) -> Vec<ColumnRead> {
    column_refs
        .iter()
        .filter_map(|raw| column_ref_from_parts(&raw.parts))
        .map(|column| ColumnRead { column })
        .collect()
}

/// Build a `ColumnReference` from a CompoundIdentifier's parts.
///
/// The last part is always the column name; the preceding parts form
/// the table identifier (`t1`, `schema.t1`, `catalog.schema.t1`).
/// Returns `None` for unqualified inputs (1 part — handled elsewhere
/// via scope-chain resolution) and 5+ part inputs (likely struct field
/// access on a qualified column, out of MVP scope).
fn column_ref_from_parts(parts: &[Ident]) -> Option<ColumnReference> {
    let (col, table_parts) = match parts.split_last() {
        Some((col, rest)) if !rest.is_empty() => (col.clone(), rest),
        _ => return None,
    };
    let table = match table_parts.len() {
        1 => TableReference {
            catalog: None,
            schema: None,
            name: table_parts[0].clone(),
        },
        2 => TableReference {
            catalog: None,
            schema: Some(table_parts[0].clone()),
            name: table_parts[1].clone(),
        },
        3 => TableReference {
            catalog: Some(table_parts[0].clone()),
            schema: Some(table_parts[1].clone()),
            name: table_parts[2].clone(),
        },
        _ => return None,
    };
    Some(ColumnReference {
        table: Some(table),
        name: col,
    })
}

/// Statement-specific write extraction. Covered:
/// - INSERT explicit column list → writes scoped to the INSERT target.
/// - UPDATE SET targets → writes scoped to the UPDATE target table
///   (qualifier is honored when the SET target is qualified, otherwise
///   the UPDATE head provides the table).
///
/// MERGE, CTAS, CREATE VIEW writes need projection-derived column
/// names and land in a later phase.
fn collect_writes(statement: &Statement) -> Result<Vec<ColumnWrite>, Error> {
    let mut writes = Vec::new();
    match statement {
        Statement::Insert(insert) => {
            if !insert.columns.is_empty() {
                let target = TableReference::try_from(insert)?;
                for col in &insert.columns {
                    writes.push(ColumnWrite {
                        column: ColumnReference {
                            table: Some(target.clone()),
                            name: col.clone(),
                        },
                    });
                }
            }
        }
        Statement::Update(update) => {
            let default_table = match &update.table.relation {
                TableFactor::Table { .. } => {
                    Some(TableReference::try_from(&update.table.relation)?)
                }
                _ => None,
            };
            for assignment in &update.assignments {
                if let Some(column) =
                    column_ref_from_assignment_target(&assignment.target, default_table.as_ref())
                {
                    writes.push(ColumnWrite { column });
                }
            }
        }
        _ => {}
    }
    Ok(writes)
}

/// Resolve a SET assignment target to a `ColumnReference`. If the
/// target is qualified (`t1.a`), the qualifier wins; otherwise the
/// `default_table` (the UPDATE head) provides the table.
fn column_ref_from_assignment_target(
    target: &AssignmentTarget,
    default_table: Option<&TableReference>,
) -> Option<ColumnReference> {
    let name = match target {
        AssignmentTarget::ColumnName(name) => name,
        AssignmentTarget::Tuple(_) => return None,
    };
    let idents: Vec<Ident> = name
        .0
        .iter()
        .map(|p| p.as_ident().cloned())
        .collect::<Option<Vec<_>>>()?;
    match idents.len() {
        1 => Some(ColumnReference {
            table: default_table.cloned(),
            name: idents.into_iter().next().unwrap(),
        }),
        2..=4 => column_ref_from_parts(&idents),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::GenericDialect;

    fn extract(sql: &str) -> StatementColumnOperations {
        let mut result = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
        result.remove(0).unwrap()
    }

    fn table(name: &str) -> TableReference {
        TableReference {
            catalog: None,
            schema: None,
            name: name.into(),
        }
    }

    fn read(table_name: &str, col: &str) -> ColumnRead {
        ColumnRead {
            column: ColumnReference {
                table: Some(table(table_name)),
                name: col.into(),
            },
        }
    }

    fn write(table_name: &str, col: &str) -> ColumnWrite {
        ColumnWrite {
            column: ColumnReference {
                table: Some(table(table_name)),
                name: col.into(),
            },
        }
    }

    // ───────── reads: qualified-only ─────────

    #[test]
    fn unqualified_select_yields_no_reads() {
        let ops = extract("SELECT a, b FROM t1");
        assert_eq!(ops.statement_kind, StatementKind::Select);
        assert!(ops.reads.is_empty());
    }

    #[test]
    fn qualified_select_collects_qualified_reads() {
        let ops = extract("SELECT t1.a, t1.b FROM t1");
        assert_eq!(ops.reads, vec![read("t1", "a"), read("t1", "b")]);
    }

    #[test]
    fn qualified_join_collects_reads_from_both_sides() {
        // Resolver walks FROM (including JOIN ON) before the projection,
        // so the predicate columns appear ahead of the projected ones.
        let ops = extract("SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.id = t2.id");
        assert_eq!(
            ops.reads,
            vec![
                read("t1", "id"),
                read("t2", "id"),
                read("t1", "a"),
                read("t2", "b"),
            ]
        );
    }

    #[test]
    fn schema_qualified_ref_resolves_to_schema_dot_table() {
        let ops = extract("SELECT s1.t1.a FROM s1.t1");
        let table_ref = TableReference {
            catalog: None,
            schema: Some("s1".into()),
            name: "t1".into(),
        };
        assert_eq!(
            ops.reads,
            vec![ColumnRead {
                column: ColumnReference {
                    table: Some(table_ref),
                    name: "a".into(),
                },
            }]
        );
    }

    #[test]
    fn where_predicate_qualified_ref_is_a_read() {
        let ops = extract("SELECT t1.a FROM t1 WHERE t1.b > 0");
        assert_eq!(ops.reads, vec![read("t1", "a"), read("t1", "b")]);
    }

    // ───────── writes: INSERT explicit column list ─────────

    #[test]
    fn insert_with_explicit_columns_writes_those_columns_on_target() {
        let ops = extract("INSERT INTO t1 (a, b) VALUES (1, 2)");
        assert_eq!(ops.writes, vec![write("t1", "a"), write("t1", "b")]);
        assert!(ops.reads.is_empty());
    }

    #[test]
    fn insert_select_records_target_writes_and_qualified_source_reads() {
        let ops = extract("INSERT INTO t1 (a) SELECT t2.b FROM t2");
        assert_eq!(ops.writes, vec![write("t1", "a")]);
        assert_eq!(ops.reads, vec![read("t2", "b")]);
    }

    #[test]
    fn insert_without_explicit_columns_yields_no_writes() {
        let ops = extract("INSERT INTO t1 SELECT t2.b FROM t2");
        assert!(ops.writes.is_empty());
        assert_eq!(ops.reads, vec![read("t2", "b")]);
    }

    // ───────── writes: UPDATE SET targets ─────────

    #[test]
    fn update_set_targets_become_writes_on_update_table() {
        let ops = extract("UPDATE t1 SET a = 1");
        assert_eq!(ops.writes, vec![write("t1", "a")]);
    }

    #[test]
    fn update_set_qualified_target_keeps_qualifier() {
        let ops = extract("UPDATE t1 SET t1.a = 1");
        assert_eq!(ops.writes, vec![write("t1", "a")]);
    }

    #[test]
    fn update_set_rhs_qualified_ref_is_a_read() {
        let ops = extract("UPDATE t1 SET a = t2.b FROM t2 WHERE t1.id = t2.id");
        assert_eq!(ops.writes, vec![write("t1", "a")]);
        assert_eq!(
            ops.reads,
            vec![read("t2", "b"), read("t1", "id"), read("t2", "id")]
        );
    }

    // ───────── delete / DDL ─────────

    #[test]
    fn delete_qualified_predicate_is_a_read() {
        let ops = extract("DELETE FROM t1 WHERE t1.id = 5");
        assert_eq!(ops.reads, vec![read("t1", "id")]);
        assert!(ops.writes.is_empty());
    }

    #[test]
    fn create_table_definitions_are_not_writes() {
        let ops = extract("CREATE TABLE t1 (a INT, b INT)");
        assert!(ops.reads.is_empty());
        assert!(ops.writes.is_empty());
    }

    // ───────── diagnostics / structure ─────────

    #[test]
    fn unsupported_statement_reports_diagnostic() {
        let ops = extract("CREATE INDEX idx ON t1 (a)");
        assert_eq!(ops.statement_kind, StatementKind::Unsupported);
        assert!(ops.reads.is_empty());
        assert!(ops.writes.is_empty());
        assert_eq!(ops.diagnostics.len(), 1);
        assert_eq!(
            ops.diagnostics[0].code,
            OperationDiagnosticCode::UnsupportedStatement
        );
    }

    #[test]
    fn multiple_statements_produce_multiple_results() {
        let result = extract_column_operations(
            &GenericDialect {},
            "SELECT t1.a FROM t1; SELECT t2.b FROM t2",
            None,
        )
        .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].as_ref().unwrap().reads, vec![read("t1", "a")]);
        assert_eq!(result[1].as_ref().unwrap().reads, vec![read("t2", "b")]);
    }

    #[test]
    fn wildcard_select_yields_no_column_ops() {
        let ops = extract("SELECT * FROM t1");
        assert!(ops.reads.is_empty());
        assert!(ops.writes.is_empty());
    }
}
