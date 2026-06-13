//! `CapturedColumnRef` — column references captured during the walk —
//! plus the walk-time resolution that fills its `resolved` /
//! `synthetic` / `confidence` fields.

use sqlparser::ast::Ident;

use crate::reference::{Confidence, TableReference};

use super::binding::{
    binding_confirms_column, binding_could_contain_column, is_synthetic_binding,
    qualifier_matches_table, synthetic_table_ref, BindingKey,
};
use super::scope::parent_chain;
use super::{Binding, CaseFold, IdentifierCasing, Resolver, ScopeId};

/// A column reference captured by the resolver during the AST walk.
///
/// `parts` mirrors `sqlparser`'s split — 1 part for bare `a`, 2 for
/// `t1.a`, 3 for `schema.t1.a`, 4 for `catalog.schema.t1.a`.
/// `scope_id` is the scope in which the reference appeared (kept for
/// diagnostics and for binding lookups at collapse time).
///
/// `resolved`, `synthetic`, `is_lineage_source`, and `confidence` are
/// computed at record time, when walk state still reflects what was
/// visible to the SQL author at that point — necessary for multi-CTE
/// chains where later CTE bindings would otherwise ambify earlier
/// resolutions, and for recording the lexical role of the reference
/// (value vs predicate) before the walker leaves the surrounding
/// clause.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CapturedColumnRef {
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
    /// True (the default) iff the reference appeared in a value
    /// position and its value flows out; `false` for refs captured
    /// in a predicate context (WHERE / HAVING / JOIN ON / EXISTS /
    /// CASE WHEN cond / aggregate FILTER / etc.). Set from
    /// [`super::Context::is_lineage_source`] at capture time. Refs
    /// with `is_lineage_source = false` are dropped from lineage
    /// edges; they still surface in `reads`.
    pub(crate) is_lineage_source: bool,
    /// Resolver confidence in the placement: a `Known` schema
    /// confirms the column ([`Confidence::Confirmed`]); the resolver
    /// adopted a candidate without firm evidence
    /// ([`Confidence::Inferred`]); multiple candidates with no
    /// tiebreaker ([`Confidence::Ambiguous`]); no candidate at all
    /// ([`Confidence::Unresolved`]). Synthetic refs (CTE / derived)
    /// can be `Confirmed` here but get filtered from the public
    /// surface, so consumers see `Confirmed` only when a catalog
    /// positively confirms the column on a real table.
    pub(crate) confidence: Confidence,
}

/// A binding that could plausibly own the unqualified column being
/// resolved, captured during one scope's scan. `confirmed` is `true`
/// only for `Known`-schema bindings that explicitly list the column —
/// drives both the "single Known witness" tiebreaker and the
/// `Confirmed` vs `Inferred` distinction in
/// [`Resolver::resolve_unqualified_ref`].
struct OwnerCandidate {
    table: TableReference,
    confirmed: bool,
    synthetic: bool,
}

impl<'a> Resolver<'a> {
    /// Record a column reference observed in the current scope.
    /// Resolution (owning table, synthetic-vs-real, confidence) is
    /// computed right now, while scope state is authoritative —
    /// later CTE bindings won't ambify what this reference saw.
    pub(super) fn capture_column_ref(&mut self, parts: Vec<Ident>) {
        let scope_id = self.current_scope_id();
        let is_lineage_source = self.context.is_lineage_source;
        let (resolved, synthetic, confidence) = self.resolve_ref(&parts, scope_id);
        self.resolution.column_refs.push(CapturedColumnRef {
            parts,
            scope_id,
            resolved,
            synthetic,
            is_lineage_source,
            confidence,
        });
    }

    fn resolve_ref(
        &self,
        parts: &[Ident],
        scope_id: ScopeId,
    ) -> (Option<TableReference>, bool, Confidence) {
        match parts {
            [] => (None, false, Confidence::Unresolved),
            [name] => self.resolve_unqualified_ref(name, scope_id),
            _ => {
                let (qualifier, [column]) = parts.split_at(parts.len() - 1) else {
                    unreachable!("len >= 2 splits cleanly into qualifier + 1-element column tail")
                };
                self.resolve_qualified_ref(qualifier, column, scope_id)
            }
        }
    }

