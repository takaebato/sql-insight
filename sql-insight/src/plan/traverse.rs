//! Walking the bound [`Operator`] tree for the extraction surfaces.
//!
//! - [`reads`] — every physical base-column reference (occurrence-based; a
//!   `Derived` ref is dropped, its read counted at the inner producer).
//! - [`table_reads`] — every base table scanned.
//! - [`column_lineage`] — `source → target` edges, by tracing each output
//!   column's value expression to its base columns ([`origins_of_expr`] /
//!   [`origins_into`], the `getColumnOrigins`-style traversal). A predicate's
//!   columns are reads but never origins, so value/filter falls out for free.
//!   A bare query targets `QueryOutput`; a DML root pairs the source's output
//!   columns positionally with the write target's columns (`Relation`).
//! - [`writes`] — every column written (DML target columns).
//! - [`table_writes`] — every table written to.
//! - [`table_lineage`] — `source → target` table edges: the read-role scans
//!   that **feed data** into a DML target (value path only — predicate
//!   subqueries do not feed).

use sqlparser::ast::Ident;

use super::operator::{Binding, ColRef, Cte, Expr, MergeClause, NamedExpr, Operator};
use crate::extractor::{ColumnLineageEdge, ColumnLineageKind, ColumnTarget, TableLineageEdge};
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableRead, TableReference};

// ===== reads =============================================================

/// Every physical base-column read the plan expresses, occurrence-based.
pub(crate) fn reads(op: &Operator) -> Vec<ColumnRead> {
    let mut out = Vec::new();
    collect_reads(op, &mut out);
    out
}

fn collect_reads(op: &Operator, out: &mut Vec<ColumnRead>) {
    for expr in own_exprs(op) {
        expr_reads(expr, out);
    }
    for child in children(op) {
        collect_reads(child, out);
    }
    // A `With` walks each CTE body once (declarations); a `CteRef` is a leaf.
    if let Operator::With(w) = op {
        for cte in &w.ctes {
            collect_reads(&cte.plan, out);
        }
    }
}

/// The reads of one expression: every column reference (value AND filter
/// position — `reads` doesn't distinguish), plus the reads of nested
/// subqueries. A `Derived` reference is dropped.
fn expr_reads(expr: &Expr, out: &mut Vec<ColumnRead>) {
    match expr {
        Expr::Column(c) => out.extend(column_read(c)),
        Expr::Call { args } => args.iter().for_each(|e| expr_reads(e, out)),
        Expr::Case { when, then, else_ } => {
            when.iter().chain(then).for_each(|e| expr_reads(e, out));
            if let Some(e) = else_ {
                expr_reads(e, out);
            }
        }
        Expr::Window {
            arg,
            partition,
            order,
        } => {
            expr_reads(arg, out);
            partition
                .iter()
                .chain(order)
                .for_each(|e| expr_reads(e, out));
        }
        Expr::Subquery(plan) | Expr::Exists(plan) => collect_reads(plan, out),
        Expr::InSubquery { expr, subquery } => {
            expr_reads(expr, out);
            collect_reads(subquery, out);
        }
    }
}

/// A column reference as a public read — `None` for a `Derived` ref (its
/// physical read was counted at the inner producer).
fn column_read(c: &ColRef) -> Option<ColumnRead> {
    let (table, resolution) = match &c.binding {
        Binding::Base { table, resolution } => (Some(table.clone()), *resolution),
        Binding::Unresolved => (None, ResolutionKind::Unresolved),
        Binding::Ambiguous => (None, ResolutionKind::Ambiguous),
        Binding::Derived => return None,
    };
    Some(ColumnRead {
        reference: ColumnReference {
            table,
            name: c.name.clone(),
        },
        resolution,
    })
}

// ===== table reads =======================================================

/// Every base table scanned (read role), occurrence-based.
pub(crate) fn table_reads(op: &Operator) -> Vec<TableRead> {
    let mut out = Vec::new();
    walk(op, &mut |o| {
        if let Operator::Scan(s) = o {
            out.push(TableRead {
                reference: s.table.clone(),
                resolution: s.resolution,
            });
        }
    });
    out
}

fn walk(op: &Operator, f: &mut impl FnMut(&Operator)) {
    f(op);
    for child in children(op) {
        walk(child, f);
    }
    // Subqueries nested in this node's expressions (a `WHERE … IN (SELECT …)`
    // / scalar subquery) are sub-plans too — their scans must surface.
    for sub in own_expr_subplans(op) {
        walk(sub, f);
    }
    if let Operator::With(w) = op {
        for cte in &w.ctes {
            walk(&cte.plan, f);
        }
    }
}

/// The sub-plans appearing in this node's *own* expressions (not its
/// children's). `walk` recurses into them; `collect_reads` reaches them via
/// `expr_reads`.
fn own_expr_subplans(op: &Operator) -> Vec<&Operator> {
    let mut out = Vec::new();
    for expr in own_exprs(op) {
        collect_subplans(expr, &mut out);
    }
    out
}

