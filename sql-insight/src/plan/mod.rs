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
//!
//! ## Current brick
//!
//! `SELECT` over `FROM` (single table, comma joins, `JOIN … ON`) with a
//! `WHERE` filter and a projection of column references / simple
//! expressions, resolved catalog-free. Catalog / open-schema resolution,
//! GROUP BY / HAVING / ORDER BY, set operations, CTE / derived /
//! subquery, DML, and `USING` fan-in are later bricks.
#![allow(dead_code)] // incubating: exercised by tests only until wired

mod binder;
mod extract;
mod ir;
