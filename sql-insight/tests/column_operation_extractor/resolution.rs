use crate::support::*;

mod catalog_strict {
    use super::*;
    use sql_insight::catalog::{Catalog, CatalogTable};
    use sql_insight::sqlparser::ast::Ident;
    use sql_insight::sqlparser::dialect::MySqlDialect;

    /// Builder over a real [`Catalog`], registering every table under a
    /// `public` schema (bare query refs resolve against it by
    /// right-anchored matching). Keeps the terse `.with(name, cols)`
    /// shape the tests were written against.
    #[derive(Debug, Default)]
    struct TestCatalog {
        catalog: Catalog,
    }

    impl TestCatalog {
        fn with(mut self, name: &str, cols: Vec<&'static str>) -> Self {
            self.catalog = std::mem::take(&mut self.catalog)
                .table(CatalogTable::new("public", name).columns(cols));
            self
        }
    }

    fn assert_column_ops_with_catalog(sql: &str, catalog: &TestCatalog, expected: ColumnOperation) {
        assert_column_ops_with_catalog_dialect(&GenericDialect {}, sql, catalog, expected);
    }

    fn assert_column_ops_with_catalog_dialect(
        dialect: &dyn Dialect,
        sql: &str,
        catalog: &TestCatalog,
        expected: ColumnOperation,
    ) {
        let options = ExtractorOptions::new().with_catalog(&catalog.catalog);
        let actual = extract_column_operations_with_options(dialect, sql, options)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .unwrap();
        assert_column_ops_inner(sql, 0, actual, expected);
    }

    // Every write target in this module is a registered (`public`) table,
    // so the resolver canonicalizes it. Override the bare top-level
    // `write` / `relation` helpers to carry the `public.<name>` identity.
    // (Sources like `s` are unregistered and stay bare via `read` / `col`.)
    fn write(table_name: &str, col: &str) -> ColumnWrite {
        ColumnWrite {
            reference: ColumnReference {
                table: Some(cataloged_table(table_name)),
                name: col.into(),
            },
            resolution: ResolutionKind::Cataloged,
        }
    }

    // Canonical (`public`) table, but a column the catalog *doesn't* list — so
    // the column resolves `Inferred` even though the table is registered.
    fn write_inferred(table_name: &str, col: &str) -> ColumnWrite {
        ColumnWrite {
            reference: ColumnReference {
                table: Some(cataloged_table(table_name)),
                name: col.into(),
            },
            resolution: ResolutionKind::Inferred,
        }
    }

    // A column-less INSERT fills its column list from the catalog, so the written
    // column surfaces in the catalog's canonical (quoted) form — like the table
    // — not as a plain identifier, so a later reference to the same column
    // matches and dedups.
    fn filled_write(table_name: &str, col: &str) -> ColumnWrite {
        ColumnWrite {
            reference: ColumnReference {
                table: Some(cataloged_table(table_name)),
                name: Ident::with_quote('"', col),
            },
            resolution: ResolutionKind::Cataloged,
        }
    }

    fn filled_relation(table_name: &str, col: &str) -> ColumnTarget {
        ColumnTarget::Relation(filled_write(table_name, col))
    }

    fn relation_inferred(table_name: &str, col: &str) -> ColumnTarget {
        ColumnTarget::Relation(write_inferred(table_name, col))
    }

