//! INCUBATING (option "a", not yet wired): the redesigned analysis engine —
//! a **standard logical plan** ([`operator::Operator`]) built by the binder
//! and walked by a column-origin traversal for the extraction surfaces.
//!
//! This replaces the design-"B" [`crate::resolver`] IR (the materialized
//! `Plan` of `resolver::ir`) with a textbook relational-algebra tree
//! (`Scan` / `Filter` / `Join` / `Aggregate` / `Project` / `SetOp` /
//! `SubqueryAlias` / `With` + `CteRef` / `Values`, plus distinct DML / DDL
//! roots). The point is **recognisability**: a reader who knows logical
//! plans can read and extend it, and lineage falls out of a standard
//! `getColumnOrigins`-style traversal rather than bespoke pre-collapsed
//! provenance. It is **not** an execution plan — nothing optimises or runs.
//!
//! Built alongside `resolver`; switched into the public extractors at
//! differential parity, then renamed to `resolver`. See the memory
//! `project_operator_redesign` for the full design lock.
//!
//! Staging: ① types (this brick) → ② bind core + traversal + differential
//! harness → ③ catalog / Open → ④ clause / aggregate → ⑤ CTE / LATERAL →
//! ⑥ DML roots → ⑦ breadth → ⑧ switch.

// Incubating: the types and (later) bind/traverse are not wired into any
// public extractor until the differential switch, so they read as dead.
#![allow(dead_code)]

mod operator;
