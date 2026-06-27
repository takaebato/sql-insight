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
    children, own_exprs, walk_plan, Binding, BoundColumn, Expr, LogicalPlan,
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
    walk_plan(plan, &mut |o| {
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
/// subqueries. A `Derived` reference is dropped. A USING fan-in produces a
/// read per owning side.
fn expr_reads(expr: &Expr, out: &mut Vec<ColumnRead>) {
    match expr {
        Expr::Column(c) => out.extend(column_read(c)),
        // A merge-column fan-in: every owning side is a read.
        Expr::Fanin(refs) => out.extend(refs.iter().filter_map(column_read)),
        _ => {}
    }
    // Walk both positions' sub-plans and sub-expressions — `reads` doesn't
    // distinguish value from filter; the classification lives in the
    // structural accessors on `Expr`, so a new variant only needs to declare
    // which lists its operands sit in.
    for sub in expr
        .value_subplans()
        .into_iter()
        .chain(expr.filter_subplans())
    {
        reads_into(sub, out);
    }
    for child in expr
        .value_operands()
        .into_iter()
        .chain(expr.filter_operands())
    {
        expr_reads(child, out);
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
        // A `Derived` ref's read was counted at the inner producer; a `Local`
        // (lambda parameter) is not a column at all — neither is a read.
        Binding::Derived | Binding::Local => return None,
    };
    Some(ColumnRead {
        reference: ColumnReference {
            table,
            name: c.name.clone(),
        },
        resolution,
    })
}