    #[test]
    fn catalog_canonicalizes_bare_ref_to_registered_full_path() {
        // `users` is written bare, but the catalog registers it under
        // `public`; a unique match canonicalizes the surfaced owning
        // table to `public.users` on both the read and the lineage
        // source. (`read_confirmed` / `col_confirmed` carry that
        // canonical identity.)
        let catalog = TestCatalog::default().with("users", vec!["a"]);
        assert_column_ops_with_catalog(
            "SELECT a FROM users",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_confirmed("users", "a")],
                writes: vec![],
                lineage: vec![passthrough(col_confirmed("users", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_known_schema_rejects_columns_not_in_table() {
        // Without catalog `SELECT a FROM t1` resolves a → t1.a
        // unconditionally (single Unknown binding heuristic). With a
        // catalog that says t1's columns are [x, y], `a` cannot come
        // from t1 — it surfaces as unresolved (table=None, resolution=
        // Unresolved on the read).
        let catalog = TestCatalog::default().with("t1", vec!["x", "y"]);
        assert_column_ops_with_catalog(
            "SELECT a FROM t1",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![unresolved("a")],
                writes: vec![],
                lineage: vec![passthrough(unresolved("a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_known_schema_resolves_columns_present_in_table() {
        let catalog = TestCatalog::default().with("t1", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "SELECT a FROM t1",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_confirmed("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col_confirmed("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_resolves_unquoted_ref_case_insensitively() {
        // The catalog declares `id` (lowercase); an unquoted `ID`
        // folds to the same key, so it resolves to t1. The column
        // name surfaces as written (`ID`) — folding governs matching,
        // not the surfaced identity.
        let catalog = TestCatalog::default().with("t1", vec!["id"]);
        assert_column_ops_with_catalog(
            "SELECT ID FROM t1",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_confirmed("t1", "ID")],
                writes: vec![],
                lineage: vec![passthrough(col_confirmed("t1", "ID"), out("ID", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_does_not_match_quoted_ref_against_unquoted_column() {
        // A quoted `"ID"` matches exactly (case-sensitive), so it does
        // not match the catalog's `id`; it stays unresolved (table=None,
        // ResolutionKind::Unresolved). Placed in WHERE so it is a read but
        // not a lineage source.
        let catalog = TestCatalog::default().with("t1", vec!["a", "id"]);
        let unresolved_quoted_id = ColumnRead {
            reference: ColumnReference {
                table: None,
                name: Ident::with_quote('"', "ID"),
            },
            resolution: ResolutionKind::Unresolved,
        };
        assert_column_ops_with_catalog(
            r#"SELECT a FROM t1 WHERE "ID" > 0"#,
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_confirmed("t1", "a"), unresolved_quoted_id],
                writes: vec![],
                lineage: vec![passthrough(col_confirmed("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_insert_without_explicit_columns_pairs_via_catalog_schema() {
        // INSERT INTO t SELECT a, b FROM s — no explicit column
        // list. With t = [x, y, z] in catalog, the resolver pairs
        // source projections positionally (s.a → t.x, s.b → t.y).
        // Unpaired catalog cols (z) get no lineage / no write.
        let catalog = TestCatalog::default().with("t", vec!["x", "y", "z"]);
        assert_column_ops_with_catalog(
            "INSERT INTO t SELECT a, b FROM s",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "a"), read("s", "b")],
                writes: vec![filled_write("t", "x"), filled_write("t", "y")],
                lineage: vec![
                    passthrough(col("s", "a"), filled_relation("t", "x")),
                    passthrough(col("s", "b"), filled_relation("t", "y")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_insert_without_explicit_columns_source_longer_than_target() {
        // 3 source projections vs t = [x, y] — pair what fits, the surplus
        // source column gets no lineage, and the overflow is flagged.
        let catalog = TestCatalog::default().with("t", vec!["x", "y"]);
        assert_column_ops_with_catalog(
            "INSERT INTO t SELECT a, b, c FROM s",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "a"), read("s", "b"), read("s", "c")],
                writes: vec![filled_write("t", "x"), filled_write("t", "y")],
                lineage: vec![
                    passthrough(col("s", "a"), filled_relation("t", "x")),
                    passthrough(col("s", "b"), filled_relation("t", "y")),
                ],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsArityMismatch)],
            },
        );
    }

    #[test]
    fn catalog_insert_explicit_columns_override_catalog_schema() {
        // Explicit (q) wins over catalog [x, y, z]. `q` isn't a catalog column
        // of `t`, so it resolves `Inferred` (the table is still canonical).
        let catalog = TestCatalog::default().with("t", vec!["x", "y", "z"]);
        assert_column_ops_with_catalog(
            "INSERT INTO t (q) SELECT a FROM s",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "a")],
                writes: vec![write_inferred("t", "q")],
                lineage: vec![passthrough(col("s", "a"), relation_inferred("t", "q"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn write_column_resolution_is_cataloged_only_when_listed() {
        // A written column in the target's catalog columns resolves `Cataloged`;
        // one that isn't (`z`) resolves `Inferred` — the write-side mirror of a
        // base column read.
        let catalog = TestCatalog::default().with("t", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "INSERT INTO t (a, z) VALUES (1, 2)",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t", "a"), write_inferred("t", "z")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_set_column_resolution_cataloged_vs_inferred() {
        // A SET target in the catalog → Cataloged; one that isn't (`z`) →
        // Inferred — the UPDATE write path, symmetric with INSERT.
        let catalog = TestCatalog::default().with("t", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "UPDATE t SET a = 1, z = 2",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![],
                writes: vec![write("t", "a"), write_inferred("t", "z")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn multi_table_update_resolves_each_set_target_against_its_own_catalog() {
        // Each SET target resolves against *its own* table's catalog columns:
        // `t1.a` is listed (Cataloged), `t2.z` isn't (Inferred). (Asserts the
        // per-column resolution directly, sidestepping MySQL's canonical
        // backtick quoting of the surfaced identities.)
        let catalog = TestCatalog::default()
            .with("t1", vec!["a", "id"])
            .with("t2", vec!["b", "id"]);
        let options = ExtractorOptions::new().with_catalog(&catalog.catalog);
        let op = extract_column_operations_with_options(
            &MySqlDialect {},
            "UPDATE t1 JOIN t2 ON t1.id = t2.id SET t1.a = 1, t2.z = 2",
            options,
        )
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .unwrap();
        let writes: Vec<_> = op
            .writes
            .iter()
            .map(|w| (w.reference.name.value.as_str(), w.resolution))
            .collect();
        assert_eq!(
            writes,
            vec![
                ("a", ResolutionKind::Cataloged),
                ("z", ResolutionKind::Inferred),
            ]
        );
    }

    #[test]
    fn catalog_merge_not_matched_insert_no_cols_pairs_via_catalog() {
        // A column-less MERGE INSERT pairs its VALUES positionally with the
        // target's catalog schema (id, a): each value writes its column and
        // feeds `source → target.col` lineage.
        let catalog = TestCatalog::default().with("t", vec!["id", "a"]);
        assert_column_ops_with_catalog(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.a)",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![
                    read_confirmed("t", "id"),
                    read("s", "id"),
                    read("s", "id"),
                    read("s", "a"),
                ],
                writes: vec![filled_write("t", "id"), filled_write("t", "a")],
                lineage: vec![
                    passthrough(col("s", "id"), filled_relation("t", "id")),
                    passthrough(col("s", "a"), filled_relation("t", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_using_merge_column_fans_in_cataloged() {
        // A USING merge column fans in to both joined tables; with a
        // catalog confirming `id` on each, both sides resolve Cataloged.
        let catalog = TestCatalog::default()
            .with("t1", vec!["id"])
            .with("t2", vec!["id"]);
        assert_column_ops_with_catalog(
            "SELECT id FROM t1 JOIN t2 USING (id)",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_confirmed("t1", "id"), read_confirmed("t2", "id")],
                writes: vec![],
                lineage: vec![
                    passthrough(col_confirmed("t1", "id"), out("id", 0)),
                    passthrough(col_confirmed("t2", "id"), out("id", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_natural_join_fans_in_schema_common_columns() {
        // A NATURAL join's merge set is the schema intersection of both sides —
        // knowable only with a catalog. `id` is common to t1 and t2, so an
        // unqualified `id` fans in to both (like USING (id)); the unique columns
        // (`a`, `b`) are not merge columns. (Catalog-free, `id` stays ambiguous
        // — see `joins_sets`.)
        let catalog = TestCatalog::default()
            .with("t1", vec!["id", "a"])
            .with("t2", vec!["id", "b"]);
        assert_column_ops_with_catalog(
            "SELECT id FROM t1 NATURAL JOIN t2",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_confirmed("t1", "id"), read_confirmed("t2", "id")],
                writes: vec![],
                lineage: vec![
                    passthrough(col_confirmed("t1", "id"), out("id", 0)),
                    passthrough(col_confirmed("t2", "id"), out("id", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_disambiguates_join_unqualified_ref() {
        // Both tables are Known via catalog; only t2 has `a`, so
        // unqualified `a` in `t1 JOIN t2` resolves to t2 (no
        // catalog: same SQL would be ambiguous).
        let catalog = TestCatalog::default()
            .with("t1", vec!["id"])
            .with("t2", vec!["id", "a"]);
        assert_column_ops_with_catalog(
            "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read_confirmed("t1", "id"),
                    read_confirmed("t2", "id"),
                    read_confirmed("t2", "a"),
                ],
                writes: vec![],
                lineage: vec![passthrough(col_confirmed("t2", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_confirmed_ambiguity_surfaces_as_ambiguous_read() {
        // Both tables Known and both declare `a`. The unqualified `a`
        // can't be resolved to a single owner; the read surfaces with
        // table=None and ResolutionKind::Ambiguous. (Without catalog the
        // same query also yields Ambiguous: two Unknown suspects with
        // no Known tiebreaker.)
        let catalog = TestCatalog::default()
            .with("t1", vec!["a"])
            .with("t2", vec!["a"]);
        assert_column_ops_with_catalog(
            "SELECT a FROM t1 JOIN t2 ON t1.a = t2.a",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read_confirmed("t1", "a"),
                    read_confirmed("t2", "a"),
                    ambiguous("a"),
                ],
                writes: vec![],
                lineage: vec![passthrough(ambiguous("a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_unresolved_unqualified_surfaces_as_unresolved_read() {
        // Catalog says t1 has [x, y]; unqualified `z` belongs to
        // nothing in scope — the read surfaces with table=None and
        // ResolutionKind::Unresolved.
        let catalog = TestCatalog::default().with("t1", vec!["x", "y"]);
        assert_column_ops_with_catalog(
            "SELECT z FROM t1",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![unresolved("z")],
                writes: vec![],
                lineage: vec![passthrough(unresolved("z"), out("z", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn no_catalog_unqualified_is_ambiguous_under_unknown_suspects() {
        // No catalog → all real-table schemas are Unknown. For the
        // unqualified `a`, both t1 and t2 are Unknown suspects with
        // no Known tiebreaker, so the read surfaces with table=None
        // and ResolutionKind::Ambiguous. No diagnostic fires:
        // diagnostics are tool-side gaps (wildcard / unsupported),
        // resolution outcomes live on the read's resolution.
        assert_column_ops(
            "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t2", "id"), ambiguous("a")],
                writes: vec![],
                lineage: vec![passthrough(ambiguous("a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }
}

/// Pins one row per case from the [`ResolutionKind`] rustdoc's behavior
/// table. Each test is the minimal SQL that exercises that arm and
/// asserts the full expected reads vector — so this module doubles as
/// behavior documentation: a reader can recover the catalog-less /
/// catalog-aware semantics by reading the test bodies.
#[cfg(test)]
mod confidence_arm_coverage {
    use super::*;
    use sql_insight::catalog::{Catalog, CatalogTable};
    use sql_insight::sqlparser::dialect::GenericDialect;

    #[derive(Debug, Default)]
    struct TestCatalog {
        catalog: Catalog,
    }

    impl TestCatalog {
        fn with(mut self, name: &str, cols: Vec<&'static str>) -> Self {
            self.catalog = std::mem::take(&mut self.catalog)
                .table(CatalogTable::new("public", name).columns(cols));
            self
        }
    }

    fn extract_reads(sql: &str, catalog: Option<&TestCatalog>) -> Vec<ColumnRead> {
        let mut options = ExtractorOptions::new();
        if let Some(c) = catalog {
            options = options.with_catalog(&c.catalog);
        }
        extract_column_operations_with_options(&GenericDialect {}, sql, options)
            .unwrap()
            .remove(0)
            .unwrap()
            .reads
    }

    #[test]
    fn catalog_less_sole_unknown_candidate_is_inferred() {
        // Real `Unknown` table is the sole candidate → Inferred.
        assert_unordered_eq!(
            extract_reads("SELECT id FROM t1", None),
            vec![read("t1", "id")]
        );
    }

    #[test]
    fn catalog_less_two_unknown_candidates_is_ambiguous() {
        // Both candidates Unknown, no Known tiebreaker → Ambiguous.
        // The `t1.id` / `t2.id` qualified refs in the ON predicate
        // remain Inferred (qualifier-bound but Unknown schema).
        assert_unordered_eq!(
            extract_reads("SELECT id FROM t1 JOIN t2 ON t1.id = t2.id", None),
            vec![read("t1", "id"), read("t2", "id"), ambiguous("id")]
        );
    }

    #[test]
    fn catalog_less_cte_known_body_drops_synthetic_confirmed_ref() {
        // CTE `cte` has a `Known` body ([id]) derived from its
        // projection. The outer `cte.id` qualified ref hits the CTE
        // binding and would be internally Cataloged, but synthetic
        // refs are dropped from public reads — only the inner real
        // `t1.id` (Inferred) surfaces.
        assert_unordered_eq!(
            extract_reads("WITH cte AS (SELECT id FROM t1) SELECT id FROM cte", None),
            vec![read("t1", "id")]
        );
    }

    #[test]
    fn catalog_less_cte_known_denies_column_is_unresolved() {
        // CTE body = [id]; `unknown_col` cannot belong to the CTE.
        // The outer ref surfaces with table=None / Unresolved.
        assert_unordered_eq!(
            extract_reads(
                "WITH cte AS (SELECT id FROM t1) SELECT id, unknown_col FROM cte",
                None
            ),
            vec![read("t1", "id"), unresolved("unknown_col")]
        );
    }

    #[test]
    fn catalog_aware_known_binding_lists_column_is_confirmed() {
        let catalog = TestCatalog::default().with("t1", vec!["a"]);
        assert_unordered_eq!(
            extract_reads("SELECT a FROM t1", Some(&catalog)),
            vec![read_confirmed("t1", "a")]
        );
    }

    #[test]
    fn catalog_aware_known_binding_missing_column_is_unresolved() {
        let catalog = TestCatalog::default().with("t1", vec!["x", "y"]);
        assert_unordered_eq!(
            extract_reads("SELECT a FROM t1", Some(&catalog)),
            vec![unresolved("a")]
        );
    }

    #[test]
    fn catalog_aware_known_witness_over_unknown_suspect_is_inferred() {
        // t1 in catalog confirms `a`; t2 is catalog-less → Unknown
        // suspect. Single Known winner adopted with Inferred (the
        // Unknown suspect could in principle also contain `a`, so we
        // don't claim Cataloged).
        let catalog = TestCatalog::default().with("t1", vec!["a"]);
        // t1 is cataloged → its identity canonicalizes to public.t1, but
        // the placement is still Inferred (t2 is an Unknown suspect).
        assert_unordered_eq!(
            extract_reads("SELECT a FROM t1, t2", Some(&catalog)),
            vec![read_with_ref(
                cataloged_table("t1"),
                "a",
                ResolutionKind::Inferred
            )]
        );
    }

    #[test]
    fn catalog_aware_two_known_confirms_is_ambiguous() {
        let catalog = TestCatalog::default()
            .with("t1", vec!["a"])
            .with("t2", vec!["a"]);
        // Both t1 and t2 confirm `a` — genuine ambiguity. The
        // qualified `t1.a` / `t2.a` in ON are Cataloged individually.
        assert_unordered_eq!(
            extract_reads("SELECT a FROM t1 JOIN t2 ON t1.a = t2.a", Some(&catalog)),
            vec![
                read_confirmed("t1", "a"),
                read_confirmed("t2", "a"),
                ambiguous("a"),
            ]
        );
    }

    #[test]
    fn qualified_ref_to_unknown_table_is_inferred() {
        // `t.col` where t is `Unknown` (no catalog). The qualifier
        // binds, but the column existence is assumed → Inferred.
        assert_unordered_eq!(
            extract_reads("SELECT t.id FROM t", None),
            vec![read("t", "id")]
        );
    }

    #[test]
    fn qualified_ref_to_known_table_listing_column_is_confirmed() {
        let catalog = TestCatalog::default().with("t", vec!["id"]);
        assert_unordered_eq!(
            extract_reads("SELECT t.id FROM t", Some(&catalog)),
            vec![read_confirmed("t", "id")]
        );
    }
}

/// Pins the qualifier-matching behavior table: for a *qualified*
/// column reference, which `FROM` binding (if any) owns it. Every row
/// is decidable from SQL structure alone, so these run catalog-free and
/// assert table identity + catalog-less resolution
/// (`Inferred` for a resolved ref, `Ambiguous` / `Unresolved` for the
/// failure modes). Catalog-confirmed (`Cataloged`) placement is covered
/// separately in [`confidence_arm_coverage`].
///
/// The matching rule is ANSI right-anchored: a qualifier matches a
/// non-aliased table when name segments are equal and each of
/// schema / catalog is equal-or-either-absent; an alias hides the
/// original name and matches only a single-segment qualifier equal to
/// the alias.
#[cfg(test)]
mod qualified_ref_arm_coverage {
    use super::*;

    fn reads(sql: &str) -> Vec<ColumnRead> {
        extract(sql).reads
    }

    /// A schema-qualified read (`schema.name.col`), catalog-less →
    /// `Inferred`.
    fn read2(schema: &str, name: &str, col: &str) -> ColumnRead {
        read_with_ref(
            TableReference {
                catalog: None,
                schema: Some(schema.into()),
                name: name.into(),
            },
            col,
            ResolutionKind::Inferred,
        )
    }

    #[test]
    fn row1_bare_qualifier_matches_schema_qualified_binding() {
        // `users` (no schema) matches `FROM mydb.users` — the binding
        // fills the omitted schema. Surfaces the binding's full identity.
        assert_unordered_eq!(
            reads("SELECT users.id FROM mydb.users"),
            vec![read2("mydb", "users", "id")]
        );
    }

    #[test]
    fn row2_full_qualifier_matches_exactly() {
        assert_unordered_eq!(
            reads("SELECT mydb.users.id FROM mydb.users"),
            vec![read2("mydb", "users", "id")]
        );
    }

    #[test]
    fn row3_contradicting_schema_is_unresolved() {
        // `otherdb.users` vs binding `mydb.users` — schema present on
        // both and differs. Contradiction a catalog can't fix.
        assert_unordered_eq!(
            reads("SELECT otherdb.users.id FROM mydb.users"),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn row4_bare_qualifier_matches_bare_binding() {
        assert_unordered_eq!(
            reads("SELECT users.id FROM users"),
            vec![read("users", "id")]
        );
    }

    #[test]
    fn row5_over_qualified_ref_resolves_to_binding_identity() {
        // `mydb.users` vs bare binding `users` — the binding leaves the
        // schema unspecified, so the ref's extra `mydb` matches as a
        // wildcard. We surface the *binding's* identity (`users`), not
        // the ref's unverified `mydb` qualifier — binding wins, for
        // consistency with row 1 (always surface what FROM declared).
        assert_unordered_eq!(
            reads("SELECT mydb.users.id FROM users"),
            vec![read("users", "id")]
        );
    }

    #[test]
    fn row6_alias_matches_alias() {
        assert_unordered_eq!(
            reads("SELECT u.id FROM mydb.users AS u"),
            vec![read2("mydb", "users", "id")]
        );
    }

    #[test]
    fn row7_alias_hides_original_bare_name() {
        // `users.id` against `FROM mydb.users AS u` — the alias hides
        // the original name, so `users` matches nothing.
        assert_unordered_eq!(
            reads("SELECT users.id FROM mydb.users AS u"),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn row8_alias_hides_original_full_name() {
        assert_unordered_eq!(
            reads("SELECT mydb.users.id FROM mydb.users AS u"),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn row9_two_unaliased_same_name_tables_are_ambiguous() {
        // `s1.users` and `s2.users` are distinct bindings (the scope
        // arena keys by full path, so same-name-different-schema tables
        // coexist). The bare qualifier `users` right-anchors to both →
        // AMBIGUOUS. (Before full-path keying these collided on the
        // last segment and the second was dropped, hiding the
        // ambiguity.)
        assert_unordered_eq!(
            reads("SELECT users.id FROM s1.users, s2.users"),
            vec![ambiguous("id")]
        );
    }

    #[test]
    fn row10_explicit_schema_disambiguates() {
        assert_unordered_eq!(
            reads("SELECT s1.users.id FROM s1.users, s2.users"),
            vec![read2("s1", "users", "id")]
        );
    }

    #[test]
    fn row11_schema_matching_no_candidate_is_unresolved() {
        assert_unordered_eq!(
            reads("SELECT s3.users.id FROM s1.users, s2.users"),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn row12_alias_disambiguates_two_aliased_bindings() {
        assert_unordered_eq!(
            reads("SELECT u1.id FROM s1.users u1, s2.users u2"),
            vec![read2("s1", "users", "id")]
        );
    }

    #[test]
    fn row13_bare_name_hidden_by_both_aliases_is_unresolved() {
        assert_unordered_eq!(
            reads("SELECT users.id FROM s1.users u1, s2.users u2"),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn row14_last_segment_uniquely_matches_one_binding() {
        // `users` matches `mydb.users` but not `mydb.orders` (name
        // differs). Unique → resolve.
        assert_unordered_eq!(
            reads("SELECT users.id FROM mydb.users, mydb.orders"),
            vec![read2("mydb", "users", "id")]
        );
    }
}
