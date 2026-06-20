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
            table: canonical_ref(first, written),
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

    pub(super) fn eq(&self, fold: CaseRule, a: &Ident, b: &Ident) -> bool {
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

    /// A minimal `Binder` over a borrowed diagnostics sink (no CTEs / outer
    /// scopes — those don't affect the pure resolution helpers tested here).
    fn binder<'a>(
        diagnostics: &'a RefCell<Vec<ColumnLevelDiagnostic>>,
        catalog: Option<&'a Catalog>,
        casing: IdentifierCasing,
    ) -> Binder<'a> {
        Binder {
            catalog,
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
        let binder = binder(&diagnostics, None, IdentifierCasing::default());

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
            (CaseRule::Upper, true),
            (CaseRule::Lower, true),
            (CaseRule::Insensitive, true),
            (CaseRule::Sensitive, false),
        ] {
            let diagnostics = RefCell::new(Vec::new());
            // Only the `table` fold matters for qualifier matching.
            let casing = IdentifierCasing {
                table: fold,
                ..IdentifierCasing::default()
            };
            let binder = binder(&diagnostics, None, casing);
            assert_eq!(
                binder.qualifier_matches_table(&qualifier, &table),
                expected,
                "fold {fold:?}"
            );
        }
    }

    fn base_cataloged(name: &str) -> Binding {
        Binding::Base {
            table: tref(None, None, name),
            resolution: ResolutionKind::Cataloged,
        }
    }

    fn base_inferred(name: &str) -> Binding {
        Binding::Base {
            table: tref(None, None, name),
            resolution: ResolutionKind::Inferred,
        }
    }

    /// The multi-candidate tiebreaker `pick` applies after candidates are
    /// gathered: 0 → `Unresolved`; exactly 1 → that binding **verbatim** (a sole
    /// candidate keeps full confidence, *not* downgraded); 2+ → if exactly one
    /// is a confirmed witness (a `Cataloged` real column or a `Derived`
    /// exposure) that witness `downgrade`d to `Inferred` (a `Derived` witness is
    /// left as-is), otherwise `Ambiguous`.
    ///
    /// | candidates                           | pick                          |
    /// |--------------------------------------|-------------------------------|
    /// | (none)                               | `Unresolved`                  |
    /// | `[Base a Inferred]`                  | `Base a Inferred`             |
    /// | `[Base a Cataloged]`                 | `Base a Cataloged` (verbatim) |
    /// | `[Derived]`                          | `Derived`                     |
    /// | `[Base a Cataloged, Base b Inferred]`| `Base a Inferred` (downgraded)|
    /// | `[Base a Inferred, Base b Inferred]` | `Ambiguous` (no witness)      |
    /// | `[Base a Cataloged, Base b Cataloged]`| `Ambiguous` (two witnesses)  |
    /// | `[Derived, Base b Inferred]`         | `Derived` (the sole witness)  |
    /// | `[Derived, Base a Cataloged]`        | `Ambiguous` (two witnesses)   |
    #[test]
    fn candidate_tiebreaker() {
        let diagnostics = RefCell::new(Vec::new());
        let binder = binder(&diagnostics, None, IdentifierCasing::default());

        let cases: Vec<(Vec<Binding>, Binding)> = vec![
            (vec![], Binding::Unresolved),
            (vec![base_inferred("a")], base_inferred("a")),
            // A sole candidate keeps its `Cataloged` confidence — only ties downgrade.
            (vec![base_cataloged("a")], base_cataloged("a")),
            (vec![Binding::Derived], Binding::Derived),
            // One confirmed witness over a suspect → the witness, downgraded.
            (
                vec![base_cataloged("a"), base_inferred("b")],
                base_inferred("a"),
            ),
            // No witness among suspects → ambiguous.
            (
                vec![base_inferred("a"), base_inferred("b")],
                Binding::Ambiguous,
            ),
            // Two witnesses → ambiguous.
            (
                vec![base_cataloged("a"), base_cataloged("b")],
                Binding::Ambiguous,
            ),
            // A `Derived` is a witness too; sole witness over a suspect, left as-is.
            (vec![Binding::Derived, base_inferred("b")], Binding::Derived),
            // `Derived` + `Cataloged` are two witnesses → ambiguous.
            (
                vec![Binding::Derived, base_cataloged("a")],
                Binding::Ambiguous,
            ),
        ];

        for (candidates, expected) in cases {
            let debug = format!("{candidates:?}");
            assert_eq!(binder.pick(candidates), expected, "candidates {debug}");
        }
    }

    /// Catalog matching outcomes: a unique registration → `Cataloged` plus the
    /// registered canonical path (and a non-empty column list); several → the
    /// written ref + `Ambiguous`; none → the written ref + `Inferred`. Matching
    /// is right-anchored (an omitted query segment is a wildcard), so a bare
    /// name resolves when exactly one schema declares it.
    ///
    /// | written          | resolution   | table (canonical / kept) |
    /// |------------------|--------------|--------------------------|
    /// | `orders`         | `Cataloged`  | `public.orders`          |
    /// | `users`          | `Ambiguous`  | `users` (kept)           |
    /// | `public.users`   | `Cataloged`  | `public.users`           |
    /// | `sales.users`    | `Cataloged`  | `sales.users`            |
    /// | `other.users`    | `Inferred`   | `other.users` (kept)     |
    /// | `missing`        | `Inferred`   | `missing` (kept)         |
    #[test]
    fn table_match_resolution_and_canonicalization() {
        let catalog = Catalog::new()
            .table(CatalogTable::new("public", "users").columns(["id", "name"]))
            .table(CatalogTable::new("public", "orders").columns(["id"]))
            .table(CatalogTable::new("sales", "users").columns(["x"]));
        let diagnostics = RefCell::new(Vec::new());
        let binder = binder(&diagnostics, Some(&catalog), IdentifierCasing::default());

        // (written, expected resolution, expected canonical / kept table)
        let cases: &[(TableReference, ResolutionKind, TableReference)] = &[
            (
                tref(None, None, "orders"),
                ResolutionKind::Cataloged,
                tref(None, Some("public"), "orders"),
            ),
            (
                tref(None, None, "users"),
                ResolutionKind::Ambiguous,
                tref(None, None, "users"),
            ),
            (
                tref(None, Some("public"), "users"),
                ResolutionKind::Cataloged,
                tref(None, Some("public"), "users"),
            ),
            (
                tref(None, Some("sales"), "users"),
                ResolutionKind::Cataloged,
                tref(None, Some("sales"), "users"),
            ),
            (
                tref(None, Some("other"), "users"),
                ResolutionKind::Inferred,
                tref(None, Some("other"), "users"),
            ),
            (
                tref(None, None, "missing"),
                ResolutionKind::Inferred,
                tref(None, None, "missing"),
            ),
        ];

        for (written, resolution, table) in cases {
            let m = binder.table_match(written);
            assert_eq!(m.resolution, *resolution, "resolution for {written:?}");
            assert_eq!(m.table, *table, "canonical for {written:?}");
            // Columns are filled only on a unique hit.
            assert_eq!(
                m.columns.is_empty(),
                *resolution != ResolutionKind::Cataloged,
                "columns for {written:?}"
            );
        }
    }

    /// A catalog's `default_schema` / `default_catalog` fill a bare reference's
    /// omitted prefix segments *before* matching, so it resolves to the
    /// fully-qualified registration (the catalog segment fills only once a
    /// schema is present).
    #[test]
    fn table_match_fills_catalog_defaults() {
        let catalog = Catalog::new()
            .default_catalog("db")
            .default_schema("public")
            .table(
                CatalogTable::new("public", "users")
                    .catalog("db")
                    .columns(["id"]),
            );
        let diagnostics = RefCell::new(Vec::new());
        let binder = binder(&diagnostics, Some(&catalog), IdentifierCasing::default());

        let m = binder.table_match(&tref(None, None, "users"));
        assert_eq!(m.resolution, ResolutionKind::Cataloged);
        assert_eq!(m.table, tref(Some("db"), Some("public"), "users"));
    }

    /// Catalog-stored segments are compared **exact** (treated as
    /// already-quoted): the query side folds / quotes per the dialect, and the
    /// result must equal the stored name verbatim. So a stored `Users` matches a
    /// query that produces exactly `Users` — a quoted `"Users"`, or any case
    /// under `Insensitive` — never an unquoted `users` that the fold
    /// lower/upper-cases away. (Hence: register catalog names in the engine's
    /// stored case. The quoting / folding matrix itself lives in `crate::casing`.)
    ///
    /// stored `Users`, vs:
    ///
    /// | fold          | `users` (unquoted) | `"Users"` (quoted) |
    /// |---------------|--------------------|--------------------|
    /// | `Upper`       | ✗ (`USERS`)        | ✓                  |
    /// | `Lower`       | ✗ (`users`)        | ✓                  |
    /// | `Sensitive`   | ✗                  | ✓                  |
    /// | `Insensitive` | ✓                  | ✓                  |
    #[test]
    fn catalog_name_is_matched_as_quoted() {
        let table = CatalogTable::new("public", "Users").columns(["id"]);
        let unquoted = tref(None, None, "users");
        let quoted = TableReference {
            catalog: None,
            schema: None,
            name: Ident::with_quote('"', "Users"),
        };

        // (fold, query, expected match)
        let cases: &[(CaseRule, &TableReference, bool)] = &[
            (CaseRule::Upper, &unquoted, false),
            (CaseRule::Upper, &quoted, true),
            (CaseRule::Lower, &unquoted, false),
            (CaseRule::Lower, &quoted, true),
            (CaseRule::Sensitive, &unquoted, false),
            (CaseRule::Sensitive, &quoted, true),
            (CaseRule::Insensitive, &unquoted, true),
            (CaseRule::Insensitive, &quoted, true),
        ];

        for (fold, query, expected) in cases {
            assert_eq!(
                catalog_table_matches(query, &table, *fold),
                *expected,
                "{fold:?}: {query:?}"
            );
        }
    }

    /// Catalog *column* names are quote-wrapped by `table_match` too, so column
    /// matching (`list_has`) is exact in the same way as table names — but
    /// governed by the **column** fold, which a dialect can set apart from the
    /// table fold. A stored `Name` matches a quoted `"Name"`, not an unquoted
    /// `name` the fold lower/upper-cases away, and `Insensitive` folds both —
    /// the same shape as the table-name matrix above.
    ///
    /// stored column `Name`, vs:
    /// | column fold   | `name` (unquoted) | `"Name"` (quoted) |
    /// |---------------|-------------------|-------------------|
    /// | `Upper`       | ✗                 | ✓                 |
    /// | `Lower`       | ✗                 | ✓                 |
    /// | `Sensitive`   | ✗                 | ✓                 |
    /// | `Insensitive` | ✓                 | ✓                 |
    #[test]
    fn catalog_columns_are_matched_as_quoted() {
        let catalog = Catalog::new().table(CatalogTable::new("public", "users").columns(["Name"]));
        let unquoted = Ident::new("name");
        let quoted = Ident::with_quote('"', "Name");

        // (column fold, query, expected match)
        let cases: &[(CaseRule, &Ident, bool)] = &[
            (CaseRule::Upper, &unquoted, false),
            (CaseRule::Upper, &quoted, true),
            (CaseRule::Lower, &unquoted, false),
            (CaseRule::Lower, &quoted, true),
            (CaseRule::Sensitive, &unquoted, false),
            (CaseRule::Sensitive, &quoted, true),
            (CaseRule::Insensitive, &unquoted, true),
            (CaseRule::Insensitive, &quoted, true),
        ];

        for (fold, query, expected) in cases {
            let diagnostics = RefCell::new(Vec::new());
            // Keep the default (`Lower`) table fold so `users` resolves; vary
            // only the column fold that `list_has` consults.
            let casing = IdentifierCasing {
                column: *fold,
                ..IdentifierCasing::default()
            };
            let binder = binder(&diagnostics, Some(&catalog), casing);
            // The columns come from the catalog (quote-wrapped by `table_match`).
            let columns = binder.table_match(&tref(None, None, "users")).columns;
            assert_eq!(
                binder.list_has(&columns, query),
                *expected,
                "{fold:?}: {query:?}"
            );
        }
    }

    /// `fill_query_defaults` completes a written ref's omitted prefix from the
    /// catalog's defaults, **schema before catalog** — and catalog-fill is gated
    /// on a *present* schema, so `default_catalog` alone never touches a bare
    /// name. (Asserts segment values; the fill quote-wraps internally for
    /// matching, which doesn't affect the resolved values.)
    ///
    /// | written     | defaults       | filled            |
    /// |-------------|----------------|-------------------|
    /// | `users`     | schema+catalog | `db.public.users` |
    /// | `s.users`   | schema+catalog | `db.s.users`      |
    /// | `c.s.users` | schema+catalog | `c.s.users`       |
    /// | `users`     | catalog only   | `users` (gated)   |
    /// | `s.users`   | catalog only   | `db.s.users`      |
    /// | `users`     | schema only    | `public.users`    |
    /// | `users`     | (none)         | `users`           |
    #[test]
    fn fill_query_defaults_rule() {
        // (catalog, schema, name) values, quoting aside.
        fn seg(t: &TableReference) -> (Option<&str>, Option<&str>, &str) {
            (
                t.catalog.as_ref().map(|i| i.value.as_str()),
                t.schema.as_ref().map(|i| i.value.as_str()),
                t.name.value.as_str(),
            )
        }

        let both = Catalog::new()
            .default_schema("public")
            .default_catalog("db");
        // bare → schema filled, then catalog (catalog-fill needs a schema).
        let f = fill_query_defaults(&tref(None, None, "users"), &both);
        assert_eq!(seg(&f), (Some("db"), Some("public"), "users"));
        // schema present → only catalog fills.
        let f = fill_query_defaults(&tref(None, Some("s"), "users"), &both);
        assert_eq!(seg(&f), (Some("db"), Some("s"), "users"));
        // fully qualified → unchanged.
        let f = fill_query_defaults(&tref(Some("c"), Some("s"), "users"), &both);
        assert_eq!(seg(&f), (Some("c"), Some("s"), "users"));

        // `default_catalog` without `default_schema`: a bare name gets nothing
        // (catalog-fill is gated on a present schema), a schema-qualified one does.
        let catalog_only = Catalog::new().default_catalog("db");
        let f = fill_query_defaults(&tref(None, None, "users"), &catalog_only);
        assert_eq!(seg(&f), (None, None, "users"));
        let f = fill_query_defaults(&tref(None, Some("s"), "users"), &catalog_only);
        assert_eq!(seg(&f), (Some("db"), Some("s"), "users"));

        // `default_schema` only: bare gets the schema, catalog stays absent.
        let schema_only = Catalog::new().default_schema("public");
        let f = fill_query_defaults(&tref(None, None, "users"), &schema_only);
        assert_eq!(seg(&f), (None, Some("public"), "users"));

        // No defaults: unchanged.
        let f = fill_query_defaults(&tref(None, None, "users"), &Catalog::new());
        assert_eq!(seg(&f), (None, None, "users"));
    }
}
