//! The analysis engine: a **standard logical plan** ([`logical_plan::LogicalPlan`])
//! built by the [`binder`] and walked for the extraction surfaces. It is **not**
//! an execution plan — nothing optimises or runs SQL.
//!
//! A textbook relational-algebra tree (`Scan` / `Filter` / `Join` / `Aggregate`
//! / `Projection` / `Sort` / `SetOp` / `SubqueryAlias` / `TableFunction` /
//! `With` + `CteRef` / `Values`, plus distinct DML / DDL roots — `Insert` /
//! `Update` / `Delete` / `Merge` / `CreateTableAs` / `CreateView` /
//! `AlterTable` / `Drop`). The point is **recognisability**: a reader who knows
//! logical plans can read and extend it, and lineage falls out of a standard
//! `getColumnOrigins`-style traversal rather than bespoke pre-collapsed
//! provenance.
//!
//! This module is the **facade** the [`crate::extractor`] layer drives:
//! [`build`] binds a statement into a `LogicalPlan` (plus diagnostics), and the
//! six free functions below — `reads` / `table_reads` / `writes` /
//! `table_writes` / `column_lineage` / `table_lineage` — are the
//! extraction surfaces over that plan. They are thin entry points: each
//! delegates to a `collect_*` walker in the matching concern submodule, so the
//! `LogicalPlan` type stays plain data (extraction is a pass *over* it, not a
//! method *on* it) and the public surface reads in one place here.
//!
//! - [`logical_plan`] — the bound logical-plan operator types (the
//!   [`LogicalPlan`] enum + its node structs) and the shared tree-navigation
//!   helpers the extraction walkers use.
//! - [`binder`] — the bind pass (AST → resolved `LogicalPlan` + diagnostics);
//!   resolution is folded in (catalog match, casing, the candidate tiebreaker,
//!   the value/filter split, USING fan-in, clause-alias visibility).
//! - [`reads`](mod@reads) — the `reads` / `table_reads` walkers.
//! - [`origins`] — the `getColumnOrigins`-style trace of an expression's value
//!   to its base columns (the value-vs-filter split falls out for free).
//! - [`lineage`] — the `column_lineage` / `table_lineage` walkers, built on the
//!   [`origins`] trace.
//! - [`tables`] — the `writes` / `table_writes` walkers.

mod binder;
mod lineage;
mod logical_plan;
mod origins;
mod reads;
mod tables;

use logical_plan::LogicalPlan;

use crate::extractor::{ColumnLineageEdge, TableLineageEdge};
use crate::reference::{ColumnRead, ColumnWrite, TableRead, TableWrite};

// `build_with_diagnostics` already folds an unmodelled statement to
// `LogicalPlan::Empty`, so it doubles as `build`.
pub(crate) use binder::build_with_diagnostics as build;

/// Source-order sort key for a surfaced reference: the `(line, column)` of the
/// written identifier token's span. The walkers emit walk order (an internal
/// artifact); the facade re-sorts by this so the surfaces are a deterministic
/// function of the SQL — a *stable* sort, so references that share a token (a
/// `USING` fan-in) keep a fixed relative order.
fn source_order(name: &sqlparser::ast::Ident) -> (u64, u64) {
    let start = name.span.start;
    (start.line, start.column)
}

/// Every physical base-column read of the statement, occurrence-based (a
/// `Derived` reference is dropped — its read is counted at the inner producer).
/// Returned in source order (by the read's written token span).
pub(crate) fn reads(plan: &LogicalPlan) -> Vec<ColumnRead> {
    let mut reads = reads::collect_reads(plan);
    reads.sort_by_key(|r| source_order(&r.reference.name));
    reads
}

/// Every base table the statement scans (read role), occurrence-based. Returned
/// in source order (by the table's written token span).
pub(crate) fn table_reads(plan: &LogicalPlan) -> Vec<TableRead> {
    let mut reads = reads::collect_table_reads(plan);
    reads.sort_by_key(|r| source_order(&r.reference.name));
    reads
}

/// Every column the statement writes — a DML root's target columns, qualified
/// by the write target and paired with the column's catalog
/// [`ResolutionKind`](crate::ResolutionKind). Order follows source order.
pub(crate) fn writes(plan: &LogicalPlan) -> Vec<ColumnWrite> {
    tables::collect_writes(plan)
}

/// Every table the statement writes to — one per DML target, each paired with
/// its catalog [`ResolutionKind`](crate::ResolutionKind).
pub(crate) fn table_writes(plan: &LogicalPlan) -> Vec<TableWrite> {
    tables::collect_table_writes(plan)
}

/// The `source → target` column-lineage edges: each output column traced to
/// its base columns (`QueryOutput` for a query, `Relation` for a DML target).
/// Returned in source order of the contributing source column (by its written
/// token span).
pub(crate) fn column_lineage(
    plan: &LogicalPlan,
    casing: crate::casing::IdentifierCasing,
) -> Vec<ColumnLineageEdge> {
    let mut edges = lineage::collect_column_lineage(plan, casing);
    edges.sort_by_key(|e| source_order(&e.source.reference.name));
    edges
}

/// The `source → target` table-lineage edges: the read-role scans that feed
/// data into a DML target. Returned in source order of the feeding source table
/// (by its written token span).
pub(crate) fn table_lineage(
    plan: &LogicalPlan,
    casing: crate::casing::IdentifierCasing,
) -> Vec<TableLineageEdge> {
    let mut edges = lineage::collect_table_lineage(plan, casing);
    edges.sort_by_key(|e| source_order(&e.source.reference.name));
    edges
}

