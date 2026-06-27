//! The **lineage** surfaces over a [`LogicalPlan`]: directed `source → target`
//! edges at column and table granularity. These back the [`crate::resolver`]
//! facade's `column_lineage` / `table_lineage` entry points.
//!
//! Column lineage drives the [`super::origins`] trace — each output column's
//! value expression is traced to its base columns. A bare query targets
//! `QueryOutput`; a DML root pairs the source's output columns positionally
//! with the write target's columns (`Relation`). Table lineage is the coarser
//! view: the read-role scans that **feed data** into a DML target (value path
//! only — predicate subqueries do not feed).

use sqlparser::ast::Ident;

use super::logical_plan::{peel_with, Expr, LogicalPlan, MergeClause, NamedExpr, Update};
use super::origins::{
    conflict_value_origins, enter_withs, origins_of_expr, output_operands, TraceContext,
};
use crate::casing::IdentifierCasing;
use crate::extractor::{ColumnLineageEdge, ColumnLineageKind, ColumnTarget, TableLineageEdge};
use crate::reference::{ColumnRead, ColumnReference, TableRead, TableReference};

// ===== column lineage ====================================================

/// The column lineage of a statement: `source → target` edges, each output
/// column traced to its base columns (`QueryOutput` for a query, `Relation` for
/// a DML target). A bare query emits `source → QueryOutput` edges; a DML root
/// pairs the source's output columns positionally with the write target's
/// columns and emits `source → Relation` edges. A leading `WITH` is peeled (its
/// CTE bodies feed the root through `CteRef` expansion, they are not lineage
/// roots). Backs [`crate::resolver::column_lineage`].
pub(super) fn collect_column_lineage(
    plan: &LogicalPlan,
    casing: IdentifierCasing,
) -> Vec<ColumnLineageEdge> {
    let mut context = TraceContext::new(plan, casing);
    let mut edges = Vec::new();
    match peel_with(plan) {
        // INSERT … <source>: pair the source's outputs with the target columns.
        // A statement-level `WITH` rides on the source (the parser attaches it
        // there, so the `With` is *inside* `input`, not above the `Insert`);
        // `enter_withs` pushes its CTEs into the context so a `CteRef` resolves.
        LogicalPlan::Insert(i) => {
            let src = enter_withs(&i.input, &mut context);
            // A wildcard in the source projection makes positions indeterminate,
            // so positional pairing would mis-attribute (`SELECT *, y` → which
            // target does `y` feed?). Drop the relation lineage — the target
            // columns still surface as `writes`, flagged by `WildcardSuppressed`
            // — matching a pure `SELECT *` source (no operands to pair).
            if !i.source_wildcard {
                relation_lineage(
                    &i.columns,
                    &i.target.reference,
                    src,
                    &mut context,
                    &mut edges,
                );
            }
            // ON CONFLICT DO UPDATE SET col = value: each `value → target.col`,
            // an `EXCLUDED.x` ref mapped to the source's like-positioned output.
            for a in &i.on_conflict {
                emit_edges(
                    conflict_value_origins(&a.value, &i.columns, src, &mut context),
                    ColumnTarget::Relation(a.target.clone()),
                    &mut edges,
                );
            }
            returning_lineage(&i.returning, &i.input, &mut context, &mut edges);
        }
        // UPDATE … SET col = expr: each assignment's RHS value traces to its
        // target column (`Relation` edge). A `Derived` RHS ref (a column of a
        // FROM derived table) traces through `input`.
        LogicalPlan::Update(u) => {
            for a in &u.assignments {
                emit_edges(
                    origins_of_expr(&a.value, &u.input, &mut context),
                    ColumnTarget::Relation(a.target.clone()),
                    &mut edges,
                );
            }
            returning_lineage(&u.returning, &u.input, &mut context, &mut edges);
        }
        // DELETE moves no data (rows go wholesale) — its only lineage is a
        // RETURNING projection of the deleted rows.
        LogicalPlan::Delete(d) => {
            returning_lineage(&d.returning, &d.input, &mut context, &mut edges)
        }
        // CTAS / CREATE VIEW move data like INSERT, but the new relation's
        // column *names* are the source outputs' own (unless an explicit list
        // overrides) — so pair each output with its own name, not a separately
        // derived list that could drift in length.
        LogicalPlan::CreateTableAs(c) => {
            let src = enter_withs(&c.input, &mut context);
            if pairs_positionally(c.source_wildcard, &c.columns) {
                created_relation_lineage(
                    &c.columns,
                    &c.target.reference,
                    src,
                    &mut context,
                    &mut edges,
                );
            }
        }
        LogicalPlan::CreateView(c) => {
            let src = enter_withs(&c.input, &mut context);
            if pairs_positionally(c.source_wildcard, &c.columns) {
                created_relation_lineage(
                    &c.columns,
                    &c.target.reference,
                    src,
                    &mut context,
                    &mut edges,
                );
            }
        }
        // MERGE: each WHEN action's value traces to its target column (an
        // UPDATE SET `RHS → target.col`; an INSERT `value → target.col`). A
        // `Derived` source ref traces through the source relation. DELETE
        // moves no value.
        LogicalPlan::Merge(m) => {
            for clause in &m.clauses {
                match clause {
                    MergeClause::Update { assignments } => {
                        for a in assignments {
                            merge_value_edges(
                                &a.target.name,
                                &a.value,
                                &m.target.reference,
                                &m.source,
                                &mut context,
                                &mut edges,
                            );
                        }
                    }
                    MergeClause::Insert { columns, values } => {
                        for (column, value) in columns.iter().zip(values) {
                            merge_value_edges(
                                column,
                                value,
                                &m.target.reference,
                                &m.source,
                                &mut context,
                                &mut edges,
                            );
                        }
                    }
                    MergeClause::Delete => {}
                }
            }
        }
        // A bare query (or unmodelled root): one `QueryOutput` group per
        // projection (a set operation has one per branch — positions restart
        // per branch, mirroring the resolver). DDL that names a table but
        // moves no value (`AlterTable` / `Drop`) flows here too — it emits no
        // lineage. Listed explicitly so a new `LogicalPlan` variant forces a
        // routing decision rather than landing here by default.
        LogicalPlan::Scan(_)
        | LogicalPlan::Filter(_)
        | LogicalPlan::Join(_)
        | LogicalPlan::Aggregate(_)
        | LogicalPlan::Projection(_)
        | LogicalPlan::Sort(_)
        | LogicalPlan::SetOp(_)
        | LogicalPlan::SubqueryAlias(_)
        | LogicalPlan::TableFunction(_)
        | LogicalPlan::With(_)
        | LogicalPlan::CteRef(_)
        | LogicalPlan::Values(_)
        | LogicalPlan::Empty
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => query_output_lineage(plan, &mut context, &mut edges),
    }
    edges
}

