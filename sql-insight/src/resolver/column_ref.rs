//! `RawColumnRef` — column references captured during the walk —
//! plus the walk-time resolution that fills its `resolved` /
//! `synthetic` / `kinds` fields.

use sqlparser::ast::Ident;

use crate::extractor::column_operation_extractor::ReadKind;
use crate::relation::TableReference;

use super::binding::{
    binding_alias_key, binding_could_contain_column, is_synthetic_binding, RelationKey,
};
use super::{Binding, RelationResolver, ScopeId};

/// A column reference captured by the resolver during the AST walk.
///
/// `parts` mirrors `sqlparser`'s split — 1 part for bare `a`, 2 for
/// `t1.a`, 3 for `schema.t1.a`, 4 for `catalog.schema.t1.a`.
/// `scope_id` is the scope in which the reference appeared (kept for
/// diagnostics and for binding lookups at composition time).
///
/// `resolved` and `synthetic` are computed at record time, when scope
/// state still reflects what was visible to the SQL author at that
/// point in the walk — necessary for multi-CTE chains where later
/// CTE bindings would otherwise ambify earlier resolutions.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RawColumnRef {
    pub(crate) parts: Vec<Ident>,
    pub(crate) scope_id: ScopeId,
    /// Owning table captured at walk time. `None` for ambiguous /
    /// no-candidate / unrecognized-qualifier-shape cases.
    pub(crate) resolved: Option<TableReference>,
    /// True iff the walk-time owning binding was synthetic
    /// (`Cte` / `DerivedTable` / `TableFunction`). Drives reads
    /// filtering and flow composition. `false` when `resolved` is
    /// `None`.
    pub(crate) synthetic: bool,
    /// SQL-clause role(s) this reference plays — captured from the
    /// resolver's `ctx.read_kind` at record time. Typically a single
    /// element; future multi-role cases (USING expansion etc.) may
    /// extend.
    pub(crate) kinds: Vec<ReadKind>,
}

/// Decode a qualified ref's leading parts (everything before the
/// column name) into a `TableReference`. 1 part = bare name, 2 =
/// schema.name, 3 = catalog.schema.name. Other lengths (0 / 4+)
/// return `None` — they're either accidentally invalid or
/// struct-field accesses on a fully qualified column, which we don't
/// model yet.
pub(super) fn table_from_qualifier_parts(parts: &[Ident]) -> Option<TableReference> {
    match parts.len() {
        1 => Some(TableReference {
            catalog: None,
            schema: None,
            name: parts[0].clone(),
        }),
        2 => Some(TableReference {
            catalog: None,
            schema: Some(parts[0].clone()),
            name: parts[1].clone(),
        }),
        3 => Some(TableReference {
            catalog: Some(parts[0].clone()),
            schema: Some(parts[1].clone()),
            name: parts[2].clone(),
        }),
        _ => None,
    }
}

impl<'a> RelationResolver<'a> {
    pub(super) fn column_refs_len(&self) -> usize {
        self.column_refs.len()
    }

    pub(super) fn column_refs_slice(&self, since: usize) -> &[RawColumnRef] {
        &self.column_refs[since..]
    }

    /// Record a column reference observed in the current scope.
    /// Resolution (owning table) and synthetic-vs-real classification
    /// are computed right now, while scope state is authoritative —
    /// later CTE bindings won't ambify what this reference saw.
    pub(super) fn record_column_ref(&mut self, parts: Vec<Ident>) {
        let scope_id = self.scopes_mut().current_scope_id();
        let (resolved, synthetic) = self.resolve_ref_at_walk(&parts, scope_id);
        let mut kinds = vec![self.ctx.read_kind];
        if self.ctx.in_case_condition {
            kinds.push(ReadKind::Conditional);
        }
        self.column_refs.push(RawColumnRef {
            parts,
            scope_id,
            resolved,
            synthetic,
            kinds,
        });
    }

    fn resolve_ref_at_walk(
        &self,
        parts: &[Ident],
        scope_id: ScopeId,
    ) -> (Option<TableReference>, bool) {
        match parts.len() {
            0 => (None, false),
            1 => self.resolve_unqualified_at_walk(&parts[0], scope_id),
            n => self.resolve_qualified_at_walk(&parts[..n - 1], scope_id),
        }
    }

    fn resolve_unqualified_at_walk(
        &self,
        name: &Ident,
        scope_id: ScopeId,
    ) -> (Option<TableReference>, bool) {
        let mut current = Some(scope_id);
        while let Some(id) = current {
            let scope = self.scopes().scope(id);
            let candidates: Vec<&Binding> = scope
                .iter_bindings()
                .filter(|b| binding_could_contain_column(b, name).is_some())
                .collect();
            if !candidates.is_empty() {
                if candidates.len() != 1 {
                    return (None, false);
                }
                let binding = candidates[0];
                let table = binding_could_contain_column(binding, name);
                return (table, is_synthetic_binding(binding));
            }
            current = scope.parent;
        }
        (None, false)
    }

    fn resolve_qualified_at_walk(
        &self,
        qualifier_parts: &[Ident],
        scope_id: ScopeId,
    ) -> (Option<TableReference>, bool) {
        let table = table_from_qualifier_parts(qualifier_parts);
        // Determine synthetic-ness by looking up the qualifier head
        // in the scope chain. Multi-segment qualifiers (s.t.col) match
        // only on the head — schema/catalog-qualified bound names are
        // rare and we don't currently bind their full path anyway.
        let synthetic = qualifier_parts
            .first()
            .map(|head| self.qualifier_is_synthetic_at_walk(head, scope_id))
            .unwrap_or(false);
        (table, synthetic)
    }

    fn qualifier_is_synthetic_at_walk(&self, qualifier: &Ident, scope_id: ScopeId) -> bool {
        let key = RelationKey::from_ident(qualifier);
        let mut current = Some(scope_id);
        while let Some(id) = current {
            let scope = self.scopes().scope(id);
            for binding in scope.iter_bindings() {
                if binding_alias_key(binding) == key {
                    return is_synthetic_binding(binding);
                }
            }
            current = scope.parent;
        }
        false
    }
}
