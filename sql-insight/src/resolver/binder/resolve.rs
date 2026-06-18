//! Column-name resolution: ranking candidate owners for a dotted reference
//! across the scope (qualified / unqualified), the catalog-aware
//! tiebreaker, and the case-folded identifier matching.

use super::*;

/// One candidate owner of a column reference, with the binding it would
/// contribute and whether it's a confirmed (catalog-listed) witness.
struct Candidate {
    binding: Binding,
    confirmed: bool,
}

/// The outcome of matching a written table reference against the catalog.
pub(super) struct TableMatch {
    pub(super) table: TableReference,
    pub(super) resolution: ResolutionKind,
    pub(super) columns: Vec<Ident>,
}

impl<'a> Binder<'a> {
    // ===== column resolution =============================================

    /// Resolve a dotted reference (`parts`) against the scope. Unqualified
    /// ranks every relation; qualified matches the qualifier first. Collapsed
    /// by [`pick`](Self::pick).
    pub(super) fn resolve(&self, parts: &[Ident], scope: &Scope) -> ColRef {
        let name = parts.last().expect("a reference has at least one segment");
        // Clause-alias visibility (GROUP BY / HAVING / ORDER BY): a bare ref
        // naming an *introduced* output alias resolves to that output
        // (`Derived`, dropped from reads — the real dependency is at the
        // projection). An *identity* passthrough falls through to the real
        // column. (Empty `query_outputs` at FROM-level, so this is a no-op there.)
        if parts.len() == 1 {
            if let Some(out) = scope.query_outputs.iter().find(|o| {
                o.name
                    .as_ref()
                    .is_some_and(|n| self.eq(self.casing.column, n, name))
            }) {
                if !out.identity {
                    return ColRef {
                        qualifier: None,
                        name: name.clone(),
                        binding: Binding::Derived,
                    };
                }
            }
        }
        // Resolve against the current scope, then fall through the enclosing
        // scopes (correlation), innermost first, before giving up.
        let binding = self
            .resolve_in(parts, &scope.relations)
            .or_else(|| {
                self.outer
                    .iter()
                    .rev()
                    .find_map(|relations| self.resolve_in(parts, relations))
            })
            .unwrap_or(Binding::Unresolved);
        ColRef {
            qualifier: (parts.len() >= 2).then(|| parts[parts.len() - 2].clone()),
            name: name.clone(),
            binding,
        }
    }

    /// Resolve `parts` against one set of relations, returning `None` when none
    /// could own the column (so the caller falls through to an enclosing
    /// scope) and `Some(binding)` once at least one is a candidate (even if
    /// that collapses to `Ambiguous` — a name owned here doesn't escape).
    fn resolve_in(&self, parts: &[Ident], relations: &[Relation]) -> Option<Binding> {
        let name = parts.last()?;
        let candidates: Vec<Candidate> = if parts.len() == 1 {
            relations
                .iter()
                .filter_map(|rel| self.unqualified_candidate(rel, name))
                .collect()
        } else {
            let qualifier_parts = &parts[..parts.len() - 1];
            let qualifier_ref = TableReference::try_from_parts(qualifier_parts);
            relations
                .iter()
                .filter_map(|rel| {
                    self.qualified_candidate(rel, qualifier_parts, qualifier_ref.as_ref(), name)
                })
                .collect()
        };
        (!candidates.is_empty()).then(|| self.pick(candidates))
    }

    /// A relation is an unqualified candidate iff it could own `name`: a
    /// `Known` schema must list it (confirmed witness); an `Open` table always
    /// could (suspect).
    fn unqualified_candidate(&self, rel: &Relation, name: &Ident) -> Option<Candidate> {
        match &rel.source {
            RelSource::Table {
                table,
                columns: Columns::Known(cols),
                ..
            } => self.list_has(cols, name).then(|| Candidate {
                binding: base(table, ResolutionKind::Cataloged),
                confirmed: true,
            }),
            RelSource::Table {
                table,
                columns: Columns::Open,
                ..
            } => Some(Candidate {
                binding: base(table, ResolutionKind::Inferred),
                confirmed: false,
            }),
            // A derived relation owns the column iff it exposes it (confirmed
            // witness, like a Known table); the origin traversal collapses it.
            RelSource::Derived { columns } => self.list_has(columns, name).then_some(Candidate {
                binding: Binding::Derived,
                confirmed: true,
            }),
            // A table function's columns are opaque — a bare name is not
            // claimed by it (stays resolvable against real tables).
            RelSource::TableFunction => None,
        }
    }

