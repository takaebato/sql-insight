//! Wildcard expansion: a projection `*` / `t.*` whose columns are completely
//! known expands into per-column outputs — reads (one per expanded column per
//! wildcard occurrence), lineage, and determinate positions (unlocking the
//! DML positional pairing). All-or-nothing per wildcard: anything not fully
//! known keeps the wildcard unexpanded and flagged (`WildcardSuppressed` — the
//! unexpandable cases stay pinned in `diagnostics`).
//!
//! Two knowledge sources drive expansion, tested in their own modules:
//! a **catalog** (base tables) and the **SQL text itself** (derived tables /
//! CTEs, whose slot views need no catalog).

use crate::support::*;

use sql_insight::catalog::{Catalog, CatalogTable};

/// Register tables under a `public` schema (bare refs right-anchor onto it),
/// mirroring `resolution.rs`'s builder.
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
    let options = ExtractorOptions::new().with_catalog(&catalog.catalog);
    let actual = extract_column_operations_with_options(&GenericDialect {}, sql, options)
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .unwrap();
    assert_column_ops_inner(sql, 0, actual, expected);
}

/// The canonical (quoted) identifier an expanded catalog column surfaces
/// with — the same form the write-side catalog fill uses, since the column
/// has no written token of its own.
fn qident(name: &str) -> Ident {
    Ident::with_quote('"', name)
}

/// An expanded catalog column as a read: canonical `public.<table>` identity,
/// quoted column name, `Cataloged` (it is listed by construction).
fn expanded_read(table_name: &str, col: &str) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: Some(cataloged_table(table_name)),
            name: qident(col),
        },
        resolution: ResolutionKind::Cataloged,
    }
}

/// A query-output target named by an expanded catalog column (quoted).
fn expanded_out(name: &str, position: usize) -> ColumnTarget {
    ColumnTarget::QueryOutput {
        name: Some(qident(name)),
        position,
    }
}

mod catalog_tables {
    use super::*;