fn collect_subplans<'a>(expr: &'a Expr, out: &mut Vec<&'a Operator>) {
    match expr {
        Expr::Column(_) => {}
        Expr::Call { args } => args.iter().for_each(|e| collect_subplans(e, out)),
        Expr::Case { when, then, else_ } => {
            when.iter()
                .chain(then)
                .for_each(|e| collect_subplans(e, out));
            if let Some(e) = else_ {
                collect_subplans(e, out);
            }
        }
        Expr::Window {
            arg,
            partition,
            order,
        } => {
            collect_subplans(arg, out);
            partition
                .iter()
                .chain(order)
                .for_each(|e| collect_subplans(e, out));
        }
        Expr::Subquery(plan) | Expr::Exists(plan) => out.push(plan),
        Expr::InSubquery { expr, subquery } => {
            collect_subplans(expr, out);
            out.push(subquery);
        }
    }
}

// ===== column lineage ====================================================

/// The column lineage of a statement. A bare query emits `source →
/// QueryOutput` edges (each output column traced to its base columns); a DML
/// root pairs the source's output columns positionally with the write
/// target's columns and emits `source → Relation` edges. A leading `WITH` is
/// peeled (its CTE bodies feed the root through `CteRef` expansion, they are
/// not lineage roots).
pub(crate) fn column_lineage(op: &Operator) -> Vec<ColumnLineageEdge> {
    let mut ctx = Ctx::new(op);
    let mut edges = Vec::new();
    match peel_with(op) {
        // INSERT … <source>: pair the source's outputs with the target columns.
        // A statement-level `WITH` rides on the source (the parser attaches it
        // there, so the `With` is *inside* `input`, not above the `Insert`);
        // `enter_withs` pushes its CTEs into the ctx so a `CteRef` resolves.
        Operator::Insert(i) => {
            let src = enter_withs(&i.input, &mut ctx);
            relation_lineage(&i.columns, &i.target, src, &mut ctx, &mut edges);
            // ON CONFLICT DO UPDATE SET col = value: each `value → target.col`,
            // an `EXCLUDED.x` ref mapped to the source's like-positioned output.
            for a in &i.on_conflict {
                let target = ColumnTarget::Relation(ColumnReference {
                    table: Some(i.target.clone()),
                    name: a.target.clone(),
                });
                for (source, kind) in conflict_value_origins(&a.value, &i.columns, src, &mut ctx) {
                    edges.push(ColumnLineageEdge {
                        source,
                        target: target.clone(),
                        kind,
                    });
                }
            }
            returning_lineage(&i.returning, &i.input, &mut ctx, &mut edges);
        }
        // UPDATE … SET col = expr: each assignment's RHS value traces to its
        // target column (`Relation` edge). A `Derived` RHS ref (a column of a
        // FROM derived table) traces through `input`.
        Operator::Update(u) => {
            for a in &u.assignments {
                let target = ColumnTarget::Relation(ColumnReference {
                    table: Some(u.target.clone()),
                    name: a.target.clone(),
                });
                for (source, kind) in origins_of_expr(&a.value, &u.input, &mut ctx) {
                    edges.push(ColumnLineageEdge {
                        source,
                        target: target.clone(),
                        kind,
                    });
                }
            }
            returning_lineage(&u.returning, &u.input, &mut ctx, &mut edges);
        }
        // DELETE moves no data (rows go wholesale) — its only lineage is a
        // RETURNING projection of the deleted rows.
        Operator::Delete(d) => returning_lineage(&d.returning, &d.input, &mut ctx, &mut edges),
        // CTAS / CREATE VIEW move data like INSERT: pair the source's outputs
        // with the new relation's columns.
        Operator::CreateTableAs(c) => {
            let src = enter_withs(&c.input, &mut ctx);
            relation_lineage(&c.columns, &c.target, src, &mut ctx, &mut edges);
        }
        Operator::CreateView(c) => {
            let src = enter_withs(&c.input, &mut ctx);
            relation_lineage(&c.columns, &c.target, src, &mut ctx, &mut edges);
        }
        // MERGE: each WHEN action's value traces to its target column (an
        // UPDATE SET `RHS → target.col`; an INSERT `value → target.col`). A
        // `Derived` source ref traces through the source relation. DELETE
        // moves no value.
        Operator::Merge(m) => {
            for clause in &m.clauses {
                match clause {
                    MergeClause::Update { assignments } => {
                        for a in assignments {
                            merge_value_edges(
                                &a.target, &a.value, &m.target, &m.source, &mut ctx, &mut edges,
                            );
                        }
                    }
                    MergeClause::Insert { columns, values } => {
                        for (column, value) in columns.iter().zip(values) {
                            merge_value_edges(
                                column, value, &m.target, &m.source, &mut ctx, &mut edges,
                            );
                        }
                    }
                    MergeClause::Delete => {}
                }
            }
        }
        // A bare query (or unmodelled root): one `QueryOutput` group per
        // projection (a set operation has one per branch — positions restart
        // per branch, mirroring the resolver).
        _ => query_output_lineage(op, &mut ctx, &mut edges),
    }
    edges
}

