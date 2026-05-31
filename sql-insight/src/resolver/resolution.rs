//! `impl Resolution` — methods on the end-of-walk result:
//!
//! - Public table queries: [`Resolution::tables`],
//!   [`Resolution::read_tables`], [`Resolution::write_tables`].
//! - Column-ref post-pass: [`Resolution::real_column_refs`] filters
//!   out refs whose walk-time owner was synthetic, so the public
//!   `reads` surface only shows real-storage references and
//!   unresolved names.
//! - Column-lineage post-pass: [`Resolution::collapsed_lineage_edges`]
//!   rewrites each lineage edge so its source resolves to a real
//!   (non-synthetic) reference by walking back through the CTE /
//!   derived body's output columns.
//! - Table-lineage post-pass:
//!   [`Resolution::collapsed_feeding_table_sources`] walks the
//!   captured `RawTableRef` events and recursively expands synthetic
//!   uses (CTE / derived) into the real tables underneath,
//!   producing the lineage-source list at table granularity.

use std::collections::HashSet;

use crate::diagnostic::ColumnLevelDiagnostic;
use crate::extractor::ColumnLineageKind;
use crate::reference::TableReference;

use super::binding::binding_alias_key;
use super::binding::BindingKey;
use super::scope::parent_chain;
use super::{
    Binding, LineageEdge, RawColumnRef, RawTableRef, Scope, ScopeId, ScopeKind, TableRefTarget,
    TableRole,
};

/// The end-of-walk result the resolver produces. Holds the scope
/// arena and the raw column refs / lineage edges collected during the
/// walk, plus accumulated diagnostics. Two post-passes inside
/// `Resolver::into_resolution` refine `column_refs` and
/// `lineage_edges` before the resolution leaves the resolver.
#[derive(Debug, Default)]
pub(crate) struct Resolution {
    /// Column-level diagnostics accumulated during the walk
    /// (`WildcardSuppressed`, `AmbiguousColumn`, `UnresolvedColumn`,
    /// `UnsupportedStatement`). Table-level extractors project this
    /// down via `ColumnLevelDiagnostic::to_table_level`.
    pub(crate) diagnostics: Vec<ColumnLevelDiagnostic>,
    /// Finalized scope arena, indexed by [`ScopeId`]. Holds every
    /// scope created during the walk — post-passes (collapse,
    /// real-column-ref filtering) re-walk via id lookups.
    pub(crate) scopes: Vec<Scope>,
    /// Column refs that survive the synthetic-binding filter (see
    /// [`Resolution::real_column_refs`]).
    pub(crate) column_refs: Vec<RawColumnRef>,
    /// Lineage edges after end-to-end collapse through CTE / derived
    /// synthetics (see [`Resolution::collapsed_lineage_edges`]).
    pub(crate) lineage_edges: Vec<LineageEdge>,
    /// Every `FROM`-position use of a table-like source captured
    /// during the walk. Drives table-lineage collapse (see
    /// [`Resolution::collapsed_feeding_table_sources`]) — an entry per
    /// physical use, occurrence-based (no dedup), matching
    /// `ColumnLineageEdge` semantics.
    pub(crate) table_refs: Vec<RawTableRef>,
}

/// Recursion ceiling for `collapse_source` — guards against
/// accidental cycles (recursive CTEs are pre-bound with `None`
/// `output_columns`, so the typical case stops there; this is a
/// defence for unexpected loops).
const MAX_COLLAPSE_DEPTH: usize = 64;

impl Resolution {
    /// Filter [`column_refs`](Resolution::column_refs) down
    /// to "real reads": references whose walk-time owning binding was
    /// a `Table` (or unresolved). Refs that pointed at a synthetic
    /// relation (`Cte` / `DerivedTable` / `TableFunction`) are dropped
    /// — synthetics aren't storage, so they don't belong in the public
    /// reads surface.
    pub(crate) fn real_column_refs(&self) -> Vec<RawColumnRef> {
        self.column_refs
            .iter()
            .filter(|raw| !raw.synthetic)
            .cloned()
            .collect()
    }

