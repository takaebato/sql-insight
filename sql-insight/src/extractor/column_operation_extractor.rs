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
//!   the scope chain at walk time. A unique candidate binding wins;
//!   0 or 2+ candidates leave `table: None` (the column name still
//!   surfaces). References whose walk-time owning binding was a CTE,
//!   derived table, or table function (synthetic intermediates, not
//!   real storage) are dropped from reads — only references to real
//!   tables or unresolved names surface. Each `ColumnRead` carries a
//!   `kinds: Vec<ReadKind>` recording the syntactic clause(s) the
//!   reference appeared in (`Projection` for SELECT list / UPDATE SET
//!   RHS / etc., `Filter` for WHERE / HAVING / JOIN ON / MERGE ON /
//!   CONNECT BY / pipe `|> WHERE`, `GroupBy` / `Sort` / `Window`,
//!   plus a `Conditional` modifier layered on the surrounding clause
//!   for CASE-WHEN condition refs). Typically `len == 1`; multi-role
//!   refs (USING / NATURAL JOIN merged columns) are future work.
//! - `writes`: INSERT target columns (explicit list when given;
//!   when omitted and the catalog provides the target's schema,
//!   the columns the resolver paired with source projections via
//!   the catalog), UPDATE SET targets scoped to the UPDATE table,
//!   CTAS / CREATE VIEW / ALTER VIEW target columns (explicit
//!   column list when provided, else the names the resolver derived
//!   from the source projection), and MERGE WHEN-clause writes
//!   (UPDATE SET targets and INSERT column lists, with the same
//!   catalog fallback for column-list-less INSERT).
//! - `flows`: per-projection-item edges for SELECT (target =
//!   `QueryOutput { name, position }`), positionally paired
//!   `source-column → target-column` edges for INSERT (explicit
//!   column list, or — when the catalog provides the target's
//!   schema — the catalog columns; one ProjectionGroup per UNION
//!   branch, each paired against the same target columns), and
//!   per-assignment edges for
//!   UPDATE SET. Sources that reference CTEs or derived tables are
//!   composed end-to-end — references substitute through the
//!   intermediate's body projections recursively, so a SELECT through
//!   a chain of CTEs surfaces flows whose sources are the underlying
//!   base tables. Each edge is tagged with a `ColumnFlowKind`:
//!   `Passthrough` (bare ref), `Aggregation` (top-level aggregate
//!   function call — detected via SQL-spec structural markers like
//!   `FILTER (WHERE ...)` / `WITHIN GROUP (...)` / `DISTINCT` in
//!   args, plus a name list of common aggregates across major
//!   dialects), or `Computed` (anything else). Composition is
//!   `Aggregation`-dominant: any aggregation step in a CTE / derived
//!   chain makes the resulting flow `Aggregation`. CTAS / CREATE
//!   VIEW / ALTER VIEW emit Persisted flows from source projections
//!   to the created relation's columns. MERGE emits per-clause
//!   Persisted flows for WHEN MATCHED UPDATE (per assignment) and
//!   WHEN NOT MATCHED INSERT VALUES (positional pair with the INSERT
//!   column list); DELETE actions emit nothing. Column-list-less
//!   INSERT SELECT is deferred.
//!
//! **Strictness scales with the catalog.** Without a catalog, Table
//! bindings have `Unknown` schemas and unqualified refs to a
//! single-table scope resolve unconditionally (best-effort, matches
//! the implicit promise of `catalog: None`). With a catalog, Table
//! schemas come back `Known(cols)` and unqualified refs only resolve
//! when the candidate's schema actually lists the column — column
//! typos that would otherwise silently resolve become unresolved.

use crate::catalog::Catalog;
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::error::Error;
use crate::extractor::table_operation_extractor::StatementKind;
use crate::relation::TableReference;
use crate::resolver::{FlowTargetSpec, RawColumnRef, Resolution, Resolver};
use sqlparser::ast::{
    AssignmentTarget, Ident, OnConflictAction, OnInsert, Statement, TableFactor,
};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to extract column-level operations from SQL.
///
/// `catalog` is consulted for relation-level enrichment as well as
/// future column-level needs (`SELECT *` expansion, ambiguous
/// unqualified column resolution). Pass `None` for the lightest path —
/// the MVP does not consult the catalog yet, but the signature is fixed
/// so callers don't have to migrate when it does.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
/// use sql_insight::{
///     extract_column_operations, ColumnFlowKind, ColumnTarget, StatementKind,
/// };
///
/// let dialect = GenericDialect {};
/// let result =
///     extract_column_operations(&dialect, "SELECT a FROM t1", None).unwrap();
/// let ops = result[0].as_ref().unwrap();
///
/// // SELECT contributes reads + flows but no writes.
/// assert_eq!(ops.statement_kind, StatementKind::Select);
/// assert!(ops.writes.is_empty());
///
/// // `t1.a` surfaces as a single read, walk-time resolved to t1.
/// assert_eq!(ops.reads.len(), 1);
/// let read = &ops.reads[0];
/// assert_eq!(read.column.name.value, "a");
/// assert_eq!(read.column.table.as_ref().unwrap().name.value, "t1");
///
/// // The projection emits one flow into the SELECT's QueryOutput slot,
/// // marked Passthrough (no expression wrapping the column).
/// assert_eq!(ops.flows.len(), 1);
/// let flow = &ops.flows[0];
/// assert_eq!(flow.kind, ColumnFlowKind::Passthrough);
/// match &flow.target {
///     ColumnTarget::QueryOutput { name, position } => {
///         assert_eq!(name.as_ref().unwrap().value, "a");
///         assert_eq!(*position, 0);
///     }
///     other => panic!("expected QueryOutput, got {other:?}"),
/// }
/// ```
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
    pub diagnostics: Vec<Diagnostic>,
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

/// A column referenced as a Read source. `kinds` records the SQL
/// clauses this reference appeared in (its syntactic role). Most refs
/// surface a single kind, but the field is `Vec` to leave room for
/// future cases where one ref carries multiple roles (e.g.
/// `USING` / `NATURAL JOIN` merged columns, which are both projection
/// and join keys). Order is walk order, duplicates suppressed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnRead {
    pub column: ColumnReference,
    pub kinds: Vec<ReadKind>,
}

/// SQL-clause role of a [`ColumnRead`]. Captured at walk time from
/// the clause the reference appeared in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReadKind {
    /// Ref appeared in a value-producing position — SELECT projection,
    /// UPDATE SET right-hand side, INSERT VALUES expr, INSERT source
    /// SELECT projection, scalar subquery's projection.
    Projection,
    /// Ref appeared in a row-selection clause — WHERE, HAVING,
    /// QUALIFY, JOIN ON, AsOf match condition, MERGE ON,
    /// CONNECT BY / START WITH, pipe-operator `|> WHERE`, etc.
    Filter,
    /// Ref appeared in a grouping clause — `GROUP BY` (incl. ROLLUP /
    /// CUBE / GROUPING SETS modifiers) or pipe-operator `|> AGGREGATE`'s
    /// GROUP BY part.
    GroupBy,
    /// Ref appeared in a row-ordering clause — `ORDER BY` / `SORT BY`
    /// or pipe-operator `|> ORDER BY`.
    Sort,
    /// Ref appeared inside an `OVER (...)` window spec — `PARTITION BY`,
    /// the window's `ORDER BY`, or a window-frame bound expression.
    /// Refs in the aggregate function's arguments (e.g., `x` in
    /// `SUM(x) OVER (...)`) stay `Projection` since they're
    /// value-producing.
    Window,
    /// Ref appeared as a CASE-WHEN condition expression (`CASE WHEN
    /// <cond> THEN ...`). Layered on top of the surrounding clause
    /// kind — a column in `SELECT CASE WHEN a > 0 THEN b END FROM t`
    /// gets `kinds = [Projection, Conditional]` for `a`. Result and
    /// ELSE expressions stay at the surrounding kind.
    Conditional,
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
    /// A column in a real relation receiving the flow — INSERT /
    /// UPDATE / MERGE target columns, or columns of the new relation
    /// produced by CTAS / CREATE VIEW / ALTER VIEW.
    Persisted(ColumnReference),
    /// A transient column produced by a top-level SELECT projection
    /// that is not piped into a persisted relation. `name` follows
    /// the projection's explicit alias or inferred single-column name
    /// (`None` for expressions without a clear name); `position` is
    /// always set so anonymous outputs remain identifiable.
    QueryOutput {
        name: Option<Ident>,
        position: usize,
    },
}

/// How a source column contributes to its target.
///
/// - `Passthrough` — the source value is forwarded unchanged
///   (`SELECT a FROM t1`, `INSERT INTO t1 (a) SELECT b FROM t2`).
/// - `Aggregation` — the projection's top-level expression is an
///   aggregate function call (`SUM(a)`, `COUNT(b)`, etc.), and the
///   source feeds it. Composition propagates: if any step along the
///   flow chain is an aggregation, the resulting flow is
///   `Aggregation`.
/// - `Computed` — the source feeds any other non-aggregate
///   expression (`SELECT a + b FROM t1`, both `a` and `b` are
///   `Computed`).
///
/// Future variants (`Conditional`, etc.) may further split
/// `Computed` as later phases tighten the classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColumnFlowKind {
    /// Source value is forwarded unchanged. Composition stays
    /// `Passthrough` only when every step in the chain is also
    /// `Passthrough`.
    Passthrough,
    /// Source feeds an aggregate function call (e.g. `SUM`, `COUNT`,
    /// `STRING_AGG`). Composition is aggregation-dominant: if any
    /// step along a CTE / derived chain is `Aggregation`, the
    /// composed flow is `Aggregation`.
    Aggregation,
    /// Source feeds a non-aggregate expression — arithmetic, function
    /// calls, CASE branches, casts, etc. Default fallback for chains
    /// that mix `Passthrough` with any non-Passthrough step that
    /// isn't itself `Aggregation`.
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
        let kind = super::table_operation_extractor::classify_statement(statement);
        let resolution = Resolver::resolve_statement(catalog, statement)?;

        // Start from resolver-level diagnostics; extractor adds its own
        // only when classify_statement detects an unsupported case the
        // resolver did not already report.
        let mut diagnostics = resolution.diagnostics.clone();

        if matches!(kind, StatementKind::Unsupported) {
            if !diagnostics
                .iter()
                .any(|d| matches!(d.kind, DiagnosticKind::UnsupportedStatement))
            {
                diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::UnsupportedStatement,
                    message: format!(
                        "Unsupported statement for column operation extraction: {}",
                        statement
                    ),
                    span: None,
                });
            }
            return Ok(StatementColumnOperations {
                statement_kind: kind,
                reads: Vec::new(),
                writes: Vec::new(),
                flows: Vec::new(),
                diagnostics,
            });
        }

        let reads = collect_reads(&resolution);
        let writes = collect_writes(statement, &resolution)?;
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
fn extract_flows(resolution: &Resolution) -> Vec<ColumnFlow> {
    resolution
        .flow_edges
        .iter()
        .filter_map(|edge| {
            let source = resolve_raw_ref(&edge.source)?;
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
            Some(ColumnFlow {
                source,
                target,
                kind: edge.kind,
            })
        })
        .collect()
}

