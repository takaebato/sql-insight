//! The resolution engine: turns a parsed `Statement` into a resolved
//! representation the extractors consume — a materialized, full-stack
//! bound logical-plan tree ([`ir::Plan`]) whose column provenance is
//! resolved bottom-up. It is **not** an execution plan — nothing here
//! optimizes or runs SQL.
//!
//! The crate-internal surface is two halves: [`build_plan`] (AST →
//! resolved `Plan`) and the `extract_*` walkers (`Plan` → reads / writes
//! / lineage / flat tables). The [`crate::extractor`] layer drives them
//! and packages the public `*Operation` types.
//!
//! - [`ir`] — the persistent operator tree types.
//! - [`binder`] — the bind pass (AST → resolved `Plan`).
//! - [`extract`] — walk a `Plan` for the operation surfaces.
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

pub(crate) use binder::build_plan;
pub(crate) use extract::{
    extract_flat_tables, extract_lineage, extract_reads, extract_table_lineage,
    extract_table_reads, extract_table_writes, extract_writes,
};
