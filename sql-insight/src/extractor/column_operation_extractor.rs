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
//! - `reads`: qualified column references decompose directly to
//!   `TableReference + name`; unqualified ones are resolved against
//!   the scope chain. A unique candidate binding wins; 0 or 2+
//!   candidates leave `table: None` (the column name still surfaces).
//! - `writes`: INSERT explicit column lists scoped to the INSERT
//!   target, and UPDATE SET targets scoped to the UPDATE table.
//!   Projection-derived writes (CTAS / CREATE VIEW / MERGE actions)
//!   and column-list-less INSERT SELECT are deferred.
//! - `flows`: per-projection-item edges for SELECT (target =
//!   `QueryOutput { name, position }`), positionally paired
//!   `source-column → target-column` edges for INSERT with explicit
//!   column list, and per-assignment edges for UPDATE SET. Each edge
//!   is tagged `Passthrough` (bare ref) or `Computed` (expression
//!   evaluation). MERGE clauses, CTAS / CREATE VIEW, INSERT without
//!   explicit columns, UNION-position fan-out, and predicate-side
//!   influence (Filter / Join / GroupBy / Sort / Window / Conditional)
//!   are deferred.
//!
//! **Strictness scales with the catalog.** Without a catalog, Table
//! bindings have `Unknown` schemas and unqualified refs to a
//! single-table scope resolve unconditionally (best-effort, matches
//! the implicit promise of `catalog: None`). With a catalog, Table
//! schemas come back `Known(cols)` and unqualified refs only resolve
//! when the candidate's schema actually lists the column — column
//! typos that would otherwise silently resolve become unresolved.

use crate::catalog::Catalog;
use crate::error::Error;
use crate::extractor::operation_extractor::{
    OperationDiagnostic, OperationDiagnosticCode, StatementKind,
};
use crate::relation::TableReference;
use crate::resolver::{FlowTargetSpec, RawColumnRef, RelationResolution, RelationResolver};
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
        let reads = collect_reads(&resolution);
        let writes = collect_writes(statement)?;
        let flows = extract_flows(&resolution);

        Ok(StatementColumnOperations {
            statement_kind: kind,
            reads,
            writes,
            flows,
            diagnostics,
        })
    }
}

/// Map the resolver's pre-built `flow_edges` 1:1 to public
/// `ColumnFlow`. Sources go through scope-chain resolution; targets
/// are already fully spec'd by the resolver.
fn extract_flows(resolution: &RelationResolution) -> Vec<ColumnFlow> {
    resolution
        .flow_edges
        .iter()
        .filter_map(|edge| {
            let source = resolve_raw_ref(&edge.source, resolution)?;
            let target = match &edge.target {
                FlowTargetSpec::QueryOutput { name, position } => ColumnTarget::QueryOutput {
                    name: name.clone(),
                    position: *position,
                },
                FlowTargetSpec::Persisted { table, column } => {
                    ColumnTarget::Persisted(ColumnReference {
                        table: Some(table.clone()),
                        name: column.clone(),
                    })
                }
            };
            let kind = if edge.bare {
                ColumnFlowKind::Passthrough
            } else {
                ColumnFlowKind::Computed
            };
            Some(ColumnFlow {
                source,
                target,
                kind,
            })
        })
        .collect()
}

fn resolve_raw_ref(raw: &RawColumnRef, resolution: &RelationResolution) -> Option<ColumnReference> {
    match raw.parts.len() {
        0 => None,
        1 => {
            let name = raw.parts[0].clone();
            let table = resolution.resolve_unqualified_column(&name, raw.scope_id);
            Some(ColumnReference { table, name })
        }
        _ => column_ref_from_parts(&raw.parts),
    }
}

/// Turn the resolver's raw column refs into [`ColumnRead`]. Qualified
/// refs decompose by part length; unqualified refs hit the scope-chain
/// resolver, which returns the owning table when a single binding in
/// the chain could carry the column (`None` for 0 or 2+ candidates —
/// the result still surfaces the column with `table: None`).
fn collect_reads(resolution: &RelationResolution) -> Vec<ColumnRead> {
    resolution
        .column_refs
        .iter()
        .filter_map(|raw| build_read_column_ref(raw, resolution))
        .map(|column| ColumnRead { column })
        .collect()
}

