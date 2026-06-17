//! The analysis engine: a **standard logical plan** ([`operator::Operator`])
//! built by the [`binder`] and walked by a column-origin [`traverse`]al for the
//! extraction surfaces. It is **not** an execution plan — nothing optimises or
//! runs SQL.
//!
//! A textbook relational-algebra tree (`Scan` / `Filter` / `Join` / `Aggregate`
//! / `Project` / `Sort` / `SetOp` / `SubqueryAlias` / `TableFunction` / `With` +
//! `CteRef` / `Values`, plus distinct DML / DDL roots — `Insert` / `Update` /
//! `Delete` / `Merge` / `CreateTableAs` / `CreateView` / `AlterTable` / `Drop`).
//! The point is **recognisability**: a reader who knows logical plans can read
//! and extend it, and lineage falls out of a standard `getColumnOrigins`-style
//! traversal rather than bespoke pre-collapsed provenance.
//!
//! The crate-internal surface is two halves: [`build_plan`] (AST → bound
//! `Operator` + diagnostics) and the `extract_*` walkers (`reads` / `writes` /
//! `lineage` / `flat_tables`). The [`crate::extractor`] layer drives them and
//! packages the public `*Operation` types.
//!
//! - [`operator`] — the bound operator-tree types.
//! - [`binder`] — the bind pass (AST → resolved `Operator` + diagnostics);
//!   resolution is folded in (catalog match, casing, the candidate tiebreaker,
//!   the value/filter split, USING fan-in, clause-alias visibility).
//! - [`traverse`] — walk an `Operator` for the extraction surfaces, tracing
//!   each output column's value expression to its base columns.

mod binder;
mod operator;
mod traverse;

// The crate-internal surface the extractors drive: `build_plan` (AST → bound
// `Operator` + diagnostics) and the `extract_*` walkers.
// `build_with_diagnostics` already folds an unmodelled statement to
// `Operator::Empty`, so it doubles as `build_plan`.
pub(crate) use binder::build_with_diagnostics as build_plan;
pub(crate) use traverse::{
    column_lineage as extract_lineage, flat_tables as extract_flat_tables, reads as extract_reads,
    table_lineage as extract_table_lineage, table_reads as extract_table_reads,
    table_writes as extract_table_writes, writes as extract_writes,
};
