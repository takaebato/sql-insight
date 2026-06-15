//! The bound logical-plan analysis engine — design **B** from the
//! 2026-06-14 grill. It turns a parsed `Statement` into a resolved
//! representation the extractors consume: a materialized, full-stack
//! operator tree ([`ir::Plan`]) whose column provenance is resolved
//! bottom-up. It is **not** an execution plan — nothing here optimizes or
//! runs SQL. This engine backs every public extractor (column / table /
//! flat / CRUD); it replaced the former flat-buffer resolver.
//!
//! - [`ir`] — the persistent operator tree types.
//! - [`binder`] — `build`: AST → resolved `Plan` (the bind pass).
//! - [`extract`] — walk a `Plan` for the operation surfaces.
//! - [`operation`] — assemble the public `ColumnOperation` from a `Plan`.
//! - [`table_operation`] — assemble the public `TableOperation` and the
//!   legacy flat table list from a `Plan`.
//!
//! ## Coverage
//!
//! SELECT / FROM / joins / WHERE / GROUP BY / HAVING / ORDER BY / USING
//! fan-in / derived tables / CTEs (incl. recursive) / subqueries / set
//! operations / DML (INSERT / UPDATE / DELETE / MERGE, RETURNING, ON
//! CONFLICT) / DDL (CTAS / CREATE VIEW / ALTER / DROP / TRUNCATE) /
//! wildcards (suppressed). Known gaps are tracked as deferred bricks
//! (pipe-operator output rewriting, MERGE catalog-fill writes).

mod binder;
mod extract;
mod ir;
pub(crate) mod operation;
pub(crate) mod table_operation;