    /// Collapse every lineage edge so its source resolves to a real
    /// (non-synthetic) reference. References whose walk-time owner is
    /// a Cte / DerivedTable with `Some` `output_columns` are replaced
    /// by walking that body's matching `OutputColumn` and emitting one
    /// edge per inner source ref — recursively, until the chain
    /// bottoms out at a real table or an unresolvable ref. The outer
    /// edge's `kind` is combined with each body column's kind via
    /// [`collapse_lineage_kinds`] (Passthrough is preserved only when
    /// both sides are Passthrough; any transforming step yields
    /// Transformation). Bounded by [`MAX_COLLAPSE_DEPTH`] as a cycle
    /// guard.
    pub(crate) fn collapsed_lineage_edges(&self) -> Vec<LineageEdge> {
        self.lineage_edges
            .iter()
            .flat_map(|edge| {
                self.collapse_source(&edge.source, edge.kind, 0)
                    .into_iter()
                    .map(|(source, kind)| LineageEdge {
                        source,
                        target: edge.target.clone(),
                        kind,
                    })
            })
            .collect()
    }

    fn collapse_source(
        &self,
        raw: &RawColumnRef,
        outer_kind: ColumnLineageKind,
        depth: usize,
    ) -> Vec<(RawColumnRef, ColumnLineageKind)> {
        if depth >= MAX_COLLAPSE_DEPTH {
            return vec![(raw.clone(), outer_kind)];
        }
        let output = match self.synthetic_owning_binding(raw) {
            Some(
                Binding::Cte {
                    output_columns: Some(o),
                    ..
                }
                | Binding::DerivedTable {
                    output_columns: Some(o),
                    ..
                },
            ) => o,
            _ => return vec![(raw.clone(), outer_kind)],
        };
        let Some(col_name) = raw.parts.last() else {
            return vec![(raw.clone(), outer_kind)];
        };
        let key = BindingKey::from_ident(col_name);
        let mut result = Vec::new();
        for operand in &output.set_operands {
            for column in &operand.columns {
                let matches = column
                    .name
                    .as_ref()
                    .is_some_and(|n| BindingKey::from_ident(n) == key);
                if !matches {
                    continue;
                }
                let collapsed = collapse_lineage_kinds(outer_kind, column.kind);
                for source in &column.source_refs {
                    result.extend(self.collapse_source(source, collapsed, depth + 1));
                }
            }
        }
        if result.is_empty() {
            vec![(raw.clone(), outer_kind)]
        } else {
            result
        }
    }

