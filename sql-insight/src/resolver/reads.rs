//! The column / table **read** surfaces over a [`LogicalPlan`]: every physical
//! base-column reference (occurrence-based) and every base table scanned. These
//! back the [`crate::resolver`] facade's `reads` / `table_reads` entry points.
//!
//! A read is *occurrence-based, by token*: each syntactic appearance of a
//! base-column reference counts, not each physical read — a column referenced in
//! both the projection and the `WHERE` clause surfaces twice. In a
//! post-projection clause (GROUP BY / HAVING / ORDER BY) a token naming a base
//! column (an identity output, e.g. `GROUP BY a`) counts as another occurrence,
//! but one naming only an introduced output alias (`ORDER BY x` for `a AS x`)
//! binds `Derived` and drops — the dependency was already counted at the
//! projection (and is carried by lineage). A `Derived` reference (a CTE /
//! derived / computed column) is likewise dropped here — its physical read was
//! already counted at the inner producer; the lineage trace reaches the real
//! column instead.

use super::logical_plan::{
    children, own_expr_subplans, own_exprs, Binding, BoundColumn, Expr, LogicalPlan,
};
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableRead};

/// Every physical base-column read, occurrence-based (a `Derived` ref is
/// dropped — its read is counted at the inner producer). Backs
/// [`crate::resolver::reads()`].
pub(super) fn collect_reads(plan: &LogicalPlan) -> Vec<ColumnRead> {
    let mut out = Vec::new();
    reads_into(plan, &mut out);
    out
}

/// Every base table scanned (read role), occurrence-based. Backs
/// [`crate::resolver::table_reads`].
pub(super) fn collect_table_reads(plan: &LogicalPlan) -> Vec<TableRead> {
    let mut out = Vec::new();
    walk(plan, &mut |o| {
        if let LogicalPlan::Scan(s) = o {
            out.push(TableRead {
                reference: s.table.clone(),
                resolution: s.resolution,
            });
        }
    });
    out
}

fn reads_into(op: &LogicalPlan, out: &mut Vec<ColumnRead>) {
    for expr in own_exprs(op) {
        expr_reads(expr, out);
    }
    for child in children(op) {
        reads_into(child, out);
    }
    // A `With` walks each CTE body once (declarations); a `CteRef` is a leaf.
    if let LogicalPlan::With(w) = op {
        for cte in &w.ctes {
            reads_into(&cte.body, out);
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
        Expr::Case {
            when,
            then,
            else_result,
        } => {
            when.iter().chain(then).for_each(|e| expr_reads(e, out));
            if let Some(e) = else_result {
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
        Expr::Subquery(plan) | Expr::Exists(plan) => reads_into(plan, out),
        Expr::InSubquery { expr, subquery } => {
            expr_reads(expr, out);
            reads_into(subquery, out);
        }
        Expr::Filter(exprs) => exprs.iter().for_each(|e| expr_reads(e, out)),
        // A merge-column fan-in: every owning side is a read.
        Expr::Fanin(refs) => out.extend(refs.iter().filter_map(column_read)),
    }
}

/// A column reference as a public read — `None` for a `Derived` ref (its
/// physical read was counted at the inner producer). Shared with the origin
/// trace, which turns a traced base column into the same `ColumnRead`.
pub(super) fn column_read(c: &BoundColumn) -> Option<ColumnRead> {
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

/// Pre-order walk of the structural tree (children + own-expression sub-plans +
/// CTE bodies), invoking `f` at every operator.
fn walk(op: &LogicalPlan, f: &mut impl FnMut(&LogicalPlan)) {
    f(op);
    for child in children(op) {
        walk(child, f);
    }
    // Subqueries nested in this node's expressions (a `WHERE … IN (SELECT …)`
    // / scalar subquery) are sub-plans too — their scans must surface.
    for sub in own_expr_subplans(op) {
        walk(sub, f);
    }
    if let LogicalPlan::With(w) = op {
        for cte in &w.ctes {
            walk(&cte.body, f);
        }
    }
}