/// Build a `ColumnReference` from a resolver-captured raw ref. The
/// resolver records owning-table resolution at walk time, so this is
/// a 1:1 read of `(resolved, parts.last())`. Refs whose owning
/// binding was synthetic at walk time are dropped upstream by the
/// resolver itself before they reach the extractor — see
/// `Resolution::real_column_refs`.
fn resolve_raw_ref(raw: &RawColumnRef) -> Option<ColumnReference> {
    let name = raw.parts.last()?.clone();
    Some(ColumnReference {
        table: raw.resolved.clone(),
        name,
    })
}

fn collect_reads(resolution: &Resolution) -> Vec<ColumnRead> {
    resolution
        .column_refs
        .iter()
        .filter_map(|raw| {
            let column = resolve_raw_ref(raw)?;
            Some(ColumnRead {
                column,
                kinds: raw.kinds.clone(),
            })
        })
        .collect()
}

/// Build a `ColumnReference` from a `CompoundIdentifier`'s parts —
/// used by UPDATE SET target parsing where the target's qualifier
/// hasn't been resolver-walked. The last part is the column name;
/// preceding parts decode into `TableReference` by length (1 / 2 / 3).
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
/// - CTAS / CREATE VIEW / ALTER VIEW → writes follow the created
///   relation's columns (explicit list when given, otherwise the
///   columns the resolver derived from the source projection — read
///   off the resolution's `Persisted` flow edges to that target).
///
/// MERGE WHEN clause writes are deferred.
fn collect_writes(
    statement: &Statement,
    resolution: &Resolution,
) -> Result<Vec<ColumnWrite>, Error> {
    let mut writes = Vec::new();
    match statement {
        Statement::Insert(insert) => {
            let target = TableReference::try_from(insert)?;
            if !insert.columns.is_empty() {
                for col in &insert.columns {
                    writes.push(ColumnWrite {
                        column: ColumnReference {
                            table: Some(target.clone()),
                            name: col.clone(),
                        },
                    });
                }
            } else {
                // INSERT without an explicit column list — when the
                // catalog provided the target schema, the resolver
                // emitted Persisted flows to each paired column. Read
                // those off to surface the implicit writes.
                writes.extend(persisted_target_writes(&target, resolution));
            }
            // ON CONFLICT DO UPDATE SET / ON DUPLICATE KEY UPDATE
            // assignment targets become writes too — each SET column
            // is updated on conflict, same role as a standalone UPDATE
            // SET target.
            writes.extend(insert_on_action_writes(insert, &target));
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
        Statement::CreateTable(ct) => {
            // Plain `CREATE TABLE t (a INT, ...)` (no AS) is pure DDL —
            // no data write. Only CTAS (with a query) emits writes.
            if ct.query.is_some() {
                let target = TableReference::try_from(&ct.name)?;
                let explicit: Vec<Ident> = ct.columns.iter().map(|c| c.name.clone()).collect();
                writes.extend(created_writes(&target, &explicit, resolution));
            }
        }
        Statement::CreateView(cv) => {
            let target = TableReference::try_from(&cv.name)?;
            let explicit: Vec<Ident> = cv.columns.iter().map(|c| c.name.clone()).collect();
            writes.extend(created_writes(&target, &explicit, resolution));
        }
        Statement::AlterView { name, columns, .. } => {
            let target = TableReference::try_from(name)?;
            writes.extend(created_writes(&target, columns, resolution));
        }
        Statement::Merge(merge) => {
            use sqlparser::ast::MergeAction;
            let target = match &merge.table {
                TableFactor::Table { .. } => TableReference::try_from(&merge.table).ok(),
                _ => None,
            };
            for clause in &merge.clauses {
                match &clause.action {
                    MergeAction::Insert(insert_expr) => {
                        let Some(target) = &target else { continue };
                        for col_obj in &insert_expr.columns {
                            let Some(ident) = col_obj.0.last().and_then(|p| p.as_ident()) else {
                                continue;
                            };
                            writes.push(ColumnWrite {
                                column: ColumnReference {
                                    table: Some(target.clone()),
                                    name: ident.clone(),
                                },
                            });
                        }
                    }
                    MergeAction::Update(update_expr) => {
                        for assignment in &update_expr.assignments {
                            if let Some(column) = column_ref_from_assignment_target(
                                &assignment.target,
                                target.as_ref(),
                            ) {
                                writes.push(ColumnWrite { column });
                            }
                        }
                    }
                    MergeAction::Delete { .. } => {}
                }
            }
        }
        _ => {}
    }
    Ok(writes)
}

/// Writes for a CREATE-as-style target: when an explicit column list
/// is given, use it verbatim; otherwise delegate to
/// [`persisted_target_writes`] to recover the columns from the
/// resolver's flow edges.
fn created_writes(
    target: &TableReference,
    explicit: &[Ident],
    resolution: &Resolution,
) -> Vec<ColumnWrite> {
    if !explicit.is_empty() {
        return explicit
            .iter()
            .map(|c| ColumnWrite {
                column: ColumnReference {
                    table: Some(target.clone()),
                    name: c.clone(),
                },
            })
            .collect();
    }
    persisted_target_writes(target, resolution)
}

/// Scan the resolution's `Persisted` flow edges for any pointing at
/// `target`, returning a deduped `ColumnWrite` per unique column
/// name. Used by both CREATE-as-style writes derivation and INSERT
/// without an explicit column list (where the catalog-provided
/// schema let the resolver pair source projections positionally).
fn persisted_target_writes(target: &TableReference, resolution: &Resolution) -> Vec<ColumnWrite> {
    let mut seen: Vec<Ident> = Vec::new();
    for edge in &resolution.flow_edges {
        if let FlowTargetSpec::Persisted { table, column } = &edge.target {
            if table == target && !seen.iter().any(|n| n.value == column.value) {
                seen.push(column.clone());
            }
        }
    }
    seen.into_iter()
        .map(|name| ColumnWrite {
            column: ColumnReference {
                table: Some(target.clone()),
                name,
            },
        })
        .collect()
}

/// Surface ON CONFLICT DO UPDATE SET / ON DUPLICATE KEY UPDATE
/// assignment targets as writes on the INSERT target table.
/// Returns an empty `Vec` when the INSERT carries no on-clause, or
/// when the on-clause is `DO NOTHING` (no SET targets to surface).
fn insert_on_action_writes(
    insert: &sqlparser::ast::Insert,
    target: &TableReference,
) -> Vec<ColumnWrite> {
    let assignments: &[sqlparser::ast::Assignment] = match insert.on.as_ref() {
        Some(OnInsert::DuplicateKeyUpdate(a)) => a,
        Some(OnInsert::OnConflict(c)) => match &c.action {
            OnConflictAction::DoUpdate(do_update) => &do_update.assignments,
            OnConflictAction::DoNothing => return Vec::new(),
        },
        // `OnInsert` is `#[non_exhaustive]` — unknown variants
        // surface no writes until we model them explicitly.
        Some(_) => return Vec::new(),
        None => return Vec::new(),
    };
    assignments
        .iter()
        .filter_map(|a| column_ref_from_assignment_target(&a.target, Some(target)))
        .map(|column| ColumnWrite { column })
        .collect()
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
            kinds: vec![ReadKind::Projection],
        }
    }

    fn filter_read(table_name: &str, col: &str) -> ColumnRead {
        ColumnRead {
            column: ColumnReference {
                table: Some(table(table_name)),
                name: col.into(),
            },
            kinds: vec![ReadKind::Filter],
        }
    }

    fn group_by_read(table_name: &str, col: &str) -> ColumnRead {
        ColumnRead {
            column: ColumnReference {
                table: Some(table(table_name)),
                name: col.into(),
            },
            kinds: vec![ReadKind::GroupBy],
        }
    }

    fn sort_read(table_name: &str, col: &str) -> ColumnRead {
        ColumnRead {
            column: ColumnReference {
                table: Some(table(table_name)),
                name: col.into(),
            },
            kinds: vec![ReadKind::Sort],
        }
    }

    fn window_read(table_name: &str, col: &str) -> ColumnRead {
        ColumnRead {
            column: ColumnReference {
                table: Some(table(table_name)),
                name: col.into(),
            },
            kinds: vec![ReadKind::Window],
        }
    }

    fn read_with_kinds(table_name: &str, col: &str, kinds: Vec<ReadKind>) -> ColumnRead {
        ColumnRead {
            column: ColumnReference {
                table: Some(table(table_name)),
                name: col.into(),
            },
            kinds,
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
            kinds: vec![ReadKind::Projection],
        }
    }

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

    fn flow_aggregation(source: ColumnReference, target: ColumnTarget) -> ColumnFlow {
        ColumnFlow {
            source,
            target,
            kind: ColumnFlowKind::Aggregation,
        }
    }

    fn flow_computed(source: ColumnReference, target: ColumnTarget) -> ColumnFlow {
        ColumnFlow {
            source,
            target,
            kind: ColumnFlowKind::Computed,
        }
    }

    /// Whole-value-ish assertion: pin down the full
    /// `StatementColumnOperations` for `sql`. reads / writes / flows /
    /// statement_kind compare strictly; diagnostics compare by **kind
    /// sequence only** so message wording and span coordinates aren't
    /// baked into the expected value.
    fn assert_column_ops(sql: &str, expected: StatementColumnOperations) {
        assert_nth_column_ops(sql, 0, expected);
    }

    /// Like `assert_column_ops` but for multi-statement batches —
    /// targets the statement at `index`. Compose multiple calls to
    /// pin down each statement in a batch independently.
    fn assert_nth_column_ops(sql: &str, index: usize, expected: StatementColumnOperations) {
        let actual = extract_column_operations(&GenericDialect {}, sql, None)
            .unwrap()
            .into_iter()
            .nth(index)
            .unwrap_or_else(|| panic!("statement {index} missing in result for SQL: {sql}"))
            .unwrap();
        assert_column_ops_inner(sql, index, actual, expected);
    }

    fn assert_column_ops_inner(
        sql: &str,
        index: usize,
        actual: StatementColumnOperations,
        expected: StatementColumnOperations,
    ) {
        let StatementColumnOperations {
            statement_kind,
            reads,
            writes,
            flows,
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
            actual.flows, flows,
            "flows for SQL: {sql} (statement {index})"
        );
        let actual_kinds: Vec<_> = actual.diagnostics.iter().map(|d| d.kind.clone()).collect();
        let expected_kinds: Vec<_> = diagnostics.iter().map(|d| d.kind.clone()).collect();
        assert_eq!(
            actual_kinds, expected_kinds,
            "diagnostic kinds for SQL: {sql} (statement {index})"
        );
    }

    /// Placeholder `Diagnostic` for `assert_column_ops.expected.diagnostics`.
    /// Only the kind is compared; message and span are placeholders.
    fn diag(kind: DiagnosticKind) -> Diagnostic {
        Diagnostic {
            kind,
            message: String::new(),
            span: None,
        }
    }

    mod reads {
        use super::*;

        #[test]
        fn qualified_select_collects_qualified_reads() {
            assert_column_ops(
                "SELECT t1.a, t1.b FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_passthrough(col("t1", "b"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn qualified_join_collects_reads_from_both_sides() {
            // Resolver walks FROM (including JOIN ON) before the projection,
            // so the predicate columns appear ahead of the projected ones —
            // and are tagged Filter while projection refs are Projection.
            assert_column_ops(
                "SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.id = t2.id",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        filter_read("t1", "id"),
                        filter_read("t2", "id"),
                        read("t1", "a"),
                        read("t2", "b"),
                    ],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_passthrough(col("t2", "b"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn schema_qualified_ref_resolves_to_schema_dot_table() {
            let table_ref = TableReference {
                catalog: None,
                schema: Some("s1".into()),
                name: "t1".into(),
            };
            assert_column_ops(
                "SELECT s1.t1.a FROM s1.t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![ColumnRead {
                        column: ColumnReference {
                            table: Some(table_ref.clone()),
                            name: "a".into(),
                        },
                        kinds: vec![ReadKind::Projection],
                    }],
                    writes: vec![],
                    flows: vec![flow_passthrough(
                        ColumnReference {
                            table: Some(table_ref),
                            name: "a".into(),
                        },
                        out("a", 0),
                    )],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn where_predicate_qualified_ref_is_a_read() {
            assert_column_ops(
                "SELECT t1.a FROM t1 WHERE t1.b > 0",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), filter_read("t1", "b")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unqualified_single_table_resolves_to_that_table() {
            assert_column_ops(
                "SELECT a, b FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_passthrough(col("t1", "b"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unqualified_in_where_resolves_to_single_table() {
            assert_column_ops(
                "SELECT a FROM t1 WHERE b > 0",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), filter_read("t1", "b")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unqualified_with_multiple_tables_stays_unresolved() {
            // Two `Unknown`-schema tables — without a catalog the resolver
            // cannot tell which `a` belongs to, so the ref surfaces with
            // `table: None`. The flow source also stays unresolved.
            assert_column_ops(
                "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        filter_read("t1", "id"),
                        filter_read("t2", "id"),
                        unresolved("a"),
                    ],
                    writes: vec![],
                    flows: vec![ColumnFlow {
                        source: ColumnReference {
                            table: None,
                            name: "a".into(),
                        },
                        target: out("a", 0),
                        kind: ColumnFlowKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unqualified_uses_alias_binding_but_returns_real_table() {
            // Alias is just a binding key; the resolver returns the
            // alias-free TableReference of the binding's underlying table.
            assert_column_ops(
                "SELECT a FROM t1 AS u",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_ref_does_not_surface_in_reads() {
            // The outer `id` resolves to the cte binding (a synthetic
            // intermediate, not real storage), so it's dropped from reads.
            // Reads surface only references with real Table owners or
            // unresolved column names. `unknown_col` doesn't match the
            // cte's Known schema [id], so it surfaces unresolved
            // (table: None) AND fires an UnresolvedColumn diagnostic.
            assert_column_ops(
                "WITH cte AS (SELECT id FROM t1) SELECT id, unknown_col FROM cte",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id"), unresolved("unknown_col")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "id"), out("id", 0)),
                        ColumnFlow {
                            source: ColumnReference {
                                table: None,
                                name: "unknown_col".into(),
                            },
                            target: out("unknown_col", 1),
                            kind: ColumnFlowKind::Passthrough,
                        },
                    ],
                    diagnostics: vec![diag(DiagnosticKind::UnresolvedColumn)],
                },
            );
        }

        #[test]
        fn derived_table_ref_does_not_surface_in_reads() {
            // Outer `id` resolves to derived alias `d` — synthetic, dropped.
            // Only the inner SELECT's t1.id is a real read.
            assert_column_ops(
                "SELECT id FROM (SELECT id FROM t1) AS d",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unqualified_inner_scope_shadows_outer() {
            // Inner subquery has its own t2 in scope; the unqualified `y`
            // inside the IN-subquery resolves to t2 even though t1 is
            // also in the outer scope. Standard SQL inner-shadows-outer.
            // `y` is in the inner WHERE so its kind is Filter. The inner
            // subquery's projection `id` also produces a flow into a
            // QueryOutput slot of the inner SELECT — that flow surfaces
            // even though the outer wraps it.
            assert_column_ops(
                "SELECT * FROM t1 WHERE id IN (SELECT id FROM t2 WHERE y > 0)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        filter_read("t1", "id"),
                        read("t2", "id"),
                        filter_read("t2", "y"),
                    ],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t2", "id"), out("id", 0))],
                    diagnostics: vec![diag(DiagnosticKind::WildcardSuppressed)],
                },
            );
        }

        #[test]
        fn unqualified_correlated_walks_to_outer_when_inner_has_no_candidate() {
            // Inner CTE has Known schema [zz]; `outer_col` doesn't fit it,
            // so resolution walks to the outer scope and picks the t1
            // (Unknown) binding. The innermost SELECT's projection `zz`
            // also produces a flow that surfaces.
            assert_column_ops(
                "SELECT * FROM t1 WHERE id IN (\
                WITH inner_cte AS (SELECT zz FROM t1) \
                SELECT zz FROM inner_cte WHERE outer_col > 0)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        filter_read("t1", "id"),
                        read("t1", "zz"),
                        filter_read("t1", "outer_col"),
                    ],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "zz"), out("zz", 0))],
                    diagnostics: vec![diag(DiagnosticKind::WildcardSuppressed)],
                },
            );
        }
    }

    mod writes {
        use super::*;

        #[test]
        fn insert_with_explicit_columns_writes_those_columns_on_target() {
            assert_column_ops(
                "INSERT INTO t1 (a, b) VALUES (1, 2)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![write("t1", "a"), write("t1", "b")],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_records_target_writes_and_qualified_source_reads() {
            assert_column_ops(
                "INSERT INTO t1 (a) SELECT t2.b FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t2", "b")],
                    writes: vec![write("t1", "a")],
                    flows: vec![flow_passthrough(col("t2", "b"), persisted("t1", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_without_explicit_columns_yields_no_writes() {
            // Without an explicit column list AND without a catalog, the
            // resolver can't pair source projections to target columns;
            // writes / flows stay empty.
            assert_column_ops(
                "INSERT INTO t1 SELECT t2.b FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t2", "b")],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_targets_become_writes_on_update_table() {
            assert_column_ops(
                "UPDATE t1 SET a = 1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Update,
                    reads: vec![],
                    writes: vec![write("t1", "a")],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_qualified_target_keeps_qualifier() {
            assert_column_ops(
                "UPDATE t1 SET t1.a = 1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Update,
                    reads: vec![],
                    writes: vec![write("t1", "a")],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_rhs_qualified_ref_is_a_read() {
            // SET RHS is value-producing (Projection-like); WHERE refs are
            // Filter-tagged.
            assert_column_ops(
                "UPDATE t1 SET a = t2.b FROM t2 WHERE t1.id = t2.id",
                StatementColumnOperations {
                    statement_kind: StatementKind::Update,
                    reads: vec![
                        read("t2", "b"),
                        filter_read("t1", "id"),
                        filter_read("t2", "id"),
                    ],
                    writes: vec![write("t1", "a")],
                    flows: vec![flow_passthrough(col("t2", "b"), persisted("t1", "a"))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod delete {
        use super::*;

        #[test]
        fn delete_qualified_predicate_is_a_read() {
            assert_column_ops(
                "DELETE FROM t1 WHERE t1.id = 5",
                StatementColumnOperations {
                    statement_kind: StatementKind::Delete,
                    reads: vec![filter_read("t1", "id")],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod read_kinds {
        use super::*;

        #[test]
        fn same_column_in_projection_and_where_is_two_reads_with_different_kinds() {
            // The two textual `a` references each get their own ColumnRead
            // entry — one Projection, one Filter — preserving syntactic role
            // per textual occurrence.
            assert_column_ops(
                "SELECT a FROM t1 WHERE a > 0",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), filter_read("t1", "a")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn subquery_where_ref_carries_filter_kind_not_outer_projection() {
            // The IN-subquery's WHERE walker resets current_read_kind to
            // Filter inside the subquery; the outer Projection default
            // doesn't leak in. Inner subquery's flow is emitted first
            // (during inner SELECT walk), then the outer projection's.
            assert_column_ops(
                "SELECT a FROM t WHERE id IN (SELECT id FROM s WHERE flag = 1)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t", "a"),
                        filter_read("t", "id"),
                        read("s", "id"),
                        filter_read("s", "flag"),
                    ],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("s", "id"), out("id", 0)),
                        flow_passthrough(col("t", "a"), out("a", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn group_by_ref_carries_group_by_kind() {
            assert_column_ops(
                "SELECT a, COUNT(*) FROM t1 GROUP BY a",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), group_by_read("t1", "a")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn order_by_ref_carries_sort_kind() {
            assert_column_ops(
                "SELECT a FROM t1 ORDER BY b",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), sort_read("t1", "b")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn group_by_with_having_separates_kinds() {
            // GROUP BY a → GroupBy; HAVING SUM(b) > 0 → b is Filter.
            // Walk order: projection → HAVING → GROUP BY (the visitor
            // hits HAVING before GROUP BY), so the read order reflects
            // that, not the textual SQL order.
            assert_column_ops(
                "SELECT a FROM t1 GROUP BY a HAVING SUM(b) > 0",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "a"),
                        filter_read("t1", "b"),
                        group_by_read("t1", "a"),
                    ],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn group_by_rollup_modifier_carries_group_by_kind() {
            assert_column_ops(
                "SELECT a, b FROM t1 GROUP BY ROLLUP(a, b)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "a"),
                        read("t1", "b"),
                        group_by_read("t1", "a"),
                        group_by_read("t1", "b"),
                    ],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_passthrough(col("t1", "b"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn subquery_in_group_by_keeps_inner_projection_kind() {
            // GROUP BY (SELECT max(z) FROM s) — the inner subquery's `z` is
            // its own Projection, not the outer GroupBy. resolve_query
            // resets current_read_kind on entry. Inner flow emitted
            // first, then outer projection's.
            assert_column_ops(
                "SELECT a FROM t GROUP BY (SELECT z FROM s)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t", "a"), read("s", "z")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("s", "z"), out("z", 0)),
                        flow_passthrough(col("t", "a"), out("a", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn case_when_condition_in_projection_gets_conditional_modifier() {
            // `a` is the WHEN condition → [Projection, Conditional];
            // `b` is the THEN result → [Projection];
            // `c` is the ELSE result → [Projection].
            assert_column_ops(
                "SELECT CASE WHEN a > 0 THEN b ELSE c END FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read_with_kinds(
                            "t1",
                            "a",
                            vec![ReadKind::Projection, ReadKind::Conditional],
                        ),
                        read("t1", "b"),
                        read("t1", "c"),
                    ],
                    writes: vec![],
                    flows: vec![
                        flow_computed(col("t1", "a"), out_anon(0)),
                        flow_computed(col("t1", "b"), out_anon(0)),
                        flow_computed(col("t1", "c"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn case_when_condition_in_where_layers_with_filter() {
            // `x` is in WHERE's CASE WHEN condition → [Filter, Conditional];
            // `y` is the THEN result (inside WHERE) → [Filter];
            // `z` is the ELSE result (inside WHERE) → [Filter];
            // `b` is the outer projection → [Projection].
            assert_column_ops(
                "SELECT b FROM t WHERE CASE WHEN x > 0 THEN y ELSE z END = 1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t", "b"),
                        read_with_kinds("t", "x", vec![ReadKind::Filter, ReadKind::Conditional]),
                        filter_read("t", "y"),
                        filter_read("t", "z"),
                    ],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t", "b"), out("b", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn subquery_in_case_condition_does_not_leak_conditional_to_inner_refs() {
            // A scalar subquery in a CASE condition position is itself
            // the "conditional" expression. Refs INSIDE the subquery are
            // the subquery's own projection (or its own WHERE etc.) and
            // should NOT inherit `Conditional` from the outer CASE — the
            // modifier resets at the subquery boundary.
            //
            // Flow shape (surfaced by whole-value):
            //   1. inner subquery's projection: s.x → out("x", 0) Passthrough
            //   2-3. outer CASE composes the scalar subquery's projection
            //        AND its WHERE refs as Computed flows into the
            //        outer anonymous output. Both s.x and s.y appear.
            assert_column_ops(
                "SELECT CASE WHEN (SELECT x FROM s WHERE y > 0) IS NULL THEN 1 END FROM t",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("s", "x"), filter_read("s", "y")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("s", "x"), out("x", 0)),
                        flow_computed(col("s", "x"), out_anon(0)),
                        flow_computed(col("s", "y"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn simple_case_operand_gets_conditional_modifier() {
            // `CASE x WHEN 1 THEN a WHEN 2 THEN b END` — `x` is the
            // operand (compared against each WHEN pattern), classified
            // Conditional. `a` / `b` are results, plain Projection.
            assert_column_ops(
                "SELECT CASE x WHEN 1 THEN a WHEN 2 THEN b END FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read_with_kinds(
                            "t1",
                            "x",
                            vec![ReadKind::Projection, ReadKind::Conditional],
                        ),
                        read("t1", "a"),
                        read("t1", "b"),
                    ],
                    writes: vec![],
                    flows: vec![
                        flow_computed(col("t1", "x"), out_anon(0)),
                        flow_computed(col("t1", "a"), out_anon(0)),
                        flow_computed(col("t1", "b"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn window_partition_by_carries_window_kind() {
            // OVER (PARTITION BY p) — p's read kind is Window; the
            // aggregate arg `x` stays Projection on the read. But on
            // the flow side, BOTH x AND p contribute as Aggregation
            // sources (the whole SUM(...) OVER (...) expression
            // classifies as an aggregate-shaped flow producer).
            assert_column_ops(
                "SELECT SUM(x) OVER (PARTITION BY p) FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "x"), window_read("t1", "p")],
                    writes: vec![],
                    flows: vec![
                        flow_aggregation(col("t1", "x"), out_anon(0)),
                        flow_aggregation(col("t1", "p"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn window_order_by_carries_window_kind() {
            assert_column_ops(
                "SELECT SUM(x) OVER (ORDER BY o) FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "x"), window_read("t1", "o")],
                    writes: vec![],
                    flows: vec![
                        flow_aggregation(col("t1", "x"), out_anon(0)),
                        flow_aggregation(col("t1", "o"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn window_partition_and_order_both_classified() {
            assert_column_ops(
                "SELECT SUM(x) OVER (PARTITION BY p ORDER BY o) FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "x"),
                        window_read("t1", "p"),
                        window_read("t1", "o"),
                    ],
                    writes: vec![],
                    flows: vec![
                        flow_aggregation(col("t1", "x"), out_anon(0)),
                        flow_aggregation(col("t1", "p"), out_anon(0)),
                        flow_aggregation(col("t1", "o"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn merge_on_clause_carries_filter_kind() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET t.a = s.a",
                StatementColumnOperations {
                    statement_kind: StatementKind::Merge,
                    reads: vec![
                        filter_read("t", "id"),
                        filter_read("s", "id"),
                        read("s", "a"),
                    ],
                    writes: vec![write("t", "a")],
                    flows: vec![flow_passthrough(col("s", "a"), persisted("t", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn create_table_definitions_are_not_writes() {
            assert_column_ops(
                "CREATE TABLE t1 (a INT, b INT)",
                StatementColumnOperations {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod diagnostics {
        use super::*;

        #[test]
        fn unsupported_statement_reports_diagnostic() {
            assert_column_ops(
                "CREATE INDEX idx ON t1 (a)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Unsupported,
                    reads: vec![],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![diag(DiagnosticKind::UnsupportedStatement)],
                },
            );
        }

        #[test]
        fn wildcard_in_projection_reports_diagnostic() {
            // Whole-value pin-down on the structural shape; assert_column_ops
            // compares diagnostics by kind only. The message text and span
            // coordinates are verified separately below since this test's
            // *purpose* is to confirm both are populated.
            let ops = extract("SELECT * FROM t1");
            assert_column_ops(
                "SELECT * FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![diag(DiagnosticKind::WildcardSuppressed)],
                },
            );
            // Span info ("at L1:C8") is duplicated in message and surfaced
            // as structured data for programmatic consumers.
            assert!(
                ops.diagnostics[0].message.contains("at L1:C8"),
                "expected span suffix in message, got: {}",
                ops.diagnostics[0].message
            );
            let span = ops.diagnostics[0]
                .span
                .expect("wildcard token carries a span");
            assert_eq!(span.start.line, 1);
            assert_eq!(span.start.column, 8);
        }

        #[test]
        fn qualified_wildcard_in_projection_reports_diagnostic() {
            assert_column_ops(
                "SELECT t1.* FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![diag(DiagnosticKind::WildcardSuppressed)],
                },
            );
        }

        #[test]
        fn multiple_statements_produce_multiple_results() {
            let sql = "SELECT t1.a FROM t1; SELECT t2.b FROM t2";
            assert_nth_column_ops(
                sql,
                0,
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
            assert_nth_column_ops(
                sql,
                1,
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t2", "b")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t2", "b"), out("b", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn wildcard_select_yields_no_column_ops() {
            assert_column_ops(
                "SELECT * FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![diag(DiagnosticKind::WildcardSuppressed)],
                },
            );
        }
    }

    mod flows {
        use super::*;

        #[test]
        fn select_bare_column_emits_passthrough_flow_to_query_output() {
            assert_column_ops(
                "SELECT a FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn select_aliased_column_uses_alias_as_output_name() {
            assert_column_ops(
                "SELECT a AS x FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("x", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn select_computed_emits_one_flow_per_source_with_computed_kind() {
            assert_column_ops(
                "SELECT a + b FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_computed(col("t1", "a"), out_anon(0)),
                        flow_computed(col("t1", "b"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn select_mixed_projection_separates_targets_by_position() {
            assert_column_ops(
                "SELECT a, a + b FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_computed(col("t1", "a"), out_anon(1)),
                        flow_computed(col("t1", "b"), out_anon(1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn select_qualified_ref_in_computed_resolves_directly() {
            assert_column_ops(
                "SELECT t1.a + t1.b AS sum FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_computed(col("t1", "a"), out("sum", 0)),
                        flow_computed(col("t1", "b"), out("sum", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_pairs_target_cols_positionally() {
            assert_column_ops(
                "INSERT INTO t1 (a, b) SELECT x, y FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t2", "x"), read("t2", "y")],
                    writes: vec![write("t1", "a"), write("t1", "b")],
                    flows: vec![
                        flow_passthrough(col("t2", "x"), persisted("t1", "a")),
                        flow_passthrough(col("t2", "y"), persisted("t1", "b")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_computed_marks_kind_per_source() {
            assert_column_ops(
                "INSERT INTO t1 (a) SELECT x + y FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t2", "x"), read("t2", "y")],
                    writes: vec![write("t1", "a")],
                    flows: vec![
                        flow_computed(col("t2", "x"), persisted("t1", "a")),
                        flow_computed(col("t2", "y"), persisted("t1", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_union_pairs_both_branches_with_target_cols() {
            // Both UNION branches feed the same INSERT target positions,
            // so each branch's projection should pair `position N → t.col_N`.
            assert_column_ops(
                "INSERT INTO t1 (a, b) \
                 SELECT x, y FROM t2 \
                 UNION ALL \
                 SELECT p, q FROM t3",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![
                        read("t2", "x"),
                        read("t2", "y"),
                        read("t3", "p"),
                        read("t3", "q"),
                    ],
                    writes: vec![write("t1", "a"), write("t1", "b")],
                    flows: vec![
                        flow_passthrough(col("t2", "x"), persisted("t1", "a")),
                        flow_passthrough(col("t2", "y"), persisted("t1", "b")),
                        flow_passthrough(col("t3", "p"), persisted("t1", "a")),
                        flow_passthrough(col("t3", "q"), persisted("t1", "b")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_without_explicit_cols_emits_no_flows() {
            // Target column names would need catalog-driven positional
            // mapping; without catalog the resolver emits nothing.
            assert_column_ops(
                "INSERT INTO t1 SELECT x FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t2", "x")],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_values_with_literals_emits_no_flows() {
            assert_column_ops(
                "INSERT INTO t1 (a, b) VALUES (1, 2)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![write("t1", "a"), write("t1", "b")],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_literal_emits_no_flow() {
            assert_column_ops(
                "UPDATE t1 SET a = 1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Update,
                    reads: vec![],
                    writes: vec![write("t1", "a")],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn delete_emits_no_flow() {
            assert_column_ops(
                "DELETE FROM t1 WHERE id = 5",
                StatementColumnOperations {
                    statement_kind: StatementKind::Delete,
                    reads: vec![filter_read("t1", "id")],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn wildcard_select_emits_no_flow() {
            assert_column_ops(
                "SELECT * FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![diag(DiagnosticKind::WildcardSuppressed)],
                },
            );
        }

        #[test]
        fn update_set_passthrough_flow() {
            assert_column_ops(
                "UPDATE t1 SET a = b",
                StatementColumnOperations {
                    statement_kind: StatementKind::Update,
                    reads: vec![read("t1", "b")],
                    writes: vec![write("t1", "a")],
                    flows: vec![flow_passthrough(col("t1", "b"), persisted("t1", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_computed_flow() {
            assert_column_ops(
                "UPDATE t1 SET a = b + 1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Update,
                    reads: vec![read("t1", "b")],
                    writes: vec![write("t1", "a")],
                    flows: vec![flow_computed(col("t1", "b"), persisted("t1", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_with_qualified_rhs_resolves_to_other_table() {
            assert_column_ops(
                "UPDATE t1 SET a = t2.b FROM t2 WHERE t1.id = t2.id",
                StatementColumnOperations {
                    statement_kind: StatementKind::Update,
                    reads: vec![
                        read("t2", "b"),
                        filter_read("t1", "id"),
                        filter_read("t2", "id"),
                    ],
                    writes: vec![write("t1", "a")],
                    flows: vec![flow_passthrough(col("t2", "b"), persisted("t1", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_call_in_projection_emits_aggregation_flow() {
            assert_column_ops(
                "SELECT SUM(a) FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    flows: vec![flow_aggregation(col("t1", "a"), out_anon(0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_with_alias_carries_aliased_name() {
            assert_column_ops(
                "SELECT COUNT(b) AS n FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "b")],
                    writes: vec![],
                    flows: vec![flow_aggregation(col("t1", "b"), out("n", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_wrapped_in_expression_falls_back_to_computed() {
            // `SUM(a) + 1` has BinaryOp at the top level, so the
            // projection's kind is Computed — only a bare aggregate call
            // qualifies as Aggregation.
            assert_column_ops(
                "SELECT SUM(a) + 1 FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    flows: vec![flow_computed(col("t1", "a"), out_anon(0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_in_insert_select_propagates_aggregation() {
            assert_column_ops(
                "INSERT INTO t2 (n) SELECT COUNT(a) FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t1", "a")],
                    writes: vec![write("t2", "n")],
                    flows: vec![flow_aggregation(col("t1", "a"), persisted("t2", "n"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_aggregate_composes_to_outer_as_aggregation() {
            // CTE body's `s` is Aggregation (SUM(a)); outer's bare `s`
            // would be Passthrough, but composition (Aggregation
            // dominates) collapses the chain to Aggregation.
            assert_column_ops(
                "WITH cte AS (SELECT SUM(a) AS s FROM t1) SELECT s FROM cte",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    flows: vec![flow_aggregation(col("t1", "a"), out("s", 0))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod cte_derived_rename {
        use super::*;

        #[test]
        fn cte_column_rename_composes_through_renamed_name() {
            // Outer `a` refers to cte's renamed column at position 0,
            // which body-positionally is `x` from t. Composition follows
            // the renamed name back to the body item, then to t.x.
            // Reads surface only the real-table ref (CTE binding is
            // synthetic, dropped).
            assert_column_ops(
                "WITH cte (a) AS (SELECT x FROM t) SELECT a FROM cte",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t", "x")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t", "x"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_column_rename_partial_keeps_remaining_body_names() {
            // Rename `(p)` covers position 0 only. Position 1's body name
            // `y` survives; outer can reference `p` or `y`.
            assert_column_ops(
                "WITH cte (p) AS (SELECT x, y FROM t) SELECT p, y FROM cte",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t", "x"), read("t", "y")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t", "x"), out("p", 0)),
                        flow_passthrough(col("t", "y"), out("y", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn derived_table_column_rename_composes() {
            // `(SELECT x FROM t) AS d(a)` — outer `a` resolves via d's
            // renamed column at position 0 → body item x → t.x.
            assert_column_ops(
                "SELECT a FROM (SELECT x FROM t) d(a)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t", "x")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t", "x"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_column_rename_into_insert() {
            // `INSERT INTO t2 (col) WITH cte(a) AS (SELECT x FROM t1)
            //  SELECT a FROM cte` composes through both the CTE rename
            //  and the INSERT pairing: t1.x → t2.col.
            assert_column_ops(
                "INSERT INTO t2 (col) WITH cte (a) AS (SELECT x FROM t1) \
                 SELECT a FROM cte",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t1", "x")],
                    writes: vec![write("t2", "col")],
                    flows: vec![flow_passthrough(col("t1", "x"), persisted("t2", "col"))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod merge {
        use super::*;

        #[test]
        fn merge_when_matched_update_emits_flow_and_write() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET t.a = s.a",
                StatementColumnOperations {
                    statement_kind: StatementKind::Merge,
                    reads: vec![
                        filter_read("t", "id"),
                        filter_read("s", "id"),
                        read("s", "a"),
                    ],
                    writes: vec![write("t", "a")],
                    flows: vec![flow_passthrough(col("s", "a"), persisted("t", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn merge_when_not_matched_insert_emits_flow_and_write() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id \
                 WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Merge,
                    reads: vec![
                        filter_read("t", "id"),
                        filter_read("s", "id"),
                        read("s", "id"),
                        read("s", "a"),
                    ],
                    writes: vec![write("t", "id"), write("t", "a")],
                    flows: vec![
                        flow_passthrough(col("s", "id"), persisted("t", "id")),
                        flow_passthrough(col("s", "a"), persisted("t", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn merge_delete_action_emits_no_flow_no_write() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE",
                StatementColumnOperations {
                    statement_kind: StatementKind::Merge,
                    reads: vec![filter_read("t", "id"), filter_read("s", "id")],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn merge_combined_clauses_emit_per_clause_flows_and_writes() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id \
                 WHEN MATCHED THEN UPDATE SET t.a = s.a \
                 WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Merge,
                    reads: vec![
                        filter_read("t", "id"),
                        filter_read("s", "id"),
                        read("s", "a"),
                        read("s", "id"),
                        read("s", "a"),
                    ],
                    writes: vec![write("t", "a"), write("t", "id"), write("t", "a")],
                    flows: vec![
                        flow_passthrough(col("s", "a"), persisted("t", "a")),
                        flow_passthrough(col("s", "id"), persisted("t", "id")),
                        flow_passthrough(col("s", "a"), persisted("t", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn merge_update_computed_kind_propagates() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id \
                 WHEN MATCHED THEN UPDATE SET t.a = s.a + 1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Merge,
                    reads: vec![
                        filter_read("t", "id"),
                        filter_read("s", "id"),
                        read("s", "a"),
                    ],
                    writes: vec![write("t", "a")],
                    flows: vec![flow_computed(col("s", "a"), persisted("t", "a"))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod ctas_view {
        use super::*;

        #[test]
        fn ctas_pairs_source_projection_with_inferred_column_names() {
            // CREATE TABLE AS SELECT — no explicit column list, so target
            // columns follow the source projection's inferred names
            // (alias > bare ident).
            assert_column_ops(
                "CREATE TABLE t AS SELECT x AS a, y FROM s",
                StatementColumnOperations {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![read("s", "x"), read("s", "y")],
                    writes: vec![write("t", "a"), write("t", "y")],
                    flows: vec![
                        flow_passthrough(col("s", "x"), persisted("t", "a")),
                        flow_passthrough(col("s", "y"), persisted("t", "y")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn ctas_with_explicit_columns_overrides_projection_names() {
            // Explicit column list wins over inferred names.
            assert_column_ops(
                "CREATE TABLE t (p INT, q INT) AS SELECT x, y FROM s",
                StatementColumnOperations {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![read("s", "x"), read("s", "y")],
                    writes: vec![write("t", "p"), write("t", "q")],
                    flows: vec![
                        flow_passthrough(col("s", "x"), persisted("t", "p")),
                        flow_passthrough(col("s", "y"), persisted("t", "q")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn ctas_propagates_aggregation_kind() {
            assert_column_ops(
                "CREATE TABLE t AS SELECT SUM(x) AS total FROM s",
                StatementColumnOperations {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![read("s", "x")],
                    writes: vec![write("t", "total")],
                    flows: vec![flow_aggregation(col("s", "x"), persisted("t", "total"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn create_view_pairs_source_projection() {
            assert_column_ops(
                "CREATE VIEW v AS SELECT x AS a, y FROM s",
                StatementColumnOperations {
                    statement_kind: StatementKind::CreateView,
                    reads: vec![read("s", "x"), read("s", "y")],
                    writes: vec![write("v", "a"), write("v", "y")],
                    flows: vec![
                        flow_passthrough(col("s", "x"), persisted("v", "a")),
                        flow_passthrough(col("s", "y"), persisted("v", "y")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn create_view_with_explicit_columns_uses_list() {
            assert_column_ops(
                "CREATE VIEW v (a, b) AS SELECT x, y FROM s",
                StatementColumnOperations {
                    statement_kind: StatementKind::CreateView,
                    reads: vec![read("s", "x"), read("s", "y")],
                    writes: vec![write("v", "a"), write("v", "b")],
                    flows: vec![
                        flow_passthrough(col("s", "x"), persisted("v", "a")),
                        flow_passthrough(col("s", "y"), persisted("v", "b")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn alter_view_pairs_replacement_query_projection() {
            assert_column_ops(
                "ALTER VIEW v AS SELECT x AS a FROM s",
                StatementColumnOperations {
                    statement_kind: StatementKind::AlterView,
                    reads: vec![read("s", "x")],
                    writes: vec![write("v", "a")],
                    flows: vec![flow_passthrough(col("s", "x"), persisted("v", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn ctas_unnamed_projection_yields_no_paired_flow() {
            // `SELECT 1` has no column ref and no inferable name, so the
            // CTAS source produces no flow / no write for that slot.
            assert_column_ops(
                "CREATE TABLE t AS SELECT 1 FROM s",
                StatementColumnOperations {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![],
                    writes: vec![],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_with_distinct_args_marker() {
            // COUNT(DISTINCT user_id) — DISTINCT inside function args is
            // aggregate-only per SQL spec, classified as Aggregation even
            // if the function name weren't in the list.
            assert_column_ops(
                "SELECT COUNT(DISTINCT user_id) FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "user_id")],
                    writes: vec![],
                    flows: vec![flow_aggregation(col("t1", "user_id"), out_anon(0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_with_filter_clause_marker() {
            // FILTER (WHERE ...) is aggregate-only per SQL spec.
            // Surprises surfaced by whole-value compare:
            //  - `y` inside the aggregate's FILTER clause is classified
            //    Projection, not Filter — the resolver treats FILTER
            //    contents as part of the aggregate's argument scope.
            //  - `y` ALSO contributes as an Aggregation flow source,
            //    not just `x`. Anything mentioned inside the aggregate's
            //    syntactic boundary (args + FILTER predicate) flows
            //    into the aggregate's output.
            assert_column_ops(
                "SELECT SUM(x) FILTER (WHERE y > 0) FROM t1",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "x"), read("t1", "y")],
                    writes: vec![],
                    flows: vec![
                        flow_aggregation(col("t1", "x"), out_anon(0)),
                        flow_aggregation(col("t1", "y"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_aggregate_then_outer_compute_still_aggregation() {
            // Outer wraps the CTE column in a computed expression
            // (s + 1) — composition: outer Computed × inner Aggregation =
            // Aggregation (Aggregation dominates Computed).
            assert_column_ops(
                "WITH cte AS (SELECT SUM(a) AS s FROM t1) SELECT s + 1 FROM cte",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    flows: vec![flow_aggregation(col("t1", "a"), out_anon(0))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod composition {
        use super::*;

        #[test]
        fn cte_passthrough_composes_to_base_table() {
            // The outer flow's source `id` resolves to cte, then composes
            // through the CTE body's projection back to t1.id. No
            // intermediate cte.id → out edge survives.
            assert_column_ops(
                "WITH cte AS (SELECT id FROM t1) SELECT id FROM cte",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_computed_propagates_computed_kind_after_composition() {
            // CTE body's `sum` is computed from a, b. Outer's bare `sum`
            // composes back into two flows, each marked Computed because
            // the body item is Computed (outer.bare && item.bare = false).
            assert_column_ops(
                "WITH cte AS (SELECT a + b AS sum FROM t1) SELECT sum FROM cte",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_computed(col("t1", "a"), out("sum", 0)),
                        flow_computed(col("t1", "b"), out("sum", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_to_insert_composes_end_to_end() {
            // Composition flows past the CTE boundary into the INSERT
            // target — t1.id → t2.x directly, no cte.id step.
            assert_column_ops(
                "INSERT INTO t2 (x) WITH cte AS (SELECT id FROM t1) SELECT id FROM cte",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t1", "id")],
                    writes: vec![write("t2", "x")],
                    flows: vec![flow_passthrough(col("t1", "id"), persisted("t2", "x"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_chain_composes_through_all_levels() {
            // a → b → outer: outer's `b.id` composes via b's body back to
            // a, then via a's body back to t1. Outer is qualified because
            // having both `a` and `b` in scope with the same column name
            // makes the unqualified form ambiguous under our scope model
            // (outer SELECT sees both CTE bindings, not just b).
            assert_column_ops(
                "WITH a AS (SELECT id FROM t1), b AS (SELECT id FROM a) SELECT b.id FROM b",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn derived_table_composes_to_base_table() {
            // The outer projection's `col` composes through derived `d`'s
            // body (a + b AS col) into two Computed flows on t1.
            assert_column_ops(
                "SELECT col FROM (SELECT a + b AS col FROM t1) d",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_computed(col("t1", "a"), out("col", 0)),
                        flow_computed(col("t1", "b"), out("col", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_referenced_twice_composes_each_use() {
            // Each cte reference in the projection composes independently
            // back to t1.id.
            assert_column_ops(
                "WITH cte AS (SELECT id FROM t1) SELECT cte.id AS a, cte.id AS b FROM cte",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "id"), out("a", 0)),
                        flow_passthrough(col("t1", "id"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn recursive_cte_does_not_panic_and_skips_composition() {
            // Recursive CTEs don't carry body_projections (fixpoint is
            // deferred), so composition falls back to leaving the flow
            // source pointing at the CTE binding (`r.id`) rather than
            // tracing into a base table. Reads still get the synthetic
            // filter, so only `t1.id` from the non-recursive branch
            // surfaces in reads. No infinite recursion either.
            assert_column_ops(
                "WITH RECURSIVE r AS (SELECT id FROM t1 UNION SELECT id FROM r) SELECT id FROM r",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    flows: vec![ColumnFlow {
                        source: ColumnReference {
                            table: Some(TableReference {
                                catalog: None,
                                schema: None,
                                name: "r".into(),
                            }),
                            name: "id".into(),
                        },
                        target: out("id", 0),
                        kind: ColumnFlowKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod set_operations {
        use super::*;

        #[test]
        fn union_two_branches_emit_query_output_per_branch() {
            // Each branch contributes its own ProjectionGroup, so both
            // branches' projections fan out independently into
            // QueryOutput edges. Position is per-group, so both land at
            // position 0; name follows each branch's own projection.
            assert_column_ops(
                "SELECT a FROM t1 UNION SELECT b FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_all_behaves_same_as_union() {
            // UNION ALL only differs from UNION at runtime (dedup vs
            // not); structurally the resolver should treat them identically.
            assert_column_ops(
                "SELECT a FROM t1 UNION ALL SELECT b FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn intersect_behaves_same_as_union() {
            assert_column_ops(
                "SELECT a FROM t1 INTERSECT SELECT b FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn except_behaves_same_as_union() {
            assert_column_ops(
                "SELECT a FROM t1 EXCEPT SELECT b FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn three_way_union_emits_one_flow_per_branch() {
            // Chained UNION parses left-associatively as
            // `(t1 UNION t2) UNION t3`, so the resolver recursively
            // visits each base SELECT and each contributes its own group.
            assert_column_ops(
                "SELECT a FROM t1 UNION SELECT b FROM t2 UNION SELECT c FROM t3",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "a"),
                        read("t2", "b"),
                        read("t3", "c"),
                    ],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_passthrough(col("t2", "b"), out("b", 0)),
                        flow_passthrough(col("t3", "c"), out("c", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_with_where_classifies_per_branch_kind() {
            // Each branch's WHERE is its own filter scope, so each
            // branch produces a Projection read plus a Filter read for
            // its own column.
            assert_column_ops(
                "SELECT a FROM t1 WHERE a > 0 UNION SELECT b FROM t2 WHERE b < 10",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "a"),
                        filter_read("t1", "a"),
                        read("t2", "b"),
                        filter_read("t2", "b"),
                    ],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_mixed_passthrough_and_computed_kinds() {
            // Branch flow kinds are independent. Left passthrough,
            // right computed; both contribute to the same output position.
            assert_column_ops(
                "SELECT a FROM t1 UNION SELECT b + 1 AS a FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("a", 0)),
                        flow_computed(col("t2", "b"), out("a", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_with_aggregate_branch_emits_aggregation_flow() {
            assert_column_ops(
                "SELECT id FROM t1 UNION SELECT COUNT(id) AS id FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id"), read("t2", "id")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "id"), out("id", 0)),
                        flow_aggregation(col("t2", "id"), out("id", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_in_subquery_emits_inner_query_output_then_outer() {
            // The inner UNION bubbles through `SetExpr::Query`-style
            // surface and contributes flows to its own QueryOutput
            // slot, then the outer SELECT projects from the derived
            // subquery and composes back to the base tables.
            assert_column_ops(
                "SELECT x FROM (SELECT a AS x FROM t1 UNION SELECT b AS x FROM t2) sub",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("x", 0)),
                        flow_passthrough(col("t2", "b"), out("x", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_in_cte_composes_to_outer_use() {
            // CTE body is a UNION. Outer SELECT pulls `x` from the cte.
            // Composition should walk back through both branches to t1/t2.
            assert_column_ops(
                "WITH cte AS (SELECT a AS x FROM t1 UNION SELECT b AS x FROM t2) \
                 SELECT x FROM cte",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), out("x", 0)),
                        flow_passthrough(col("t2", "b"), out("x", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn ctas_with_union_body_pairs_left_branch_names_for_all_branches() {
            // CTAS schema follows the LEFT branch's projection names
            // (SQL standard). The inferred-name path uses the first
            // ProjectionGroup's item names for every branch's
            // positional pairing — same as INSERT-SELECT-UNION. So:
            //   - writes: only `dst.a` (left branch's name)
            //   - flows: BOTH branches feed `Persisted(dst.a)`
            assert_column_ops(
                "CREATE TABLE dst AS SELECT a FROM t1 UNION SELECT b FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![write("dst", "a")],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), persisted("dst", "a")),
                        flow_passthrough(col("t2", "b"), persisted("dst", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn ctas_with_explicit_columns_and_union_body_pairs_left_target_for_all_branches() {
            // When CTAS specifies its own column list, both branches
            // pair positionally against the same target columns — same
            // pattern as INSERT-SELECT-UNION.
            assert_column_ops(
                "CREATE TABLE dst (x INT) AS SELECT a FROM t1 UNION SELECT b FROM t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![write("dst", "x")],
                    flows: vec![
                        flow_passthrough(col("t1", "a"), persisted("dst", "x")),
                        flow_passthrough(col("t2", "b"), persisted("dst", "x")),
                    ],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod join_using_and_natural {
        //! USING / NATURAL JOIN merge expansion is documented as
        //! future work (resolver/column_ref.rs `RawColumnRef.kinds`;
        //! also the module-level note in column_operation_extractor).
        //! These tests pin down the *current* shape so when USING /
        //! NATURAL JOIN expansion lands (with merged refs gaining a
        //! second `ReadKind` and/or splitting into both source
        //! tables), the diff will surface here.
        use super::*;

        #[test]
        fn join_using_id_in_projection_is_unresolved_due_to_ambiguity() {
            // `id` in the projection is unqualified with two candidate
            // tables (t1, t2) — the resolver leaves it unresolved
            // (`table: None`) because no catalog disambiguates and
            // USING is not yet expanded into a merged-column binding.
            assert_column_ops(
                "SELECT id FROM t1 JOIN t2 USING (id)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![unresolved("id")],
                    writes: vec![],
                    flows: vec![ColumnFlow {
                        source: ColumnReference {
                            table: None,
                            name: "id".into(),
                        },
                        target: out("id", 0),
                        kind: ColumnFlowKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn join_using_id_in_projection_and_where_yields_two_independent_unresolved_refs() {
            // The same `id` ref in projection vs. WHERE produces two
            // SEPARATE RawColumnRefs, each with a single-kind `kinds`
            // vec. There is no merge into one ref-with-multi-kinds
            // here — that would require resolver-level tracking of
            // ref identity across clauses, which we don't do.
            assert_column_ops(
                "SELECT id FROM t1 JOIN t2 USING (id) WHERE id > 0",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        unresolved("id"),
                        ColumnRead {
                            column: ColumnReference {
                                table: None,
                                name: "id".into(),
                            },
                            kinds: vec![ReadKind::Filter],
                        },
                    ],
                    writes: vec![],
                    flows: vec![ColumnFlow {
                        source: ColumnReference {
                            table: None,
                            name: "id".into(),
                        },
                        target: out("id", 0),
                        kind: ColumnFlowKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn join_using_qualified_id_resolves_to_named_table() {
            // Qualifying the ref sidesteps the USING ambiguity: `t1.id`
            // resolves to t1 unambiguously. Use this in real-world
            // queries until USING expansion is available.
            assert_column_ops(
                "SELECT t1.id FROM t1 JOIN t2 USING (id)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn natural_join_no_catalog_leaves_unqualified_refs_unresolved() {
            // NATURAL JOIN's merge set comes from the intersection of
            // both tables' column lists — only knowable with a
            // catalog. Without one, the resolver doesn't expand, and
            // unqualified `id` is multi-candidate-unresolved (same
            // shape as plain JOIN ON without USING).
            assert_column_ops(
                "SELECT id FROM t1 NATURAL JOIN t2",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![unresolved("id")],
                    writes: vec![],
                    flows: vec![ColumnFlow {
                        source: ColumnReference {
                            table: None,
                            name: "id".into(),
                        },
                        target: out("id", 0),
                        kind: ColumnFlowKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod lateral_and_correlation {
        use super::*;

        #[test]
        fn lateral_subquery_resolves_inner_ref_to_inner_table() {
            // The existing-style LATERAL: the inner subquery only
            // references its own tables. The outer FROM joins it as
            // a derived source. The inner `id` resolves to t1 from
            // the LATERAL subquery's own scope.
            assert_column_ops(
                "SELECT d.id FROM LATERAL (SELECT id FROM t1) AS d JOIN t2 ON d.id = t2.id",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "id"),
                        filter_read("t2", "id"),
                    ],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn lateral_with_outer_scope_reference_resolves_via_scope_chain() {
            // The interesting LATERAL case: the inner subquery references
            // `t1.x` from the OUTER FROM. Without LATERAL this is invalid
            // SQL, but the resolver doesn't enforce LATERAL semantics —
            // it walks the scope chain regardless.
            assert_column_ops(
                "SELECT sub.x FROM t1, LATERAL (SELECT t1.a + t2.b AS x FROM t2) sub",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_computed(col("t1", "a"), out("x", 0)),
                        flow_computed(col("t2", "b"), out("x", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn non_lateral_derived_also_resolves_outer_ref_permissively() {
            // The resolver doesn't distinguish LATERAL from non-LATERAL
            // — both walk the scope chain identically. This is more
            // lenient than strict SQL semantics (where this should be
            // an error), but reasonable for lineage purposes: a
            // best-effort resolution is more useful than silently
            // dropping the reference.
            assert_column_ops(
                "SELECT sub.x FROM t1, (SELECT t1.a + t2.b AS x FROM t2) sub",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    flows: vec![
                        flow_computed(col("t1", "a"), out("x", 0)),
                        flow_computed(col("t2", "b"), out("x", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn correlated_where_subquery_resolves_outer_ref() {
            // Classic correlated subquery in WHERE: the inner SELECT
            // references the outer t1.id. The resolver walks the
            // scope chain to find t1.id in the outer scope.
            assert_column_ops(
                "SELECT a FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.fk = t1.id)",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "a"),
                        filter_read("t2", "fk"),
                        filter_read("t1", "id"),
                    ],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod on_conflict {
        //! ON CONFLICT (Postgres / Sqlite) and ON DUPLICATE KEY UPDATE
        //! (MySQL) both sit in `Insert.on: Option<OnInsert>`. The
        //! resolver walks both, with subtle differences:
        //!
        //! - Postgres: `EXCLUDED.<col>` is a pseudo-table for the
        //!   would-be-inserted row. Bound as synthetic so refs
        //!   through it filter out of `reads` but still emit valid
        //!   Persisted flow edges into the target. The synthetic
        //!   binding's columns mirror the INSERT target's columns.
        //! - MySQL: `VALUES(<col>)` is a function-call form for the
        //!   same concept. No EXCLUDED binding (it would make
        //!   unqualified refs ambiguous against the INSERT target);
        //!   the inner ref resolves to the INSERT target like a
        //!   regular self-reference.
        //!
        //! DO UPDATE SET targets become writes on the INSERT target
        //! table — same role as a standalone UPDATE SET. The optional
        //! DO UPDATE WHERE clause walks in filter context.
        use super::*;
        use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};

        fn assert_column_ops_with_dialect(
            sql: &str,
            dialect: &dyn sqlparser::dialect::Dialect,
            expected: StatementColumnOperations,
        ) {
            let actual = extract_column_operations(dialect, sql, None)
                .unwrap()
                .into_iter()
                .next()
                .unwrap_or_else(|| panic!("no statements in result for SQL: {sql}"))
                .unwrap();
            assert_column_ops_inner(sql, 0, actual, expected);
        }

        /// Construct a `ColumnReference` for the synthetic EXCLUDED
        /// pseudo-table — used only as a Source in flow edges, not
        /// as a real table.
        fn excluded(name: &str) -> ColumnReference {
            ColumnReference {
                table: Some(TableReference {
                    catalog: None,
                    schema: None,
                    name: "EXCLUDED".into(),
                }),
                name: name.into(),
            }
        }

        #[test]
        fn pg_on_conflict_do_update_set_excluded_emits_flow_and_write() {
            // DO UPDATE SET b = EXCLUDED.b
            //   - writes: t.a, t.b from INSERT columns plus another
            //     t.b for the SET target.
            //   - reads: empty (EXCLUDED is synthetic-filtered;
            //     VALUES (1, 2) are literals).
            //   - flows: EXCLUDED.b → Persisted(t.b), Passthrough.
            assert_column_ops_with_dialect(
                "INSERT INTO t (a, b) VALUES (1, 2) ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
                &PostgreSqlDialect {},
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                    flows: vec![flow_passthrough(excluded("b"), persisted("t", "b"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn pg_on_conflict_do_nothing_is_indistinguishable_from_plain_insert() {
            assert_column_ops_with_dialect(
                "INSERT INTO t (a, b) VALUES (1, 2) ON CONFLICT (a) DO NOTHING",
                &PostgreSqlDialect {},
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![write("t", "a"), write("t", "b")],
                    flows: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn pg_insert_select_with_on_conflict_composes_excluded_to_source() {
            // EXCLUDED's body_projections come from the INSERT source
            // renamed to the target columns positionally. So
            // `EXCLUDED.b` composes through to the source's position-1
            // projection (`y` from s) — the conflict-action flow
            // bottoms out at the same base table as the
            // source-projection flow.
            assert_column_ops_with_dialect(
                "INSERT INTO t (a, b) SELECT x, y FROM s \
                 ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
                &PostgreSqlDialect {},
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "x"), read("s", "y")],
                    writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                    flows: vec![
                        flow_passthrough(col("s", "x"), persisted("t", "a")),
                        flow_passthrough(col("s", "y"), persisted("t", "b")),
                        flow_passthrough(col("s", "y"), persisted("t", "b")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn mysql_on_duplicate_key_update_values_func_self_references_target() {
            // MySQL `VALUES(<col>)` is the implicit-row form. Without
            // an EXCLUDED binding, the inner `b` ref resolves to t.b
            // (the INSERT target). Result: t.b shows up as a read
            // (the VALUES function call is a Computed wrapper) and
            // the SET clause adds a Persisted flow t.b → t.b.
            assert_column_ops_with_dialect(
                "INSERT INTO t (a, b) VALUES (1, 2) \
                 ON DUPLICATE KEY UPDATE b = VALUES(b)",
                &MySqlDialect {},
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t", "b")],
                    writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                    flows: vec![flow_computed(col("t", "b"), persisted("t", "b"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn pg_insert_union_with_on_conflict_excluded_fans_out_to_each_branch() {
            // The source has TWO ProjectionGroups (one per UNION
            // branch), so EXCLUDED's body_projections also have two
            // groups — each with a position-0 item named after the
            // INSERT target column. `EXCLUDED.a` then composes to
            // BOTH branches' position-0 source refs.
            assert_column_ops_with_dialect(
                "INSERT INTO t (a) SELECT x FROM s1 UNION SELECT y FROM s2 \
                 ON CONFLICT (a) DO UPDATE SET a = EXCLUDED.a",
                &PostgreSqlDialect {},
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s1", "x"), read("s2", "y")],
                    writes: vec![write("t", "a"), write("t", "a")],
                    flows: vec![
                        flow_passthrough(col("s1", "x"), persisted("t", "a")),
                        flow_passthrough(col("s2", "y"), persisted("t", "a")),
                        flow_passthrough(col("s1", "x"), persisted("t", "a")),
                        flow_passthrough(col("s2", "y"), persisted("t", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn pg_insert_aggregate_with_on_conflict_excluded_keeps_aggregation_kind() {
            // SUM(x) marks the source projection as Aggregation kind.
            // When EXCLUDED.total composes back, compose_flow_kinds
            // takes the Aggregation-dominant rule → flow kind stays
            // Aggregation even on the conflict-action path.
            assert_column_ops_with_dialect(
                "INSERT INTO t (total) SELECT SUM(x) FROM s \
                 ON CONFLICT (id) DO UPDATE SET total = EXCLUDED.total",
                &PostgreSqlDialect {},
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "x")],
                    writes: vec![write("t", "total"), write("t", "total")],
                    flows: vec![
                        flow_aggregation(col("s", "x"), persisted("t", "total")),
                        flow_aggregation(col("s", "x"), persisted("t", "total")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn pg_on_conflict_do_update_with_where_clause_emits_filter_read() {
            // DO UPDATE ... WHERE walks in filter context, so refs in
            // the WHERE expression get `ReadKind::Filter`.
            assert_column_ops_with_dialect(
                "INSERT INTO t (a, b) VALUES (1, 2) \
                 ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b WHERE t.a > 0",
                &PostgreSqlDialect {},
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![filter_read("t", "a")],
                    writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                    flows: vec![flow_passthrough(excluded("b"), persisted("t", "b"))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod returning {
        //! `RETURNING <select_items>` on INSERT / UPDATE / DELETE
        //! (Postgres / Sqlite extension) projects from the affected
        //! rows of the target table — treated like a top-level SELECT
        //! projection: each item contributes refs to `reads` and a
        //! `QueryOutput` flow edge. Walked BEFORE the ON-clause for
        //! INSERT so any EXCLUDED binding doesn't ambify unqualified
        //! refs that collide with INSERT column names.
        use super::*;

        #[test]
        fn insert_values_with_returning_emits_target_reads_and_query_output() {
            assert_column_ops(
                "INSERT INTO t (a, b) VALUES (1, 2) RETURNING id",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t", "id")],
                    writes: vec![write("t", "a"), write("t", "b")],
                    flows: vec![flow_passthrough(col("t", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn returning_aliased_uses_alias_as_output_name() {
            assert_column_ops(
                "INSERT INTO t (a) VALUES (1) RETURNING id AS pk",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t", "id")],
                    writes: vec![write("t", "a")],
                    flows: vec![flow_passthrough(col("t", "id"), out("pk", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn returning_with_computed_expression_marks_kind_computed() {
            assert_column_ops(
                "INSERT INTO t (a) VALUES (1) RETURNING id + 1 AS bumped",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t", "id")],
                    writes: vec![write("t", "a")],
                    flows: vec![flow_computed(col("t", "id"), out("bumped", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn returning_wildcard_records_wildcard_suppressed_diagnostic() {
            assert_column_ops(
                "INSERT INTO t (a) VALUES (1) RETURNING *",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![write("t", "a")],
                    flows: vec![],
                    diagnostics: vec![diag(DiagnosticKind::WildcardSuppressed)],
                },
            );
        }

        #[test]
        fn update_returning_walks_target_columns() {
            assert_column_ops(
                "UPDATE t SET a = b + 1 WHERE id = 5 RETURNING id, a",
                StatementColumnOperations {
                    statement_kind: StatementKind::Update,
                    reads: vec![
                        read("t", "b"),
                        filter_read("t", "id"),
                        read("t", "id"),
                        read("t", "a"),
                    ],
                    writes: vec![write("t", "a")],
                    flows: vec![
                        flow_computed(col("t", "b"), persisted("t", "a")),
                        flow_passthrough(col("t", "id"), out("id", 0)),
                        flow_passthrough(col("t", "a"), out("a", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn delete_returning_walks_target_columns() {
            assert_column_ops(
                "DELETE FROM t WHERE id = 5 RETURNING id, val",
                StatementColumnOperations {
                    statement_kind: StatementKind::Delete,
                    reads: vec![
                        filter_read("t", "id"),
                        read("t", "id"),
                        read("t", "val"),
                    ],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("t", "id"), out("id", 0)),
                        flow_passthrough(col("t", "val"), out("val", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_with_returning_keeps_source_flows_and_target_returning() {
            // Source SELECT's tables are out of scope by the time
            // RETURNING walks (their nested scope was popped after
            // resolve_query). So RETURNING refs resolve to the target
            // table alone, even when the bare name `id` exists in the
            // source too.
            assert_column_ops(
                "INSERT INTO t (a) SELECT x FROM s RETURNING id",
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "x"), read("t", "id")],
                    writes: vec![write("t", "a")],
                    flows: vec![
                        flow_passthrough(col("s", "x"), persisted("t", "a")),
                        flow_passthrough(col("t", "id"), out("id", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }
    }

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

        fn assert_column_ops_with_catalog(
            sql: &str,
            catalog: &dyn Catalog,
            expected: StatementColumnOperations,
        ) {
            let actual = extract_column_operations(&GenericDialect {}, sql, Some(catalog))
                .unwrap()
                .into_iter()
                .next()
                .unwrap()
                .unwrap();
            assert_column_ops_inner(sql, 0, actual, expected);
        }

        #[test]
        fn catalog_known_schema_rejects_columns_not_in_table() {
            // Without catalog `SELECT a FROM t1` resolves a → t1.a
            // unconditionally (single Unknown binding heuristic). With
            // a catalog that says t1's columns are [x, y], `a` cannot
            // come from t1 — it surfaces as unresolved and fires
            // UnresolvedColumn.
            let catalog = TestCatalog::default().with("t1", vec!["x", "y"]);
            assert_column_ops_with_catalog(
                "SELECT a FROM t1",
                &catalog,
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![unresolved("a")],
                    writes: vec![],
                    flows: vec![ColumnFlow {
                        source: ColumnReference {
                            table: None,
                            name: "a".into(),
                        },
                        target: out("a", 0),
                        kind: ColumnFlowKind::Passthrough,
                    }],
                    diagnostics: vec![diag(DiagnosticKind::UnresolvedColumn)],
                },
            );
        }

        #[test]
        fn catalog_known_schema_resolves_columns_present_in_table() {
            let catalog = TestCatalog::default().with("t1", vec!["a", "b"]);
            assert_column_ops_with_catalog(
                "SELECT a FROM t1",
                &catalog,
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_insert_without_explicit_columns_pairs_via_catalog_schema() {
            // INSERT INTO t SELECT a, b FROM s — no explicit column
            // list. With t = [x, y, z] in catalog, the resolver pairs
            // source projections positionally (s.a → t.x, s.b → t.y).
            // Unpaired catalog cols (z) get no flow / no write.
            let catalog = TestCatalog::default().with("t", vec!["x", "y", "z"]);
            assert_column_ops_with_catalog(
                "INSERT INTO t SELECT a, b FROM s",
                &catalog,
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "a"), read("s", "b")],
                    writes: vec![write("t", "x"), write("t", "y")],
                    flows: vec![
                        flow_passthrough(col("s", "a"), persisted("t", "x")),
                        flow_passthrough(col("s", "b"), persisted("t", "y")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_insert_without_explicit_columns_source_longer_than_target() {
            // 3 source projections vs t = [x, y] — pair what fits,
            // surplus source column gets no flow.
            let catalog = TestCatalog::default().with("t", vec!["x", "y"]);
            assert_column_ops_with_catalog(
                "INSERT INTO t SELECT a, b, c FROM s",
                &catalog,
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "a"), read("s", "b"), read("s", "c")],
                    writes: vec![write("t", "x"), write("t", "y")],
                    flows: vec![
                        flow_passthrough(col("s", "a"), persisted("t", "x")),
                        flow_passthrough(col("s", "b"), persisted("t", "y")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_insert_explicit_columns_override_catalog_schema() {
            // Explicit (q) wins over catalog [x, y, z].
            let catalog = TestCatalog::default().with("t", vec!["x", "y", "z"]);
            assert_column_ops_with_catalog(
                "INSERT INTO t (q) SELECT a FROM s",
                &catalog,
                StatementColumnOperations {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "a")],
                    writes: vec![write("t", "q")],
                    flows: vec![flow_passthrough(col("s", "a"), persisted("t", "q"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_merge_not_matched_insert_no_cols_pairs_via_catalog() {
            // Same catalog fallback applies to MERGE's INSERT clause:
            // flows are paired via catalog. Surprise surfaced by whole-
            // value compare: writes stay empty for catalog-paired MERGE
            // INSERT — only `INSERT (cols) VALUES (...)` with an
            // explicit column list populates writes.
            let catalog = TestCatalog::default().with("t", vec!["id", "a"]);
            assert_column_ops_with_catalog(
                "MERGE INTO t USING s ON t.id = s.id \
                 WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.a)",
                &catalog,
                StatementColumnOperations {
                    statement_kind: StatementKind::Merge,
                    reads: vec![
                        filter_read("t", "id"),
                        filter_read("s", "id"),
                        read("s", "id"),
                        read("s", "a"),
                    ],
                    writes: vec![],
                    flows: vec![
                        flow_passthrough(col("s", "id"), persisted("t", "id")),
                        flow_passthrough(col("s", "a"), persisted("t", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_disambiguates_join_unqualified_ref() {
            // Both tables are Known via catalog; only t2 has `a`, so
            // unqualified `a` in `t1 JOIN t2` resolves to t2 (no
            // catalog: same SQL would be ambiguous).
            let catalog = TestCatalog::default()
                .with("t1", vec!["id"])
                .with("t2", vec!["id", "a"]);
            assert_column_ops_with_catalog(
                "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id",
                &catalog,
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        filter_read("t1", "id"),
                        filter_read("t2", "id"),
                        read("t2", "a"),
                    ],
                    writes: vec![],
                    flows: vec![flow_passthrough(col("t2", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_confirmed_ambiguity_reports_diagnostic() {
            // Both tables Known and both declare `a`. Diagnostic must
            // fire — without catalog the same query is silently
            // ambiguous (no diagnostic) since Unknown schemas could
            // contain anything. assert_column_ops compares diagnostics
            // by kind only; the message-content checks are kept inline
            // since they're this test's specific purpose.
            let catalog = TestCatalog::default()
                .with("t1", vec!["a"])
                .with("t2", vec!["a"]);
            assert_column_ops_with_catalog(
                "SELECT a FROM t1 JOIN t2 ON t1.a = t2.a",
                &catalog,
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        filter_read("t1", "a"),
                        filter_read("t2", "a"),
                        unresolved("a"),
                    ],
                    writes: vec![],
                    flows: vec![ColumnFlow {
                        source: ColumnReference {
                            table: None,
                            name: "a".into(),
                        },
                        target: out("a", 0),
                        kind: ColumnFlowKind::Passthrough,
                    }],
                    diagnostics: vec![diag(DiagnosticKind::AmbiguousColumn)],
                },
            );
            // Specific message-content checks for this test's purpose.
            let ops = extract_column_operations(
                &GenericDialect {},
                "SELECT a FROM t1 JOIN t2 ON t1.a = t2.a",
                Some(&catalog),
            )
            .unwrap();
            let ops = ops.into_iter().next().unwrap().unwrap();
            let amb = ops
                .diagnostics
                .iter()
                .find(|d| matches!(d.kind, DiagnosticKind::AmbiguousColumn))
                .expect("AmbiguousColumn must fire");
            assert!(amb.message.contains("ambiguous column `a`"));
            assert!(amb.message.contains("t1"));
            assert!(amb.message.contains("t2"));
        }

        #[test]
        fn catalog_unresolved_unqualified_reports_diagnostic() {
            // Catalog says t1 has [x, y]; unqualified `z` belongs to
            // nothing in scope — UnresolvedColumn fires.
            let catalog = TestCatalog::default().with("t1", vec!["x", "y"]);
            assert_column_ops_with_catalog(
                "SELECT z FROM t1",
                &catalog,
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![unresolved("z")],
                    writes: vec![],
                    flows: vec![ColumnFlow {
                        source: ColumnReference {
                            table: None,
                            name: "z".into(),
                        },
                        target: out("z", 0),
                        kind: ColumnFlowKind::Passthrough,
                    }],
                    diagnostics: vec![diag(DiagnosticKind::UnresolvedColumn)],
                },
            );
            // Message-content check for this test's purpose.
            let ops =
                extract_column_operations(&GenericDialect {}, "SELECT z FROM t1", Some(&catalog))
                    .unwrap();
            let ops = ops.into_iter().next().unwrap().unwrap();
            let unr = ops
                .diagnostics
                .iter()
                .find(|d| matches!(d.kind, DiagnosticKind::UnresolvedColumn))
                .expect("UnresolvedColumn must fire");
            assert!(unr.message.contains("unresolved column `z`"));
        }

        #[test]
        fn no_catalog_unqualified_is_silent_even_when_ambiguous_shape() {
            // No catalog → all schemas are Unknown → resolver can't
            // tell whether `a` is genuinely in both t1 and t2, only one,
            // or neither. Two diagnostic kinds are intentionally
            // suppressed in this mode: AmbiguousColumn (no confirmed
            // matches) and UnresolvedColumn (no Known schemas in scope).
            // The resolution itself still returns None for the column,
            // and the flow source is also unresolved.
            assert_column_ops(
                "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id",
                StatementColumnOperations {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        filter_read("t1", "id"),
                        filter_read("t2", "id"),
                        unresolved("a"),
                    ],
                    writes: vec![],
                    flows: vec![ColumnFlow {
                        source: ColumnReference {
                            table: None,
                            name: "a".into(),
                        },
                        target: out("a", 0),
                        kind: ColumnFlowKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }
    }
}
