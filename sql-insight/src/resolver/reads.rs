//! The column / table **read** surfaces of a [`LogicalPlan`]: every physical
//! base-column reference (occurrence-based) and every base table scanned.
//!
//! A read is *occurrence-based*: a column referenced in both the projection and
//! the `WHERE` clause surfaces twice. A `Derived` reference (a CTE / derived /
//! computed column) is dropped here — its physical read was already counted at
//! the inner producer; the lineage trace reaches the real column instead.

use super::logical_plan::{
    children, own_expr_subplans, own_exprs, Binding, ColRef, Expr, LogicalPlan,
};
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableRead};

impl LogicalPlan {
    /// Every physical base-column read, occurrence-based (a `Derived` ref is
    /// dropped — its read is counted at the inner producer).
    pub(crate) fn reads(&self) -> Vec<ColumnRead> {
        let mut out = Vec::new();
        collect_reads(self, &mut out);
        out
    }

    /// Every base table scanned (read role), occurrence-based.
    pub(crate) fn table_reads(&self) -> Vec<TableRead> {
        let mut out = Vec::new();
        walk(self, &mut |o| {
            if let LogicalPlan::Scan(s) = o {
                out.push(TableRead {
                    reference: s.table.clone(),
                    resolution: s.resolution,
                });
            }
        });
        out
    }
}

fn collect_reads(op: &LogicalPlan, out: &mut Vec<ColumnRead>) {
    for expr in own_exprs(op) {
        expr_reads(expr, out);
    }
    for child in children(op) {
        collect_reads(child, out);
    }
    // A `With` walks each CTE body once (declarations); a `CteRef` is a leaf.
    if let LogicalPlan::With(w) = op {
        for cte in &w.ctes {
            collect_reads(&cte.body, out);
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
        Expr::Filter(exprs) => exprs.iter().for_each(|e| expr_reads(e, out)),
        // A merge-column fan-in: every owning side is a read.
        Expr::Fanin(refs) => out.extend(refs.iter().filter_map(column_read)),
    }
}

/// A column reference as a public read — `None` for a `Derived` ref (its
/// physical read was counted at the inner producer). Shared with the origin
/// trace, which turns a traced base column into the same `ColumnRead`.
pub(super) fn column_read(c: &ColRef) -> Option<ColumnRead> {
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