/// Bare-query lineage: each output column becomes a `QueryOutput` target at
/// its position, one edge per traced origin. `output_operands` peels the
/// clause layers and `With`, and yields one operand per set-operation branch.
fn query_output_lineage<'a>(op: &'a Operator, ctx: &mut Ctx<'a>, out: &mut Vec<ColumnLineageEdge>) {
    for (outputs, input) in output_operands(op) {
        for (position, ne) in outputs.iter().enumerate() {
            let target = ColumnTarget::QueryOutput {
                name: ne.name.clone(),
                position,
            };
            for (source, kind) in origins_of_expr(&ne.expr, input, ctx) {
                out.push(ColumnLineageEdge {
                    source,
                    target: target.clone(),
                    kind,
                });
            }
        }
    }
}

/// DML relation lineage: pair each source output operand's columns positionally
/// with the target `columns` (so a UNION-sourced INSERT pairs every branch) and
/// emit one `Relation` edge per traced origin.
fn relation_lineage<'a>(
    columns: &[Ident],
    target: &TableReference,
    input: &'a Operator,
    ctx: &mut Ctx<'a>,
    out: &mut Vec<ColumnLineageEdge>,
) {
    for (outputs, src_input) in output_operands(input) {
        for (target_column, ne) in columns.iter().zip(outputs) {
            let tgt = ColumnTarget::Relation(ColumnReference {
                table: Some(target.clone()),
                name: target_column.clone(),
            });
            for (source, kind) in origins_of_expr(&ne.expr, src_input, ctx) {
                out.push(ColumnLineageEdge {
                    source,
                    target: tgt.clone(),
                    kind,
                });
            }
        }
    }
}

/// Emit a DML statement's `RETURNING` lineage: each returned column is a
/// `QueryOutput` target (the statement both writes and projects), its origins
/// traced through `input` (the written relation / FROM scope).
fn returning_lineage<'a>(
    returning: &'a [NamedExpr],
    input: &'a Operator,
    ctx: &mut Ctx<'a>,
    out: &mut Vec<ColumnLineageEdge>,
) {
    for (position, ne) in returning.iter().enumerate() {
        let target = ColumnTarget::QueryOutput {
            name: ne.name.clone(),
            position,
        };
        for (source, kind) in origins_of_expr(&ne.expr, input, ctx) {
            out.push(ColumnLineageEdge {
                source,
                target: target.clone(),
                kind,
            });
        }
    }
}

/// The origins of an ON CONFLICT DO UPDATE value. Like [`origins_of_expr`],
/// but an `EXCLUDED.col` reference (a `Derived` ref, qualified `excluded`) maps
/// to the INSERT source's like-positioned output column — or, when the source
/// has no inspectable projection (a `VALUES` source), to the `EXCLUDED.col`
/// pseudo-column itself (a synthetic lineage source, not a read).
fn conflict_value_origins<'a>(
    value: &'a Expr,
    columns: &[Ident],
    source: &'a Operator,
    ctx: &mut Ctx<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match value {
        // A `Derived` ref here is `EXCLUDED.col` (the only synthetic relation in
        // a conflict scope). Map it to the source's `col`-positioned output.
        Expr::Column(c) if matches!(c.binding, Binding::Derived) => {
            match columns.iter().position(|t| idents_eq(t, &c.name)) {
                Some(i) => match output_operands(source).first() {
                    Some((outputs, input)) if outputs.len() > i => {
                        origins_of_expr(&outputs[i].expr, input, ctx)
                    }
                    // No inspectable projection (VALUES): EXCLUDED.col stays a
                    // synthetic pseudo-source.
                    _ => vec![(excluded_source(c), ColumnLineageKind::Passthrough)],
                },
                None => Vec::new(),
            }
        }
        // A non-EXCLUDED ref (a target column, MySQL `VALUES(col)` inner, …)
        // and the structural variants trace like any value.
        Expr::Column(_) => origins_of_expr(value, source, ctx),
        Expr::Call { args } => transform(
            args.iter()
                .flat_map(|e| conflict_value_origins(e, columns, source, ctx)),
        ),
        Expr::Case { then, else_, .. } => {
            let mut sources: Vec<_> = then
                .iter()
                .flat_map(|e| conflict_value_origins(e, columns, source, ctx))
                .collect();
            if let Some(e) = else_ {
                sources.extend(conflict_value_origins(e, columns, source, ctx));
            }
            transform(sources)
        }
        Expr::Window { arg, .. } => transform(conflict_value_origins(arg, columns, source, ctx)),
        Expr::Subquery(plan) => transform(query_col0_origins(plan, ctx)),
        Expr::Exists(_) | Expr::InSubquery { .. } => Vec::new(),
    }
}