    /// Walk the scope chain for an unqualified column reference and
    /// return the `(table, synthetic, confidence)` triple that
    /// [`Self::capture_column_ref`] stores on the captured ref. Pure:
    /// no walker state mutated.
    ///
    /// Resolution rule (lexical shadowing, innermost-out): the first
    /// scope with at least one candidate binding wins. Within that
    /// scope:
    /// - Exactly one candidate → owner is that binding. Confidence is
    ///   [`Confidence::Confirmed`] if a `Known` schema confirmed the
    ///   column, else [`Confidence::Inferred`].
    /// - Multiple candidates with exactly one `Known` confirmation
    ///   (Known-witness-over-`Unknown`-suspects): the `Known` winner
    ///   is adopted as owner with [`Confidence::Inferred`] — the
    ///   leftover `Unknown` suspects could in principle also contain
    ///   the column, so the placement is strong but not confirmed.
    /// - Multiple candidates with zero or 2+ `Known` confirmations →
    ///   `table: None` / [`Confidence::Ambiguous`].
    ///
    /// No matching scope anywhere in the chain →
    /// `table: None` / [`Confidence::Unresolved`].
    fn resolve_unqualified_ref(
        &self,
        name: &Ident,
        scope_id: ScopeId,
    ) -> (Option<TableReference>, bool, Confidence) {
        let column_fold = self.resolution.casing.column;
        for id in parent_chain(&self.resolution.scopes, scope_id) {
            let scope = &self.resolution.scopes[id.0];
            let candidates: Vec<OwnerCandidate> = scope
                .iter_bindings()
                .filter_map(|b| {
                    let table = binding_could_contain_column(b, name, column_fold)?;
                    Some(OwnerCandidate {
                        table,
                        confirmed: binding_confirms_column(b, name, column_fold),
                        synthetic: is_synthetic_binding(b),
                    })
                })
                .collect();
            match candidates.as_slice() {
                [] => continue,
                [c] => {
                    let confidence = if c.confirmed {
                        Confidence::Confirmed
                    } else {
                        Confidence::Inferred
                    };
                    return (Some(c.table.clone()), c.synthetic, confidence);
                }
                _ => {
                    let confirmed_count = candidates.iter().filter(|c| c.confirmed).count();
                    return if confirmed_count == 1 {
                        // Known witness wins over Unknown suspects: take
                        // the single confirmed binding, but flag the
                        // placement as Inferred rather than Confirmed
                        // since the leftover suspects could in principle
                        // also contain the column.
                        let winner = candidates
                            .into_iter()
                            .find(|c| c.confirmed)
                            .expect("confirmed_count == 1");
                        (Some(winner.table), winner.synthetic, Confidence::Inferred)
                    } else {
                        (None, false, Confidence::Ambiguous)
                    };
                }
            }
        }
        (None, false, Confidence::Unresolved)
    }

