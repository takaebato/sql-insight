//! `RawColumnRef` — column references captured during the walk —
//! plus the walk-time resolution that fills its `resolved` /
//! `synthetic` fields.

use sqlparser::ast::Ident;

use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::reference::TableReference;

use super::binding::{
    binding_alias_key, binding_confirms_column, binding_could_contain_column,
    binding_has_known_columns, is_synthetic_binding, normalize_span, span_suffix, BindingKey,
};
use super::{Binding, Resolver, ScopeId};

/// A column reference captured by the resolver during the AST walk.
///
/// `parts` mirrors `sqlparser`'s split — 1 part for bare `a`, 2 for
/// `t1.a`, 3 for `schema.t1.a`, 4 for `catalog.schema.t1.a`.
/// `scope_id` is the scope in which the reference appeared (kept for
/// diagnostics and for binding lookups at collapse time).
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
    /// filtering and lineage collapse. `false` when `resolved` is
    /// `None`.
    pub(crate) synthetic: bool,
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

impl<'a> Resolver<'a> {
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
        let scope_id = self.current_scope_id();
        let (resolved, synthetic) = self.resolve_ref_at_walk(&parts, scope_id);
        self.column_refs.push(RawColumnRef {
            parts,
            scope_id,
            resolved,
            synthetic,
        });
    }

    fn resolve_ref_at_walk(
        &mut self,
        parts: &[Ident],
        scope_id: ScopeId,
    ) -> (Option<TableReference>, bool) {
        match parts.len() {
            0 => (None, false),
            1 => self.resolve_unqualified_at_walk(&parts[0], scope_id),
            n => self.resolve_qualified_at_walk(&parts[..n - 1], scope_id),
        }
    }

    /// Walk the scope chain for an unqualified column reference. Emits
    /// `AmbiguousColumn` when two or more bindings with `Known` schemas
    /// confirm the column, and `UnresolvedColumn` when no in-scope
    /// binding contains it but at least one scope had a `Known` schema
    /// (catalog-aware mode). Both diagnostics are suppressed when every
    /// candidate / scope is `Unknown`, since `Unknown` schemas could
    /// hold anything and silence is the safer default without catalog
    /// enrichment.
    fn resolve_unqualified_at_walk(
        &mut self,
        name: &Ident,
        scope_id: ScopeId,
    ) -> (Option<TableReference>, bool) {
        let mut current = Some(scope_id);
        let mut had_known_schemas_anywhere = false;
        let mut resolved: Option<(TableReference, bool)> = None;
        // (candidate tables, confirmed-by-Known count)
        let mut ambiguity: Option<(Vec<TableReference>, usize)> = None;

        while let Some(id) = current {
            let scope = self.scope(id);
            if scope.iter_bindings().any(binding_has_known_columns) {
                had_known_schemas_anywhere = true;
            }
            let matches: Vec<(TableReference, bool, bool)> = scope
                .iter_bindings()
                .filter_map(|b| {
                    let tbl = binding_could_contain_column(b, name)?;
                    Some((
                        tbl,
                        binding_confirms_column(b, name),
                        is_synthetic_binding(b),
                    ))
                })
                .collect();
            if !matches.is_empty() {
                if matches.len() == 1 {
                    let (tbl, _, syn) = matches.into_iter().next().unwrap();
                    resolved = Some((tbl, syn));
                } else {
                    let confirmed = matches.iter().filter(|(_, c, _)| *c).count();
                    let candidates: Vec<TableReference> =
                        matches.into_iter().map(|(t, _, _)| t).collect();
                    ambiguity = Some((candidates, confirmed));
                }
                break;
            }
            current = scope.parent;
        }

        if let Some((tbl, syn)) = resolved {
            return (Some(tbl), syn);
        }
        if let Some((candidates, confirmed_count)) = ambiguity {
            if confirmed_count >= 2 {
                let span = normalize_span(name.span);
                let names: Vec<String> = candidates.iter().map(|t| t.name.value.clone()).collect();
                self.record_diagnostic(ColumnLevelDiagnostic {
                    kind: ColumnLevelDiagnosticKind::AmbiguousColumn,
                    message: format!(
                        "ambiguous column `{}`{} — matches in: [{}]",
                        name.value,
                        span_suffix(span),
                        names.join(", ")
                    ),
                    span,
                });
            }
            return (None, false);
        }
        if had_known_schemas_anywhere {
            let span = normalize_span(name.span);
            self.record_diagnostic(ColumnLevelDiagnostic {
                kind: ColumnLevelDiagnosticKind::UnresolvedColumn,
                message: format!(
                    "unresolved column `{}`{} — no in-scope relation with a known schema contains it",
                    name.value,
                    span_suffix(span),
                ),
                span,
            });
        }
        (None, false)
    }

    fn resolve_qualified_at_walk(
        &self,
        qualifier_parts: &[Ident],
        scope_id: ScopeId,
    ) -> (Option<TableReference>, bool) {
        // Look up the binding for the qualifier head in the scope chain.
        // Multi-segment qualifiers (s.t.col) match only on the head —
        // schema/catalog-qualified bound names are rare and we don't
        // currently bind their full path anyway.
        let binding = qualifier_parts
            .first()
            .and_then(|head| self.binding_for_qualifier(head, scope_id));
        let synthetic = binding.map(is_synthetic_binding).unwrap_or(false);
        // Canonicalize a single-segment qualifier bound to a real table
        // to that binding's alias-free underlying `TableReference`, so an
        // aliased ref (`u.a` over `FROM t1 AS u`) surfaces the real table
        // `t1` — matching how unqualified refs resolve. Synthetic bindings
        // (CTE / derived / table function) keep the qualifier verbatim so
        // lineage collapse can re-find the owning binding by name;
        // multi-segment qualifiers are already real identities and pass
        // through untouched.
        let table = match binding {
            Some(Binding::Table { table, .. }) if qualifier_parts.len() == 1 => {
                Some((**table).clone())
            }
            _ => table_from_qualifier_parts(qualifier_parts),
        };
        (table, synthetic)
    }

    fn binding_for_qualifier(&self, head: &Ident, scope_id: ScopeId) -> Option<&Binding> {
        let key = BindingKey::from_ident(head);
        let mut current = Some(scope_id);
        while let Some(id) = current {
            let scope = self.scope(id);
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
