//! Column-level extraction: exhaustive black-box coverage of the public
//! `extract_column_operations` contract. Organized by concern
//! (cross-cutting: resolution / casing / lineage / reads-semantics /
//! diagnostics), by construct (joins-sets / cte-and-dml / writes-deletes),
//! and by systematic AST-variant arm coverage.

#[macro_use]
mod support;

mod arm_coverage;
mod casing;
mod cte_and_dml;
mod diagnostics;
mod joins_sets;
mod lineage;
mod reads_semantics;
mod resolution;
mod writes_deletes;
