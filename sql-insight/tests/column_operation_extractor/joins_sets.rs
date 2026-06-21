use crate::support::*;

mod set_operations {
    use super::*;

    #[test]
    fn union_two_branches_emit_query_output_per_branch() {
        // Each branch contributes its own output-column list, so both
        // branches' projections fan out independently into
        // QueryOutput edges. Position is per-group, so both land at
        // position 0; name follows each branch's own projection.
        assert_column_ops(
            "SELECT a FROM t1 UNION SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_all_behaves_same_as_union() {
        // UNION ALL only differs from UNION at runtime (dedup vs
        // not); structurally the resolver should treat them identically.
        assert_column_ops(
            "SELECT a FROM t1 UNION ALL SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn intersect_behaves_same_as_union() {
        assert_column_ops(
            "SELECT a FROM t1 INTERSECT SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn except_behaves_same_as_union() {
        assert_column_ops(
            "SELECT a FROM t1 EXCEPT SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn three_way_union_emits_one_lineage_edge_per_branch() {
        // Chained UNION parses left-associatively as
        // `(t1 UNION t2) UNION t3`, so the resolver recursively
        // visits each base SELECT and each contributes its own group.
        assert_column_ops(
            "SELECT a FROM t1 UNION SELECT b FROM t2 UNION SELECT c FROM t3",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b"), read("t3", "c")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                    passthrough(col("t3", "c"), out("c", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_with_where_classifies_per_branch_kind() {
        // Each branch's WHERE is its own filter scope, so each
        // branch produces a Projection read plus a Filter read for
        // its own column.
        assert_column_ops(
            "SELECT a FROM t1 WHERE a > 0 UNION SELECT b FROM t2 WHERE b < 10",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "a"),
                    read("t1", "a"),
                    read("t2", "b"),
                    read("t2", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_mixed_passthrough_and_transformation_kinds() {
        // Branch lineage kinds are independent. Left passthrough, right
        // transformation; both contribute to the same output position.
        assert_column_ops(
            "SELECT a FROM t1 UNION SELECT b + 1 AS a FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    transformation(col("t2", "b"), out("a", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_with_aggregate_branch_emits_transformation_edge() {
        assert_column_ops(
            "SELECT id FROM t1 UNION SELECT COUNT(id) AS id FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t2", "id")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "id"), out("id", 0)),
                    transformation(col("t2", "id"), out("id", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_in_subquery_collapses_both_branches_to_outer() {
        // The inner UNION lives in a derived subquery; the outer
        // SELECT projects from it and collapses back to the base
        // tables of both branches — no intermediate QueryOutput
        // edge for the subquery survives.
        assert_column_ops(
            "SELECT x FROM (SELECT a AS x FROM t1 UNION SELECT b AS x FROM t2) sub",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("x", 0)),
                    passthrough(col("t2", "b"), out("x", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_in_cte_collapses_to_outer_use() {
        // CTE body is a UNION. Outer SELECT pulls `x` from the cte.
        // Composition should walk back through both branches to t1/t2.
        assert_column_ops(
            "WITH cte AS (SELECT a AS x FROM t1 UNION SELECT b AS x FROM t2) \
             SELECT x FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("x", 0)),
                    passthrough(col("t2", "b"), out("x", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ctas_with_union_body_pairs_left_branch_names_for_all_branches() {
        // CTAS schema follows the LEFT branch's projection names
        // (SQL standard). The inferred-name path uses the first
        // the branch's column names for every branch's
        // positional pairing — same as INSERT-SELECT-UNION. So:
        //   - writes: only `dst.a` (left branch's name)
        //   - lineage: BOTH branches feed `Relation(dst.a)`
        assert_column_ops(
            "CREATE TABLE dst AS SELECT a FROM t1 UNION SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![write("dst", "a")],
                lineage: vec![
                    passthrough(col("t1", "a"), relation("dst", "a")),
                    passthrough(col("t2", "b"), relation("dst", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ctas_with_explicit_columns_and_union_body_pairs_left_target_for_all_branches() {
        // When CTAS specifies its own column list, both branches
        // pair positionally against the same target columns — same
        // pattern as INSERT-SELECT-UNION.
        assert_column_ops(
            "CREATE TABLE dst (x INT) AS SELECT a FROM t1 UNION SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![write("dst", "x")],
                lineage: vec![
                    passthrough(col("t1", "a"), relation("dst", "x")),
                    passthrough(col("t2", "b"), relation("dst", "x")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_with_trailing_order_by_ref_is_unresolved() {
        // ORDER BY on the whole UNION is visited in the outer query
        // scope, AFTER both branch scopes have been popped. The
        // ORDER BY column refers to a UNION output column, not a
        // real table — so `a` resolves to None (no in-scope
        // binding).
        assert_column_ops(
            "SELECT a FROM t1 UNION SELECT b FROM t2 ORDER BY a",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b"), unresolved("a")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_with_trailing_limit_literal_adds_nothing() {
        // LIMIT 10 is a literal — no column refs, no extra lineage.
        assert_column_ops(
            "SELECT a FROM t1 UNION SELECT b FROM t2 LIMIT 10",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }
}

mod join_using_and_natural {
    //! `JOIN … USING (col)` merge columns fan in: an unqualified ref to
    //! a USING column resolves to *every* joined relation that could own
    //! it (the COALESCE-style merged column has no single owner), so it
    //! surfaces one read / lineage source per side rather than an
    //! ambiguous `table: None`. NATURAL JOIN stays unexpanded (its merge
    //! set needs both schemas, like wildcard expansion).
    use super::*;

    #[test]
    fn join_using_id_in_projection_fans_in_to_both_tables() {
        // `id` is a USING merge column → the projection ref fans in to
        // both joined tables (t1.id and t2.id), each an Inferred read
        // and a lineage source into the output. No catalog, so neither
        // side is Cataloged.
        assert_column_ops(
            "SELECT id FROM t1 JOIN t2 USING (id)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t2", "id")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "id"), out("id", 0)),
                    passthrough(col("t2", "id"), out("id", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_using_id_fans_in_at_each_occurrence() {
        // The merge column fans in independently per occurrence: the
        // projection `id` and the WHERE `id` each expand to t1.id +
        // t2.id. WHERE is filter position, so its two refs are reads
        // only (no lineage).
        assert_column_ops(
            "SELECT id FROM t1 JOIN t2 USING (id) WHERE id > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "id"),
                    read("t2", "id"),
                    read("t1", "id"),
                    read("t2", "id"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "id"), out("id", 0)),
                    passthrough(col("t2", "id"), out("id", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_using_column_fans_in_to_target() {
        // The merged source column feeds the target from both sides:
        // both t1.id and t2.id flow into dst.id (fan-in lineage into a
        // named relation).
        assert_column_ops(
            "INSERT INTO dst (id) SELECT id FROM t1 JOIN t2 USING (id)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t1", "id"), read("t2", "id")],
                writes: vec![write("dst", "id")],
                lineage: vec![
                    passthrough(col("t1", "id"), relation("dst", "id")),
                    passthrough(col("t2", "id"), relation("dst", "id")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_using_qualified_id_resolves_to_named_table() {
        // Qualifying the ref sidesteps the USING ambiguity: `t1.id`
        // resolves to t1 unambiguously. Use this in real-world
        // queries until USING expansion is available.
        assert_column_ops(
            "SELECT t1.id FROM t1 JOIN t2 USING (id)",
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
    fn natural_join_no_catalog_leaves_unqualified_refs_unresolved() {
        // NATURAL JOIN's merge set comes from the intersection of
        // both tables' column lists — only knowable with a
        // catalog. Without one, the resolver doesn't expand, and
        // unqualified `id` is multi-candidate-unresolved (same
        // shape as plain JOIN ON without USING).
        assert_column_ops(
            "SELECT id FROM t1 NATURAL JOIN t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![ambiguous("id")],
                writes: vec![],
                lineage: vec![passthrough(ambiguous("id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }
}

mod lateral_and_correlation {
    use super::*;

    #[test]
    fn lateral_subquery_resolves_inner_ref_to_inner_table() {
        // The existing-style LATERAL: the inner subquery only
        // references its own tables. The outer FROM joins it as
        // a derived source. The inner `id` resolves to t1 from
        // the LATERAL subquery's own scope.
        assert_column_ops(
            "SELECT d.id FROM LATERAL (SELECT id FROM t1) AS d JOIN t2 ON d.id = t2.id",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t2", "id")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn lateral_with_outer_scope_reference_resolves_via_scope_chain() {
        // The interesting LATERAL case: the inner subquery references
        // `t1.x` from the OUTER FROM. Without LATERAL this is invalid
        // SQL, but the resolver doesn't enforce LATERAL semantics —
        // it walks the scope chain regardless.
        assert_column_ops(
            "SELECT sub.x FROM t1, LATERAL (SELECT t1.a + t2.b AS x FROM t2) sub",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out("x", 0)),
                    transformation(col("t2", "b"), out("x", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn non_lateral_derived_does_not_resolve_a_sibling_ref() {
        // LATERAL is enforced: a non-LATERAL derived table can't see its FROM
        // siblings, so `t1.a` inside it doesn't resolve (it's `Unresolved`,
        // surfaced with `table: None`) — an improvement over a permissive
        // scope-chain walk that would mis-bind it to the sibling `t1`. Its own
        // FROM column `t2.b` resolves normally.
        assert_column_ops(
            "SELECT sub.x FROM t1, (SELECT t1.a + t2.b AS x FROM t2) sub",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![unresolved("a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(unresolved("a"), out("x", 0)),
                    transformation(col("t2", "b"), out("x", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn correlated_where_subquery_resolves_outer_ref() {
        // Classic correlated subquery in WHERE: the inner SELECT
        // references the outer t1.id. The resolver walks the
        // scope chain to find t1.id in the outer scope.
        assert_column_ops(
            "SELECT a FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.fk = t1.id)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "fk"), read("t1", "id")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }
}

mod values_as_relation {
    //! `VALUES` can stand in for a row-source in three positions:
    //! - INSERT … VALUES (already covered in `lineage` / `on_conflict`)
    //! - SELECT … FROM (VALUES …) AS t(x, y)   — derived table
    //! - WITH cte(x, y) AS (VALUES …) SELECT … — CTE body
    //!
    //! VALUES doesn't carry projection items the resolver can
    //! capture (literals have no source refs), so lineage from these
    //! variants bottom out at the synthetic binding — no
    //! collapse to a real table is possible.
    use super::*;

    #[test]
    fn values_as_derived_table_with_aliases_emits_synthetic_refs_only() {
        // The derived table `t` carries schema [x, y] from the
        // alias rename, but its output_columns is None (VALUES
        // contributes no OutputColumns). So `t.x` is recorded as
        // a synthetic ref pointing at the derived binding; reads
        // filter it out, and lineage keeps `t.x` as the source
        // (collapse can't collapse further).
        assert_column_ops(
            "SELECT x, y FROM (VALUES (1, 'a'), (2, 'b')) AS t(x, y)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![
                    passthrough(read("t", "x"), out("x", 0)),
                    passthrough(read("t", "y"), out("y", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn values_as_cte_body_with_aliases_emits_synthetic_refs_only() {
        assert_column_ops(
            "WITH cte(id, val) AS (VALUES (1, 'a'), (2, 'b')) SELECT id FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![passthrough(read("cte", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn values_with_column_ref_in_row_does_not_resolve_a_sibling() {
        // A column ref inside a non-LATERAL `VALUES` row is walked (it surfaces
        // in reads) but, with LATERAL enforced, can't see the FROM sibling
        // `t1`, so `t1.a` is `Unresolved`. The derived column `v.x` is a
        // synthetic source (VALUES rows have no base columns).
        assert_column_ops(
            "SELECT v.x FROM t1, (VALUES (t1.a)) AS v(x)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![unresolved("a")],
                writes: vec![],
                lineage: vec![passthrough(read("v", "x"), out("x", 0))],
                diagnostics: vec![],
            },
        );
    }
}

mod join_arm_coverage {
    //! `join_constraint`'s `JoinOperator` arms: every constraint-carrying
    //! join type still yields its `ON` predicate's reads, and the
    //! constraint-less `CROSS APPLY` yields none. One terse case per
    //! parseable variant, so a new sqlparser `JoinOperator` forces a case
    //! here. SQL is qualified (`t.a`) for deterministic reads.
    use super::*;
    use sql_insight::sqlparser::dialect::{
        Dialect, GenericDialect, MsSqlDialect, MySqlDialect, SnowflakeDialect,
    };

    fn join_reads(sql: &str, dialect: &dyn Dialect) -> Vec<ColumnRead> {
        extract_column_operations(dialect, sql)
            .unwrap()
            .remove(0)
            .unwrap()
            .reads
    }

    /// The `ON t.id = u.id` constraint of any constraint-carrying join
    /// surfaces both sides as reads, alongside the projection's `t.a`.
    fn on_constraint_reads() -> Vec<ColumnRead> {
        vec![read("t", "a"), read("t", "id"), read("u", "id")]
    }

    #[test]
    fn semi_join() {
        assert_unordered_eq!(
            join_reads(
                "SELECT t.a FROM t SEMI JOIN u ON t.id = u.id",
                &GenericDialect {}
            ),
            on_constraint_reads()
        );
    }

    #[test]
    fn anti_join() {
        assert_unordered_eq!(
            join_reads(
                "SELECT t.a FROM t ANTI JOIN u ON t.id = u.id",
                &GenericDialect {}
            ),
            on_constraint_reads()
        );
    }

    #[test]
    fn left_semi_join() {
        assert_unordered_eq!(
            join_reads(
                "SELECT t.a FROM t LEFT SEMI JOIN u ON t.id = u.id",
                &GenericDialect {}
            ),
            on_constraint_reads()
        );
    }

    #[test]
    fn left_anti_join() {
        assert_unordered_eq!(
            join_reads(
                "SELECT t.a FROM t LEFT ANTI JOIN u ON t.id = u.id",
                &GenericDialect {}
            ),
            on_constraint_reads()
        );
    }

    #[test]
    fn straight_join() {
        assert_unordered_eq!(
            join_reads(
                "SELECT t.a FROM t STRAIGHT_JOIN u ON t.id = u.id",
                &MySqlDialect {}
            ),
            on_constraint_reads()
        );
    }

    #[test]
    fn asof_join() {
        // ASOF carries both a MATCH_CONDITION and an ON constraint;
        // `join_constraint` returns the ON, so reads mirror the others.
        assert_unordered_eq!(
            join_reads(
                "SELECT t.a FROM t ASOF JOIN u MATCH_CONDITION (t.ts >= u.ts) ON t.id = u.id",
                &SnowflakeDialect {},
            ),
            on_constraint_reads()
        );
    }

    #[test]
    fn cross_apply_has_no_constraint() {
        // CROSS APPLY carries no `ON` (join_constraint → None); only the
        // applied subquery's read and the projection surface.
        assert_unordered_eq!(
            join_reads(
                "SELECT t.a FROM t CROSS APPLY (SELECT u.id FROM u) sub",
                &MsSqlDialect {},
            ),
            vec![read("t", "a"), read("u", "id")]
        );
    }
}