    /// Resolve a qualified column reference (`t.col`, `s.t.col`,
    /// `c.s.t.col`) against the scope chain. Mirrors
    /// [`Self::resolve_unqualified_ref`]: walk innermost-out, the first
    /// scope with at least one matching binding wins, branch on the
    /// candidate count.
    ///
    /// A binding matches per ANSI qualifier rules (see
    /// [`qualified_candidate`]): non-aliased real tables match by
    /// right-anchored path ([`qualifier_matches_table`]); aliased and
    /// synthetic bindings expose only their single name and so match
    /// only a single-segment qualifier equal to it.
    ///
    /// - 0 candidates → the qualifier names nothing in scope (a table
    ///   not in FROM, a contradicting schema, or an alias-hidden
    ///   original name): `table: None` / [`Confidence::Unresolved`].
    /// - 1 candidate → resolved. [`Confidence::Confirmed`] if a `Known`
    ///   schema lists the column, else [`Confidence::Inferred`].
    /// - 2+ candidates → [`Confidence::Ambiguous`] (`table: None`).
    fn resolve_qualified_ref(
        &self,
        qualifier_parts: &[Ident],
        column_name: &Ident,
        scope_id: ScopeId,
    ) -> (Option<TableReference>, bool, Confidence) {
        // Decode the qualifier into a `TableReference` for right-anchored
        // matching against real-table bindings. `None` when the qualifier
        // overshoots the catalog.schema.name depth (5-part refs): no real
        // table can match, leaving only single-segment alias / synthetic
        // bindings matchable.
        let qualifier_ref = TableReference::try_from_parts(qualifier_parts);
        let casing = self.resolution.casing;
        for id in parent_chain(&self.resolution.scopes, scope_id) {
            let scope = &self.resolution.scopes[id.0];
            let candidates: Vec<OwnerCandidate> = scope
                .iter_bindings()
                .filter_map(|b| {
                    qualified_candidate(
                        b,
                        qualifier_parts,
                        qualifier_ref.as_ref(),
                        column_name,
                        casing,
                    )
                })
                .collect();
            match candidates.as_slice() {
                [] => continue,
                [c] => {
                    let confidence = if c.confirmed {
                        Confidence::Confirmed
                    } else {
                        Confidence::Inferred
                    };
                    return (Some(c.table.clone()), c.synthetic, confidence);
                }
                _ => return (None, false, Confidence::Ambiguous),
            }
        }
        (None, false, Confidence::Unresolved)
    }
}

/// Decide whether `binding` is a candidate owner of a qualified
/// reference and, if so, what table / confidence it contributes.
///
/// - Non-aliased real table: right-anchored path match via
///   [`qualifier_matches_table`]; resolves to the alias-free table.
/// - Aliased real table: the alias hides the original name
///   (ANSI / Postgres / MySQL), so it matches only a single-segment
///   qualifier equal to the alias; resolves to the alias-free table.
/// - Synthetic (CTE / derived / table function): exposes only its
///   single name; resolves to a name-only synthetic ref so lineage
///   collapse can re-find the owning binding by name.
fn qualified_candidate(
    binding: &Binding,
    qualifier_parts: &[Ident],
    qualifier_ref: Option<&TableReference>,
    column_name: &Ident,
    casing: IdentifierCasing,
) -> Option<OwnerCandidate> {
    let (table, synthetic) = match binding {
        Binding::Table {
            table, alias: None, ..
        } => {
            let q = qualifier_ref?;
            qualifier_matches_table(q, table, casing.table).then(|| ((**table).clone(), false))?
        }
        Binding::Table {
            table,
            alias: Some(alias),
            ..
        } => qualifier_is_single(qualifier_parts, alias, casing.table_alias)
            .then(|| ((**table).clone(), false))?,
        Binding::Cte { name, .. } => qualifier_is_single(qualifier_parts, name, casing.table_alias)
            .then(|| (synthetic_table_ref(name), true))?,
        Binding::DerivedTable { alias, .. } | Binding::TableFunction { alias, .. } => {
            qualifier_is_single(qualifier_parts, alias, casing.table_alias)
                .then(|| (synthetic_table_ref(alias), true))?
        }
    };
    Some(OwnerCandidate {
        table,
        confirmed: binding_confirms_column(binding, column_name, casing.column),
        synthetic,
    })
}

/// `true` iff `qualifier_parts` is a single segment whose
/// [`BindingKey`] (under the table-alias `fold`) equals `name`'s — the
/// only shape an alias / synthetic (single exposed name) can match.
fn qualifier_is_single(qualifier_parts: &[Ident], name: &Ident, fold: CaseFold) -> bool {
    matches!(
        qualifier_parts,
        [only] if BindingKey::new(only, fold) == BindingKey::new(name, fold)
    )
}