/// The `EXCLUDED.col` pseudo-table lineage source (when the source can't be
/// collapsed through): the qualifier (`EXCLUDED`, original text) as the table.
fn excluded_source(c: &ColRef) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: c.qualifier.clone().map(|q| TableReference {
                catalog: None,
                schema: None,
                name: q,
            }),
            name: c.name.clone(),
        },
        resolution: ResolutionKind::Inferred,
    }
}

/// A synthetic single-segment lineage source `table.name` (`Inferred`) — for a
/// table-function column, whose produced value flows out but has no base
/// column to collapse to.
fn synthetic_source(table: &Ident, name: &Ident) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: Some(TableReference {
                catalog: None,
                schema: None,
                name: table.clone(),
            }),
            name: name.clone(),
        },
        resolution: ResolutionKind::Inferred,
    }
}

/// Emit the `value → target.column` lineage edges of one MERGE WHEN value, its
/// origins traced through the `source` relation.
fn merge_value_edges<'a>(
    column: &Ident,
    value: &'a Expr,
    target: &TableReference,
    source: &'a Operator,
    ctx: &mut Ctx<'a>,
    out: &mut Vec<ColumnLineageEdge>,
) {
    let tgt = ColumnTarget::Relation(ColumnReference {
        table: Some(target.clone()),
        name: column.clone(),
    });
    for (src, kind) in origins_of_expr(value, source, ctx) {
        out.push(ColumnLineageEdge {
            source: src,
            target: tgt.clone(),
            kind,
        });
    }
}

/// A query's output operands: one `(output columns, producing input)` per
/// set-operation branch (a plain query has a single operand). Peels the clause
/// layers above the projection (GROUP BY / HAVING `Filter`, ORDER BY `Sort`)
/// and `With`.
fn output_operands(op: &Operator) -> Vec<(&[NamedExpr], &Operator)> {
    match op {
        Operator::Project(p) => vec![(&p.exprs, &p.input)],
        Operator::Sort(s) => output_operands(&s.input),
        Operator::Filter(f) => output_operands(&f.input),
        Operator::With(w) => output_operands(&w.body),
        Operator::SetOp(so) => {
            let mut operands = output_operands(&so.left);
            operands.extend(output_operands(&so.right));
            operands
        }
        _ => Vec::new(),
    }
}

// ===== writes / table writes / table lineage =============================

/// Every column the statement writes — a DML root's target columns, qualified
/// by the write target. Order follows source order (the public contract). A
/// leading `WITH` is peeled.
pub(crate) fn writes(op: &Operator) -> Vec<ColumnReference> {
    match peel_with(op) {
        // INSERT columns, then any ON CONFLICT DO UPDATE SET targets (extra
        // writes on the same relation).
        Operator::Insert(i) => {
            let mut w = qualify(&i.columns, &i.target);
            w.extend(i.on_conflict.iter().map(|a| ColumnReference {
                table: Some(i.target.clone()),
                name: a.target.clone(),
            }));
            w
        }
        // Each SET assignment writes its target column.
        Operator::Update(u) => u
            .assignments
            .iter()
            .map(|a| ColumnReference {
                table: Some(u.target.clone()),
                name: a.target.clone(),
            })
            .collect(),
        // CTAS / CREATE VIEW write the new relation's columns; ALTER TABLE
        // writes its column-naming operations' columns.
        Operator::CreateTableAs(c) => qualify(&c.columns, &c.target),
        Operator::CreateView(c) => qualify(&c.columns, &c.target),
        Operator::AlterTable(a) => qualify(&a.columns, &a.target),
        // MERGE writes each WHEN action's target columns (UPDATE SET targets;
        // INSERT columns paired with values).
        Operator::Merge(m) => m
            .clauses
            .iter()
            .flat_map(|clause| merge_clause_writes(clause, &m.target))
            .collect(),
        _ => Vec::new(),
    }
}

/// The columns one MERGE WHEN action writes, qualified by the target.
fn merge_clause_writes(clause: &MergeClause, target: &TableReference) -> Vec<ColumnReference> {
    match clause {
        MergeClause::Update { assignments } => assignments
            .iter()
            .map(|a| ColumnReference {
                table: Some(target.clone()),
                name: a.target.clone(),
            })
            .collect(),
        // Only columns paired with a value are written (a column-less / short
        // INSERT writes nothing; `zip` stops at the shorter side).
        MergeClause::Insert { columns, values } => columns
            .iter()
            .zip(values)
            .map(|(name, _)| ColumnReference {
                table: Some(target.clone()),
                name: name.clone(),
            })
            .collect(),
        MergeClause::Delete => Vec::new(),
    }
}

/// Every table the statement writes to — one per DML target. A leading `WITH`
/// is peeled.
pub(crate) fn table_writes(op: &Operator) -> Vec<TableReference> {
    match peel_with(op) {
        Operator::Insert(i) => vec![i.target.clone()],
        Operator::Update(u) => vec![u.target.clone()],
        // A DELETE removes rows from each of its targets.
        Operator::Delete(d) => d.targets.clone(),
        Operator::CreateTableAs(c) => vec![c.target.clone()],
        Operator::CreateView(c) => vec![c.target.clone()],
        Operator::AlterTable(a) => vec![a.target.clone()],
        Operator::Merge(m) => vec![m.target.clone()],
        // DROP / TRUNCATE name their relations directly as write targets.
        Operator::Drop(d) => d.targets.clone(),
        _ => Vec::new(),
    }
}

