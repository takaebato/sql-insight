//! The analysis engine: a **standard logical plan** ([`logical_plan::LogicalPlan`])
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
//! The crate-internal surface is two concepts: [`build`] (AST → bound
//! `LogicalPlan` + diagnostics) and the [`logical_plan::LogicalPlan`] itself, whose
//! extraction surfaces (`reads` / `writes` / `column_lineage` / `table_reads`
//! / `table_writes` / `table_lineage` / `flat_tables`) are its methods. The
//! [`crate::extractor`] layer drives them and packages the public `*Operation`
//! types.
//!
//! - [`logical_plan`] — the bound logical-plan operator types (the
//!   [`LogicalPlan`](logical_plan::LogicalPlan) enum + its node structs).
//! - [`binder`] — the bind pass (AST → resolved `LogicalPlan` + diagnostics);
//!   resolution is folded in (catalog match, casing, the candidate tiebreaker,
//!   the value/filter split, USING fan-in, clause-alias visibility).
//! - [`traverse`] — the `LogicalPlan` extraction methods, tracing each output
//!   column's value expression to its base columns.

mod binder;
mod logical_plan;
mod traverse;

// The crate-internal surface the extractors drive is two concepts: [`build`]
// (AST → bound `LogicalPlan` + diagnostics) and the `LogicalPlan` itself,
// whose extraction surfaces (`reads` / `writes` / `column_lineage` /
// `table_reads` / `table_writes` / `table_lineage` / `flat_tables`) are its
// methods (defined in `traverse`). `build_with_diagnostics` already folds an
// unmodelled statement to `LogicalPlan::Empty`, so it doubles as `build`.
pub(crate) use binder::build_with_diagnostics as build;