/// Bare-query lineage: each output column becomes a `QueryOutput` target at
/// its position, one edge per traced origin. `output_operands` peels the
/// clause layers and `With`, and yields one operand per set-operation branch.
fn query_output_lineage<'a>(
    op: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
    out: &mut Vec<ColumnLineageEdge>,
) {
    let operands = output_operands(op);
    // A set operation has one result schema whose column names come from the
    // first (left-most) branch — SQL's conventional rule (see the crate-level
    // "Set operations follow the left side"). Every branch's like-positioned
    // output feeds that same result column, so the target name is the left
    // branch's, never a right branch's local alias (which names no result
    // column). Position already restarts per branch, aligning them; a plain
    // query has a single operand, so this is a no-op there.
    let result_names: Vec<Option<Ident>> = match operands.first() {
        Some(o) => o.outputs.iter().map(|ne| ne.name.clone()).collect(),
        None => Vec::new(),
    };
    for operand in &operands {
        let outputs = operand.outputs;
        operand.trace(context, |input, cx| {
            for (position, ne) in outputs.iter().enumerate() {
                let target = ColumnTarget::QueryOutput {
                    name: result_names.get(position).cloned().flatten(),
                    position,
                };
                emit_edges(origins_of_expr(&ne.expr, input, cx), target, out);
            }
        });
    }
}