/// Table-level lineage: one `source → target` edge per read-role scan that
/// **feeds data** into a DML target, occurrence-based. Feeding sources are the
/// scans on the value / data path of the source — FROM / JOIN relations, value
/// (projection) subqueries, and referenced CTE bodies — never predicate
/// (filter) subqueries. A bare query, or a statement that moves no data, has
/// no table lineage.
pub(crate) fn table_lineage(op: &Operator) -> Vec<TableLineageEdge> {
    // `Ctx::new` peels leading WITHs and keeps their CTE bodies, so a `CteRef`
    // on the feeding path resolves to the body's feeding scans.
    let mut ctx = Ctx::new(op);
    let mut sources = Vec::new();
    let target = match peel_with(op) {
        Operator::Insert(i) => {
            feeding_scans(&i.input, &mut ctx, &mut sources);
            &i.target
        }
        // UPDATE feeds from the FROM relations (the read `input`) AND any value
        // subquery in a SET RHS; the WHERE predicate / a self-reference to the
        // target do not feed.
        Operator::Update(u) => {
            feeding_scans(&u.input, &mut ctx, &mut sources);
            for a in &u.assignments {
                expr_feeding(&a.value, &mut ctx, &mut sources);
            }
            &u.target
        }
        // CTAS / CREATE VIEW move data like INSERT; ALTER / DROP do not.
        Operator::CreateTableAs(c) => {
            feeding_scans(&c.input, &mut ctx, &mut sources);
            &c.target
        }
        Operator::CreateView(c) => {
            feeding_scans(&c.input, &mut ctx, &mut sources);
            &c.target
        }
        // MERGE feeds from the source relation plus each *written* WHEN value
        // (an UPDATE SET RHS; an INSERT value paired with a column). The ON /
        // predicate reads and an unpaired INSERT value do not feed.
        Operator::Merge(m) => {
            feeding_scans(&m.source, &mut ctx, &mut sources);
            for clause in &m.clauses {
                match clause {
                    MergeClause::Update { assignments } => {
                        for a in assignments {
                            expr_feeding(&a.value, &mut ctx, &mut sources);
                        }
                    }
                    MergeClause::Insert { columns, values } => {
                        for (_col, value) in columns.iter().zip(values) {
                            expr_feeding(value, &mut ctx, &mut sources);
                        }
                    }
                    MergeClause::Delete => {}
                }
            }
            &m.target
        }
        _ => return Vec::new(),
    };
    sources
        .into_iter()
        .map(|source| TableLineageEdge {
            source,
            target: target.clone(),
        })
        .collect()
}

/// Qualify bare written column names with the write target.
fn qualify(columns: &[Ident], target: &TableReference) -> Vec<ColumnReference> {
    columns
        .iter()
        .map(|name| ColumnReference {
            table: Some(target.clone()),
            name: name.clone(),
        })
        .collect()
}

/// Collect the read-role scans that feed data up through `op` (a value / data
/// path): a join feeds both sides, a filter passes only its input (its
/// predicate subqueries do not feed), a projection also pulls its value
/// subqueries. A `CteRef` resolves to the referenced CTE body's feeding scans;
/// the `Ctx` active-set terminates a recursive self-reference.
fn feeding_scans<'a>(op: &'a Operator, ctx: &mut Ctx<'a>, out: &mut Vec<TableRead>) {
    match op {
        Operator::Scan(s) => out.push(TableRead {
            reference: s.table.clone(),
            resolution: s.resolution,
        }),
        Operator::Filter(f) => feeding_scans(&f.input, ctx, out),
        Operator::Join(j) => {
            feeding_scans(&j.left, ctx, out);
            feeding_scans(&j.right, ctx, out);
        }
        Operator::Project(p) => {
            feeding_scans(&p.input, ctx, out);
            for ne in &p.exprs {
                expr_feeding(&ne.expr, ctx, out);
            }
        }
        Operator::Aggregate(a) => feeding_scans(&a.input, ctx, out),
        Operator::Sort(s) => feeding_scans(&s.input, ctx, out),
        Operator::SubqueryAlias(sa) => feeding_scans(&sa.input, ctx, out),
        // A PIVOT / … feeds from its wrapped inner table; the function args
        // are filter-position reads and do not feed.
        Operator::TableFunction(tf) => feeding_scans(&tf.input, ctx, out),
        Operator::SetOp(so) => {
            feeding_scans(&so.left, ctx, out);
            feeding_scans(&so.right, ctx, out);
        }
        Operator::With(w) => {
            let added = w.ctes.len();
            w.ctes.iter().for_each(|c| ctx.ctes.push(c));
            feeding_scans(&w.body, ctx, out);
            ctx.ctes.truncate(ctx.ctes.len() - added);
        }
        Operator::CteRef(r) => {
            if ctx.active.iter().any(|n| n == &r.name.value) {
                return; // recursive self-reference — terminate
            }
            if let Some(cte) = ctx
                .ctes
                .iter()
                .rev()
                .find(|c| idents_eq(&c.name, &r.name))
                .copied()
            {
                ctx.active.push(r.name.value.clone());
                feeding_scans(&cte.plan, ctx, out);
                ctx.active.pop();
            }
        }
        // A nested data-mover feeds through its source; DELETE / DROP / ALTER /
        // VALUES move no row data into a feeding path.
        Operator::Insert(i) => feeding_scans(&i.input, ctx, out),
        Operator::Update(u) => feeding_scans(&u.input, ctx, out),
        Operator::CreateTableAs(c) => feeding_scans(&c.input, ctx, out),
        Operator::CreateView(c) => feeding_scans(&c.input, ctx, out),
        Operator::Merge(m) => feeding_scans(&m.source, ctx, out),
        Operator::Delete(_)
        | Operator::Drop(_)
        | Operator::AlterTable(_)
        | Operator::Values(_)
        | Operator::Empty => {}
    }
}

