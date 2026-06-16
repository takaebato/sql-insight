use super::*;

impl Binder<'_> {
    /// Record a `WildcardSuppressed` diagnostic for a projection wildcard
    /// (`*` / `t.*` / `(expr).*`) left unexpanded, carrying the wildcard
    /// token's location (a zero-line span is treated as unknown).
    pub(super) fn record_wildcard_suppressed(&self, description: &str, span: Span) {
        let span = (span.start.line != 0).then_some(span);
        let suffix = match span {
            Some(s) => format!(" at L{}:C{}", s.start.line, s.start.column),
            None => String::new(),
        };
        self.diagnostics.borrow_mut().push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::WildcardSuppressed,
            message: format!(
                "{description}{suffix} left unexpanded — column lineage will be incomplete for this projection"
            ),
            span,
        });
    }

    /// Build a table reference from a parsed name, recording a
    /// `TooManyTableQualifiers` diagnostic and returning `None` when the
    /// name has more identifiers than `catalog.schema.name` (the only way a
    /// parsed name fails to convert). The dropped relation thus stays
    /// observable rather than vanishing silently.
    pub(super) fn table_ref(&self, name: &ObjectName) -> Option<TableReference> {
        match TableReference::try_from_name(name) {
            Ok(table) => Some(table),
            Err(_) => {
                self.record_too_many_table_qualifiers(name);
                None
            }
        }
    }

    /// Like [`table_ref`](Self::table_ref) for a FROM / target table factor:
    /// a plain `Table` factor records the over-qualified diagnostic; a
    /// derived / function factor simply has no table identity (no
    /// diagnostic — it isn't an over-qualified name).
    pub(super) fn table_factor_ref(&self, factor: &TableFactor) -> Option<TableReference> {
        match factor {
            TableFactor::Table { name, .. } => self.table_ref(name),
            other => TableReference::try_from(other).ok(),
        }
    }

    /// Record a `TooManyTableQualifiers` diagnostic for an over-qualified
    /// table name (more than `catalog.schema.name`), carrying its location.
    pub(super) fn record_too_many_table_qualifiers(&self, name: &ObjectName) {
        let span = name
            .0
            .first()
            .and_then(|part| part.as_ident())
            .map(|ident| ident.span)
            .filter(|s| s.start.line != 0);
        let suffix = match span {
            Some(s) => format!(" at L{}:C{}", s.start.line, s.start.column),
            None => String::new(),
        };
        self.diagnostics.borrow_mut().push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::TooManyTableQualifiers,
            message: format!(
                "table reference `{name}`{suffix} has too many qualifiers (max catalog.schema.name) — dropped"
            ),
            span,
        });
    }

    /// Resolve a single column reference to its pre-collapsed provenance.
    /// Mirrors the resolver: unqualified scans candidate relations
    /// (Known-witness over Open suspects); qualified matches the
    /// qualifier first. Catalog-free everything is `Open` → `Inferred` /
    /// `Ambiguous`. A name no relation in the current scope can own falls
    /// through to the enclosing scopes (correlation), innermost-first,
    /// before giving up as `Unresolved`.
    pub(super) fn resolve_ref(&self, parts: &[Ident], scope: &Scope) -> Vec<ProvenanceSource> {
        let Some(column) = parts.last() else {
            return Vec::new();
        };
        // Output-alias visibility (GROUP BY / HAVING / ORDER BY): a bare
        // name matching an enclosing output column is a reference to that
        // output — return its provenance so an introduced alias resolves
        // to its real sources instead of a phantom stored column. Empty at
        // FROM-level (no outputs). Only the current scope's outputs are
        // visible; correlation reaches enclosing *relations*, not aliases.
        if parts.len() == 1 {
            // An *introduced* output alias (computed expr or rename) named
            // here is a reference to that output value, not a stored
            // column — return its sources marked synthetic-origin so they
            // stay lineage sources but aren't double-counted as reads (the
            // physical reads are already at the projection). An *identity*
            // alias (`GROUP BY a` for a bare `a`) falls through to normal
            // resolution, so it resolves to the real column with *this*
            // reference's own span and counts as its own occurrence.
            if let Some(output) = scope
                .outputs
                .iter()
                .find(|c| self.name_matches(c.name.as_ref(), column))
            {
                if !self.is_identity_output(output) {
                    return output
                        .provenance
                        .iter()
                        .map(|source| ProvenanceSource {
                            synthetic_origin: true,
                            ..source.clone()
                        })
                        .collect();
                }
            }
            // USING merge column: fan in to every relation that could own
            // it — one source per side — instead of resolving to one /
            // ambiguous. A catalog narrows the fan-in to declaring
            // relations (Cataloged); catalog-free it reaches every joined
            // relation (Inferred).
            if self.list_has(&scope.merge_columns, column) {
                let sources: Vec<ProvenanceSource> = scope
                    .relations
                    .iter()
                    .filter_map(|rel| self.unqualified_candidate(rel, column))
                    .flat_map(|candidate| candidate.provenance)
                    .collect();
                if !sources.is_empty() {
                    return sources;
                }
            }
        }
        if let Some(sources) = self.resolve_in_relations(parts, &scope.relations, column) {
            return sources;
        }
        for outer in self.outer_scopes.iter().rev() {
            if let Some(sources) = self.resolve_in_relations(parts, outer, column) {
                return sources;
            }
        }
        vec![passthrough(unresolved(column))]
    }

    /// Whether an output column is a bare-column passthrough under its own
    /// name (its single source's column name matches the output name).
    pub(super) fn is_identity_output(&self, output: &BoundColumn) -> bool {
        let Some(name) = &output.name else {
            return false;
        };
        matches!(
            output.provenance.as_slice(),
            [source] if self.name_matches(Some(&source.read.reference.name), name)
        )
    }

    /// Resolve `parts` against one set of relations, returning `None` when
    /// none of them could own the column (so the caller can fall through
    /// to an enclosing scope) and `Some(sources)` once at least one is a
    /// candidate — even if that resolves `Ambiguous` (a name owned within
    /// this scope doesn't escape it).
    pub(super) fn resolve_in_relations(
        &self,
        parts: &[Ident],
        relations: &[Relation],
        column: &Ident,
    ) -> Option<Vec<ProvenanceSource>> {
        let candidates: Vec<Candidate> = if parts.len() == 1 {
            relations
                .iter()
                .filter_map(|rel| self.unqualified_candidate(rel, column))
                .collect()
        } else {
            // The qualifier is every segment but the column. Decode it into
            // a `TableReference` for right-anchored matching; `None` when it
            // overshoots `catalog.schema.name` depth (a 5+-part ref), which
            // no real table can match.
            let qualifier_parts = &parts[..parts.len() - 1];
            let qualifier_ref = TableReference::try_from_parts(qualifier_parts);
            relations
                .iter()
                .filter_map(|rel| {
                    self.qualified_candidate(rel, qualifier_parts, qualifier_ref.as_ref(), column)
                })
                .collect()
        };
        (!candidates.is_empty()).then(|| self.pick(candidates, column))
    }

    pub(super) fn name_matches(&self, name: Option<&Ident>, other: &Ident) -> bool {
        name.is_some_and(|n| self.casing.column.normalize(n) == self.casing.column.normalize(other))
    }

    /// A relation is an unqualified candidate iff it could own `column`:
    /// a `Known` schema must list it; an `Open` real table always could;
    /// a derived relation must expose it.
    pub(super) fn unqualified_candidate(
        &self,
        rel: &Relation,
        column: &Ident,
    ) -> Option<Candidate> {
        match &rel.source {
            RelationSource::Table {
                table,
                columns: RelationColumns::Known(cols),
            } => self.list_has(cols, column).then(|| Candidate {
                provenance: vec![passthrough(read(table, column, ResolutionKind::Cataloged))],
                confirmed: true,
                synthetic: false,
            }),
            RelationSource::Table {
                table,
                columns: RelationColumns::Open,
            } => Some(Candidate {
                provenance: vec![passthrough(read(table, column, ResolutionKind::Inferred))],
                confirmed: false,
                synthetic: false,
            }),
            RelationSource::Derived { columns } => self.derived_candidate(rel, columns, column),
            // A table function's columns are opaque; a bare name is not
            // claimed by it (so it stays resolvable against real tables).
            RelationSource::TableFunction => None,
        }
    }

    /// A candidate owner of a qualified reference. The qualifier matches a
    /// **non-aliased real table** by right-anchored path
    /// (`catalog.schema.name`, omitted segments wildcard, table casing) and
    /// anything with a single exposed name (an aliased table / derived /
    /// CTE / table function) by a single-segment alias match (table-alias
    /// casing). A qualifier that overshoots the path depth (a 5+-part ref,
    /// `qualifier_ref` is `None`) matches no real table. When the qualifier
    /// pins a `Known` table that doesn't list the column it still resolves
    /// (`Inferred`); a derived relation that doesn't expose it contributes
    /// nothing.
    pub(super) fn qualified_candidate(
        &self,
        rel: &Relation,
        qualifier_parts: &[Ident],
        qualifier_ref: Option<&TableReference>,
        column: &Ident,
    ) -> Option<Candidate> {
        let qualifier_ok = match &rel.source {
            RelationSource::Table { table, .. } if rel.alias.is_none() => {
                qualifier_ref.is_some_and(|q| self.qualifier_matches_table(q, table))
            }
            _ => rel
                .exposed_name()
                .is_some_and(|name| matches!(qualifier_parts, [only] if self.ident_eq(only, name))),
        };
        if !qualifier_ok {
            return None;
        }
        match &rel.source {
            RelationSource::Table {
                table,
                columns: RelationColumns::Known(cols),
            } => {
                let confirmed = self.list_has(cols, column);
                Some(Candidate {
                    provenance: vec![passthrough(read(
                        table,
                        column,
                        if confirmed {
                            ResolutionKind::Cataloged
                        } else {
                            ResolutionKind::Inferred
                        },
                    ))],
                    confirmed,
                    synthetic: false,
                })
            }
            RelationSource::Table {
                table,
                columns: RelationColumns::Open,
            } => Some(Candidate {
                provenance: vec![passthrough(read(table, column, ResolutionKind::Inferred))],
                confirmed: false,
                synthetic: false,
            }),
            RelationSource::Derived { columns } => self.derived_candidate(rel, columns, column),
            // A reference qualified by a table function's alias resolves to
            // it: the produced column isn't stored, so it's marked
            // synthetic-origin — excluded from `reads`, but still a lineage
            // source (the value flows out of the function), exactly as the
            // resolver surfaces a synthetic table-function reference.
            RelationSource::TableFunction => rel.exposed_name().map(|name| {
                let table = TableReference {
                    catalog: None,
                    schema: None,
                    name: name.clone(),
                };
                Candidate {
                    provenance: vec![ProvenanceSource {
                        synthetic_origin: true,
                        ..passthrough(read(&table, column, ResolutionKind::Inferred))
                    }],
                    confirmed: true,
                    synthetic: true,
                }
            }),
        }
    }

    /// A derived relation is a candidate iff it exposes an output column
    /// named `column`; its (already collapsed) provenance is the
    /// candidate's. Synthetic — the witness tiebreaker keeps it verbatim.
    /// Every source is marked `synthetic_origin`: this reference goes
    /// *through* the derived / CTE relation, so its physical read was
    /// already counted at the relation's own body (it stays a lineage
    /// source, but not a `reads` occurrence). A column with no underlying
    /// source (a `VALUES` literal / `SELECT` constant) falls back to a
    /// synthetic self-reference (`alias.column`), so it is still a lineage
    /// source rather than vanishing.
    pub(super) fn derived_candidate(
        &self,
        rel: &Relation,
        columns: &[BoundColumn],
        column: &Ident,
    ) -> Option<Candidate> {
        let exposed = columns
            .iter()
            .find(|c| self.name_matches(c.name.as_ref(), column))?;
        let provenance = if exposed.provenance.is_empty() {
            rel.exposed_name()
                .map(|name| {
                    let table = TableReference {
                        catalog: None,
                        schema: None,
                        name: name.clone(),
                    };
                    ProvenanceSource {
                        synthetic_origin: true,
                        ..passthrough(read(&table, column, ResolutionKind::Inferred))
                    }
                })
                .into_iter()
                .collect()
        } else {
            exposed
                .provenance
                .iter()
                .map(|source| ProvenanceSource {
                    synthetic_origin: true,
                    ..source.clone()
                })
                .collect()
        };
        Some(Candidate {
            provenance,
            confirmed: true,
            synthetic: true,
        })
    }

    /// Collapse candidates to the reference's provenance (the resolver's
    /// rule): none → `Unresolved`; one → its provenance verbatim (already
    /// `Cataloged` / `Inferred` / collapsed); several with exactly one
    /// confirmed → that Known witness wins (a real table downgrades to
    /// `Inferred`, a synthetic one keeps its provenance); otherwise
    /// `Ambiguous`.
    pub(super) fn pick(&self, candidates: Vec<Candidate>, column: &Ident) -> Vec<ProvenanceSource> {
        if candidates.is_empty() {
            return vec![passthrough(unresolved(column))];
        }
        if candidates.len() == 1 {
            return candidates.into_iter().next().unwrap().provenance;
        }
        let mut confirmed = candidates.into_iter().filter(|c| c.confirmed);
        match (confirmed.next(), confirmed.next()) {
            (Some(witness), None) => {
                if witness.synthetic {
                    witness.provenance
                } else {
                    downgrade_to_inferred(witness.provenance)
                }
            }
            _ => vec![passthrough(ambiguous(column))],
        }
    }

    pub(super) fn list_has(&self, columns: &[Ident], column: &Ident) -> bool {
        columns
            .iter()
            .any(|c| self.casing.column.normalize(c) == self.casing.column.normalize(column))
    }

    pub(super) fn ident_eq(&self, a: &Ident, b: &Ident) -> bool {
        self.casing.table_alias.normalize(a) == self.casing.table_alias.normalize(b)
    }

    /// Right-anchored match of a decoded qualifier against a real table's
    /// `catalog.schema.name`, under the dialect's *table* casing: the name
    /// must match, and each present qualifier segment must match its
    /// counterpart (an omitted segment is a wildcard, so a bare `users`
    /// matches `mydb.users` but `otherdb.users` does not).
    pub(super) fn qualifier_matches_table(
        &self,
        qualifier: &TableReference,
        table: &TableReference,
    ) -> bool {
        let fold = self.casing.table;
        let eq = |a: &Ident, b: &Ident| fold.normalize(a) == fold.normalize(b);
        let opt_eq = |a: Option<&Ident>, b: Option<&Ident>| match (a, b) {
            (Some(x), Some(y)) => eq(x, y),
            _ => true,
        };
        eq(&qualifier.name, &table.name)
            && opt_eq(qualifier.schema.as_ref(), table.schema.as_ref())
            && opt_eq(qualifier.catalog.as_ref(), table.catalog.as_ref())
    }

    /// Canonicalize a write target to its registered full path when the
    /// catalog uniquely identifies it, so write surfaces agree with the
    /// (canonicalized) read side. An ambiguous / missing / catalog-free
    /// target — including a freshly created CTAS / view name — stays as
    /// written (`table_match` returns the written ref in those cases).
    pub(super) fn canonical_target(&self, target: TableReference) -> TableReference {
        self.table_match(&target).table
    }

    /// Match a query table reference against the catalog by right-anchored,
    /// dialect-cased comparison (after default-fill). A unique hit yields
    /// the registered table's canonical identity, its column names, and
    /// `Cataloged`; several hits yield the written ref and `Ambiguous`; no
    /// catalog or no hit yields the written ref and `Inferred`. Mirrors the
    /// resolver's `catalog_match` but keeps the resolution kind so the
    /// `Scan`'s table-level resolution survives (the resolver collapsed
    /// ambiguous / miss to "open").
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
}
