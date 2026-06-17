//! The **write** surfaces of a [`LogicalPlan`] plus the legacy flat table list.
//!
//! - [`writes`](LogicalPlan::writes) — every column a DML root writes (INSERT
//!   columns, UPDATE SET targets, CTAS / CREATE VIEW / ALTER columns, MERGE
//!   WHEN-clause writes), qualified by the target relation.
//! - [`table_writes`](LogicalPlan::table_writes) — one entry per DML target.
//! - [`flat_tables`](LogicalPlan::flat_tables) — every table the statement
//!   references, the legacy un-bucketed table surface.
//!
//! Write targets are trivially resolved by construction (they come straight
//! from SQL syntax), so they carry no resolution kind.

use sqlparser::ast::Ident;

use super::logical_plan::{children, own_expr_subplans, peel_with, LogicalPlan, MergeClause};
use crate::reference::{ColumnReference, TableReference};

impl LogicalPlan {
    /// Every column written — a DML root's target columns.
    pub(crate) fn writes(&self) -> Vec<ColumnReference> {
        collect_writes(self)
    }

    /// Every table written to — one per DML target.
    pub(crate) fn table_writes(&self) -> Vec<TableReference> {
        collect_table_writes(self)
    }

    /// The flat list of every table the statement references — one per
    /// relation binding (the legacy table surface).
    pub(crate) fn flat_tables(&self) -> Vec<TableReference> {
        let mut out = Vec::new();
        collect_flat(self, &mut out);
        out
    }
}

// ===== writes ============================================================

/// Every column the statement writes — a DML root's target columns, qualified
/// by the write target. Order follows source order (the public contract). A
/// leading `WITH` is peeled.
fn collect_writes(op: &LogicalPlan) -> Vec<ColumnReference> {
    match peel_with(op) {
        // INSERT columns, then any ON CONFLICT DO UPDATE SET targets (extra
        // writes on the same relation).
        LogicalPlan::Insert(i) => {
            let mut w = qualify(&i.columns, &i.target);
            w.extend(i.on_conflict.iter().map(|a| ColumnReference {
                table: Some(i.target.clone()),
                name: a.target.clone(),
            }));
            w
        }
        // Each SET assignment writes its target column.
        LogicalPlan::Update(u) => u
            .assignments
            .iter()
            .map(|a| ColumnReference {
                table: Some(u.target.clone()),
                name: a.target.clone(),
            })
            .collect(),
        // CTAS / CREATE VIEW write the new relation's columns; ALTER TABLE
        // writes its column-naming operations' columns.
        LogicalPlan::CreateTableAs(c) => qualify(&c.columns, &c.target),
        LogicalPlan::CreateView(c) => qualify(&c.columns, &c.target),
        LogicalPlan::AlterTable(a) => qualify(&a.columns, &a.target),
        // MERGE writes each WHEN action's target columns (UPDATE SET targets;
        // INSERT columns paired with values).
        LogicalPlan::Merge(m) => m
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
fn collect_table_writes(op: &LogicalPlan) -> Vec<TableReference> {
    match peel_with(op) {
        LogicalPlan::Insert(i) => vec![i.target.clone()],
        LogicalPlan::Update(u) => vec![u.target.clone()],
        // A DELETE removes rows from each of its targets.
        LogicalPlan::Delete(d) => d.targets.clone(),
        LogicalPlan::CreateTableAs(c) => vec![c.target.clone()],
        LogicalPlan::CreateView(c) => vec![c.target.clone()],
        LogicalPlan::AlterTable(a) => vec![a.target.clone()],
        LogicalPlan::Merge(m) => vec![m.target.clone()],
        // DROP / TRUNCATE name their relations directly as write targets.
        LogicalPlan::Drop(d) => d.targets.clone(),
        _ => Vec::new(),
    }
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

// ===== flat tables (legacy) ==============================================

/// Walk the value sub-plans in a DML root's own expressions (a SET / WHEN /
/// conflict-value scalar subquery's scans bind too, like the resolver counts
/// them on the write input).
fn collect_flat_subplans(op: &LogicalPlan, out: &mut Vec<TableReference>) {
    for sub in own_expr_subplans(op) {
        collect_flat(sub, out);
    }
}

fn collect_flat(op: &LogicalPlan, out: &mut Vec<TableReference>) {
    match op {
        LogicalPlan::Scan(s) => out.push(s.table.clone()),
        // DROP / TRUNCATE relations are bindings with no scan.
        LogicalPlan::Drop(d) => out.extend(d.targets.iter().cloned()),
        // A write target is external to the input (never a scan here): count it,
        // then walk the source for its read scans.
        LogicalPlan::Insert(i) => {
            out.push(i.target.clone());
            collect_flat(&i.input, out);
            collect_flat_subplans(op, out);
        }
        LogicalPlan::Update(u) => {
            out.push(u.target.clone());
            collect_flat(&u.input, out);
            collect_flat_subplans(op, out);
        }
        LogicalPlan::Merge(m) => {
            out.push(m.target.clone());
            collect_flat(&m.source, out);
            collect_flat_subplans(op, out);
        }
        LogicalPlan::CreateTableAs(c) => {
            out.push(c.target.clone());
            collect_flat(&c.input, out);
        }
        LogicalPlan::CreateView(c) => {
            out.push(c.target.clone());
            collect_flat(&c.input, out);
        }
        LogicalPlan::AlterTable(a) => out.push(a.target.clone()),
        // A DELETE target often coincides with a FROM scan already collected;
        // count only the targets that didn't merge into a row source.
        LogicalPlan::Delete(d) => {
            let before = out.len();
            collect_flat(&d.input, out);
            let from: Vec<TableReference> = out[before..].to_vec();
            for target in &d.targets {
                if !from.contains(target) {
                    out.push(target.clone());
                }
            }
        }
        LogicalPlan::With(w) => {
            collect_flat(&w.body, out);
            for cte in &w.ctes {
                collect_flat(&cte.body, out);
            }
        }
        // A table function is opaque (not a table binding), but a PIVOT-style
        // inner table below it and any scan in an argument subquery bind.
        LogicalPlan::TableFunction(tf) => {
            collect_flat(&tf.input, out);
            collect_flat_subplans(op, out);
        }
        // A `VALUES` row's expressions can hold a subquery whose scans bind.
        LogicalPlan::Values(_) => collect_flat_subplans(op, out),
        LogicalPlan::CteRef(_) | LogicalPlan::Empty => {}
        // Structural query operators: walk children and any expression
        // sub-plans (a WHERE / scalar subquery's scans bind too).
        LogicalPlan::Filter(_)
        | LogicalPlan::Join(_)
        | LogicalPlan::Aggregate(_)
        | LogicalPlan::Projection(_)
        | LogicalPlan::Sort(_)
        | LogicalPlan::SetOp(_)
        | LogicalPlan::SubqueryAlias(_) => {
            for child in children(op) {
                collect_flat(child, out);
            }
            for sub in own_expr_subplans(op) {
                collect_flat(sub, out);
            }
        }
    }
}
