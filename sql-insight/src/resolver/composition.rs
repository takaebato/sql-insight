//! Post-walk passes on `Resolution`:
//!
//! - [`Resolution::composed_flow_edges`] rewrites each flow
//!   edge so its source resolves to a real (non-synthetic) reference
//!   by walking back through CTE / derived body projections.
//! - [`Resolution::real_column_refs`] filters out refs whose
//!   walk-time owner was synthetic, so the public `reads` surface
//!   only shows real-storage references and unresolved names.

use crate::extractor::column_operation_extractor::ColumnLineageKind;

use super::binding::{binding_alias_key, BindingKey};
use super::{Binding, FlowEdge, RawColumnRef, Resolution};

/// Recursion ceiling for `substitute_source` — guards against
/// accidental cycles (recursive CTEs are pre-bound with empty
/// body_projections, so the typical case stops there; this is a
/// defence for unexpected loops).
const MAX_COMPOSITION_DEPTH: usize = 64;

impl Resolution {
    /// Filter [`column_refs`](Resolution::column_refs) down
    /// to "real reads": references whose walk-time owning binding was
    /// a `Table` (or unresolved). Refs that pointed at a synthetic
    /// intermediate (`Cte` / `DerivedTable` / `TableFunction`) are
    /// dropped — those intermediates aren't storage, so they don't
    /// belong in the public reads surface.
    pub(crate) fn real_column_refs(&self) -> Vec<RawColumnRef> {
        self.column_refs
            .iter()
            .filter(|raw| !raw.synthetic)
            .cloned()
            .collect()
    }

    /// Compose every flow edge so its source resolves to a real
    /// (non-synthetic) reference. References whose walk-time owner
    /// is a Cte / DerivedTable with non-empty `body_projections` get
    /// substituted by walking that body's matching `ProjectionItem`
    /// and emitting one edge per inner source ref — recursively,
    /// until the chain bottoms out at a real table or an unresolvable
    /// ref. The outer edge's `kind` is combined with each body
    /// item's kind via [`compose_flow_kinds`] (Aggregation dominates;
    /// Passthrough is preserved only when both sides are
    /// Passthrough). Bounded by [`MAX_COMPOSITION_DEPTH`] as a cycle
    /// guard.
    pub(crate) fn composed_flow_edges(&self) -> Vec<FlowEdge> {
        self.flow_edges
            .iter()
            .flat_map(|edge| {
                self.substitute_source(&edge.source, edge.kind, 0)
                    .into_iter()
                    .map(|(source, kind)| FlowEdge {
                        source,
                        target: edge.target.clone(),
                        kind,
                    })
            })
            .collect()
    }

    fn substitute_source(
        &self,
        raw: &RawColumnRef,
        outer_kind: ColumnLineageKind,
        depth: usize,
    ) -> Vec<(RawColumnRef, ColumnLineageKind)> {
        if depth >= MAX_COMPOSITION_DEPTH {
            return vec![(raw.clone(), outer_kind)];
        }
        let body_projections = match self.synthetic_owning_binding(raw) {
            Some(Binding::Cte {
                body_projections, ..
            }) => body_projections,
            Some(Binding::DerivedTable {
                body_projections, ..
            }) => body_projections,
            _ => return vec![(raw.clone(), outer_kind)],
        };
        if body_projections.is_empty() {
            return vec![(raw.clone(), outer_kind)];
        }
        let Some(col_name) = raw.parts.last() else {
            return vec![(raw.clone(), outer_kind)];
        };
        let key = BindingKey::from_ident(col_name);
        let mut result = Vec::new();
        for group in body_projections {
            for item in &group.items {
                let matches = item
                    .name
                    .as_ref()
                    .is_some_and(|n| BindingKey::from_ident(n) == key);
                if !matches {
                    continue;
                }
                let composed = compose_flow_kinds(outer_kind, item.kind);
                for source in &item.source_refs {
                    result.extend(self.substitute_source(source, composed, depth + 1));
                }
            }
        }
        if result.is_empty() {
            vec![(raw.clone(), outer_kind)]
        } else {
            result
        }
    }

    /// Look up the binding a synthetic-owning raw ref points at, by
    /// matching the walk-time-captured table name against scope
    /// bindings. Name match is unique within IndexMap, so this avoids
    /// the column-membership ambiguity that scope-chain resolution
    /// can hit when CTEs accumulate. Returns `None` for non-synthetic
    /// refs.
    fn synthetic_owning_binding(&self, raw: &RawColumnRef) -> Option<&Binding> {
        if !raw.synthetic {
            return None;
        }
        let table = raw.resolved.as_ref()?;
        let key = BindingKey::from_ident(&table.name);
        let mut current = Some(raw.scope_id);
        while let Some(id) = current {
            let scope = &self.scopes[id.0];
            for binding in scope.iter_bindings() {
                if binding_alias_key(binding) == key {
                    return Some(binding);
                }
            }
            current = scope.parent;
        }
        None
    }
}

/// Combine two flow kinds along a substitution edge: the result is
/// `Passthrough` only when both sides are `Passthrough`; any
/// `Transformation` step makes the whole composed chain a
/// `Transformation`.
fn compose_flow_kinds(outer: ColumnLineageKind, inner: ColumnLineageKind) -> ColumnLineageKind {
    if outer == ColumnLineageKind::Passthrough && inner == ColumnLineageKind::Passthrough {
        ColumnLineageKind::Passthrough
    } else {
        ColumnLineageKind::Transformation
    }
}