/// Push one `source → target` edge per traced origin — the shared tail of every
/// column-lineage producer. Given an expression's already-traced origins (value
/// sources with their composed [`ColumnLineageKind`]) and a fixed target, emit
/// the edges. Decouples *how* origins are computed (`origins_of_expr` /
/// `conflict_value_origins`) from edge construction.
fn emit_edges(
    origins: impl IntoIterator<Item = (ColumnRead, ColumnLineageKind)>,
    target: ColumnTarget,
    out: &mut Vec<ColumnLineageEdge>,
) {
    for (source, kind) in origins {
        out.push(ColumnLineageEdge {
            source,
            target: target.clone(),
            kind,
        });
    }
}

/// DML relation lineage: pair each source output operand's columns positionally
/// with the target `columns` (so a UNION-sourced INSERT pairs every branch) and
/// emit one `Relation` edge per traced origin.
fn relation_lineage<'a>(
    columns: &[Ident],
    target: &TableReference,
    input: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
    out: &mut Vec<ColumnLineageEdge>,
) {
    for operand in output_operands(input) {
        let outputs = operand.outputs;
        operand.trace(context, |src_input, cx| {
            for (target_column, ne) in columns.iter().zip(outputs) {
                let tgt = ColumnTarget::Relation(ColumnReference {
                    table: Some(target.clone()),
                    name: target_column.clone(),
                });
                emit_edges(origins_of_expr(&ne.expr, src_input, cx), tgt, out);
            }
        });
    }
}

/// Whether a created relation's (CTAS / CREATE VIEW) column lineage can be
/// paired safely. An *explicit* column list pairs positionally with the source
/// projection, so an unexpanded wildcard makes those positions indeterminate —
/// skip, like INSERT. The *implicit* form follows the source outputs' own
/// names, so a wildcard there merely omits the unexpanded columns without
/// misattributing — keep it.
fn pairs_positionally(source_wildcard: bool, explicit: &[Ident]) -> bool {
    !source_wildcard || explicit.is_empty()
}

/// CTAS / CREATE VIEW relation lineage. Unlike [`relation_lineage`] (INSERT,
/// whose target columns are an independent list), a created relation's column
/// *names* come from the source itself: an explicit `(a, b)` list when written,
/// else the result schema's inferred names. The schema follows the **left**
/// (first) branch of a set operation (SQL's rule), so every branch's
/// like-positioned output feeds that same target column. An anonymous output
/// with no explicit name is unnameable, so it emits no edge — without shifting
/// later positions, the bug a length-mismatched zip caused. Kept in step with
/// [`super::tables`]'s `created_relation_writes`.
fn created_relation_lineage<'a>(
    explicit: &[Ident],
    target: &TableReference,
    input: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
    out: &mut Vec<ColumnLineageEdge>,
) {
    let operands = output_operands(input);
    let result_names: Vec<Option<Ident>> = if explicit.is_empty() {
        match operands.first() {
            Some(o) => o.outputs.iter().map(|ne| ne.name.clone()).collect(),
            None => Vec::new(),
        }
    } else {
        explicit.iter().cloned().map(Some).collect()
    };
    for operand in &operands {
        let outputs = operand.outputs;
        operand.trace(context, |src_input, cx| {
            for (position, ne) in outputs.iter().enumerate() {
                let Some(name) = result_names.get(position).cloned().flatten() else {
                    continue;
                };
                let tgt = ColumnTarget::Relation(ColumnReference {
                    table: Some(target.clone()),
                    name,
                });
                emit_edges(origins_of_expr(&ne.expr, src_input, cx), tgt, out);
            }
        });
    }
}

