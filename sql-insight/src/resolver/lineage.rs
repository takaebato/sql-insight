//! `LineageEdge` / `LineageTargetSpec` and the resolver helpers that
//! emit them — directly into the `lineage_edges` buffer, fanned out
//! from a snapshot of recorded column refs, or driven by a
//! [`BodyOutput`](super::BodyOutput)'s set operands via a
//! closure-supplied target.

use sqlparser::ast::{Ident, Query};

use crate::error::Error;
use crate::extractor::ColumnLineageKind;
use crate::reference::TableReference;

use super::{OutputColumn, RawColumnRef, ResolvedQuery, Resolver, SetOperand};

/// A pre-resolution column lineage record. `source` still needs
/// scope-chain resolution (for unqualified parts); `target` is fully
/// spec'd by the resolver; `kind` is the public `ColumnLineageKind` to
/// surface (collapsed further by `collapsed_lineage_edges` when the
/// source goes through a synthetic relation).
///
/// Created by callers from a [`super::BodyOutput`]'s set operands
/// (for SELECT-style lineage edges — INSERT pairs with target columns,
/// top-level / nested SELECTs emit `QueryOutput`) or directly by
/// UPDATE / similar walkers that already know their write target.
#[derive(Debug, Clone)]
pub(crate) struct LineageEdge {
    /// Source column ref as recorded at the walk site. Still carries
    /// `scope_id` so collapse can re-resolve through synthetic
    /// relations.
    pub(crate) source: RawColumnRef,
    /// Where the value flows to — a transient SELECT output column
    /// or a real-relation column.
    pub(crate) target: LineageTargetSpec,
    /// `Passthrough` (bare forwarded column) or `Transformation`
    /// (anything value-changing). Composed with the inner edge's
    /// kind when the source goes through a CTE / derived synthetic.
    pub(crate) kind: ColumnLineageKind,
}

/// Target spec for a [`LineageEdge`]. `QueryOutput` is for transient
/// SELECT output columns; `Relation` is for INSERT / UPDATE / etc.
/// target columns that live in a real relation.
#[derive(Debug, Clone)]
pub(crate) enum LineageTargetSpec {
    QueryOutput {
        /// Output column's inferred name: explicit alias > bare
        /// identifier > `None` for computed expressions / wildcards.
        name: Option<Ident>,
        /// Zero-based position in the SELECT's projection list. The
        /// position is the stable handle when `name` is `None`.
        position: usize,
    },
    Relation {
        /// The relation being written to (INSERT / UPDATE target,
        /// CTAS / CREATE VIEW output, MERGE target).
        table: TableReference,
        /// The specific column within `table` receiving the value.
        column: Ident,
    },
}

impl<'a> Resolver<'a> {
    /// Emit one `LineageEdge` per `RawColumnRef` recorded into
    /// `column_refs` since position `since`, all pointing to the same
    /// `target` with the given `kind`. The typical caller snapshots
    /// `self.column_refs.len()` before walking an expression, walks
    /// it, then calls this with the snapshot to fan the new refs out
    /// as edges. Used by UPDATE / MERGE assignment loops and MERGE
    /// INSERT-VALUES emission.
    pub(super) fn push_edges_from_refs_since(
        &mut self,
        since: usize,
        target: LineageTargetSpec,
        kind: ColumnLineageKind,
    ) {
        let sources: Vec<RawColumnRef> = self.resolution.column_refs[since..].to_vec();
        for source in sources {
            self.resolution.lineage_edges.push(LineageEdge {
                source,
                target: target.clone(),
                kind,
            });
        }
    }

    /// For each `(operand, position, column)` across `operands`, ask
    /// `target_for(position, column)` to produce a `LineageTargetSpec`;
    /// when it returns `Some(target)`, fan out one `LineageEdge` per
    /// `column.source_refs` to that target, carrying the column's
    /// `ColumnLineageKind`. The closure shape lets the same loop drive
    /// `QueryOutput` emission, INSERT positional pairing, and CTAS /
    /// view's explicit-or-inferred column pairing.
    pub(super) fn emit_per_output_column<F>(&mut self, operands: &[SetOperand], mut target_for: F)
    where
        F: FnMut(usize, &OutputColumn) -> Option<LineageTargetSpec>,
    {
        for operand in operands {
            for (position, column) in operand.columns.iter().enumerate() {
                let Some(target) = target_for(position, column) else {
                    continue;
                };
                for source in &column.source_refs {
                    self.resolution.lineage_edges.push(LineageEdge {
                        source: source.clone(),
                        target: target.clone(),
                        kind: column.kind,
                    });
                }
            }
        }
    }

    /// Emit `QueryOutput` lineage edges for every output column in
    /// `resolved`. The default disposition for queries whose output
    /// is not bound to a relation target (top-level SELECT, scalar
    /// subqueries, derived tables, CTE bodies, predicate subqueries).
    pub(super) fn emit_query_output_edges(&mut self, resolved: &ResolvedQuery) {
        let Some(output) = resolved.output_columns.as_ref() else {
            return;
        };
        self.emit_per_output_column(&output.set_operands, |position, column| {
            Some(LineageTargetSpec::QueryOutput {
                name: column.name.clone(),
                position,
            })
        });
    }

    /// Convenience wrapper: resolve `query` and emit `QueryOutput`
    /// edges for its output columns in one shot. Use this from any
    /// caller that doesn't have a special target — INSERT calls the
    /// raw `resolve_query` instead so it can pair output columns with
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
