//! The column-origin traversal: tracing an expression's **value** to the base
//! columns it derives from, each with a composed [`ColumnLineageKind`]. This is
//! the `getColumnOrigins`-style core that [`super::lineage`] drives to build
//! the `source → target` edges.
//!
//! The value-vs-filter split falls out of this trace for free: filter-position
//! operands (CASE conditions, window partition / order keys, EXISTS / IN tests)
//! are *not* traced — they surface as reads, never as origins.

use sqlparser::ast::Ident;

use super::logical_plan::{idents_eq, Binding, ColRef, Cte, Expr, LogicalPlan, NamedExpr};
use super::reads::column_read;
use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableReference};

/// CTE environment for expanding `CteRef`s during a trace, plus the
/// active-set that terminates a recursive self-reference.
pub(super) struct Ctx<'a> {
    pub(super) ctes: Vec<&'a Cte>,
    pub(super) active: Vec<String>,
}

impl<'a> Ctx<'a> {
    pub(super) fn new(op: &'a LogicalPlan) -> Self {
        // Collect leading `With` declarations so a `CteRef` on a traced path
        // resolves to its body.
        let mut ctes = Vec::new();
        let mut node = op;
        while let LogicalPlan::With(w) = node {
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
pub(super) fn origins_of_expr<'a>(
    expr: &'a Expr,
    input: &'a LogicalPlan,
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
        // A merge-column fan-in: each owning side is a `Passthrough` origin.
        Expr::Fanin(refs) => refs
            .iter()
            .filter_map(|c| column_read(c).map(|r| (r, ColumnLineageKind::Passthrough)))
            .collect(),
        // Tests / suppressed operands contribute no value origin (reads only).
        Expr::Exists(_) | Expr::InSubquery { .. } | Expr::Filter(_) => Vec::new(),
    }
}

/// Whether a (sub)plan's rows are synthesised by `VALUES` (peeling the clause
/// layers / a leading `With`): a column of such a relation has no base column
/// to collapse to, so a reference to it is a synthetic source.
fn values_backed(op: &LogicalPlan) -> bool {
    match op {
        LogicalPlan::Values(_) => true,
        LogicalPlan::Sort(s) => values_backed(&s.input),
        LogicalPlan::Filter(f) => values_backed(&f.input),
        LogicalPlan::With(w) => values_backed(&w.body),
        _ => false,
    }
}

/// Trace the named output column of `op` down to its base origins (used to
/// expand a `Derived` reference through its producing operator).
fn origins_into<'a>(
    op: &'a LogicalPlan,
    qualifier: Option<&Ident>,
    name: &Ident,
    ctx: &mut Ctx<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match op {
        LogicalPlan::Projection(p) => match find_named(&p.exprs, name) {
            Some(ne) => origins_of_expr(&ne.expr, &p.input, ctx),
            None => Vec::new(),
        },
        // The projection resolves against the FROM scope, so a column is a
        // base ref that returns directly, not a named `Aggregate` output —
        // a `Derived` ref tracing down just passes through to the input.
        LogicalPlan::Aggregate(a) => origins_into(&a.input, qualifier, name, ctx),
        LogicalPlan::Filter(f) => origins_into(&f.input, qualifier, name, ctx),
        LogicalPlan::Sort(s) => origins_into(&s.input, qualifier, name, ctx),
        LogicalPlan::Join(j) => {
            let mut o = origins_into(&j.left, qualifier, name, ctx);
            o.extend(origins_into(&j.right, qualifier, name, ctx));
            o
        }
        LogicalPlan::SubqueryAlias(sa) => {
            if !qualifier.is_none_or(|q| idents_eq(q, &sa.alias)) {
                Vec::new()
            } else if values_backed(&sa.input) {
                // A `(VALUES …) AS t(x)` relation synthesises rows with no base
                // columns, so the exposed column is a synthetic source (t.x).
                vec![(
                    synthetic_source(&sa.alias, name),
                    ColumnLineageKind::Passthrough,
                )]
            } else {
                origins_into(&sa.input, None, name, ctx)
            }
        }
        // An opaque table function: its produced columns are dynamic, so a ref
        // through its alias is a synthetic lineage source (the alias as table)
        // — not collapsible to a base column.
        LogicalPlan::TableFunction(tf) => match &tf.alias {
            Some(alias) if qualifier.is_none_or(|q| idents_eq(q, alias)) => {
                vec![(
                    synthetic_source(alias, name),
                    ColumnLineageKind::Passthrough,
                )]
            }
            _ => Vec::new(),
        },
        LogicalPlan::SetOp(so) => {
            let mut o = origins_into(&so.left, qualifier, name, ctx);
            o.extend(origins_into(&so.right, qualifier, name, ctx));
            o
        }
        LogicalPlan::With(w) => {
            let added = w.ctes.len();
            w.ctes.iter().for_each(|c| ctx.ctes.push(c));
            let o = origins_into(&w.body, qualifier, name, ctx);
            ctx.ctes.truncate(ctx.ctes.len() - added);
            o
        }
        LogicalPlan::CteRef(r) => {
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
            // A VALUES-backed CTE has no traceable base columns — the exposed
            // column is a synthetic source (cte.col).
            if values_backed(&cte.body) {
                return vec![(
                    synthetic_source(&r.name, name),
                    ColumnLineageKind::Passthrough,
                )];
            }
            ctx.active.push(r.name.value.clone());
            let o = origins_into(&cte.body, None, name, ctx);
            ctx.active.pop();
            o
        }
        // A `Derived` reference resolves at a producer's named output (a
        // `Projection` / `Aggregate` expr), never at a raw `Scan` — a reference to
        // a base column is `Binding::Base` and returns directly, not via this
        // traversal. So a `Scan` reached here (e.g. the other side of a join
        // the qualified name doesn't own) contributes nothing.
        LogicalPlan::Scan(_) | LogicalPlan::Values(_) | LogicalPlan::Empty => Vec::new(),
        // DML/DDL roots are not column producers traced into here.
        LogicalPlan::Insert(_)
        | LogicalPlan::Update(_)
        | LogicalPlan::Delete(_)
        | LogicalPlan::Merge(_)
        | LogicalPlan::CreateTableAs(_)
        | LogicalPlan::CreateView(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => Vec::new(),
    }
}

/// The origins of a (sub)query's first output column (a scalar subquery's
/// value).
fn query_col0_origins<'a>(
    op: &'a LogicalPlan,
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

/// The origins of an ON CONFLICT DO UPDATE value. Like [`origins_of_expr`],
/// but an `EXCLUDED.col` reference (a `Derived` ref, qualified `excluded`) maps
/// to the INSERT source's like-positioned output column — or, when the source
/// has no inspectable projection (a `VALUES` source), to the `EXCLUDED.col`
/// pseudo-column itself (a synthetic lineage source, not a read).
pub(super) fn conflict_value_origins<'a>(
    value: &'a Expr,
    columns: &[Ident],
    source: &'a LogicalPlan,
    ctx: &mut Ctx<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match value {
        // A `Derived` ref here is `EXCLUDED.col` (the only synthetic relation in
        // a conflict scope). Map it to the source's `col`-positioned output —
        // fanning out to every set-operation branch. A source with no
        // inspectable projection (VALUES) keeps the `EXCLUDED.col` pseudo-source.
        Expr::Column(c) if matches!(c.binding, Binding::Derived) => {
            let Some(i) = columns.iter().position(|t| idents_eq(t, &c.name)) else {
                return Vec::new();
            };
            let operands = output_operands(source);
            if operands.is_empty() {
                return vec![(excluded_source(c), ColumnLineageKind::Passthrough)];
            }
            let mut out = Vec::new();
            for (outputs, input) in operands {
                if let Some(ne) = outputs.get(i) {
                    out.extend(origins_of_expr(&ne.expr, input, ctx));
                }
            }
            out
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
        Expr::Fanin(refs) => refs
            .iter()
            .filter_map(|c| column_read(c).map(|r| (r, ColumnLineageKind::Passthrough)))
            .collect(),
        Expr::Exists(_) | Expr::InSubquery { .. } | Expr::Filter(_) => Vec::new(),
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

/// A query's output operands: one `(output columns, producing input)` per
/// set-operation branch (a plain query has a single operand). Peels the clause
/// layers above the projection (GROUP BY / HAVING `Filter`, ORDER BY `Sort`)
/// and `With`.
pub(super) fn output_operands(op: &LogicalPlan) -> Vec<(&[NamedExpr], &LogicalPlan)> {
    match op {
        LogicalPlan::Projection(p) => vec![(&p.exprs, &p.input)],
        LogicalPlan::Sort(s) => output_operands(&s.input),
        LogicalPlan::Filter(f) => output_operands(&f.input),
        LogicalPlan::With(w) => output_operands(&w.body),
        LogicalPlan::SetOp(so) => {
            let mut operands = output_operands(&so.left);
            operands.extend(output_operands(&so.right));
            operands
        }
        _ => Vec::new(),
    }
}

/// Peel leading `With` nodes off `op`, pushing their CTE declarations into
/// `ctx` so a `CteRef` below resolves during the trace, and return the peeled
/// root. (`Ctx::new` already does this for a query's leading `WITH`; a DML
/// source carries its own `WITH`, reached only here.) The push is not popped —
/// the ctx is per-statement scratch, discarded after the walk.
pub(super) fn enter_withs<'a>(op: &'a LogicalPlan, ctx: &mut Ctx<'a>) -> &'a LogicalPlan {
    let mut node = op;
    while let LogicalPlan::With(w) = node {
        w.ctes.iter().for_each(|c| ctx.ctes.push(c));
        node = &w.body;
    }
    node
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