/// Emit a DML statement's `RETURNING` lineage: each returned column is a
/// `QueryOutput` target (the statement both writes and projects), its origins
/// traced through `input` (the written relation / FROM scope).
fn returning_lineage<'a>(
    returning: &'a [NamedExpr],
    input: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
    out: &mut Vec<ColumnLineageEdge>,
) {
    for (position, ne) in returning.iter().enumerate() {
        let target = ColumnTarget::QueryOutput {
            name: ne.name.clone(),
            position,
        };
        emit_edges(origins_of_expr(&ne.expr, input, context), target, out);
    }
}

/// Emit the `value → target.column` lineage edges of one MERGE WHEN value, its
/// origins traced through the `source` relation.
fn merge_value_edges<'a>(
    column: &Ident,
    value: &'a Expr,
    target: &TableReference,
    source: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
    out: &mut Vec<ColumnLineageEdge>,
) {
    let tgt = ColumnTarget::Relation(ColumnReference {
        table: Some(target.clone()),
        name: column.clone(),
    });
    emit_edges(origins_of_expr(value, source, context), tgt, out);
}

// ===== table lineage =====================================================

/// Table-level lineage: one `source → target` edge per read-role scan that
/// **feeds data** into a DML target. Occurrence-based on the *source* side
/// (a real table joined twice contributes two edges), but a CTE body — one
/// declaration shared by every `WITH … FROM c x JOIN c y` reference —
/// contributes once (it is materialized once and feeds once); without that
/// fold the same `base → target` edge would be emitted per reference,
/// mirroring nothing in the bound relation (a `Derived` ref qualified ambiguously
/// would still resolve once). Feeding sources are the scans on the value /
/// data path of the source — FROM / JOIN relations, value (projection)
/// subqueries, and referenced CTE bodies — never predicate (filter)
/// subqueries. A bare query, or a statement that moves no data, has no table
/// lineage. Backs [`crate::resolver::table_lineage`].
pub(super) fn collect_table_lineage(
    plan: &LogicalPlan,
    casing: IdentifierCasing,
) -> Vec<TableLineageEdge> {
    // `TraceContext::new` peels leading WITHs and keeps their CTE bodies, so a `CteRef`
    // on the feeding path resolves to the body's feeding scans. `fed_ctes`
    // is the set of CTE bodies already fed: a CTE body materializes once, so it feeds
    // the target once regardless of how many `CteRef`s point at it. It keys on
    // the resolved body's *identity* (its pointer), not the CTE name, so two
    // distinct CTEs that happen to share a name (shadowing across scopes) each
    // feed — a name key would collapse them. (The active-set on `context` terminates
    // a *recursive* self-reference; this set folds *distinct* references to the
    // *same* declaration — orthogonal concerns.)
    let mut context = TraceContext::new(plan, casing);
    let mut sources = Vec::new();
    let mut fed_ctes: Vec<usize> = Vec::new();
    let target = match peel_with(plan) {
        LogicalPlan::Insert(i) => {
            feeding_scans(&i.input, &mut context, &mut fed_ctes, &mut sources);
            // ON CONFLICT DO UPDATE SET col = value: a value-position subquery
            // (`= (SELECT … FROM other)`) feeds the target, like an UPDATE SET
            // RHS. An `EXCLUDED.x` ref feeds nothing new — it names the INSERT
            // source row, already collected from `i.input`.
            for a in &i.on_conflict {
                expr_feeding(&a.value, &mut context, &mut fed_ctes, &mut sources);
            }
            &i.target.reference
        }
        // UPDATE is per-assignment: each SET RHS's feeding sources flow into
        // *that assignment's* resolved target table — a multi-table
        // `UPDATE t1 JOIN t2 SET t2.b = t1.c` writes t1.c into t2, not the root.
        // (Handled out of line since it has several targets, unlike the
        // single-target DML below.)
        LogicalPlan::Update(u) => return update_table_lineage(u, &mut context),
        // CTAS / CREATE VIEW move data like INSERT; ALTER / DROP do not.
        LogicalPlan::CreateTableAs(c) => {
            feeding_scans(&c.input, &mut context, &mut fed_ctes, &mut sources);
            &c.target.reference
        }
        LogicalPlan::CreateView(c) => {
            feeding_scans(&c.input, &mut context, &mut fed_ctes, &mut sources);
            &c.target.reference
        }
        // MERGE feeds from the source relation plus each *written* WHEN value
        // (an UPDATE SET RHS; an INSERT value paired with a column). The ON /
        // predicate reads and an unpaired INSERT value do not feed.
        LogicalPlan::Merge(m) => {
            feeding_scans(&m.source, &mut context, &mut fed_ctes, &mut sources);
            for clause in &m.clauses {
                match clause {
                    MergeClause::Update { assignments } => {
                        for a in assignments {
                            expr_feeding(&a.value, &mut context, &mut fed_ctes, &mut sources);
                        }
                    }
                    MergeClause::Insert { columns, values } => {
                        for (_col, value) in columns.iter().zip(values) {
                            expr_feeding(value, &mut context, &mut fed_ctes, &mut sources);
                        }
                    }
                    MergeClause::Delete => {}
                }
            }
            &m.target.reference
        }
        // No table lineage: a bare query (no target), DDL that moves no value
        // (`Drop` / `Truncate` modelled as `Drop` / `AlterTable`), and the
        // structural / synthetic operators reached at root. Listed explicitly
        // so a new `LogicalPlan` variant forces a target decision.
        LogicalPlan::Scan(_)
        | LogicalPlan::Filter(_)
        | LogicalPlan::Join(_)
        | LogicalPlan::Aggregate(_)
        | LogicalPlan::Projection(_)
        | LogicalPlan::Sort(_)
        | LogicalPlan::SetOp(_)
        | LogicalPlan::SubqueryAlias(_)
        | LogicalPlan::TableFunction(_)
        | LogicalPlan::With(_)
        | LogicalPlan::CteRef(_)
        | LogicalPlan::Values(_)
        | LogicalPlan::Empty
        | LogicalPlan::Delete(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => return Vec::new(),
    };
    sources
        .into_iter()
        .map(|source| TableLineageEdge {
            source,
            target: target.clone(),
        })
        .collect()
}

/// Table lineage for an UPDATE: each SET assignment's RHS value feeds *its own*
/// resolved target table — so a multi-table `UPDATE t1 JOIN t2 SET t2.b = t1.c`
/// emits `t1 → t2`, not the root. The sources are the RHS value's traced base
/// columns projected to their tables (mirroring the column lineage, so the two
/// surfaces agree), as distinct `source → target` table pairs. A self-flow
/// (`SET a = a + 1` → `t → t`) is kept, matching both the column lineage (which
/// carries `t.a → t.a`) and the self-insert table edge (`INSERT INTO t SELECT *
/// FROM t` → `t → t`). The WHERE predicate and joined relations don't feed on
/// their own; only a value through a SET RHS does.
fn update_table_lineage<'a>(
    u: &'a Update,
    context: &mut TraceContext<'a>,
) -> Vec<TableLineageEdge> {
    let mut edges: Vec<TableLineageEdge> = Vec::new();
    for a in &u.assignments {
        let Some(target) = &a.target.table else {
            continue;
        };
        for (read, _kind) in origins_of_expr(&a.value, &u.input, context) {
            let Some(source) = read.reference.table else {
                continue;
            };
            if edges
                .iter()
                .any(|e| e.source.reference == source && &e.target == target)
            {
                continue; // distinct (source, target) pairs
            }
            edges.push(TableLineageEdge {
                source: TableRead {
                    reference: source,
                    resolution: read.resolution,
                },
                target: target.clone(),
            });
        }
    }
    edges
}

