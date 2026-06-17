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
//! seven free functions below — `reads` / `table_reads` / `writes` /
//! `table_writes` / `column_lineage` / `table_lineage` / `flat_tables` — are the
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
//! - [`tables`] — the `writes` / `table_writes` / `flat_tables` walkers.

mod binder;
mod lineage;
mod logical_plan;
mod origins;
mod reads;
mod tables;

use logical_plan::LogicalPlan;

use crate::extractor::{ColumnLineageEdge, TableLineageEdge};
use crate::reference::{ColumnRead, ColumnReference, TableRead, TableReference};

// `build_with_diagnostics` already folds an unmodelled statement to
// `LogicalPlan::Empty`, so it doubles as `build`.
pub(crate) use binder::build_with_diagnostics as build;

/// Every physical base-column read of the statement, occurrence-based (a
/// `Derived` reference is dropped — its read is counted at the inner producer).
pub(crate) fn reads(plan: &LogicalPlan) -> Vec<ColumnRead> {
    reads::collect_reads(plan)
}

/// Every base table the statement scans (read role), occurrence-based.
pub(crate) fn table_reads(plan: &LogicalPlan) -> Vec<TableRead> {
    reads::collect_table_reads(plan)
}

/// Every column the statement writes — a DML root's target columns, qualified
/// by the write target. Order follows source order.
pub(crate) fn writes(plan: &LogicalPlan) -> Vec<ColumnReference> {
    tables::collect_writes(plan)
}

/// Every table the statement writes to — one per DML target.
pub(crate) fn table_writes(plan: &LogicalPlan) -> Vec<TableReference> {
    tables::collect_table_writes(plan)
}

/// The `source → target` column-lineage edges: each output column traced to
/// its base columns (`QueryOutput` for a query, `Relation` for a DML target).
pub(crate) fn column_lineage(plan: &LogicalPlan) -> Vec<ColumnLineageEdge> {
    lineage::collect_column_lineage(plan)
}

/// The `source → target` table-lineage edges: the read-role scans that feed
/// data into a DML target.
pub(crate) fn table_lineage(plan: &LogicalPlan) -> Vec<TableLineageEdge> {
    lineage::collect_table_lineage(plan)
}

/// The flat list of every table the statement references — one per relation
/// binding (the legacy table surface).
pub(crate) fn flat_tables(plan: &LogicalPlan) -> Vec<TableReference> {
    tables::collect_flat_tables(plan)
}
