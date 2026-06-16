//! Walking the bound [`Operator`] tree for the extraction surfaces.
//!
//! - [`reads`] — every physical base-column reference (occurrence-based; a
//!   `Derived` ref is dropped, its read counted at the inner producer).
//! - [`table_reads`] — every base table scanned.
//! - [`column_lineage`] — `source → QueryOutput` edges, by tracing each
//!   output column's value expression to its base columns ([`origins_of_expr`]
//!   / [`origins_into`], the `getColumnOrigins`-style traversal). A predicate's
//!   columns are reads but never origins, so value/filter falls out for free.
//!
//! DML lineage / writes and table lineage arrive with the DML bricks.

use sqlparser::ast::Ident;

use super::operator::{Binding, ColRef, Cte, Expr, NamedExpr, Operator};
use crate::extractor::{ColumnLineageEdge, ColumnLineageKind, ColumnTarget};
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableRead};

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

/// The `source → QueryOutput` lineage of a top-level query: each output
/// column traced to its base columns. (DML targets are a later brick.)
pub(crate) fn column_lineage(op: &Operator) -> Vec<ColumnLineageEdge> {
    let mut ctx = Ctx::new(op);
    let mut edges = Vec::new();
    // One operand per query (a set operation has one per branch — positions
    // restart at zero per branch, mirroring the resolver).
    for (outputs, input) in output_operands(op) {
        for (position, ne) in outputs.iter().enumerate() {
            let target = ColumnTarget::QueryOutput {
                name: ne.name.clone(),
                position,
            };
            for (source, kind) in origins_of_expr(&ne.expr, input, &mut ctx) {
                edges.push(ColumnLineageEdge {
                    source,
                    target: target.clone(),
                    kind,
                });
            }
        }
    }
    edges
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
        Operator::Values(v) => v.rows.iter().flatten().collect(),
        Operator::Insert(i) => i.returning.iter().map(|ne| &ne.expr).collect(),
        Operator::Update(u) => u
            .assignments
            .iter()
            .map(|a| &a.value)
            .chain(u.returning.iter().map(|ne| &ne.expr))
            .collect(),
        Operator::Delete(d) => d.returning.iter().map(|ne| &ne.expr).collect(),
        Operator::Scan(_)
        | Operator::SubqueryAlias(_)
        | Operator::SetOp(_)
        | Operator::With(_)
        | Operator::CteRef(_)
        | Operator::Empty
        | Operator::Merge(_)
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