/// The value-position subqueries of an expression feed table lineage (a scalar
/// projection subquery), mirroring `origins_of_expr`: `when` conditions, window
/// keys, and EXISTS / IN tests are filter position and do not feed.
fn expr_feeding<'a>(expr: &'a Expr, ctx: &mut Ctx<'a>, out: &mut Vec<TableRead>) {
    match expr {
        Expr::Column(_) => {}
        Expr::Call { args } => args.iter().for_each(|e| expr_feeding(e, ctx, out)),
        Expr::Case { then, else_, .. } => {
            then.iter().for_each(|e| expr_feeding(e, ctx, out));
            if let Some(e) = else_ {
                expr_feeding(e, ctx, out);
            }
        }
        Expr::Window { arg, .. } => expr_feeding(arg, ctx, out),
        Expr::Subquery(plan) => feeding_scans(plan, ctx, out),
        Expr::Exists(_) | Expr::InSubquery { .. } => {}
    }
}

/// Peel leading `With` nodes to the wrapped root (a query or DML root).
fn peel_with(op: &Operator) -> &Operator {
    let mut node = op;
    while let Operator::With(w) = node {
        node = &w.body;
    }
    node
}

/// Peel leading `With` nodes off `op`, pushing their CTE declarations into
/// `ctx` so a `CteRef` below resolves during the trace, and return the peeled
/// root. (`Ctx::new` already does this for a query's leading `WITH`; a DML
/// source carries its own `WITH`, reached only here.) The push is not popped —
/// the ctx is per-statement scratch, discarded after the walk.
fn enter_withs<'a>(op: &'a Operator, ctx: &mut Ctx<'a>) -> &'a Operator {
    let mut node = op;
    while let Operator::With(w) = node {
        w.ctes.iter().for_each(|c| ctx.ctes.push(c));
        node = &w.body;
    }
    node
}

// ===== the column-origin traversal =======================================

/// CTE environment for expanding `CteRef`s during a trace, plus the
/// active-set that terminates a recursive self-reference.
struct Ctx<'a> {
    ctes: Vec<&'a Cte>,
    active: Vec<String>,
}

impl<'a> Ctx<'a> {
    fn new(op: &'a Operator) -> Self {
        // Collect leading `With` declarations so a `CteRef` on a traced path
        // resolves to its body.
        let mut ctes = Vec::new();
        let mut node = op;
        while let Operator::With(w) = node {
            ctes.extend(w.ctes.iter());
            node = &w.body;
        }
        Ctx {
            ctes,
            active: Vec::new(),
        }
    }
}

/// The base-column origins of an expression's **value**, each with the
/// composed lineage kind. Filter-position operands (CASE conditions, window
/// keys, EXISTS / IN tests) are not traced — they are reads, not origins.
fn origins_of_expr<'a>(
    expr: &'a Expr,
    input: &'a Operator,
    ctx: &mut Ctx<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match expr {
        Expr::Column(c) => match &c.binding {
            // Base / unresolved / ambiguous refs are their own origin.
            Binding::Base { .. } | Binding::Unresolved | Binding::Ambiguous => column_read(c)
                .map(|r| vec![(r, ColumnLineageKind::Passthrough)])
                .unwrap_or_default(),
            // A derived ref traces through the producer that defines it.
            Binding::Derived => origins_into(input, c.qualifier.as_ref(), &c.name, ctx),
        },
        Expr::Call { args } => transform(args.iter().flat_map(|e| origins_of_expr(e, input, ctx))),
        Expr::Case { then, else_, .. } => {
            // `when` conditions are filter — only the results are value.
            let mut sources: Vec<_> = then
                .iter()
                .flat_map(|e| origins_of_expr(e, input, ctx))
                .collect();
            if let Some(e) = else_ {
                sources.extend(origins_of_expr(e, input, ctx));
            }
            transform(sources)
        }
        // The function argument is value; partition / order keys are filter.
        Expr::Window { arg, .. } => transform(origins_of_expr(arg, input, ctx)),
        // A scalar subquery's first output column flows as a transformation.
        Expr::Subquery(plan) => transform(query_col0_origins(plan, ctx)),
        // Tests contribute no value origin (their columns are reads only).
        Expr::Exists(_) | Expr::InSubquery { .. } => Vec::new(),
    }
}