/// Which `WHEN` actions a `MERGE` carries — `Some` for a (possibly `WITH`-
/// wrapped) `Merge` root, `None` otherwise. Derived from the binder's
/// normalized [`logical_plan::MergeClause`] so callers don't re-walk the raw
/// AST (and don't have to repeat the `Statement::Query(SetExpr::Merge)`
/// unwrap that catches `WITH … MERGE`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MergeActions {
    pub(crate) has_insert: bool,
    pub(crate) has_update: bool,
    pub(crate) has_delete: bool,
}

impl MergeActions {
    /// `true` when at least one `WHEN` action physically writes a row into the
    /// target (an `INSERT` or `UPDATE` clause). A MERGE that is only `DELETE`
    /// clauses uses its source solely to pick target rows and moves no data.
    pub(crate) fn writes_data(&self) -> bool {
        self.has_insert || self.has_update
    }
}

/// Summarize a `MERGE` root's `WHEN` clauses from the bound plan. Returns
/// `None` for any non-MERGE root.
pub(crate) fn merge_actions(plan: &LogicalPlan) -> Option<MergeActions> {
    use logical_plan::{peel_with, MergeClause};
    let LogicalPlan::Merge(merge) = peel_with(plan) else {
        return None;
    };
    let mut actions = MergeActions::default();
    for clause in &merge.clauses {
        match clause {
            MergeClause::Insert { .. } => actions.has_insert = true,
            MergeClause::Update { .. } => actions.has_update = true,
            MergeClause::Delete => actions.has_delete = true,
        }
    }
    Some(actions)
}

/// Whether an `INSERT` root performs an on-conflict *update* — an upsert
/// (`ON CONFLICT DO UPDATE` / MySQL `ON DUPLICATE KEY UPDATE`), which both
/// inserts and updates the target. A plain INSERT and `ON CONFLICT DO NOTHING`
/// (no conflict assignments) are `false`. Derived from the bound plan (peeling
/// a leading `WITH`) so callers don't re-walk the raw AST.
pub(crate) fn insert_updates_on_conflict(plan: &LogicalPlan) -> bool {
    use logical_plan::peel_with;
    matches!(peel_with(plan), LogicalPlan::Insert(i) if !i.on_conflict.is_empty())
}

/// Whether the statement carries a *data-modifying CTE* — a write root that
/// isn't the outer root (`WITH c AS (INSERT … RETURNING …) SELECT … FROM c`).
/// Such a statement classifies by its outer verb (often a read `SELECT`) yet
/// still moves data, so the lineage gate must look past the outer kind.
pub(crate) fn has_data_modifying_cte(plan: &LogicalPlan) -> bool {
    use logical_plan::{dml_roots, peel_with};
    let outer = peel_with(plan);
    dml_roots(plan)
        .iter()
        .any(|root| !std::ptr::eq(*root, outer))
}

/// The CRUD-bucket contribution of a statement's *data-modifying CTEs* — each
/// CTE body's write target placed in the bucket its own verb implies (INSERT →
/// create, UPDATE → update, DELETE → delete), independent of the outer verb that
/// `StatementKind` reports. `outer_writes` carries the outer root's *own* write
/// targets so the CRUD extractor buckets them by the outer kind (not the flat
/// union of all roots, which would mis-place the CTE writes). `None` when the
/// statement has no data-modifying CTE — the flat write list is already correct.
pub(crate) struct DataModifyingCteCrud {
    pub(crate) create: Vec<TableWrite>,
    pub(crate) update: Vec<TableWrite>,
    pub(crate) delete: Vec<TableWrite>,
    pub(crate) outer_writes: Vec<TableWrite>,
}

pub(crate) fn data_modifying_cte_crud(plan: &LogicalPlan) -> Option<DataModifyingCteCrud> {
    use logical_plan::{dml_roots, peel_with};
    let outer = peel_with(plan);
    let roots = dml_roots(plan);
    if roots.iter().all(|root| std::ptr::eq(*root, outer)) {
        return None;
    }
    let mut crud = DataModifyingCteCrud {
        create: Vec::new(),
        update: Vec::new(),
        delete: Vec::new(),
        outer_writes: Vec::new(),
    };
    for root in roots {
        let targets = tables::write_root_tables(root);
        if std::ptr::eq(root, outer) {
            crud.outer_writes = targets;
            continue;
        }
        match root {
            // An upsert CTE both inserts and updates, like the outer-root upsert.
            LogicalPlan::Insert(i) => {
                crud.create.extend(targets.iter().cloned());
                if !i.on_conflict.is_empty() {
                    crud.update.extend(targets);
                }
            }
            LogicalPlan::Update(_) | LogicalPlan::AlterTable(_) => crud.update.extend(targets),
            LogicalPlan::Delete(_) | LogicalPlan::Drop(_) => crud.delete.extend(targets),
            LogicalPlan::CreateTableAs(_) | LogicalPlan::CreateView(_) => {
                crud.create.extend(targets)
            }
            // A MERGE can't appear as a CTE body in practice; bucket its target
            // conservatively as a write rather than dropping it.
            LogicalPlan::Merge(_) => crud.create.extend(targets),
            // Not a write root — contributes nothing. Listed so a new variant
            // forces a bucket decision.
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
            | LogicalPlan::Empty => {}
        }
    }
    Some(crud)
}
