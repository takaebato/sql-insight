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

use super::logical_plan::{idents_eq, peel_with, Expr, LogicalPlan, MergeClause, NamedExpr};
use super::origins::{conflict_value_origins, enter_withs, origins_of_expr, output_operands, Ctx};
use crate::extractor::{ColumnLineageEdge, ColumnTarget, TableLineageEdge};
use crate::reference::{ColumnReference, TableRead, TableReference};

// ===== column lineage ====================================================

/// The column lineage of a statement: `source → target` edges, each output
/// column traced to its base columns (`QueryOutput` for a query, `Relation` for
/// a DML target). A bare query emits `source → QueryOutput` edges; a DML root
/// pairs the source's output columns positionally with the write target's
/// columns and emits `source → Relation` edges. A leading `WITH` is peeled (its
/// CTE bodies feed the root through `CteRef` expansion, they are not lineage
/// roots). Backs [`crate::resolver::column_lineage`].
pub(super) fn collect_column_lineage(op: &LogicalPlan) -> Vec<ColumnLineageEdge> {
    let mut ctx = Ctx::new(op);
    let mut edges = Vec::new();
    match peel_with(op) {
        // INSERT … <source>: pair the source's outputs with the target columns.
        // A statement-level `WITH` rides on the source (the parser attaches it
        // there, so the `With` is *inside* `input`, not above the `Insert`);
        // `enter_withs` pushes its CTEs into the ctx so a `CteRef` resolves.
        LogicalPlan::Insert(i) => {
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
        LogicalPlan::Update(u) => {
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
        LogicalPlan::Delete(d) => returning_lineage(&d.returning, &d.input, &mut ctx, &mut edges),
        // CTAS / CREATE VIEW move data like INSERT: pair the source's outputs
        // with the new relation's columns.
        LogicalPlan::CreateTableAs(c) => {
            let src = enter_withs(&c.input, &mut ctx);
            relation_lineage(&c.columns, &c.target, src, &mut ctx, &mut edges);
        }
        LogicalPlan::CreateView(c) => {
            let src = enter_withs(&c.input, &mut ctx);
            relation_lineage(&c.columns, &c.target, src, &mut ctx, &mut edges);
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
fn query_output_lineage<'a>(
    op: &'a LogicalPlan,
    ctx: &mut Ctx<'a>,
    out: &mut Vec<ColumnLineageEdge>,
) {
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
    input: &'a LogicalPlan,
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
    input: &'a LogicalPlan,
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

/// Emit the `value → target.column` lineage edges of one MERGE WHEN value, its
/// origins traced through the `source` relation.
fn merge_value_edges<'a>(
    column: &Ident,
    value: &'a Expr,
    target: &TableReference,
    source: &'a LogicalPlan,
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

// ===== table lineage =====================================================

/// Table-level lineage: one `source → target` edge per read-role scan that
/// **feeds data** into a DML target, occurrence-based. Feeding sources are the
/// scans on the value / data path of the source — FROM / JOIN relations, value
/// (projection) subqueries, and referenced CTE bodies — never predicate
/// (filter) subqueries. A bare query, or a statement that moves no data, has
/// no table lineage. Backs [`crate::resolver::table_lineage`].
pub(super) fn collect_table_lineage(op: &LogicalPlan) -> Vec<TableLineageEdge> {
    // `Ctx::new` peels leading WITHs and keeps their CTE bodies, so a `CteRef`
    // on the feeding path resolves to the body's feeding scans.
    let mut ctx = Ctx::new(op);
    let mut sources = Vec::new();
    let target = match peel_with(op) {
        LogicalPlan::Insert(i) => {
            feeding_scans(&i.input, &mut ctx, &mut sources);
            &i.target
        }
        // UPDATE feeds from the FROM relations (the read `input`) AND any value
        // subquery in a SET RHS; the WHERE predicate / a self-reference to the
        // target do not feed.
        LogicalPlan::Update(u) => {
            feeding_scans(&u.input, &mut ctx, &mut sources);
            for a in &u.assignments {
                expr_feeding(&a.value, &mut ctx, &mut sources);
            }
            &u.target
        }
        // CTAS / CREATE VIEW move data like INSERT; ALTER / DROP do not.
        LogicalPlan::CreateTableAs(c) => {
            feeding_scans(&c.input, &mut ctx, &mut sources);
            &c.target
        }
        LogicalPlan::CreateView(c) => {
            feeding_scans(&c.input, &mut ctx, &mut sources);
            &c.target
        }
        // MERGE feeds from the source relation plus each *written* WHEN value
        // (an UPDATE SET RHS; an INSERT value paired with a column). The ON /
        // predicate reads and an unpaired INSERT value do not feed.
        LogicalPlan::Merge(m) => {
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

/// Collect the read-role scans that feed data up through `op` (a value / data
/// path): a join feeds both sides, a filter passes only its input (its
/// predicate subqueries do not feed), a projection also pulls its value
/// subqueries. A `CteRef` resolves to the referenced CTE body's feeding scans;
/// the `Ctx` active-set terminates a recursive self-reference.
fn feeding_scans<'a>(op: &'a LogicalPlan, ctx: &mut Ctx<'a>, out: &mut Vec<TableRead>) {
    match op {
        LogicalPlan::Scan(s) => out.push(TableRead {
            reference: s.table.clone(),
            resolution: s.resolution,
        }),
        LogicalPlan::Filter(f) => feeding_scans(&f.input, ctx, out),
        LogicalPlan::Join(j) => {
            feeding_scans(&j.left, ctx, out);
            feeding_scans(&j.right, ctx, out);
        }
        LogicalPlan::Projection(p) => {
            feeding_scans(&p.input, ctx, out);
            for ne in &p.exprs {
                expr_feeding(&ne.expr, ctx, out);
            }
        }
        LogicalPlan::Aggregate(a) => feeding_scans(&a.input, ctx, out),
        LogicalPlan::Sort(s) => feeding_scans(&s.input, ctx, out),
        LogicalPlan::SubqueryAlias(sa) => feeding_scans(&sa.input, ctx, out),
        // A PIVOT / … feeds from its wrapped inner table; the function args
        // are filter-position reads and do not feed.
        LogicalPlan::TableFunction(tf) => feeding_scans(&tf.input, ctx, out),
        LogicalPlan::SetOp(so) => {
            feeding_scans(&so.left, ctx, out);
            feeding_scans(&so.right, ctx, out);
        }
        LogicalPlan::With(w) => {
            let added = w.ctes.len();
            w.ctes.iter().for_each(|c| ctx.ctes.push(c));
            feeding_scans(&w.body, ctx, out);
            ctx.ctes.truncate(ctx.ctes.len() - added);
        }
        LogicalPlan::CteRef(r) => {
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
                feeding_scans(&cte.body, ctx, out);
                ctx.active.pop();
            }
        }
        // A nested data-mover feeds through its source; DELETE / DROP / ALTER /
        // VALUES move no row data into a feeding path.
        LogicalPlan::Insert(i) => feeding_scans(&i.input, ctx, out),
        LogicalPlan::Update(u) => feeding_scans(&u.input, ctx, out),
        LogicalPlan::CreateTableAs(c) => feeding_scans(&c.input, ctx, out),
        LogicalPlan::CreateView(c) => feeding_scans(&c.input, ctx, out),
        LogicalPlan::Merge(m) => feeding_scans(&m.source, ctx, out),
        LogicalPlan::Delete(_)
        | LogicalPlan::Drop(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Values(_)
        | LogicalPlan::Empty => {}
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
        Expr::Exists(_) | Expr::InSubquery { .. } | Expr::Filter(_) | Expr::Fanin(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::super::binder::build_with_diagnostics;
    use super::super::{column_lineage, reads, table_reads};
    use super::*;
    use crate::casing::IdentifierCasing;
    use crate::extractor::ColumnLineageKind;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    fn plan(sql: &str) -> LogicalPlan {
        let statements = Parser::parse_sql(&GenericDialect {}, sql).unwrap();
        build_with_diagnostics(
            &statements[0],
            None,
            IdentifierCasing::for_dialect(&GenericDialect {}),
        )
        .0
    }

    fn read_names(op: &LogicalPlan) -> Vec<String> {
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

    fn lineage_strs(op: &LogicalPlan) -> Vec<String> {
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