/// Trace the named output column of `op` down to its base origins (used to
/// expand a `Derived` reference through its producing operator).
fn origins_into<'a>(
    op: &'a Operator,
    qualifier: Option<&Ident>,
    name: &Ident,
    ctx: &mut Ctx<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match op {
        Operator::Project(p) => match find_named(&p.exprs, name) {
            Some(ne) => origins_of_expr(&ne.expr, &p.input, ctx),
            None => Vec::new(),
        },
        Operator::Aggregate(a) => {
            if let Some(ne) = find_named(&a.group_by, name) {
                origins_of_expr(&ne.expr, &a.input, ctx)
            } else if let Some(ne) = find_named(&a.aggregates, name) {
                transform(origins_of_expr(&ne.expr, &a.input, ctx))
            } else {
                Vec::new()
            }
        }
        Operator::Filter(f) => origins_into(&f.input, qualifier, name, ctx),
        Operator::Sort(s) => origins_into(&s.input, qualifier, name, ctx),
        Operator::Join(j) => {
            let mut o = origins_into(&j.left, qualifier, name, ctx);
            o.extend(origins_into(&j.right, qualifier, name, ctx));
            o
        }
        Operator::SubqueryAlias(sa) => {
            if qualifier.is_none_or(|q| idents_eq(q, &sa.alias)) {
                origins_into(&sa.input, None, name, ctx)
            } else {
                Vec::new()
            }
        }
        // An opaque table function: its produced columns are dynamic, so a ref
        // through its alias is a synthetic lineage source (the alias as table)
        // — not collapsible to a base column.
        Operator::TableFunction(tf) => match &tf.alias {
            Some(alias) if qualifier.is_none_or(|q| idents_eq(q, alias)) => {
                vec![(
                    synthetic_source(alias, name),
                    ColumnLineageKind::Passthrough,
                )]
            }
            _ => Vec::new(),
        },
        Operator::SetOp(so) => {
            let mut o = origins_into(&so.left, qualifier, name, ctx);
            o.extend(origins_into(&so.right, qualifier, name, ctx));
            o
        }
        Operator::With(w) => {
            let added = w.ctes.len();
            w.ctes.iter().for_each(|c| ctx.ctes.push(c));
            let o = origins_into(&w.body, qualifier, name, ctx);
            ctx.ctes.truncate(ctx.ctes.len() - added);
            o
        }
        Operator::CteRef(r) => {
            if ctx.active.iter().any(|n| n == &r.name.value) {
                return Vec::new(); // recursive self-reference — terminate
            }
            let Some(cte) = ctx
                .ctes
                .iter()
                .rev()
                .find(|c| idents_eq(&c.name, &r.name))
                .copied()
            else {
                return Vec::new();
            };
            ctx.active.push(r.name.value.clone());
            let o = origins_into(&cte.plan, None, name, ctx);
            ctx.active.pop();
            o
        }
        // A `Derived` reference resolves at a producer's named output (a
        // `Project` / `Aggregate` expr), never at a raw `Scan` — a reference to
        // a base column is `Binding::Base` and returns directly, not via this
        // traversal. So a `Scan` reached here (e.g. the other side of a join
        // the qualified name doesn't own) contributes nothing.
        Operator::Scan(_) | Operator::Values(_) | Operator::Empty => Vec::new(),
        // DML/DDL roots are not column producers traced into here.
        Operator::Insert(_)
        | Operator::Update(_)
        | Operator::Delete(_)
        | Operator::Merge(_)
        | Operator::CreateTableAs(_)
        | Operator::CreateView(_)
        | Operator::AlterTable(_)
        | Operator::Drop(_) => Vec::new(),
    }
}

/// The origins of a (sub)query's first output column (a scalar subquery's
/// value).
fn query_col0_origins<'a>(
    op: &'a Operator,
    ctx: &mut Ctx<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match output_operands(op).first() {
        Some((outputs, input)) => match outputs.first() {
            Some(ne) => origins_of_expr(&ne.expr, input, ctx),
            None => Vec::new(),
        },
        None => Vec::new(),
    }
}

// ===== helpers ===========================================================

fn transform(
    sources: impl IntoIterator<Item = (ColumnRead, ColumnLineageKind)>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    sources
        .into_iter()
        .map(|(r, _)| (r, ColumnLineageKind::Transformation))
        .collect()
}

