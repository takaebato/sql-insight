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
//! The crate-internal surface is two concepts: [`build`] (AST → bound
//! `LogicalPlan` + diagnostics) and the [`logical_plan::LogicalPlan`] itself,
//! whose extraction surfaces (`reads` / `writes` / `column_lineage` /
//! `table_reads` / `table_writes` / `table_lineage` / `flat_tables`) are its
//! methods. The [`crate::extractor`] layer drives them and packages the public
//! `*Operation` types.
//!
//! - [`logical_plan`] — the bound logical-plan operator types (the
//!   [`LogicalPlan`](logical_plan::LogicalPlan) enum + its node structs) and the
//!   shared tree-navigation helpers the extraction modules walk with.
//! - [`binder`] — the bind pass (AST → resolved `LogicalPlan` + diagnostics);
//!   resolution is folded in (catalog match, casing, the candidate tiebreaker,
//!   the value/filter split, USING fan-in, clause-alias visibility).
//! - [`reads`] — the column / table read surfaces.
//! - [`origins`] — the `getColumnOrigins`-style trace of an expression's value
//!   to its base columns (the value-vs-filter split falls out for free).
//! - [`lineage`] — the column / table `source → target` edges, built on the
//!   [`origins`] trace.
//! - [`tables`] — the write surfaces and the legacy flat table list.

mod binder;
mod lineage;
mod logical_plan;
mod origins;
mod reads;
mod tables;

// The crate-internal surface the extractors drive is two concepts: [`build`]
// (AST → bound `LogicalPlan` + diagnostics) and the `LogicalPlan` itself,
// whose extraction surfaces (`reads` / `writes` / `column_lineage` /
// `table_reads` / `table_writes` / `table_lineage` / `flat_tables`) are its
// methods (each defined in the matching extraction submodule).
// `build_with_diagnostics` already folds an unmodelled statement to
// `LogicalPlan::Empty`, so it doubles as `build`.
pub(crate) use binder::build_with_diagnostics as build;