/// Collect the read-role scans that feed data up through `op` (a value / data
/// path): a join feeds both sides, a filter passes only its input (its
/// predicate subqueries do not feed), a projection also pulls its value
/// subqueries. A `CteRef` resolves to the referenced CTE body's feeding scans
/// — but only the first reference to a given declaration does; the body is
/// materialized once and `fed_ctes` tracks which declarations (by body
/// identity, not name) have already contributed. The `TraceContext` active-set
/// terminates a *recursive* self-reference (a separate concern).
fn feeding_scans<'a>(
    op: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
    fed_ctes: &mut Vec<usize>,
    out: &mut Vec<TableRead>,
) {
    match op {
        LogicalPlan::Scan(s) => out.push(TableRead {
            reference: s.table.clone(),
            resolution: s.resolution,
        }),
        LogicalPlan::Filter(f) => feeding_scans(&f.input, context, fed_ctes, out),
        LogicalPlan::Join(j) => {
            feeding_scans(&j.left, context, fed_ctes, out);
            feeding_scans(&j.right, context, fed_ctes, out);
        }
        LogicalPlan::Projection(p) => {
            feeding_scans(&p.input, context, fed_ctes, out);
            for ne in &p.exprs {
                expr_feeding(&ne.expr, context, fed_ctes, out);
            }
        }
        LogicalPlan::Aggregate(a) => feeding_scans(&a.input, context, fed_ctes, out),
        LogicalPlan::Sort(s) => feeding_scans(&s.input, context, fed_ctes, out),
        LogicalPlan::SubqueryAlias(sa) => feeding_scans(&sa.input, context, fed_ctes, out),
        // A PIVOT / … feeds from its wrapped inner table; the function args
        // are filter-position reads and do not feed.
        LogicalPlan::TableFunction(tf) => feeding_scans(&tf.input, context, fed_ctes, out),
        LogicalPlan::SetOp(so) => {
            feeding_scans(&so.left, context, fed_ctes, out);
            feeding_scans(&so.right, context, fed_ctes, out);
        }
        LogicalPlan::With(w) => {
            context.with_decls(&w.ctes, |context| {
                feeding_scans(&w.body, context, fed_ctes, out)
            });
        }
        LogicalPlan::CteRef(r) => {
            // A CTE body materializes once: the first reference to a given
            // declaration expands it, any further reference to the *same*
            // declaration is a no-op (it would emit a duplicate
            // `cte-body-scan → target` edge while `reads` — the body is walked
            // once at its declaration — stays folded). The dedup keys on the
            // resolved body's identity (its pointer), set inside `enter_cte`
            // after name resolution, so two distinct CTEs sharing a name
            // (shadowing across scopes) each feed; a name key would collapse
            // them and drop one's sources.
            context.enter_cte(&r.name, |context, body| {
                let id = body as *const LogicalPlan as usize;
                if fed_ctes.contains(&id) {
                    return;
                }
                fed_ctes.push(id);
                feeding_scans(body, context, fed_ctes, out)
            });
        }
        // A nested data-mover feeds through its source; DELETE / DROP / ALTER /
        // VALUES move no row data into a feeding path.
        LogicalPlan::Insert(i) => feeding_scans(&i.input, context, fed_ctes, out),
        LogicalPlan::Update(u) => feeding_scans(&u.input, context, fed_ctes, out),
        LogicalPlan::CreateTableAs(c) => feeding_scans(&c.input, context, fed_ctes, out),
        LogicalPlan::CreateView(c) => feeding_scans(&c.input, context, fed_ctes, out),
        LogicalPlan::Merge(m) => feeding_scans(&m.source, context, fed_ctes, out),
        LogicalPlan::Delete(_)
        | LogicalPlan::Drop(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Values(_)
        | LogicalPlan::Empty => {}
    }
}

/// The value-position subqueries of an expression feed table lineage (a scalar
/// projection subquery), mirroring `origins_of_expr`: `when` conditions, window
/// keys, and EXISTS / IN tests are filter position and do not feed. The
/// classification lives on the structural accessors on `Expr`, so this walker
/// reads cleanly off `value_subplans` and `value_operands` — a new `Expr`
/// variant declares its operand lists once and this stays in step.
fn expr_feeding<'a>(
    expr: &'a Expr,
    context: &mut TraceContext<'a>,
    fed_ctes: &mut Vec<usize>,
    out: &mut Vec<TableRead>,
) {
    for sub in expr.value_subplans() {
        feeding_scans(sub, context, fed_ctes, out);
    }
    for child in expr.value_operands() {
        expr_feeding(child, context, fed_ctes, out);
    }
}

