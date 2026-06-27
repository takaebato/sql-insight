//! The **write** surfaces over a [`LogicalPlan`] plus the flat table list.
//! These back the [`crate::resolver`] facade's `writes` / `table_writes`
//! / `flat_tables` entry points:
//!
//! - `writes` — every column a DML root writes (INSERT columns, UPDATE SET
//!   targets, CTAS / CREATE VIEW / ALTER columns, MERGE WHEN-clause writes),
//!   qualified by the target relation.
//! - `table_writes` — one entry per DML target.
//! - `flat_tables` — every table the statement references, the
//!   un-bucketed table surface.
//!
//! Write targets are trivially resolved by construction (they come straight
//! from SQL syntax), so they carry no resolution kind.

use sqlparser::ast::Ident;

use super::logical_plan::{peel_with, walk_plan, LogicalPlan, MergeClause};
use super::origins::output_operands;
use crate::reference::{ColumnReference, TableReference};

// ===== writes ============================================================

/// Every column the statement writes — a DML root's target columns, qualified
/// by the write target. Order follows source order (the public contract). A
/// leading `WITH` is peeled. Backs [`crate::resolver::writes`].
pub(super) fn collect_writes(plan: &LogicalPlan) -> Vec<ColumnReference> {
    match peel_with(plan) {
        // INSERT columns, then any ON CONFLICT DO UPDATE SET targets (extra
        // writes on the same relation).
        LogicalPlan::Insert(i) => {
            let mut w = qualify(&i.columns, &i.target);
            w.extend(i.on_conflict.iter().map(|a| a.target.clone()));
            w
        }
        // Each SET assignment writes its (already resolved) target column.
        LogicalPlan::Update(u) => u.assignments.iter().map(|a| a.target.clone()).collect(),
        // CTAS / CREATE VIEW write the new relation's columns; ALTER TABLE
        // writes its column-naming operations' columns.
        LogicalPlan::CreateTableAs(c) => created_relation_writes(&c.columns, &c.input, &c.target),
        LogicalPlan::CreateView(c) => created_relation_writes(&c.columns, &c.input, &c.target),
        LogicalPlan::AlterTable(a) => qualify(&a.columns, &a.target),
        // MERGE writes each WHEN action's target columns (UPDATE SET targets;
        // INSERT columns paired with values).
        LogicalPlan::Merge(m) => m
            .clauses
            .iter()
            .flat_map(|clause| merge_clause_writes(clause, &m.target))
            .collect(),
        // No column writes: read-only / structural query operators, the bare
        // FROM-less / synthetic relations, and DML / DDL whose target rows go
        // wholesale (DELETE / DROP / TRUNCATE) — none qualify a column with a
        // write target. Listed explicitly so a new `LogicalPlan` variant
        // forces a write-placement decision rather than silently emitting
        // nothing here.
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
        | LogicalPlan::Drop(_) => Vec::new(),
    }
}