    #[test]
    fn bare_wildcard_expands_a_cataloged_table() {
        let catalog = TestCatalog::default().with("t", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "SELECT * FROM t",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![expanded_read("t", "a"), expanded_read("t", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(expanded_read("t", "a"), expanded_out("a", 0)),
                    passthrough(expanded_read("t", "b"), expanded_out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualified_wildcard_expands_only_its_relation() {
        // `t.*` needs only `t`'s columns — the catalog-unknown join side
        // doesn't block it (a bare `*` would stay suppressed here).
        let catalog = TestCatalog::default().with("t", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "SELECT t.* FROM t JOIN u ON t.a = u.k",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    expanded_read("t", "a"),
                    expanded_read("t", "b"),
                    read_with_ref(cataloged_table("t"), "a", ResolutionKind::Cataloged),
                    read("u", "k"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(expanded_read("t", "a"), expanded_out("a", 0)),
                    passthrough(expanded_read("t", "b"), expanded_out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn expanded_columns_sort_at_the_wildcard_in_schema_order() {
        // Surfaces are source-ordered; every expanded column carries the `*`
        // token's span, so the whole block sorts at the wildcard — in schema
        // order (the facade's sort is stable) — between the written
        // neighbours.
        let catalog = TestCatalog::default().with("t", vec!["a", "b"]);
        let options = ExtractorOptions::new().with_catalog(&catalog.catalog);
        let ops = extract_column_operations_with_options(
            &GenericDialect {},
            "SELECT b, * FROM t",
            options,
        )
        .unwrap()
        .remove(0)
        .unwrap();
        let names: Vec<&str> = ops
            .reads
            .iter()
            .map(|r| r.reference.name.value.as_str())
            .collect();
        assert_eq!(names, ["b", "a", "b"]);
    }

    #[test]
    fn each_wildcard_occurrence_expands_separately() {
        // Occurrence-based reads: `SELECT *, *` reads every column twice —
        // one expansion per written wildcard, exactly like writing the
        // column list out twice. (The SQL standard allows a bare `*` only as
        // the whole select list, and engines vary on `*, *` — but sqlparser
        // accepts it and this crate analyses what's written; the double
        // occurrence is the tersest pin of the per-occurrence contract.)
        let catalog = TestCatalog::default().with("t", vec!["a"]);
        assert_column_ops_with_catalog(
            "SELECT *, * FROM t",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![expanded_read("t", "a"), expanded_read("t", "a")],
                writes: vec![],
                lineage: vec![
                    passthrough(expanded_read("t", "a"), expanded_out("a", 0)),
                    passthrough(expanded_read("t", "a"), expanded_out("a", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn bare_wildcard_expands_a_multi_relation_scope() {
        // Every relation known → concatenated in FROM order, each block in
        // schema order.
        let catalog = TestCatalog::default()
            .with("t", vec!["a"])
            .with("u", vec!["b"]);
        assert_column_ops_with_catalog(
            "SELECT * FROM t JOIN u ON t.a = u.b",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    expanded_read("t", "a"),
                    expanded_read("u", "b"),
                    read_with_ref(cataloged_table("t"), "a", ResolutionKind::Cataloged),
                    read_with_ref(cataloged_table("u"), "b", ResolutionKind::Cataloged),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(expanded_read("t", "a"), expanded_out("a", 0)),
                    passthrough(expanded_read("u", "b"), expanded_out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn expanded_output_is_clause_visible_as_identity() {
        // An expanded column behaves like a written `SELECT a`: `GROUP BY a`
        // falls through to the real column (an identity output — another
        // read), not a phantom `Derived` alias.
        let catalog = TestCatalog::default().with("t", vec!["a"]);
        assert_column_ops_with_catalog(
            "SELECT * FROM t GROUP BY a",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    expanded_read("t", "a"),
                    read_with_ref(cataloged_table("t"), "a", ResolutionKind::Cataloged),
                ],
                writes: vec![],
                lineage: vec![passthrough(expanded_read("t", "a"), expanded_out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ordinal_resolves_through_expanded_outputs() {
        // `GROUP BY 1` names the first *output* — determinate once the
        // wildcard expands, so the ordinal falls through to the real column
        // (an identity re-read), exactly like `GROUP BY a`.
        let catalog = TestCatalog::default().with("t", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "SELECT * FROM t GROUP BY 1",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    expanded_read("t", "a"),
                    expanded_read("t", "b"),
                    expanded_read("t", "a"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(expanded_read("t", "a"), expanded_out("a", 0)),
                    passthrough(expanded_read("t", "b"), expanded_out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn a_subquery_wildcard_expands_its_own_scope_only() {
        // SQL scoping: a subquery's `*` covers the subquery's own FROM, never
        // the enclosing relations — the correlated `t.a` stays a plain filter
        // read, and `t`'s columns don't leak into the expansion.
        let catalog = TestCatalog::default()
            .with("t", vec!["a", "b"])
            .with("s", vec!["x", "y"]);
        assert_column_ops_with_catalog(
            "SELECT (SELECT * FROM s WHERE s.x = t.a) FROM t",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    expanded_read("s", "x"),
                    expanded_read("s", "y"),
                    read_with_ref(cataloged_table("s"), "x", ResolutionKind::Cataloged),
                    read_with_ref(cataloged_table("t"), "a", ResolutionKind::Cataloged),
                ],
                writes: vec![],
                // A scalar subquery projects its first output — s.x flows into
                // the (anonymous) outer value; the wildcard's second slot has
                // no outer consumer.
                lineage: vec![transformation(expanded_read("s", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn expansion_respects_the_dialect_column_fold() {
        // ClickHouse folds neither case: the expanded (quoted) columns still
        // pair with a same-case clause reference (`GROUP BY a` is an identity
        // re-read), while a wrong-case `GROUP BY A` matches nothing — the
        // sensitive fold applies to expanded outputs exactly as to written
        // ones.
        use sql_insight::sqlparser::dialect::ClickHouseDialect;
        let catalog = Catalog::new().table(CatalogTable::unqualified("t").columns(["a", "b"]));
        let ops = |sql: &str| {
            extract_column_operations_with_options(
                &ClickHouseDialect {},
                sql,
                ExtractorOptions::new().with_catalog(&catalog),
            )
            .unwrap()
            .remove(0)
            .unwrap()
        };
        let same_case = ops("SELECT * FROM t GROUP BY a");
        let key = same_case.reads.last().unwrap();
        assert_eq!(key.reference.name.value, "a");
        assert_eq!(key.resolution, ResolutionKind::Cataloged);
        let wrong_case = ops("SELECT * FROM t GROUP BY A");
        let key = wrong_case.reads.last().unwrap();
        assert_eq!(key.reference.name.value, "A");
        assert_eq!(key.resolution, ResolutionKind::Unresolved);
    }

    #[test]
    fn pipe_select_wildcard_expands() {
        // `|> SELECT *` binds through the same item path as a projection.
        let catalog = TestCatalog::default().with("t", vec!["a"]);
        assert_column_ops_with_catalog(
            "FROM t |> SELECT *",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![expanded_read("t", "a")],
                writes: vec![],
                lineage: vec![passthrough(expanded_read("t", "a"), expanded_out("a", 0))],
                diagnostics: vec![],
            },
        );
    }
}

mod derived_relations {
    use super::*;

    #[test]
    fn wildcard_expands_a_derived_table_without_a_catalog() {
        // The derived table's slot view comes from the SQL text alone.
        assert_column_ops(
            "SELECT * FROM (SELECT a, b + c AS s FROM t) d",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a"), col("t", "b"), col("t", "c")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t", "a"), out("a", 0)),
                    transformation(col("t", "b"), out("s", 1)),
                    transformation(col("t", "c"), out("s", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn anonymous_slot_expands_positionally_with_no_name() {
        // An unaliased expression occupies its position namelessly — the
        // expansion doesn't fabricate a (dialect-dependent) auto-name, it
        // surfaces `QueryOutput { name: None, position }`.
        assert_column_ops(
            "SELECT * FROM (SELECT a, b + c FROM t) d",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a"), col("t", "b"), col("t", "c")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t", "a"), out("a", 0)),
                    transformation(col("t", "b"), out_anon(1)),
                    transformation(col("t", "c"), out_anon(1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn duplicate_output_names_trace_by_position_not_name() {
        // The producer exposes `id` twice — legal under `*`. A name-keyed
        // trace would collapse both outputs onto the first `id`; the
        // expansion is positional, so each output keeps its own source.
        assert_column_ops(
            "SELECT * FROM (SELECT t1.id, t2.id FROM t1 JOIN t2 ON t1.k = t2.k) d",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    col("t1", "id"),
                    col("t2", "id"),
                    col("t1", "k"),
                    col("t2", "k"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "id"), out("id", 0)),
                    passthrough(col("t2", "id"), out("id", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn wildcard_expands_a_cte_reference() {
        assert_column_ops(
            "WITH c AS (SELECT a FROM t) SELECT * FROM c",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn wildcard_expands_an_unaliased_sole_derived_table() {
        // No alias → no qualifier for the trace to match, allowed only as
        // the scope's sole relation.
        assert_column_ops(
            "SELECT * FROM (SELECT a FROM t)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alias_column_list_names_an_anonymous_slot() {
        // `d(x, n)` renames positionally, giving the unaliased `count(*)` a
        // real name — the view is complete, so the wildcard expands.
        assert_column_ops(
            "SELECT * FROM (SELECT a, count(*) FROM t GROUP BY a) d(x, n)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a"), col("t", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("x", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn values_backed_slots_trace_into_cells() {
        // An expanded slot over a `VALUES` relation traces into the
        // like-positioned cell of every row: the literal cell contributes no
        // source (a constant column has no data dependency), the subquery
        // cell reaches its real column — no pseudo-column of `v` surfaces.
        assert_column_ops(
            "SELECT * FROM (VALUES (1, (SELECT max(y) FROM s))) AS v(a, b)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("s", "y")],
                writes: vec![],
                lineage: vec![transformation(col("s", "y"), out("b", 1))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn wildcard_expands_transitively_through_a_wildcard_body() {
        // The inner `*` expands from the catalog, which completes the derived
        // view — so the outer `*` expands through it. reads stay inner-only
        // (a derived slot is not a physical read).
        let catalog = TestCatalog::default().with("t", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "SELECT * FROM (SELECT * FROM t) d",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![expanded_read("t", "a"), expanded_read("t", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(expanded_read("t", "a"), expanded_out("a", 0)),
                    passthrough(expanded_read("t", "b"), expanded_out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn recursive_cte_self_reference_expands_from_the_anchor_shape() {
        // The provisional declaration carries the anchor's (complete) shape,
        // so the recursive branch's `SELECT * FROM c` expands; its
        // self-referential trace terminates on the active-set (the single
        // anchor edge remains).
        assert_column_ops(
            "WITH RECURSIVE c AS (SELECT a FROM t UNION ALL SELECT * FROM c) SELECT c.a FROM c",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn slot_trace_walks_filter_join_and_scan_dead_ends() {
        // The slot trace shares the named trace's walk: it passes the WHERE
        // `Filter`, descends both `Join` sides, dead-ends on the base-table
        // `Scan` (a scan claims no positional slot), and only the matching
        // `CteRef` descends into the producer.
        assert_column_ops(
            "WITH d AS (SELECT a, b FROM s) \
             SELECT d.* FROM u0 JOIN d ON u0.k = d.a WHERE u0.f > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("s", "a"), col("s", "b"), col("u0", "k"), col("u0", "f")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("s", "a"), out("a", 0)),
                    passthrough(col("s", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn slot_trace_passes_the_outer_group_by_aggregate() {
        // An outer GROUP BY layers an `Aggregate` between the projection and
        // the derived boundary — the slot trace passes through it (the
        // grouping key names an expanded output, a `Derived` clause ref, so
        // it adds no read).
        assert_column_ops(
            "SELECT * FROM (SELECT a FROM t) d GROUP BY a",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn sibling_derived_tables_expand_without_cross_claims() {
        // Each relation's slots descend only through the alias-matching
        // boundary — the walk visits the sibling `SubqueryAlias` and rejects
        // it on the qualifier, so no crossed edges.
        assert_column_ops(
            "SELECT * FROM (SELECT a FROM t) x, (SELECT b FROM u) y",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a"), col("u", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t", "a"), out("a", 0)),
                    passthrough(col("u", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualified_slots_skip_an_unaliased_sibling_producer() {
        // `y.*` expands even though an *unaliased* derived sibling sits in
        // scope (the sole-relation rule restricts only the unaliased
        // relation's own expansion). Its inline `Projection` is reached on
        // the walk and rejected — a qualified slot never claims a bare
        // producer — so `y`'s slots reach only `u`.
        assert_column_ops(
            "SELECT y.* FROM (SELECT a FROM t), (SELECT b FROM u) AS y",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a"), col("u", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("u", "b"), out("b", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unaliased_derived_with_leading_with_expands() {
        // An unaliased derived table binds its body inline — a leading WITH
        // included. The slot trace registers the `With`'s declarations on
        // the way down, so the body's CTE reference resolves.
        assert_column_ops(
            "SELECT * FROM (WITH w AS (SELECT a FROM t) SELECT a FROM w)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unaliased_values_slots_trace_into_cells() {
        // An unaliased `(VALUES …)` binds its row set inline (no
        // `SubqueryAlias` boundary), so the slot trace claims the `Values`
        // node directly — the subquery cell reaches its real column, the
        // literal cell contributes nothing, and both slots stay anonymous.
        assert_column_ops(
            "SELECT * FROM (VALUES ((SELECT max(y) FROM s), 2))",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("s", "y")],
                writes: vec![],
                lineage: vec![transformation(col("s", "y"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn values_backed_cte_slots_trace_into_cells() {
        // Same through the `CteRef` boundary: the expansion succeeds (the
        // slot view is complete — no diagnostic), and the constant cells
        // simply contribute no lineage, exactly like `SELECT 1 AS a`.
        assert_column_ops(
            "WITH v (a, b) AS (VALUES (1, 2)) SELECT * FROM v",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn one_cte_expands_under_each_alias() {
        // Two references to one CTE: each alias's `*` expands its own slots
        // and the trace descends only through the matching `CteRef` (no
        // duplicated / crossed edges); the body's read is still counted once
        // at the declaration.
        assert_column_ops(
            "WITH c AS (SELECT a FROM t) SELECT * FROM c x JOIN c y",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t", "a"), out("a", 0)),
                    passthrough(col("t", "a"), out("a", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_alias_list_renames_an_expanded_wildcard_body() {
        // The CTE's own `*` expands from the catalog; the declared `(c1, c2)`
        // list renames the slots positionally, so the outer `*` exposes the
        // declared names while lineage still reaches the base columns.
        let catalog = TestCatalog::default().with("t", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "WITH c (c1, c2) AS (SELECT * FROM t) SELECT * FROM c",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![expanded_read("t", "a"), expanded_read("t", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(expanded_read("t", "a"), out("c1", 0)),
                    passthrough(expanded_read("t", "b"), out("c2", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn anonymous_only_derived_expands_silently() {
        // `(SELECT 1)` exposes one anonymous, constant slot: the expansion
        // succeeds (no diagnostic — the shape is fully known), and the slot
        // contributes no read or lineage (nothing to trace to).
        assert_column_ops(
            "SELECT * FROM (SELECT 1)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn a_suppressed_branch_expands_the_other_but_poisons_completeness() {
        // Expansion is per-wildcard, per-branch-scope: the cataloged `s`
        // branch expands even though the unknown `u` branch's `*` stays
        // suppressed (one flag). The suppressed branch simply projects no
        // slots, so only the expanded branch feeds the result columns.
        let catalog = TestCatalog::default().with("s", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "SELECT * FROM s UNION ALL SELECT * FROM u",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![expanded_read("s", "a"), expanded_read("s", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(expanded_read("s", "a"), expanded_out("a", 0)),
                    passthrough(expanded_read("s", "b"), expanded_out("b", 1)),
                ],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
        // Completeness composes across branches (left AND right): one
        // suppressed branch makes the whole set operation's slot count
        // indeterminate, so a wildcard over it as a derived table stays
        // suppressed too (second flag) — the expanded left branch doesn't
        // vouch for the union.
        assert_column_ops_with_catalog(
            "SELECT * FROM (SELECT * FROM s UNION ALL SELECT * FROM u) d",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![expanded_read("s", "a"), expanded_read("s", "b")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![
                    diag(ColumnLevelDiagnosticKind::WildcardSuppressed),
                    diag(ColumnLevelDiagnosticKind::WildcardSuppressed),
                ],
            },
        );
    }

    #[test]
    fn union_branches_expand_and_merge_positionally() {
        let catalog = TestCatalog::default().with("s", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "SELECT * FROM s UNION ALL SELECT p, q FROM u",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    expanded_read("s", "a"),
                    expanded_read("s", "b"),
                    col("u", "p"),
                    col("u", "q"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(expanded_read("s", "a"), expanded_out("a", 0)),
                    passthrough(expanded_read("s", "b"), expanded_out("b", 1)),
                    passthrough(col("u", "p"), expanded_out("a", 0)),
                    passthrough(col("u", "q"), expanded_out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }
}

mod dml_pairing {
    use super::*;

    /// A registered (`public`) write target's column write.
    fn write_confirmed(table_name: &str, col: &str) -> ColumnWrite {
        ColumnWrite {
            reference: ColumnReference {
                table: Some(cataloged_table(table_name)),
                name: col.into(),
            },
            resolution: ResolutionKind::Cataloged,
        }
    }

    #[test]
    fn insert_pairs_an_expanded_wildcard_source_positionally() {
        // The expanded source has a determinate count, so `source_wildcard`
        // stays off and the positional pairing proceeds.
        let catalog = TestCatalog::default().with("s", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "INSERT INTO t (x, y) SELECT * FROM s",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![expanded_read("s", "a"), expanded_read("s", "b")],
                writes: vec![write("t", "x"), write("t", "y")],
                lineage: vec![
                    passthrough(expanded_read("s", "a"), relation("t", "x")),
                    passthrough(expanded_read("s", "b"), relation("t", "y")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_arity_mismatch_against_an_expanded_source_is_flagged() {
        // Schema drift stays honest: the expanded count disagrees with the
        // explicit list, so the mismatch diagnostic fires (and pairing zips
        // to the shorter side).
        let catalog = TestCatalog::default().with("s", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "INSERT INTO t (x) SELECT * FROM s",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![expanded_read("s", "a"), expanded_read("s", "b")],
                writes: vec![write("t", "x")],
                lineage: vec![passthrough(expanded_read("s", "a"), relation("t", "x"))],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsArityMismatch)],
            },
        );
    }

    #[test]
    fn ctas_pairs_an_expanded_wildcard_source() {
        // The created relation inherits the expanded (canonical) names; its
        // columns aren't in any catalog yet, so the writes are `Inferred`.
        let catalog = TestCatalog::default().with("s", vec!["a", "b"]);
        let quoted_write = |col: &str| ColumnWrite {
            reference: ColumnReference {
                table: Some(table("d")),
                name: qident(col),
            },
            resolution: ResolutionKind::Inferred,
        };
        let quoted_relation = |col: &str| ColumnTarget::Relation(quoted_write(col));
        assert_column_ops_with_catalog(
            "CREATE TABLE d AS SELECT * FROM s",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![expanded_read("s", "a"), expanded_read("s", "b")],
                writes: vec![quoted_write("a"), quoted_write("b")],
                lineage: vec![
                    passthrough(expanded_read("s", "a"), quoted_relation("a")),
                    passthrough(expanded_read("s", "b"), quoted_relation("b")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_source_wildcard_expands_through_a_trailing_order_by() {
        // A trailing ORDER BY layers a `Sort` over the source projection —
        // the pairing peels it, and the sort key is a plain filter read.
        let catalog = TestCatalog::default().with("s", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "INSERT INTO t (x, y) SELECT * FROM s ORDER BY a",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![
                    expanded_read("s", "a"),
                    expanded_read("s", "b"),
                    read_with_ref(cataloged_table("s"), "a", ResolutionKind::Cataloged),
                ],
                writes: vec![write("t", "x"), write("t", "y")],
                lineage: vec![
                    passthrough(expanded_read("s", "a"), relation("t", "x")),
                    passthrough(expanded_read("s", "b"), relation("t", "y")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn columnless_self_insert_expands_and_self_pairs() {
        // `INSERT INTO t SELECT * FROM t`: the expanded source determines the
        // arity, so the column-less target fills from the catalog — every
        // column pairs with itself (`t → t`), and the target reads through
        // its own source scan.
        let catalog = TestCatalog::default().with("t", vec!["a", "b"]);
        let quoted_write = |col: &str| ColumnWrite {
            reference: ColumnReference {
                table: Some(cataloged_table("t")),
                name: qident(col),
            },
            resolution: ResolutionKind::Cataloged,
        };
        assert_column_ops_with_catalog(
            "INSERT INTO t SELECT * FROM t",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![expanded_read("t", "a"), expanded_read("t", "b")],
                writes: vec![quoted_write("a"), quoted_write("b")],
                lineage: vec![
                    passthrough(
                        expanded_read("t", "a"),
                        ColumnTarget::Relation(quoted_write("a")),
                    ),
                    passthrough(
                        expanded_read("t", "b"),
                        ColumnTarget::Relation(quoted_write("b")),
                    ),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_from_returning_wildcard_expands_target_then_from() {
        // The UPDATE RETURNING scope is the target plus the FROM relations
        // (a written `RETURNING s.x` resolves there), so `*` expands both —
        // target block first, mirroring the scope order.
        let catalog = TestCatalog::default()
            .with("t", vec!["a", "b"])
            .with("s", vec!["x", "y"]);
        assert_column_ops_with_catalog(
            "UPDATE t SET a = 1 FROM s RETURNING *",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![
                    expanded_read("t", "a"),
                    expanded_read("t", "b"),
                    expanded_read("s", "x"),
                    expanded_read("s", "y"),
                ],
                writes: vec![write_confirmed("t", "a")],
                lineage: vec![
                    passthrough(expanded_read("t", "a"), expanded_out("a", 0)),
                    passthrough(expanded_read("t", "b"), expanded_out("b", 1)),
                    passthrough(expanded_read("s", "x"), expanded_out("x", 2)),
                    passthrough(expanded_read("s", "y"), expanded_out("y", 3)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn delete_using_returning_wildcard_expands_target_then_using() {
        // `RETURNING *` on `DELETE … USING` yields the target's columns
        // first, then the USING relations' — matching PostgreSQL (verified on
        // 17: `a | b | k | x`). The binder still *binds* USING first (a
        // target alias must resolve against it); only the clause scope is
        // target-first.
        let catalog = TestCatalog::default()
            .with("t", vec!["a", "b"])
            .with("s", vec!["x", "y"]);
        assert_column_ops_with_catalog(
            "DELETE FROM t USING s WHERE t.a = s.x RETURNING *",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![
                    read_with_ref(cataloged_table("t"), "a", ResolutionKind::Cataloged),
                    read_with_ref(cataloged_table("s"), "x", ResolutionKind::Cataloged),
                    expanded_read("t", "a"),
                    expanded_read("t", "b"),
                    expanded_read("s", "x"),
                    expanded_read("s", "y"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(expanded_read("t", "a"), expanded_out("a", 0)),
                    passthrough(expanded_read("t", "b"), expanded_out("b", 1)),
                    passthrough(expanded_read("s", "x"), expanded_out("x", 2)),
                    passthrough(expanded_read("s", "y"), expanded_out("y", 3)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn returning_wildcard_expands_over_the_target() {
        // `RETURNING *` projects the written relation — the expansion makes
        // the target's own data referenced, so it reads (exactly as a written
        // `RETURNING a, b` would).
        let catalog = TestCatalog::default().with("t", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "INSERT INTO t (a) VALUES (1) RETURNING *",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![expanded_read("t", "a"), expanded_read("t", "b")],
                writes: vec![write_confirmed("t", "a")],
                lineage: vec![
                    passthrough(expanded_read("t", "a"), expanded_out("a", 0)),
                    passthrough(expanded_read("t", "b"), expanded_out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }
}

mod guards {
    use super::*;

    #[test]
    fn merge_columns_suppress_a_bare_wildcard() {
        // The standard coalesces `USING` merge columns into join-structure-
        // dependent positions the flat scope no longer encodes — a bare `*`
        // stays unexpanded even with both sides cataloged.
        let catalog = TestCatalog::default()
            .with("t", vec!["k", "x"])
            .with("u", vec!["k", "y"]);
        assert_column_ops_with_catalog(
            "SELECT * FROM t JOIN u USING (k)",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn qualified_wildcard_is_unaffected_by_merge_columns() {
        // `t.*` never coalesces (the standard keeps every own column,
        // including its copy of `k`), so it expands under the same join.
        let catalog = TestCatalog::default()
            .with("t", vec!["k", "x"])
            .with("u", vec!["k", "y"]);
        assert_column_ops_with_catalog(
            "SELECT t.* FROM t JOIN u USING (k)",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![expanded_read("t", "k"), expanded_read("t", "x")],
                writes: vec![],
                lineage: vec![
                    passthrough(expanded_read("t", "k"), expanded_out("k", 0)),
                    passthrough(expanded_read("t", "x"), expanded_out("x", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn an_unknown_relation_suppresses_the_bare_wildcard() {
        // All-or-nothing: one catalog-unknown relation in scope makes the
        // whole `*` indeterminate — no partial expansion.
        let catalog = TestCatalog::default().with("t", vec!["a"]);
        assert_column_ops_with_catalog(
            "SELECT * FROM t, unknown_u",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn a_wildcard_modifier_suppresses_expansion() {
        // `EXCLUDE` (and every other wildcard modifier) changes the expanded
        // set — ignoring it would misreport reads / lineage, so the wildcard
        // stays unexpanded even with the catalog present.
        let catalog = TestCatalog::default().with("t", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "SELECT * EXCLUDE (a) FROM t",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn an_incomplete_derived_view_suppresses_the_outer_wildcard() {
        // Catalog-free, the inner `*` can't expand — the derived view's slot
        // count is indeterminate, so the outer `*` stays suppressed too (two
        // flags: one per wildcard).
        assert_column_ops(
            "SELECT * FROM (SELECT * FROM t) d",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![
                    diag(ColumnLevelDiagnosticKind::WildcardSuppressed),
                    diag(ColumnLevelDiagnosticKind::WildcardSuppressed),
                ],
            },
        );
    }

    #[test]
    fn an_anonymous_slot_blocks_nothing_but_a_table_function_does() {
        // An anonymous output is expandable (positionally); an opaque table
        // function's shape is unknown, so its presence suppresses a bare `*`.
        assert_column_ops(
            "SELECT * FROM UNNEST(x) AS u",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![unresolved("x")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn an_ilike_modifier_suppresses_expansion() {
        // Snowflake `* ILIKE 'pattern'` filters the expanded set by name —
        // unmodelled, so the wildcard stays unexpanded (same guard family as
        // EXCLUDE).
        use sql_insight::sqlparser::dialect::SnowflakeDialect;
        let catalog = Catalog::new().table(CatalogTable::unqualified("T").columns(["A", "B"]));
        let ops = extract_column_operations_with_options(
            &SnowflakeDialect {},
            "SELECT * ILIKE 'a%' FROM t",
            ExtractorOptions::new().with_catalog(&catalog),
        )
        .unwrap()
        .remove(0)
        .unwrap();
        assert!(
            ops.reads.is_empty(),
            "suppressed → no reads: {:?}",
            ops.reads
        );
        assert_eq!(
            ops.diagnostics
                .iter()
                .map(|d| d.kind.clone())
                .collect::<Vec<_>>(),
            vec![ColumnLevelDiagnosticKind::WildcardSuppressed]
        );
    }

    #[test]
    fn duplicate_derived_aliases_suppress_the_bare_wildcard() {
        // Illegal SQL the parser accepts: two derived relations exposing the
        // same name. A derived slot finds its producer *by that name* during
        // the trace, so expanding would collect the other relation's origins
        // too (crossed edges) — refuse rather than mis-attribute. The inner
        // producers' own reads still surface.
        assert_column_ops(
            "WITH d AS (SELECT a FROM t) SELECT * FROM d, (SELECT b FROM u) AS d",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![col("t", "a"), col("u", "b")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn an_ambiguous_qualifier_suppresses_the_qualified_wildcard() {
        // Two relations answer to `x` — the qualifier doesn't pin one, so
        // `x.*` stays unexpanded.
        let catalog = TestCatalog::default().with("t", vec!["a"]);
        assert_column_ops_with_catalog(
            "SELECT x.* FROM t AS x, t AS x",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }
}