#[cfg(test)]
mod tests {
    use super::super::binder::build_with_diagnostics;
    use super::super::{column_lineage, reads, table_reads};
    use super::*;
    use crate::casing::{canonical_quote, IdentifierCasing, IdentifierStyle};
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    fn plan(sql: &str) -> LogicalPlan {
        let statements = Parser::parse_sql(&GenericDialect {}, sql).unwrap();
        let style = IdentifierStyle {
            casing: IdentifierCasing::for_dialect(&GenericDialect {}),
            quote: canonical_quote(&GenericDialect {}),
        };
        build_with_diagnostics(&statements[0], None, style).0
    }

    fn read_names(plan: &LogicalPlan) -> Vec<String> {
        let mut v: Vec<String> = reads(plan)
            .iter()
            .map(|r| match &r.reference.table {
                Some(t) => format!("{}.{}", t.name.value, r.reference.name.value),
                None => format!("?.{}", r.reference.name.value),
            })
            .collect();
        v.sort();
        v
    }

    fn lineage_strs(plan: &LogicalPlan) -> Vec<String> {
        let casing = IdentifierCasing::for_dialect(&GenericDialect {});
        let mut v: Vec<String> = column_lineage(plan, casing)
            .iter()
            .map(|e| {
                let src = e
                    .source
                    .reference
                    .table
                    .as_ref()
                    .map_or("?".to_string(), |t| t.name.value.clone());
                let tgt = match &e.target {
                    ColumnTarget::QueryOutput { name, .. } => {
                        name.as_ref().map_or("?".to_string(), |n| n.value.clone())
                    }
                    ColumnTarget::Relation(r) => r.name.value.clone(),
                };
                let k = match e.kind {
                    ColumnLineageKind::Passthrough => "=",
                    ColumnLineageKind::Transformation => "~",
                };
                format!("{}.{} {}> {}", src, e.source.reference.name.value, k, tgt)
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn reads_occurrence_based_across_clauses() {
        // `a` referenced in projection AND where → two reads (occurrence).
        assert_eq!(
            read_names(&plan("SELECT a FROM t WHERE a > 0")),
            vec!["t.a", "t.a"]
        );
    }

    #[test]
    fn passthrough_vs_transformation_lineage() {
        assert_eq!(
            lineage_strs(&plan("SELECT a, b + c AS s FROM t")),
            vec!["t.a => a", "t.b ~> s", "t.c ~> s"]
        );
    }

    #[test]
    fn join_reads_both_sides_predicate_not_an_origin() {
        let op = plan("SELECT t1.x FROM t1 JOIN t2 ON t1.id = t2.id");
        // reads: x (proj) + the two ON columns.
        assert_eq!(read_names(&op), vec!["t1.id", "t1.x", "t2.id"]);
        // lineage: only the projected x → output; ON columns are not origins.
        assert_eq!(lineage_strs(&op), vec!["t1.x => x"]);
    }

    #[test]
    fn table_reads_one_per_scan() {
        let op = plan("SELECT t1.x FROM t1 JOIN t2 ON t1.id = t2.id");
        let mut names: Vec<String> = table_reads(&op)
            .iter()
            .map(|r| r.reference.name.value.clone())
            .collect();
        names.sort();
        assert_eq!(names, vec!["t1", "t2"]);
    }
}
