//! The expression-walk accumulators the [`Binder`] fills while collecting
//! an expression's references and nested sub-plans, split by position
//! (value vs filter). Scratch — fields are `pub(super)` so the binder's
//! `collect_*` methods read and write them directly.
//!
//! [`Binder`]: super::binder

use super::ir::{BoundColumn, Plan, ProvenanceSource};
use crate::reference::ColumnRead;

/// Accumulator for walking one expression. References split by position:
/// `sources` are value references (they flow to the output → lineage),
/// `filter_reads` are predicate references (they only influence which rows
/// / values are produced → reads but not lineage). Sub-plans of nested
/// subqueries split the same way: `value_subplans` sit in value position
/// (their output feeds the enclosing value → lineage), `filter_subplans`
/// sit in a predicate (`EXISTS` / `IN` / a `CASE` condition → reads only).
/// `is_suppressed` marks the current position as a filter and routes a
/// subquery to the right list.
#[derive(Default)]
pub(super) struct ExprCollector {
    pub(super) sources: Vec<ProvenanceSource>,
    pub(super) filter_reads: Vec<ColumnRead>,
    pub(super) value_subplans: Vec<Plan>,
    pub(super) filter_subplans: Vec<Plan>,
    pub(super) is_suppressed: bool,
}

/// One value expression bound into an output column, with its
/// position-split side effects. `value_subplans` feed lineage (a scalar
/// subquery whose result flows to the column), while `filter_reads` /
/// `filter_subplans` are predicate-only (a `CASE` condition, an `EXISTS`
/// test) — reads that don't feed, destined for a non-feeding position.
pub(super) struct BoundValue {
    pub(super) column: BoundColumn,
    pub(super) value_subplans: Vec<Plan>,
    pub(super) filter_reads: Vec<ColumnRead>,
    pub(super) filter_subplans: Vec<Plan>,
}

impl ExprCollector {
    /// A value position (a projection item / SET / VALUES RHS): references
    /// flow as lineage sources unless a sub-expression suppresses them.
    pub(super) fn value() -> Self {
        Self::default()
    }

    /// A filter position (WHERE / ON / clause predicate / DML predicate):
    /// the whole expression is a predicate, so every reference is a read.
    pub(super) fn filter() -> Self {
        Self {
            is_suppressed: true,
            ..Self::default()
        }
    }

    /// Run `f` with the position forced to a filter (a predicate
    /// sub-expression — a `CASE` condition, an `EXISTS` test, a sort /
    /// partition key), restoring the prior position afterward.
    pub(super) fn suppressed(&mut self, f: impl FnOnce(&mut Self)) {
        let prev = self.is_suppressed;
        self.is_suppressed = true;
        f(self);
        self.is_suppressed = prev;
    }

    /// Drain a filter-context collector (WHERE / ON / clause / arg / pipe):
    /// its reads plus *all* its sub-plans. In a filter position no sub-plan
    /// feeds lineage, so value- and filter-position sub-plans merge into one
    /// non-feeding list (a filter collector never collects value sub-plans
    /// anyway, since `is_suppressed` is never cleared).
    pub(super) fn into_filter_parts(self) -> (Vec<ColumnRead>, Vec<Plan>) {
        let mut subplans = self.value_subplans;
        subplans.extend(self.filter_subplans);
        (self.filter_reads, subplans)
    }
}