fn build_read_column_ref(
    raw: &RawColumnRef,
    resolution: &RelationResolution,
) -> Option<ColumnReference> {
    match raw.parts.len() {
        0 => None,
        1 => {
            let name = raw.parts[0].clone();
            let table = resolution.resolve_unqualified_column(&name, raw.scope_id);
            Some(ColumnReference { table, name })
        }
        _ => column_ref_from_parts(&raw.parts),
    }
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

    fn unresolved(col: &str) -> ColumnRead {
        ColumnRead {
            column: ColumnReference {
                table: None,
                name: col.into(),
            },
        }
    }

    // ───────── reads: qualified ─────────

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

    // ───────── reads: unqualified resolution ─────────

    #[test]
    fn unqualified_single_table_resolves_to_that_table() {
        let ops = extract("SELECT a, b FROM t1");
        assert_eq!(ops.reads, vec![read("t1", "a"), read("t1", "b")]);
    }

    #[test]
    fn unqualified_in_where_resolves_to_single_table() {
        let ops = extract("SELECT a FROM t1 WHERE b > 0");
        assert_eq!(ops.reads, vec![read("t1", "a"), read("t1", "b")]);
    }

    #[test]
    fn unqualified_with_multiple_tables_stays_unresolved() {
        // Two `Unknown`-schema tables — without a catalog the resolver
        // cannot tell which `a` belongs to, so the ref surfaces with
        // `table: None`.
        let ops = extract("SELECT a FROM t1 JOIN t2 ON t1.id = t2.id");
        assert_eq!(
            ops.reads,
            vec![read("t1", "id"), read("t2", "id"), unresolved("a"),]
        );
    }

    #[test]
    fn unqualified_uses_alias_binding_but_returns_real_table() {
        // Alias is just a binding key; the resolver returns the
        // alias-free TableReference of the binding's underlying table.
        let ops = extract("SELECT a FROM t1 AS u");
        assert_eq!(ops.reads, vec![read("t1", "a")]);
    }

    #[test]
    fn unqualified_resolves_to_cte_when_cte_schema_contains_it() {
        // CTE schema is inferred from its body's projection
        // (Known([id])), so `id` resolves to `cte` while `unknown_col`
        // doesn't.
        let ops = extract("WITH cte AS (SELECT id FROM t1) SELECT id, unknown_col FROM cte");
        // The outer scope has only the `cte` binding visible.
        let cte_id = ColumnReference {
            table: Some(table("cte")),
            name: "id".into(),
        };
        assert!(
            ops.reads.contains(&ColumnRead { column: cte_id }),
            "expected cte.id in {:?}",
            ops.reads
        );
        assert!(
            ops.reads
                .iter()
                .any(|r| r.column.name.value == "unknown_col" && r.column.table.is_none()),
            "expected unresolved unknown_col in {:?}",
            ops.reads
        );
    }

    #[test]
    fn unqualified_resolves_to_derived_table_alias() {
        let ops = extract("SELECT id FROM (SELECT id FROM t1) AS d");
        // `id` in outer SELECT should resolve to d (the derived
        // table). Inner SELECT also reads id (from t1).
        assert!(ops.reads.contains(&ColumnRead {
            column: ColumnReference {
                table: Some(table("d")),
                name: "id".into(),
            },
        }));
        assert!(ops.reads.contains(&read("t1", "id")));
    }

    #[test]
    fn unqualified_inner_scope_shadows_outer() {
        // Inner subquery has its own t2 in scope; the unqualified `y`
        // inside the IN-subquery resolves to t2 even though t1 is
        // also in the outer scope. Standard SQL inner-shadows-outer.
        let ops = extract("SELECT * FROM t1 WHERE id IN (SELECT id FROM t2 WHERE y > 0)");
        assert!(ops.reads.contains(&read("t2", "y")));
    }

    #[test]
    fn unqualified_correlated_walks_to_outer_when_inner_has_no_candidate() {
        // Inner CTE has Known schema [zz]; `outer_col` doesn't fit it,
        // so resolution walks to the outer scope and picks the t1
        // (Unknown) binding.
        let ops = extract(
            "SELECT * FROM t1 WHERE id IN (\
                WITH inner_cte AS (SELECT zz FROM t1) \
                SELECT zz FROM inner_cte WHERE outer_col > 0)",
        );
        // The point: `outer_col` walks past the CTE binding (Known
        // schema doesn't list it) and lands on the outer t1 (Unknown).
        // Note that t1 appears twice in the chain (outer and inside
        // the CTE body) — they're separate scopes; the inner
        // inner_cte scope's t1 isn't the same scope as the outer.
        // For this test we just check that `outer_col` resolves
        // somewhere reasonable rather than the exact target.
        assert!(ops
            .reads
            .iter()
            .any(|r| r.column.name.value == "outer_col" && r.column.table.is_some()));
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

    // ───────── flows ─────────

    fn out(name: &str, position: usize) -> ColumnTarget {
        ColumnTarget::QueryOutput {
            name: Some(name.into()),
            position,
        }
    }

    fn out_anon(position: usize) -> ColumnTarget {
        ColumnTarget::QueryOutput {
            name: None,
            position,
        }
    }

    fn persisted(table_name: &str, col: &str) -> ColumnTarget {
        ColumnTarget::Persisted(ColumnReference {
            table: Some(table(table_name)),
            name: col.into(),
        })
    }

    fn col(table_name: &str, name: &str) -> ColumnReference {
        ColumnReference {
            table: Some(table(table_name)),
            name: name.into(),
        }
    }

    fn flow_passthrough(source: ColumnReference, target: ColumnTarget) -> ColumnFlow {
        ColumnFlow {
            source,
            target,
            kind: ColumnFlowKind::Passthrough,
        }
    }

    fn flow_computed(source: ColumnReference, target: ColumnTarget) -> ColumnFlow {
        ColumnFlow {
            source,
            target,
            kind: ColumnFlowKind::Computed,
        }
    }

    #[test]
    fn select_bare_column_emits_passthrough_flow_to_query_output() {
        let ops = extract("SELECT a FROM t1");
        assert_eq!(
            ops.flows,
            vec![flow_passthrough(col("t1", "a"), out("a", 0))]
        );
    }

    #[test]
    fn select_aliased_column_uses_alias_as_output_name() {
        let ops = extract("SELECT a AS x FROM t1");
        assert_eq!(
            ops.flows,
            vec![flow_passthrough(col("t1", "a"), out("x", 0))]
        );
    }

    #[test]
    fn select_computed_emits_one_flow_per_source_with_computed_kind() {
        let ops = extract("SELECT a + b FROM t1");
        assert_eq!(
            ops.flows,
            vec![
                flow_computed(col("t1", "a"), out_anon(0)),
                flow_computed(col("t1", "b"), out_anon(0)),
            ]
        );
    }

    #[test]
    fn select_mixed_projection_separates_targets_by_position() {
        let ops = extract("SELECT a, a + b FROM t1");
        assert_eq!(
            ops.flows,
            vec![
                flow_passthrough(col("t1", "a"), out("a", 0)),
                flow_computed(col("t1", "a"), out_anon(1)),
                flow_computed(col("t1", "b"), out_anon(1)),
            ]
        );
    }

    #[test]
    fn select_qualified_ref_in_computed_resolves_directly() {
        let ops = extract("SELECT t1.a + t1.b AS sum FROM t1");
        assert_eq!(
            ops.flows,
            vec![
                flow_computed(col("t1", "a"), out("sum", 0)),
                flow_computed(col("t1", "b"), out("sum", 0)),
            ]
        );
    }

    #[test]
    fn insert_select_pairs_target_cols_positionally() {
        let ops = extract("INSERT INTO t1 (a, b) SELECT x, y FROM t2");
        assert_eq!(
            ops.flows,
            vec![
                flow_passthrough(col("t2", "x"), persisted("t1", "a")),
                flow_passthrough(col("t2", "y"), persisted("t1", "b")),
            ]
        );
    }

    #[test]
    fn insert_select_computed_marks_kind_per_source() {
        let ops = extract("INSERT INTO t1 (a) SELECT x + y FROM t2");
        assert_eq!(
            ops.flows,
            vec![
                flow_computed(col("t2", "x"), persisted("t1", "a")),
                flow_computed(col("t2", "y"), persisted("t1", "a")),
            ]
        );
    }

    #[test]
    fn insert_select_union_pairs_both_branches_with_target_cols() {
        // Both UNION branches feed the same INSERT target positions,
        // so each branch's projection should pair `position N → t.col_N`.
        let ops = extract(
            "INSERT INTO t1 (a, b) \
             SELECT x, y FROM t2 \
             UNION ALL \
             SELECT p, q FROM t3",
        );
        assert_eq!(
            ops.flows,
            vec![
                flow_passthrough(col("t2", "x"), persisted("t1", "a")),
                flow_passthrough(col("t2", "y"), persisted("t1", "b")),
                flow_passthrough(col("t3", "p"), persisted("t1", "a")),
                flow_passthrough(col("t3", "q"), persisted("t1", "b")),
            ]
        );
    }

    #[test]
    fn insert_without_explicit_cols_emits_no_flows() {
        // Target column names would need positional mapping against
        // the table schema (catalog). Deferred.
        let ops = extract("INSERT INTO t1 SELECT x FROM t2");
        assert!(ops.flows.is_empty());
    }

    #[test]
    fn insert_values_with_literals_emits_no_flows() {
        let ops = extract("INSERT INTO t1 (a, b) VALUES (1, 2)");
        assert!(ops.flows.is_empty());
    }

    #[test]
    fn update_set_passthrough_flow() {
        let ops = extract("UPDATE t1 SET a = b");
        assert_eq!(
            ops.flows,
            vec![flow_passthrough(col("t1", "b"), persisted("t1", "a"))]
        );
    }

    #[test]
    fn update_set_computed_flow() {
        let ops = extract("UPDATE t1 SET a = b + 1");
        assert_eq!(
            ops.flows,
            vec![flow_computed(col("t1", "b"), persisted("t1", "a"))]
        );
    }

    #[test]
    fn update_set_with_qualified_rhs_resolves_to_other_table() {
        let ops = extract("UPDATE t1 SET a = t2.b FROM t2 WHERE t1.id = t2.id");
        assert_eq!(
            ops.flows,
            vec![flow_passthrough(col("t2", "b"), persisted("t1", "a"))]
        );
    }

    #[test]
    fn update_set_literal_emits_no_flow() {
        let ops = extract("UPDATE t1 SET a = 1");
        assert!(ops.flows.is_empty());
    }

    #[test]
    fn delete_emits_no_flow() {
        let ops = extract("DELETE FROM t1 WHERE id = 5");
        assert!(ops.flows.is_empty());
    }

    #[test]
    fn wildcard_select_emits_no_flow() {
        let ops = extract("SELECT * FROM t1");
        assert!(ops.flows.is_empty());
    }

    // ───────── reads: catalog-strict resolution ─────────

    mod catalog_strict {
        use super::*;
        use crate::catalog::{Catalog, ColumnSchema};
        use sqlparser::ast::Ident;
        use std::collections::HashMap;

        #[derive(Debug, Default)]
        struct TestCatalog {
            tables: HashMap<String, Vec<&'static str>>,
        }

        impl TestCatalog {
            fn with(mut self, name: &str, cols: Vec<&'static str>) -> Self {
                self.tables.insert(name.to_string(), cols);
                self
            }
        }

        impl Catalog for TestCatalog {
            fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>> {
                self.tables.get(table.name.value.as_str()).map(|cols| {
                    cols.iter()
                        .map(|c| ColumnSchema {
                            name: Ident::new(*c),
                        })
                        .collect()
                })
            }
        }

        fn extract_with_catalog(sql: &str, catalog: &dyn Catalog) -> StatementColumnOperations {
            let mut result =
                extract_column_operations(&GenericDialect {}, sql, Some(catalog)).unwrap();
            result.remove(0).unwrap()
        }

        #[test]
        fn catalog_known_schema_rejects_columns_not_in_table() {
            // Without catalog `SELECT a FROM t1` resolves a → t1.a
            // unconditionally (single Unknown binding heuristic). With
            // a catalog that says t1's columns are [x, y], `a` cannot
            // come from t1 — it surfaces as unresolved.
            let catalog = TestCatalog::default().with("t1", vec!["x", "y"]);
            let ops = extract_with_catalog("SELECT a FROM t1", &catalog);
            assert_eq!(ops.reads, vec![unresolved("a")]);
        }

        #[test]
        fn catalog_known_schema_resolves_columns_present_in_table() {
            let catalog = TestCatalog::default().with("t1", vec!["a", "b"]);
            let ops = extract_with_catalog("SELECT a FROM t1", &catalog);
            assert_eq!(ops.reads, vec![read("t1", "a")]);
        }

        #[test]
        fn catalog_disambiguates_join_unqualified_ref() {
            // Both tables are Known via catalog; only t2 has `a`, so
            // unqualified `a` in `t1 JOIN t2` resolves to t2 (no
            // catalog: same SQL would be ambiguous).
            let catalog = TestCatalog::default()
                .with("t1", vec!["id"])
                .with("t2", vec!["id", "a"]);
            let ops = extract_with_catalog("SELECT a FROM t1 JOIN t2 ON t1.id = t2.id", &catalog);
            assert!(ops.reads.contains(&read("t2", "a")));
        }
    }
}
