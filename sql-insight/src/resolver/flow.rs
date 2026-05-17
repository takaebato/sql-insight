//! `FlowEdge` / `FlowTargetSpec` and the resolver helpers that emit
//! them — directly into the `flow_edges` buffer, or fanned out from
//! a snapshot of recorded column refs, or driven by a projection
//! group via a closure-supplied target.

use sqlparser::ast::{Ident, Query};

use crate::error::Error;
use crate::extractor::column_operation_extractor::ColumnFlowKind;
use crate::relation::TableReference;

use super::{ProjectionGroup, ProjectionItem, RawColumnRef, ResolvedQuery, Resolver};

/// A pre-resolution column flow record. `source` still needs
/// scope-chain resolution (for unqualified parts); `target` is fully
/// spec'd by the resolver; `kind` is the public `ColumnFlowKind` to
/// surface (composed further by `composed_flow_edges` when the source
/// goes through a synthetic intermediate).
///
/// Created by callers from [`ProjectionGroup`]s (for SELECT-style
/// flows — INSERT pairs with target columns, top-level / nested
/// SELECTs emit `QueryOutput`) or directly by UPDATE / similar
/// walkers that already know their write target.
#[derive(Debug, Clone)]
pub(crate) struct FlowEdge {
    pub(crate) source: RawColumnRef,
    pub(crate) target: FlowTargetSpec,
    pub(crate) kind: ColumnFlowKind,
}

/// Target spec for a [`FlowEdge`]. `QueryOutput` is for transient
/// SELECT output columns; `Persisted` is for INSERT / UPDATE / etc.
/// target columns that live in a real relation.
#[derive(Debug, Clone)]
pub(crate) enum FlowTargetSpec {
    QueryOutput {
        name: Option<Ident>,
        position: usize,
    },
    Persisted {
        table: TableReference,
        column: Ident,
    },
}

impl<'a> Resolver<'a> {
    pub(super) fn push_flow_edge(&mut self, edge: FlowEdge) {
        self.flow_edges.push(edge);
    }

    /// Emit one `FlowEdge` per `RawColumnRef` recorded into
    /// `column_refs` since position `since`, all pointing to the same
    /// `target` with the given `kind`. The typical caller snapshots
    /// `column_refs_len()` before walking an expression, walks it,
    /// then calls this with the snapshot to fan the new refs out as
    /// edges. Used by UPDATE / MERGE assignment loops and MERGE
    /// INSERT-VALUES emission.
    pub(super) fn push_edges_from_refs_since(
        &mut self,
        since: usize,
        target: FlowTargetSpec,
        kind: ColumnFlowKind,
    ) {
        for offset in 0..(self.column_refs_len() - since) {
            let source = self.column_refs_slice(since)[offset].clone();
            self.push_flow_edge(FlowEdge {
                source,
                target: target.clone(),
                kind,
            });
        }
    }

    /// For each `(group, position, item)` in `projections`, ask
    /// `target_for(position, item)` to produce a `FlowTargetSpec`;
    /// when it returns `Some(target)`, fan out one `FlowEdge` per
    /// `item.source_refs` to that target, carrying the item's
    /// `ColumnFlowKind`. The closure shape lets the same loop drive
    /// `QueryOutput` emission, INSERT positional pairing, and CTAS /
    /// view's explicit-or-inferred column pairing.
    pub(super) fn emit_per_projection<F>(
        &mut self,
        projections: &[ProjectionGroup],
        mut target_for: F,
    ) where
        F: FnMut(usize, &ProjectionItem) -> Option<FlowTargetSpec>,
    {
        for group in projections {
            for (position, item) in group.items.iter().enumerate() {
                let Some(target) = target_for(position, item) else {
                    continue;
                };
                for source in &item.source_refs {
                    self.push_flow_edge(FlowEdge {
                        source: source.clone(),
                        target: target.clone(),
                        kind: item.kind,
                    });
                }
            }
        }
    }

    /// Emit `QueryOutput` flow edges for every projection item in
    /// `resolved`. The default disposition for queries whose output
    /// is not bound to a persisted target (top-level SELECT, scalar
    /// subqueries, derived tables, CTE bodies, predicate subqueries).
    pub(super) fn emit_query_output_edges(&mut self, resolved: &ResolvedQuery) {
        self.emit_per_projection(&resolved.projections, |position, item| {
            Some(FlowTargetSpec::QueryOutput {
                name: item.name.clone(),
                position,
            })
        });
    }

    /// Convenience wrapper: resolve `query` and emit `QueryOutput`
    /// edges for its projections in one shot. Use this from any
    /// caller that doesn't have a special target — INSERT calls the
    /// raw `resolve_query` instead so it can pair projections with
    /// its target columns.
    pub(super) fn resolve_query_emitting_query_output(
        &mut self,
        query: &Query,
    ) -> Result<ResolvedQuery, Error> {
        let resolved = self.resolve_query(query)?;
        self.emit_query_output_edges(&resolved);
        Ok(resolved)
    }
}