    /// A relation is a qualified candidate iff the qualifier matches it: a
    /// non-aliased real table by right-anchored path, anything else by its
    /// single exposed (alias) name. A `Known` table that doesn't list the
    /// column still resolves (`Inferred`) — the qualifier pins it.
    fn qualified_candidate(
        &self,
        rel: &Relation,
        qualifier_parts: &[Ident],
        qualifier_ref: Option<&TableReference>,
        name: &Ident,
    ) -> Option<Candidate> {
        let qualifier_ok = match &rel.source {
            RelSource::Table { table, .. } if rel.alias.is_none() => {
                qualifier_ref.is_some_and(|q| self.qualifier_matches_table(q, table))
            }
            _ => rel.exposed_name().is_some_and(|exposed| {
                matches!(qualifier_parts, [only] if self.eq(self.casing.table_alias, only, exposed))
            }),
        };
        if !qualifier_ok {
            return None;
        }
        match &rel.source {
            RelSource::Table {
                table,
                columns: Columns::Known(cols),
                ..
            } => {
                let confirmed = self.list_has(cols, name);
                let resolution = if confirmed {
                    ResolutionKind::Cataloged
                } else {
                    ResolutionKind::Inferred
                };
                Some(Candidate {
                    binding: base(table, resolution),
                    confirmed,
                })
            }
            RelSource::Table {
                table,
                columns: Columns::Open,
                ..
            } => Some(Candidate {
                binding: base(table, ResolutionKind::Inferred),
                confirmed: false,
            }),
            RelSource::Derived { columns } => self.list_has(columns, name).then_some(Candidate {
                binding: Binding::Derived,
                confirmed: true,
            }),
            // A ref qualified by a table function's alias resolves to it: a
            // `Derived` binding the traversal turns into the synthetic
            // `alias.col` lineage source (dropped from reads).
            RelSource::TableFunction => Some(Candidate {
                binding: Binding::Derived,
                confirmed: true,
            }),
        }
    }

    /// Collapse candidates to a [`Binding`]: none → `Unresolved`; one → its
    /// binding verbatim; several with exactly one confirmed witness → that
    /// witness, downgraded to `Inferred` (Known-witness-over-Open); otherwise
    /// `Ambiguous`.
    fn pick(&self, candidates: Vec<Candidate>) -> Binding {
        match candidates.len() {
            0 => Binding::Unresolved,
            1 => candidates.into_iter().next().unwrap().binding,
            _ => {
                let mut confirmed = candidates.into_iter().filter(|c| c.confirmed);
                match (confirmed.next(), confirmed.next()) {
                    (Some(witness), None) => downgrade(witness.binding),
                    _ => Binding::Ambiguous,
                }
            }
        }
    }

    /// Match a written table reference against the catalog (after default-fill,
    /// right-anchored, dialect-cased). Unique hit → canonical identity + Known
    /// columns + `Cataloged`; several → written ref + `Ambiguous`; no catalog
    /// or no hit → written ref + `Inferred`.
    pub(super) fn table_match(&self, written: &TableReference) -> TableMatch {
        let no_hit = |resolution| TableMatch {
            table: written.clone(),
            resolution,
            columns: Vec::new(),
        };
        let Some(catalog) = self.catalog else {
            return no_hit(ResolutionKind::Inferred);
        };
        let filled = fill_query_defaults(written, catalog);
        let fold = self.casing.table;
        let mut hits = catalog
            .tables()
            .iter()
            .filter(|t| catalog_table_matches(&filled, t, fold));
        let Some(first) = hits.next() else {
            return no_hit(ResolutionKind::Inferred);
        };
        if hits.next().is_some() {
            return no_hit(ResolutionKind::Ambiguous);
        }
        let columns = first
            .column_names()
            .iter()
            .map(|c| Ident::with_quote('"', c))
            .collect();
        TableMatch {
            table: canonical_ref(first),
            resolution: ResolutionKind::Cataloged,
            columns,
        }
    }

    /// Right-anchored match of a decoded qualifier against a real table's
    /// `catalog.schema.name`, under the dialect's table casing (an omitted
    /// qualifier segment is a wildcard).
    pub(super) fn qualifier_matches_table(
        &self,
        qualifier: &TableReference,
        table: &TableReference,
    ) -> bool {
        let fold = self.casing.table;
        let opt_eq = |a: Option<&Ident>, b: Option<&Ident>| match (a, b) {
            (Some(x), Some(y)) => fold.normalize(x) == fold.normalize(y),
            _ => true,
        };
        fold.normalize(&qualifier.name) == fold.normalize(&table.name)
            && opt_eq(qualifier.schema.as_ref(), table.schema.as_ref())
            && opt_eq(qualifier.catalog.as_ref(), table.catalog.as_ref())
    }

    pub(super) fn list_has(&self, columns: &[Ident], name: &Ident) -> bool {
        columns.iter().any(|c| self.eq(self.casing.column, c, name))
    }

    /// The target table's catalog column names (unquoted), for filling in a
    /// column-less `INSERT` / `MERGE … INSERT`. Empty without a unique catalog
    /// hit. (The matched columns are quote-wrapped for case-folded resolution;
    /// the written column list takes the plain identifier.)
    pub(super) fn catalog_columns(&self, target: &TableReference) -> Vec<Ident> {
        self.table_match(target)
            .columns
            .iter()
            .map(|c| Ident::new(&c.value))
            .collect()
    }

    pub(super) fn eq(&self, fold: CaseFold, a: &Ident, b: &Ident) -> bool {
        fold.normalize(a) == fold.normalize(b)
    }
}
