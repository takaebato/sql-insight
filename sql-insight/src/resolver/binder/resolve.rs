//! Column-name resolution: ranking candidate owners for a dotted reference
//! across the scope (qualified / unqualified), the catalog-aware
//! tiebreaker, and the case-folded identifier matching.

use super::*;

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
    pub(super) fn resolve(&self, parts: &[Ident], scope: &Scope) -> BoundColumn {
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
                    return BoundColumn {
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
        BoundColumn {
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
        let candidates: Vec<Binding> = if parts.len() == 1 {
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
    /// `Cataloged` schema must list it (confirmed witness); an `Unknown` table always
    /// could (suspect).
    fn unqualified_candidate(&self, rel: &Relation, name: &Ident) -> Option<Binding> {
        match rel {
            Relation::Table {
                table,
                columns: Columns::Cataloged(cols),
                ..
            } => self
                .list_has(cols, name)
                .then(|| base(table, ResolutionKind::Cataloged)),
            Relation::Table {
                table,
                columns: Columns::Unknown,
                ..
            } => Some(base(table, ResolutionKind::Inferred)),
            // A derived relation owns the column iff it exposes it (confirmed
            // witness, like a Cataloged table); the origin traversal collapses it.
            Relation::Derived { columns, .. } => {
                self.list_has(columns, name).then_some(Binding::Derived)
            }
            // A table function's columns are opaque — a bare name is not
            // claimed by it (stays resolvable against real tables).
            Relation::TableFunction { .. } => None,
        }
    }

    /// A relation is a qualified candidate iff the qualifier matches it: a
    /// non-aliased real table by right-anchored path, anything else by its
    /// single exposed (alias) name. A `Cataloged` table that doesn't list the
    /// column still resolves (`Inferred`) — the qualifier pins it.
    fn qualified_candidate(
        &self,
        rel: &Relation,
        qualifier_parts: &[Ident],
        qualifier_ref: Option<&TableReference>,
        name: &Ident,
    ) -> Option<Binding> {
        let qualifier_ok = match rel {
            Relation::Table {
                table, alias: None, ..
            } => qualifier_ref.is_some_and(|q| self.qualifier_matches_table(q, table)),
            _ => rel.exposed_name().is_some_and(|exposed| {
                matches!(qualifier_parts, [only] if self.eq(self.casing.table_alias, only, exposed))
            }),
        };
        if !qualifier_ok {
            return None;
        }
        match rel {
            Relation::Table {
                table,
                columns: Columns::Cataloged(cols),
                ..
            } => {
                // A listed column is a `Cataloged` witness; the qualifier still
                // pins an unlisted one, surfaced `Inferred` (not a witness).
                let resolution = if self.list_has(cols, name) {
                    ResolutionKind::Cataloged
                } else {
                    ResolutionKind::Inferred
                };
                Some(base(table, resolution))
            }
            Relation::Table {
                table,
                columns: Columns::Unknown,
                ..
            } => Some(base(table, ResolutionKind::Inferred)),
            Relation::Derived { columns, .. } => {
                self.list_has(columns, name).then_some(Binding::Derived)
            }
            // A ref qualified by a table function's alias resolves to it: a
            // `Derived` binding the traversal turns into the synthetic
            // `alias.col` lineage source (dropped from reads).
            Relation::TableFunction { .. } => Some(Binding::Derived),
        }
    }

    /// Collapse candidate bindings to one: none → `Unresolved`; one → it
    /// verbatim; several with exactly one confirmed witness → that witness,
    /// downgraded to `Inferred` (`Cataloged`-witness-over-`Unknown`); otherwise
    /// `Ambiguous`.
    fn pick(&self, candidates: Vec<Binding>) -> Binding {
        match candidates.len() {
            0 => Binding::Unresolved,
            1 => candidates.into_iter().next().unwrap(),
            _ => {
                let mut witnesses = candidates.into_iter().filter(is_confirmed);
                match (witnesses.next(), witnesses.next()) {
                    (Some(witness), None) => downgrade(witness),
                    _ => Binding::Ambiguous,
                }
            }
        }
    }

    /// Match a written table reference against the catalog (after default-fill,
    /// right-anchored, dialect-cased). Unique hit → canonical identity + its
    /// column list + `Cataloged`; several → written ref + `Ambiguous`; no catalog
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

/// A confirmed witness for the multi-candidate tiebreaker: a catalog-listed
/// real column (`Cataloged`) or a derived / CTE column the producing query
/// exposes — as opposed to an `Unknown`-table suspect (`Inferred`). Candidate
/// bindings are only ever `Base` or `Derived` (`Unresolved` / `Ambiguous` are
/// `pick` outcomes, not inputs), so this distinguishes every case.
fn is_confirmed(binding: &Binding) -> bool {
    matches!(
        binding,
        Binding::Derived
            | Binding::Base {
                resolution: ResolutionKind::Cataloged,
                ..
            }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `Binder` over a borrowed diagnostics sink — only its `casing`
    /// matters here (`qualifier_matches_table` ignores catalog / ctes / outer).
    fn binder<'a>(
        diagnostics: &'a RefCell<Vec<ColumnLevelDiagnostic>>,
        casing: IdentifierCasing,
    ) -> Binder<'a> {
        Binder {
            catalog: None,
            casing,
            ctes: Vec::new(),
            outer: Vec::new(),
            diagnostics,
        }
    }

    /// Build a table reference from optional `catalog` / `schema` segments.
    fn tref(catalog: Option<&str>, schema: Option<&str>, name: &str) -> TableReference {
        TableReference {
            catalog: catalog.map(Ident::new),
            schema: schema.map(Ident::new),
            name: Ident::new(name),
        }
    }

    /// Right-anchored qualifier matching: a `qualifier` matches a `table` when
    /// the `name` segments are equal and each of `schema` / `catalog` is equal
    /// **or absent on either side** (an omitted segment is a wildcard). Folding
    /// is the dialect's table case — here `Lower` over lowercase idents, so a
    /// no-op; the quoting / casing matrix lives in [`crate::casing`].
    ///
    /// | qualifier          | table              | matches |
    /// |--------------------|--------------------|---------|
    /// | `users`            | `users`            | ✓       |
    /// | `users`            | `public.users`     | ✓       |
    /// | `users`            | `db.public.users`  | ✓       |
    /// | `public.users`     | `public.users`     | ✓       |
    /// | `public.users`     | `users`            | ✓       |
    /// | `public.users`     | `other.users`      | ✗       |
    /// | `db.public.users`  | `db.public.users`  | ✓       |
    /// | `db.public.users`  | `public.users`     | ✓       |
    /// | `db1.public.users` | `db2.public.users` | ✗       |
    /// | `orders`           | `users`            | ✗       |
    /// | `public.orders`    | `public.users`     | ✗       |
    #[test]
    fn right_anchored_qualifier_matrix() {
        let diagnostics = RefCell::new(Vec::new());
        let binder = binder(&diagnostics, IdentifierCasing::default());

        // (qualifier, table, expected)
        let cases: &[(TableReference, TableReference, bool)] = &[
            (tref(None, None, "users"), tref(None, None, "users"), true),
            (
                tref(None, None, "users"),
                tref(None, Some("public"), "users"),
                true,
            ),
            (
                tref(None, None, "users"),
                tref(Some("db"), Some("public"), "users"),
                true,
            ),
            (
                tref(None, Some("public"), "users"),
                tref(None, Some("public"), "users"),
                true,
            ),
            // over-qualified ref vs barer table: the table's absent schema is a wildcard.
            (
                tref(None, Some("public"), "users"),
                tref(None, None, "users"),
                true,
            ),
            // both schemas present and differ: contradiction.
            (
                tref(None, Some("public"), "users"),
                tref(None, Some("other"), "users"),
                false,
            ),
            (
                tref(Some("db"), Some("public"), "users"),
                tref(Some("db"), Some("public"), "users"),
                true,
            ),
            (
                tref(Some("db"), Some("public"), "users"),
                tref(None, Some("public"), "users"),
                true,
            ),
            // catalogs both present and differ: contradiction.
            (
                tref(Some("db1"), Some("public"), "users"),
                tref(Some("db2"), Some("public"), "users"),
                false,
            ),
            // name differs: never matches, regardless of qualifier.
            (tref(None, None, "orders"), tref(None, None, "users"), false),
            (
                tref(None, Some("public"), "orders"),
                tref(None, Some("public"), "users"),
                false,
            ),
        ];

        for (qualifier, table, expected) in cases {
            assert_eq!(
                binder.qualifier_matches_table(qualifier, table),
                *expected,
                "{qualifier:?} vs {table:?}"
            );
        }
    }

    /// The match folds each segment by the dialect's *table* case, so an
    /// unquoted `Users` qualifier matches a `users` table under `Upper` /
    /// `Lower`, but not under `Sensitive` (the false-merge-avoiding fold).
    #[test]
    fn matching_applies_the_table_fold() {
        let qualifier = tref(None, None, "Users");
        let table = tref(None, None, "users");
        for (fold, expected) in [
            (CaseFold::Upper, true),
            (CaseFold::Lower, true),
            (CaseFold::Insensitive, true),
            (CaseFold::Sensitive, false),
        ] {
            let diagnostics = RefCell::new(Vec::new());
            // Only the `table` fold matters for qualifier matching.
            let casing = IdentifierCasing {
                table: fold,
                ..IdentifierCasing::default()
            };
            let binder = binder(&diagnostics, casing);
            assert_eq!(
                binder.qualifier_matches_table(&qualifier, &table),
                expected,
                "fold {fold:?}"
            );
        }
    }
}
