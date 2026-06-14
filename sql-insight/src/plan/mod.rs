//! Incubating bound logical-plan — design **B** from the 2026-06-14
//! grill. This is the **resolver, reimplemented**: same role (turn a
//! parsed `Statement` into a resolved representation the extractors
//! consume), new internal IR. Where [`crate::resolver`] produces a flat
//! `Resolution` (scope arena + captured refs + collapse post-passes),
//! this produces a materialized, full-stack operator tree
//! ([`ir::Plan`]) whose column provenance is resolved bottom-up. It is
//! **not** an execution plan — nothing here optimizes or runs SQL.
//!
//! Built **alongside** the current resolver (strangler migration) and
//! not reachable from any public API yet. When it reaches parity it
//! takes over the `resolver` module name and `Plan` replaces
//! `Resolution`; until then it incubates under `plan` (`#![allow(dead_code)]`).
//!
//! - [`ir`] — the persistent operator tree types.
//! - [`binder`] — `build`: AST → resolved `Plan` (the bind pass).
//! - [`extract`] — walk a `Plan` for the operation surfaces, plus the
//!   differential harness that pins the binder's output against the
//!   current resolver.
//! - [`operation`] — assemble the public `ColumnOperation` from a `Plan`.
//! - [`table_operation`] — assemble the public `TableOperation` (reads /
//!   writes; lineage is a follow-up brick) from a `Plan`.
//!
//! ## Coverage
//!
//! Column-level extraction is at full differential parity with the
//! resolver across the covered corpus (SELECT / FROM / joins / WHERE /
//! GROUP BY / HAVING / ORDER BY / USING fan-in / derived tables / CTEs /
//! subqueries / set operations / DML / DDL / wildcards). Table-level
//! extraction covers `reads` / `writes`; table-level `lineage` and the
//! legacy table / CRUD extractors are later bricks.
#![allow(dead_code)] // incubating: exercised by tests only until wired

mod binder;
mod extract;
mod ir;
pub(crate) mod operation;
pub(crate) mod table_operation;
