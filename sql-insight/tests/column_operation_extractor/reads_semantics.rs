use crate::support::*;

mod reads {
    use super::*;

    /// The public surfaces are returned in **source order** (by each read's
    /// written token span), independent of the internal walk. Walk order would
    /// surface the ORDER BY key first (its `Sort` is the outermost node); the
    /// facade re-sorts, so reads come back projection `a` (col 8), WHERE `c`
    /// (col 23), ORDER BY `b` (col 38).
    #[test]
    fn reads_are_returned_in_source_order() {
        let ops = extract("SELECT a FROM t WHERE c > 0 ORDER BY b");
        let names: Vec<&str> = ops
            .reads
            .iter()
            .map(|r| r.reference.name.value.as_str())
            .collect();
        assert_eq!(names, ["a", "c", "b"]);
    }

    #[test]
    fn qualified_select_collects_qualified_reads() {
        assert_column_ops(
            "SELECT t1.a, t1.b FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualified_join_collects_reads_from_both_sides() {
        // Resolver walks FROM (including JOIN ON) before the projection,
        // so the predicate columns appear ahead of the projected ones —
        // and are tagged Filter while projection refs are Projection.
        assert_column_ops(
            "SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.id = t2.id",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "id"),
                    read("t2", "id"),
                    read("t1", "a"),
                    read("t2", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualified_ref_through_alias_resolves_to_real_table() {
        // `u` is an alias of `t1`; the qualified ref `u.a` resolves
        // to the alias-free real table `t1`, matching how an
        // unqualified ref resolves. Alias is use-site decoration,
        // not part of the column's identity.
        assert_column_ops(
            "SELECT u.a FROM t1 AS u",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualified_refs_through_aliases_on_both_join_sides_resolve_to_real_tables() {
        // Implicit aliases (`t1 a`, `t2 b`) on both join sides; every
        // qualified ref canonicalizes to its real table. JOIN ON is
        // walked during FROM, so the predicate reads precede the
        // projection reads.
        assert_column_ops(
            "SELECT a.x, b.y FROM t1 a JOIN t2 b ON a.id = b.id",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "id"),
                    read("t2", "id"),
                    read("t1", "x"),
                    read("t2", "y"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "x"), out("x", 0)),
                    passthrough(col("t2", "y"), out("y", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aliased_filter_ref_resolves_to_real_table_and_stays_out_of_lineage() {
        // A WHERE-only column through an alias resolves to the real
        // table for `reads`, but a filter column is not a value
        // contributor, so it never appears in `lineage`.
        assert_column_ops(
            "SELECT u.a FROM t1 AS u WHERE u.b > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn schema_qualified_ref_resolves_to_schema_dot_table() {
        let table_ref = TableReference {
            catalog: None,
            schema: Some("s1".into()),
            name: "t1".into(),
        };
        assert_column_ops(
            "SELECT s1.t1.a FROM s1.t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_with_ref(
                    table_ref.clone(),
                    "a",
                    ResolutionKind::Inferred,
                )],
                writes: vec![],
                lineage: vec![passthrough(
                    read_with_ref(table_ref, "a", ResolutionKind::Inferred),
                    out("a", 0),
                )],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_qualified_ref_resolves_to_catalog_dot_schema_dot_table() {
        // `c1.s1.t1.a` — 4-part ref. parts.last() is the column;
        // the preceding 3 parts decode into TableReference's
        // catalog / schema / name fields.
        let table_ref = TableReference {
            catalog: Some("c1".into()),
            schema: Some("s1".into()),
            name: "t1".into(),
        };
        assert_column_ops(
            "SELECT c1.s1.t1.a FROM c1.s1.t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_with_ref(
                    table_ref.clone(),
                    "a",
                    ResolutionKind::Inferred,
                )],
                writes: vec![],
                lineage: vec![passthrough(
                    read_with_ref(table_ref, "a", ResolutionKind::Inferred),
                    out("a", 0),
                )],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unqualified_ref_against_catalog_qualified_table_inherits_full_qualifier() {
        // `SELECT a FROM c1.s1.t1` — the unqualified `a` resolves
        // to the catalog-qualified binding, picking up the full
        // qualifier in the ColumnReference.
        let table_ref = TableReference {
            catalog: Some("c1".into()),
            schema: Some("s1".into()),
            name: "t1".into(),
        };
        assert_column_ops(
            "SELECT a FROM c1.s1.t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_with_ref(
                    table_ref.clone(),
                    "a",
                    ResolutionKind::Inferred,
                )],
                writes: vec![],
                lineage: vec![passthrough(
                    read_with_ref(table_ref, "a", ResolutionKind::Inferred),
                    out("a", 0),
                )],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn five_part_ref_overshoots_qualifier_decoder_and_is_unresolved() {
        // sqlparser parses `extra.c1.s1.t1.a` into 5 parts. The
        // qualifier decoder caps at 3 parts (catalog / schema /
        // name) — anything longer is a struct-field access on a
        // fully qualified column, which we don't model. The ref
        // is recorded with `table: None`.
        assert_column_ops(
            "SELECT extra.c1.s1.t1.a FROM c1.s1.t1",
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
    fn where_predicate_qualified_ref_is_a_read() {
        assert_column_ops(
            "SELECT t1.a FROM t1 WHERE t1.b > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unqualified_single_table_resolves_to_that_table() {
        assert_column_ops(
            "SELECT a, b FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unqualified_in_where_resolves_to_single_table() {
        assert_column_ops(
            "SELECT a FROM t1 WHERE b > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unqualified_with_multiple_tables_is_ambiguous() {
        // Two `Unknown`-schema tables — without a catalog the
        // resolver can't pick between them, so `a` surfaces with
        // `table: None` and `ResolutionKind::Ambiguous`. The lineage
        // source inherits the same ambiguous read.
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

    #[test]
    fn unqualified_uses_alias_binding_but_returns_real_table() {
        // Alias is just a binding key; the resolver returns the
        // alias-free TableReference of the binding's underlying table.
        assert_column_ops(
            "SELECT a FROM t1 AS u",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_ref_does_not_surface_in_reads() {
        // The outer `id` resolves to the cte binding (a synthetic
        // intermediate, not real storage), so it's dropped from reads.
        // Reads surface only references with real Table owners or
        // unresolved column names. `unknown_col` doesn't match the
        // cte's Known schema [id], so it surfaces unresolved
        // (table: None) with ResolutionKind::Unresolved on the read.
        assert_column_ops(
            "WITH cte AS (SELECT id FROM t1) SELECT id, unknown_col FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), unresolved("unknown_col")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "id"), out("id", 0)),
                    passthrough(unresolved("unknown_col"), out("unknown_col", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn derived_table_ref_does_not_surface_in_reads() {
        // Outer `id` resolves to derived alias `d` — synthetic, dropped.
        // Only the inner SELECT's t1.id is a real read.
        assert_column_ops(
            "SELECT id FROM (SELECT id FROM t1) AS d",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unqualified_inner_scope_shadows_outer() {
        // Inner subquery has its own t2 in scope; the unqualified `y`
        // inside the IN-subquery resolves to t2 even though t1 is
        // also in the outer scope. Standard SQL inner-shadows-outer.
        // The predicate subquery emits no lineage (it feeds a filter);
        // it still surfaces its refs in reads. The outer `*` is a
        // suppressed wildcard, so there is no lineage at all.
        assert_column_ops(
            "SELECT * FROM t1 WHERE id IN (SELECT id FROM t2 WHERE y > 0)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t2", "id"), read("t2", "y")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn unqualified_correlated_walks_to_outer_when_inner_has_no_candidate() {
        // Inner CTE has Known schema [zz]; `outer_col` doesn't fit it,
        // so resolution walks to the outer scope and picks the t1
        // (Unknown) binding. The predicate subquery emits no lineage;
        // the outer `*` is a suppressed wildcard, so no lineage at all.
        assert_column_ops(
            "SELECT * FROM t1 WHERE id IN (\
            WITH inner_cte AS (SELECT zz FROM t1) \
            SELECT zz FROM inner_cte WHERE outer_col > 0)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t1", "zz"), read("t1", "outer_col")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }
}

// Columns from every clause (projection / WHERE / GROUP BY /
// ORDER BY / OVER / CASE / HAVING / …) surface in `reads` as plain
// occurrence entries — `reads` no longer tags a syntactic clause.
// These tests pin down WHICH refs surface (occurrence-based, dups
// kept) and the lineage they produce.
mod reads_by_clause {
    use super::*;

    #[test]
    fn same_column_in_projection_and_where_is_two_reads() {
        // The two textual `a` references each get their own `reads`
        // entry (occurrence-based — duplicates are kept).
        assert_column_ops(
            "SELECT a FROM t1 WHERE a > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn predicate_subquery_surfaces_reads_but_no_lineage() {
        // The IN-subquery feeds a filter, so it emits NO lineage
        // (Option B: nested subqueries resolve raw, no intermediate
        // QueryOutput edge). Its refs (s.id, s.flag) still surface
        // in reads. Only the outer projection `a` contributes a lineage edge.
        assert_column_ops(
            "SELECT a FROM t WHERE id IN (SELECT id FROM s WHERE flag = 1)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t", "a"),
                    read("t", "id"),
                    read("s", "id"),
                    read("s", "flag"),
                ],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn scalar_subquery_in_projection_feeds_only_outer() {
        // `SELECT a, (SELECT max(x) FROM s) AS m FROM t`:
        //  - the scalar subquery does NOT emit its own QueryOutput
        //    edge (Option B: raw resolve). Its source `s.x` is
        //    captured by the enclosing projection item, which emits
        //    the single meaningful edge `s.x → out("m", 1)`,
        //    Transformation (the item is a subquery expression).
        //  - `a` is a plain passthrough at position 0.
        assert_column_ops(
            "SELECT a, (SELECT max(x) FROM s) AS m FROM t",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a"), read("s", "x")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t", "a"), out("a", 0)),
                    transformation(col("s", "x"), out("m", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn is_null_predicate_ref_surfaces_as_read() {
        // `WHERE x IS NULL` — x surfaces in reads like any other
        // WHERE ref; it is not a lineage source (predicate-only).
        assert_column_ops(
            "SELECT a FROM t1 WHERE b IS NULL",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn is_not_null_predicate_ref_surfaces_as_read() {
        assert_column_ops(
            "SELECT a FROM t1 WHERE b IS NOT NULL",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_ref_surfaces_as_read() {
        assert_column_ops(
            "SELECT a, COUNT(*) FROM t1 GROUP BY a",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn order_by_ref_surfaces_as_read() {
        assert_column_ops(
            "SELECT a FROM t1 ORDER BY b",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_key_not_projected_is_filter() {
        // `g` is used only as a partition key — it appears in
        // reads but doesn't flow to the output. `x` is the
        // aggregated value (lineage source).
        assert_column_ops(
            "SELECT SUM(x) FROM t1 GROUP BY g",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "g")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn distinct_on_key_is_filter() {
        // DISTINCT ON (k) chooses which row of each duplicate
        // group survives. `k` decides; it doesn't flow as value.
        // (Qualified `t1.k` so it resolves even though the walker
        // visits DISTINCT ON before binding FROM tables.)
        assert_column_ops(
            "SELECT DISTINCT ON (t1.k) a FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "k"), read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn limit_subquery_column_is_filter() {
        // LIMIT (SELECT n FROM cfg) — the subquery's value
        // determines the row count, but `cfg.n` itself doesn't
        // flow to the output.
        assert_column_ops(
            "SELECT a FROM t1 LIMIT (SELECT n FROM cfg)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("cfg", "n")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_and_having_refs_both_surface() {
        // `a` (projection + GROUP BY) and `b` (HAVING) all surface.
        // Walk order: projection → HAVING → GROUP BY (the visitor
        // hits HAVING before GROUP BY), so the read order reflects
        // that, not the textual SQL order.
        assert_column_ops(
            "SELECT a FROM t1 GROUP BY a HAVING SUM(b) > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b"), read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_rollup_modifier_refs_surface() {
        assert_column_ops(
            "SELECT a, b FROM t1 GROUP BY ROLLUP(a, b)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "a"),
                    read("t1", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_cube_modifier_refs_surface() {
        assert_column_ops(
            "SELECT a, b FROM t1 GROUP BY CUBE(a, b)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "a"),
                    read("t1", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_grouping_sets_walks_each_set_member() {
        // GROUPING SETS ((a, b), (a), ()) — every named column
        // inside any set surfaces as a read. The empty set
        // contributes nothing.
        assert_column_ops(
            "SELECT a, b FROM t1 GROUP BY GROUPING SETS ((a, b), (a), ())",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "a"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_mixed_plain_and_rollup_collects_both() {
        // `GROUP BY a, ROLLUP(b, c)` — `a` is a plain GROUP BY ref;
        // `b`, `c` are inside the ROLLUP expression. All three
        // surface as reads.
        assert_column_ops(
            "SELECT a, b, c FROM t1 GROUP BY a, ROLLUP(b, c)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "c"),
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "c"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                    passthrough(col("t1", "c"), out("c", 2)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn subquery_in_group_by_surfaces_reads_but_no_inner_lineage() {
        // GROUP BY (SELECT z FROM s) — the subquery's `z` surfaces in
        // reads, but the subquery emits no lineage (Option B: raw
        // resolve, no intermediate QueryOutput). Only the outer
        // projection `a` contributes a lineage edge.
        assert_column_ops(
            "SELECT a FROM t GROUP BY (SELECT z FROM s)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a"), read("s", "z")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn case_in_projection_refs_surface_and_transform() {
        // All three columns surface in `reads`, but only THEN (`b`)
        // and ELSE (`c`) carry value to the output. The WHEN
        // condition (`a`) is a predicate — it decides which
        // result is selected, its own value doesn't flow.
        assert_column_ops(
            "SELECT CASE WHEN a > 0 THEN b ELSE c END FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b"), read("t1", "c")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "b"), out_anon(0)),
                    transformation(col("t1", "c"), out_anon(0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn case_in_where_refs_surface_as_reads() {
        // The CASE sits in WHERE: its condition (`x`) and results
        // (`y`, `z`) surface as reads (not lineage sources — the CASE
        // feeds a predicate). `b` is the outer projection.
        assert_column_ops(
            "SELECT b FROM t WHERE CASE WHEN x > 0 THEN y ELSE z END = 1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t", "b"),
                    read("t", "x"),
                    read("t", "y"),
                    read("t", "z"),
                ],
                writes: vec![],
                lineage: vec![passthrough(col("t", "b"), out("b", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn scalar_subquery_in_case_condition_contributes_no_lineage() {
        // The scalar subquery lives entirely inside the CASE WHEN
        // cond, which is a predicate. Inner refs (`s.x` from its
        // projection, `s.y` from its WHERE) are tagged
        // `is_lineage_source = false` at capture and dropped from the outer
        // projection's `source_refs`. The output is `1 / NULL`,
        // determined by the subquery's IS NULL test — no inner
        // column's value flows out.
        assert_column_ops(
            "SELECT CASE WHEN (SELECT x FROM s WHERE y > 0) IS NULL THEN 1 END FROM t",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn exists_subquery_inside_projection_case_excludes_inner() {
        // The EXISTS subquery sits in the CASE WHEN cond — a
        // doubly-predicate position. `x.id` is in the EXISTS
        // subquery's WHERE, `s.flag` in the EXISTS subquery's
        // projection — both `is_lineage_source = false`. THEN / ELSE
        // carry values from `s.a` / `s.b`, which flow.
        assert_column_ops(
            "SELECT \
             CASE WHEN EXISTS (SELECT s.flag FROM x WHERE x.id = s.id) THEN s.a ELSE s.b END \
         FROM s",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("s", "flag"),
                    read("x", "id"),
                    read("s", "id"),
                    read("s", "a"),
                    read("s", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    transformation(col("s", "a"), out_anon(0)),
                    transformation(col("s", "b"), out_anon(0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn simple_case_operand_and_results_surface() {
        // `CASE x WHEN 1 THEN a WHEN 2 THEN b END` — `x` is the
        // value being matched (predicate), `1` and `2` are literal
        // patterns, `a` and `b` are the values that flow when the
        // match succeeds. All three columns appear in reads; only
        // `a` and `b` feed lineage.
        assert_column_ops(
            "SELECT CASE x WHEN 1 THEN a WHEN 2 THEN b END FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out_anon(0)),
                    transformation(col("t1", "b"), out_anon(0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn simple_case_with_column_when_pattern_all_surface() {
        // `CASE x WHEN y THEN a ELSE b END` — `x` (operand) and `y`
        // (WHEN-pattern) are both predicate-position: they decide
        // the match, their values don't flow. `a` and `b` are the
        // value-position results.
        assert_column_ops(
            "SELECT CASE x WHEN y THEN a ELSE b END FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "x"),
                    read("t1", "y"),
                    read("t1", "a"),
                    read("t1", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out_anon(0)),
                    transformation(col("t1", "b"), out_anon(0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn window_partition_by_is_filter() {
        // OVER (PARTITION BY p) — `x` is the aggregated value,
        // `p` is a partition key. Both surface in reads, but `p`
        // doesn't flow to output (it decides per-row grouping,
        // not the produced value), so only `x` is a lineage source.
        assert_column_ops(
            "SELECT SUM(x) OVER (PARTITION BY p) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "p")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn window_order_by_is_filter() {
        // OVER (ORDER BY o) — `o` is a sort key for the window
        // frame, not a value source. Same disposition as a top-
        // level ORDER BY column.
        assert_column_ops(
            "SELECT SUM(x) OVER (ORDER BY o) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "o")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn window_partition_and_order_are_filters() {
        // Combined: `p` and `o` both surface in reads; neither
        // contributes value to the output.
        assert_column_ops(
            "SELECT SUM(x) OVER (PARTITION BY p ORDER BY o) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "p"), read("t1", "o")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn window_with_literal_frame_bounds_does_not_add_refs() {
        // Frame bounds with literal integers (`3 PRECEDING`,
        // `CURRENT ROW`) walk via visit_expr but produce no
        // column refs — same shape as the no-frame version.
        // PARTITION BY / ORDER BY columns are filters, so the
        // lineage source list is `x` alone.
        assert_column_ops(
            "SELECT SUM(x) OVER (PARTITION BY p ORDER BY o \
                                 ROWS BETWEEN 3 PRECEDING AND CURRENT ROW) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "p"), read("t1", "o")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn window_with_unbounded_frame_bounds_does_not_add_refs() {
        // UNBOUNDED PRECEDING / UNBOUNDED FOLLOWING are bound
        // variants without an associated expr — visit_window_frame_bound
        // returns Ok without walking anything. ORDER BY `o` is
        // a filter (sort key).
        assert_column_ops(
            "SELECT SUM(x) OVER (ORDER BY o \
                                 ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) \
             FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "o")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_on_clause_refs_surface_as_reads_not_lineage() {
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET t.a = s.a",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![read("t", "id"), read("s", "id"), read("s", "a")],
                writes: vec![write("t", "a")],
                lineage: vec![passthrough(col("s", "a"), relation("t", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn create_table_definitions_are_not_writes() {
        assert_column_ops(
            "CREATE TABLE t1 (a INT, b INT)",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }
}

/// Projection-alias visibility in GROUP BY / HAVING / ORDER BY: an
/// unqualified ref there that names an *introduced* output alias
/// (computed expression or renamed column) is a reference to that
/// output, not a stored column, so it's dropped from reads rather than
/// surfacing as a phantom. Identity passthroughs (`SELECT a … GROUP BY
/// a`) and qualified refs stay — see `reads_by_clause` for those.
mod output_alias_visibility {
    use super::*;

    #[test]
    fn order_by_computed_alias_is_suppressed() {
        // `total` = `a + b` is a computed alias — no stored `total`
        // column exists. The ORDER BY ref to it must not surface as a
        // phantom `t.total`; the real dependency (a, b) is already
        // captured at the projection.
        assert_column_ops(
            "SELECT a + b AS total FROM t ORDER BY total",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a"), read("t", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t", "a"), out("total", 0)),
                    transformation(col("t", "b"), out("total", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_renamed_alias_is_suppressed() {
        // `a AS x` renames the column — `x` is an alias, not a stored
        // column, so GROUP BY x is dropped (the dep `a` is in reads via
        // the projection).
        assert_column_ops(
            "SELECT a AS x FROM t GROUP BY x",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("x", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn having_computed_alias_is_suppressed() {
        // `s` = `SUM(a)` is a computed alias; HAVING s references the
        // aggregate output, not a stored column.
        assert_column_ops(
            "SELECT SUM(a) AS s FROM t HAVING s > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a")],
                writes: vec![],
                lineage: vec![transformation(col("t", "a"), out("s", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualify_window_alias_is_suppressed() {
        // `rn` aliases a window output; QUALIFY references it post-projection,
        // not a stored column — so no phantom `t.rn` read. The real dependency
        // (the window's `PARTITION BY a`) is counted at the projection.
        assert_column_ops(
            "SELECT a, ROW_NUMBER() OVER (PARTITION BY a) AS rn FROM t QUALIFY rn = 1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a"), read("t", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualify_window_key_is_a_read() {
        // A window key referenced only in QUALIFY's inline window (`b` is not
        // projected) is a genuine new read — QUALIFY adds reads even as it
        // suppresses alias references. Still no lineage (filter position).
        assert_column_ops(
            "SELECT a FROM t QUALIFY ROW_NUMBER() OVER (PARTITION BY b) = 1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a"), read("t", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualify_base_column_is_a_read() {
        // A QUALIFY referencing a real column (not an output alias) is a
        // (filter-position) read, like any predicate.
        assert_column_ops(
            "SELECT a, b FROM t QUALIFY b > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a"), read("t", "b"), read("t", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t", "a"), out("a", 0)),
                    passthrough(col("t", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualified_ref_in_order_by_is_not_an_alias() {
        // `t.total` is qualified — aliases are unqualified-only, so it's
        // taken as a (best-effort) column of `t`, not the computed
        // alias. It surfaces as a read; suppression only applies to
        // bare names.
        assert_column_ops(
            "SELECT a + b AS total FROM t ORDER BY t.total",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a"), read("t", "b"), read("t", "total")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t", "a"), out("total", 0)),
                    transformation(col("t", "b"), out("total", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn subquery_in_order_by_keeps_its_reads() {
        // The clause tag resets at the subquery boundary, so the
        // subquery's own `x` isn't mis-tagged / suppressed — it surfaces
        // as a normal read of `s`.
        assert_column_ops(
            "SELECT a FROM t ORDER BY (SELECT MAX(x) FROM s)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a"), read("s", "x")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }
}