fn find_named<'a>(exprs: &'a [NamedExpr], name: &Ident) -> Option<&'a NamedExpr> {
    exprs
        .iter()
        .find(|ne| ne.name.as_ref().is_some_and(|n| idents_eq(n, name)))
}

/// Case-sensitive identifier equality. The traversal follows bindings the
/// binder already resolved (case-folded), so it matches output names exactly;
/// a dialect fold is not needed here.
fn idents_eq(a: &Ident, b: &Ident) -> bool {
    a.value == b.value
}

/// This operator's own expressions (not its children's).
fn own_exprs(op: &Operator) -> Vec<&Expr> {
    match op {
        Operator::Filter(f) => f.predicate.iter().collect(),
        Operator::Join(j) => j.on.iter().collect(),
        Operator::Project(p) => p.exprs.iter().map(|ne| &ne.expr).collect(),
        Operator::Aggregate(a) => a
            .group_by
            .iter()
            .chain(&a.aggregates)
            .map(|ne| &ne.expr)
            .collect(),
        Operator::Sort(s) => s.keys.iter().collect(),
        // A table function's argument expressions are reads.
        Operator::TableFunction(tf) => tf.args.iter().collect(),
        Operator::Values(v) => v.rows.iter().flatten().collect(),
        // RETURNING items, conflict-action SET values, and the conflict
        // predicate are all reads (an `EXCLUDED` ref within them is `Derived`,
        // so dropped).
        Operator::Insert(i) => i
            .returning
            .iter()
            .map(|ne| &ne.expr)
            .chain(i.on_conflict.iter().map(|a| &a.value))
            .chain(i.conflict_predicate.iter())
            .collect(),
        Operator::Update(u) => u
            .assignments
            .iter()
            .map(|a| &a.value)
            .chain(u.returning.iter().map(|ne| &ne.expr))
            .collect(),
        Operator::Delete(d) => d.returning.iter().map(|ne| &ne.expr).collect(),
        // MERGE: the ON / per-clause predicates (filter reads) plus each WHEN
        // action's value expressions (SET RHS / INSERT values).
        Operator::Merge(m) => {
            let mut exprs: Vec<&Expr> = m.on.iter().collect();
            for clause in &m.clauses {
                match clause {
                    MergeClause::Update { assignments } => {
                        exprs.extend(assignments.iter().map(|a| &a.value));
                    }
                    MergeClause::Insert { values, .. } => exprs.extend(values.iter()),
                    MergeClause::Delete => {}
                }
            }
            exprs
        }
        Operator::Scan(_)
        | Operator::SubqueryAlias(_)
        | Operator::SetOp(_)
        | Operator::With(_)
        | Operator::CteRef(_)
        | Operator::Empty
        | Operator::CreateTableAs(_)
        | Operator::CreateView(_)
        | Operator::AlterTable(_)
        | Operator::Drop(_) => Vec::new(),
    }
}

/// This operator's structural child operators (not those nested in
/// expressions — those are walked by `expr_reads`; not CTE bodies — `walk` /
/// `collect_reads` handle `With` specially).
fn children(op: &Operator) -> Vec<&Operator> {
    match op {
        Operator::Filter(f) => vec![&f.input],
        Operator::Join(j) => vec![&j.left, &j.right],
        Operator::Aggregate(a) => vec![&a.input],
        Operator::Project(p) => vec![&p.input],
        Operator::Sort(s) => vec![&s.input],
        Operator::SetOp(so) => vec![&so.left, &so.right],
        Operator::SubqueryAlias(sa) => vec![&sa.input],
        // The inner (wrapped) table of a PIVOT / … is a child; the function
        // args are own expressions.
        Operator::TableFunction(tf) => vec![&tf.input],
        Operator::With(w) => vec![&w.body],
        Operator::Insert(i) => vec![&i.input],
        Operator::Update(u) => vec![&u.input],
        Operator::Delete(d) => vec![&d.input],
        Operator::Merge(m) => vec![&m.source],
        Operator::CreateTableAs(c) => vec![&c.input],
        Operator::CreateView(c) => vec![&c.input],
        Operator::Scan(_)
        | Operator::CteRef(_)
        | Operator::Values(_)
        | Operator::Empty
        | Operator::AlterTable(_)
        | Operator::Drop(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::binder::build;
    use super::*;
    use crate::casing::IdentifierCasing;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    fn plan(sql: &str) -> Operator {
        let statements = Parser::parse_sql(&GenericDialect {}, sql).unwrap();
        build(
            &statements[0],
            None,
            IdentifierCasing::for_dialect(&GenericDialect {}),
        )
    }

    fn read_names(op: &Operator) -> Vec<String> {
        let mut v: Vec<String> = reads(op)
            .iter()
            .map(|r| match &r.reference.table {
                Some(t) => format!("{}.{}", t.name.value, r.reference.name.value),
                None => format!("?.{}", r.reference.name.value),
            })
            .collect();
        v.sort();
        v
    }

    fn lineage_strs(op: &Operator) -> Vec<String> {
        let mut v: Vec<String> = column_lineage(op)
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