/// The columns one MERGE WHEN action writes, qualified by the target.
fn merge_clause_writes(clause: &MergeClause, target: &TableReference) -> Vec<ColumnReference> {
    match clause {
        MergeClause::Update { assignments } => {
            assignments.iter().map(|a| a.target.clone()).collect()
        }
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
/// is peeled. Backs [`crate::resolver::table_writes`].
pub(super) fn collect_table_writes(plan: &LogicalPlan) -> Vec<TableReference> {
    match peel_with(plan) {
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
        // No write target: read-only / structural query operators and the
        // FROM-less / synthetic relations. Listed explicitly so a new
        // `LogicalPlan` variant forces a target decision here.
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
        | LogicalPlan::Empty => Vec::new(),
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

/// The columns a CTAS / CREATE VIEW writes. An *explicit* column list
/// (`CREATE TABLE t (a, b) AS …`) is authoritative; the implicit form takes
/// each source output's inferred name (an anonymous output is unnameable, so
/// dropped — never positionally shifting later columns). A set-op source takes
/// its result schema from the left branch. Kept in step with
/// [`super::lineage`]'s `created_relation_lineage` so writes and lineage agree.
fn created_relation_writes(
    explicit: &[Ident],
    input: &LogicalPlan,
    target: &TableReference,
) -> Vec<ColumnReference> {
    if !explicit.is_empty() {
        return qualify(explicit, target);
    }
    match output_operands(input).first() {
        Some(operand) => operand
            .outputs
            .iter()
            .filter_map(|ne| ne.name.clone())
            .map(|name| ColumnReference {
                table: Some(target.clone()),
                name,
            })
            .collect(),
        None => Vec::new(),
    }
}

// ===== flat tables =======================================================

/// The flat list of every table the statement references — one per relation
/// binding (the un-bucketed table surface). Backs [`crate::resolver::flat_tables`].
pub(super) fn collect_flat_tables(plan: &LogicalPlan) -> Vec<TableReference> {
    let mut out = Vec::new();
    collect_flat_into(plan, &mut out);
    out
}

/// Shared visitor over the structural [`walk_plan`]: at each operator, push
/// whatever table binding it names (a `Scan` table, a DML / DDL root's write
/// target). `walk_plan` already recurses children, value subplans, and CTE
/// bodies, so a DML's target is pushed once at the root and the input's scans
/// follow naturally. `DELETE` is the one shape this can't model — its target
/// often coincides with a FROM scan already collected, so handled separately.
/// The relations directly in a DELETE input's FROM / USING clause — the
/// structural scans, *not* those nested in a WHERE-predicate subquery. Used to
/// dedup a DELETE target that coincides with a FROM relation (`DELETE t1 FROM
/// t1 …`) without folding away a same-named table that only appears in a
/// predicate subquery (a distinct occurrence). Stops at expression operands, so
/// a predicate / projection subquery is not descended; a `CteRef` names a CTE
/// (not a base relation) and is skipped.
fn direct_from_scans(op: &LogicalPlan, out: &mut Vec<TableReference>) {
    match op {
        LogicalPlan::Scan(s) => out.push(s.table.clone()),
        LogicalPlan::Filter(f) => direct_from_scans(&f.input, out),
        LogicalPlan::Aggregate(a) => direct_from_scans(&a.input, out),
        LogicalPlan::Sort(s) => direct_from_scans(&s.input, out),
        LogicalPlan::SubqueryAlias(sa) => direct_from_scans(&sa.input, out),
        LogicalPlan::TableFunction(tf) => direct_from_scans(&tf.input, out),
        LogicalPlan::With(w) => direct_from_scans(&w.body, out),
        LogicalPlan::Join(j) => {
            direct_from_scans(&j.left, out);
            direct_from_scans(&j.right, out);
        }
        LogicalPlan::SetOp(so) => {
            direct_from_scans(&so.left, out);
            direct_from_scans(&so.right, out);
        }
        // Not a structural FROM relation: a `CteRef` (a CTE name), `Values`,
        // `Empty`, a `Projection` (its value subqueries are not direct FROM),
        // and DML / DDL roots. Listed explicitly so a new relational operator
        // forces a decision here.
        LogicalPlan::Projection(_)
        | LogicalPlan::CteRef(_)
        | LogicalPlan::Values(_)
        | LogicalPlan::Empty
        | LogicalPlan::Insert(_)
        | LogicalPlan::Update(_)
        | LogicalPlan::Delete(_)
        | LogicalPlan::Merge(_)
        | LogicalPlan::CreateTableAs(_)
        | LogicalPlan::CreateView(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => {}
    }
}

fn collect_flat_into(plan: &LogicalPlan, out: &mut Vec<TableReference>) {
    // A CTE-prefixed DELETE binds as `With { body: Delete }`, so peel the
    // leading WITH(s) to reach it (mirroring the sibling write walkers'
    // `peel_with`) — walking their CTE bodies into the flat list first. The
    // target is deduped against the DELETE's *direct* FROM / USING relations
    // only (where it may recur as a scan, `DELETE t1 FROM t1 …`); a same-named
    // table that only appears in a WHERE-predicate subquery or a CTE body is a
    // distinct binding and stays listed separately.
    if let LogicalPlan::Delete(d) = peel_with(plan) {
        let mut node = plan;
        while let LogicalPlan::With(w) = node {
            w.ctes.iter().for_each(|c| collect_flat_into(&c.body, out));
            node = &w.body;
        }
        collect_flat_into(&d.input, out);
        let mut from = Vec::new();
        direct_from_scans(&d.input, &mut from);
        for target in &d.targets {
            if !from.contains(target) {
                out.push(target.clone());
            }
        }
        return;
    }
    walk_plan(plan, &mut |op| match op {
        LogicalPlan::Scan(s) => out.push(s.table.clone()),
        // DROP / TRUNCATE — multiple targets in one statement, no input.
        LogicalPlan::Drop(d) => out.extend(d.targets.iter().cloned()),
        LogicalPlan::Insert(i) => out.push(i.target.clone()),
        LogicalPlan::Update(u) => out.push(u.target.clone()),
        LogicalPlan::Merge(m) => out.push(m.target.clone()),
        LogicalPlan::CreateTableAs(c) => {
            out.push(c.target.clone());
            // `LIKE` / `CLONE` schema template — a referenced table with no
            // row-data role, surfaced only in this flat list.
            out.extend(c.schema_source.clone());
        }
        LogicalPlan::CreateView(c) => out.push(c.target.clone()),
        LogicalPlan::AlterTable(a) => out.push(a.target.clone()),
        // Operators that name no table of their own: pure relational shape,
        // CTE plumbing, synthetic leaves, and `Delete` (its targets are
        // collected by the peel-aware early-return above, before this walk).
        // Listed
        // explicitly — like the sibling write walkers — so a new
        // table-naming `LogicalPlan` variant is a compile error here, not a
        // silent omission from the flat list.
        LogicalPlan::Filter(_)
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
        | LogicalPlan::Delete(_) => {}
    });
}