    /// Collapse [`RawTableRef`]s into the real-table lineage source
    /// list: for each top-level use, emit the real table directly, or
    /// recurse into the synthetic's `body_scope` subtree to gather the
    /// real tables underneath. Uses whose scope has a `Predicate`
    /// ancestor (WHERE / JOIN ON / etc.) are filtered out — they're
    /// filter position, not data-feeding.
    ///
    /// Occurrence-based: a statement using the same source more than
    /// once (`FROM s AS x JOIN s AS y`, repeated `FROM cte` across
    /// UNION operands) emits one entry per use. Consumers wanting set
    /// semantics dedup via `HashSet::from_iter`. Matches
    /// [`Resolution::collapsed_lineage_edges`] (column-level) on
    /// multiplicity.
    ///
    /// Cycle-safe: each visited synthetic `body_scope` is recorded so
    /// recursive CTE self-references terminate after one pass.
    pub(crate) fn collapsed_feeding_table_sources(&self) -> Vec<TableReference> {
        // Body scopes of every synthetic (CTE / true derived). The top
        // loop skips uses inside these subtrees — those uses are only
        // reachable via the synthetic's own `Synthetic` use, never
        // standalone. Without this, a `FROM s` inside `WITH cte AS
        // (... FROM s) SELECT ... FROM cte` would be picked up twice:
        // once as a top-level Real use and once via the CTE recursion.
        let synthetic_body_scopes: HashSet<ScopeId> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.values())
            .filter_map(|binding| match binding {
                Binding::Cte { body_scope, .. } => Some(*body_scope),
                Binding::DerivedTable {
                    body_scope: Some(scope),
                    ..
                } => Some(*scope),
                _ => None,
            })
            .collect();
        let mut out = Vec::new();
        let mut visited = HashSet::new();
        for raw in &self.table_refs {
            // Both filters reduce to "does any ancestor of raw.scope_id
            // satisfy a predicate?" — fold them into a single parent
            // walk with O(1) HashSet lookup per step, instead of one
            // walk for the predicate check and one walk per synthetic
            // body scope.
            let skip = parent_chain(&self.scopes, raw.scope_id).any(|id| {
                self.scopes[id.0].kind == ScopeKind::Predicate
                    || synthetic_body_scopes.contains(&id)
            });
            if skip {
                continue;
            }
            self.collect_use(raw, &mut out, &mut visited);
        }
        out
    }

    fn collect_use(
        &self,
        raw: &RawTableRef,
        out: &mut Vec<TableReference>,
        visited: &mut HashSet<ScopeId>,
    ) {
        match &raw.target {
            TableRefTarget::Real(table) => out.push(table.clone()),
            TableRefTarget::Synthetic { body_scope } => {
                if !visited.insert(*body_scope) {
                    // Recursive CTE self-reference — terminate the
                    // chain. The first pass through the body has
                    // already collected its real tables.
                    return;
                }
                for nested in &self.table_refs {
                    if !self.is_in_scope_subtree(nested.scope_id, *body_scope) {
                        continue;
                    }
                    if self.has_predicate_ancestor(nested.scope_id) {
                        continue;
                    }
                    self.collect_use(nested, out, visited);
                }
            }
        }
    }

    /// Walk parent chain from `scope_id`; return true iff `ancestor` is
    /// reached. Inclusive (a scope is its own subtree's root).
    fn is_in_scope_subtree(&self, scope_id: ScopeId, ancestor: ScopeId) -> bool {
        parent_chain(&self.scopes, scope_id).any(|id| id == ancestor)
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
        parent_chain(&self.scopes, raw.scope_id).find_map(|id| {
            self.scopes[id.0]
                .iter_bindings()
                .find(|b| binding_alias_key(b) == key)
        })
    }

    /// All tables touched by the statement, in scope-arena order. The
    /// union of [`Self::read_tables`] and [`Self::write_tables`] (with
    /// duplicates when a single table carries both roles).
    pub(crate) fn tables(&self) -> Vec<TableReference> {
        self.scopes
            .iter()
            .flat_map(|scope| scope.iter_bindings())
            .filter_map(|binding| match binding {
                Binding::Table { table, .. } => Some((**table).clone()),
                _ => None,
            })
            .collect()
    }

    /// Every table referenced as a Read source, in scope-arena order.
    /// Includes tables inside predicate subqueries (e.g. `x` in
    /// `WHERE id IN (SELECT id FROM x)`). Use
    /// [`Self::collapsed_feeding_table_sources`] for the stricter
    /// "feeds the enclosing write target" filter.
    pub(crate) fn read_tables(&self) -> Vec<TableReference> {
        self.collect_tables_by_role(TableRole::Read)
    }

    /// Every table referenced as a Write target, in scope-arena order.
    pub(crate) fn write_tables(&self) -> Vec<TableReference> {
        self.collect_tables_by_role(TableRole::Write)
    }

    fn collect_tables_by_role(&self, role: TableRole) -> Vec<TableReference> {
        self.scopes
            .iter()
            .flat_map(|scope| scope.iter_bindings())
            .filter_map(|binding| match binding {
                Binding::Table { table, roles, .. } if roles.contains(&role) => {
                    Some((**table).clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Walk parent chain from `scope_id`; return true iff any scope
    /// along the way carries `ScopeKind::Predicate`. Drives the
    /// filter-position exclusion in
    /// [`Self::collapsed_feeding_table_sources`].
    pub(super) fn has_predicate_ancestor(&self, scope_id: ScopeId) -> bool {
        parent_chain(&self.scopes, scope_id)
            .any(|id| self.scopes[id.0].kind == ScopeKind::Predicate)
    }
}

/// Combine two lineage kinds along one collapse step: the result is
/// `Passthrough` only when both sides are `Passthrough`; any
/// `Transformation` step makes the whole collapsed chain a
/// `Transformation`.
fn collapse_lineage_kinds(outer: ColumnLineageKind, inner: ColumnLineageKind) -> ColumnLineageKind {
    if outer == ColumnLineageKind::Passthrough && inner == ColumnLineageKind::Passthrough {
        ColumnLineageKind::Passthrough
    } else {
        ColumnLineageKind::Transformation
    }
}
