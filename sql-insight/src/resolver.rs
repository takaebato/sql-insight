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
/// Returned in source order of the contributing source column (by its written
/// token span).
pub(crate) fn column_lineage(plan: &LogicalPlan) -> Vec<ColumnLineageEdge> {
    let mut edges = lineage::collect_column_lineage(plan);
    edges.sort_by_key(|e| source_order(&e.source.reference.name));
    edges
}

/// The `source → target` table-lineage edges: the read-role scans that feed
/// data into a DML target. Returned in source order of the feeding source table
/// (by its written token span).
pub(crate) fn table_lineage(plan: &LogicalPlan) -> Vec<TableLineageEdge> {
    let mut edges = lineage::collect_table_lineage(plan);
    edges.sort_by_key(|e| source_order(&e.source.reference.name));
    edges
}

/// The flat list of every table the statement references — one per relation
/// binding (the un-bucketed table surface). Returned in source order (by each
/// table's written token span).
pub(crate) fn flat_tables(plan: &LogicalPlan) -> Vec<TableReference> {
    let mut tables = tables::collect_flat_tables(plan);
    tables.sort_by_key(|t| source_order(&t.name));
    tables
}
