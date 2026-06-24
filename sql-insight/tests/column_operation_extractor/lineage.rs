use crate::support::*;

mod projections {
    use super::*;

    #[test]
    fn select_bare_column_emits_passthrough_edge_to_query_output() {
        assert_column_ops(
            "SELECT a FROM t1",
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
    fn select_aliased_column_uses_alias_as_output_name() {
        assert_column_ops(
            "SELECT a AS x FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("x", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn select_arithmetic_emits_one_transformation_edge_per_source() {
        assert_column_ops(
            "SELECT a + b FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
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
    fn select_mixed_projection_separates_targets_by_position() {
        assert_column_ops(
            "SELECT a, a + b FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    transformation(col("t1", "a"), out_anon(1)),
                    transformation(col("t1", "b"), out_anon(1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn select_qualified_ref_in_expression_resolves_directly() {
        assert_column_ops(
            "SELECT t1.a + t1.b AS sum FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out("sum", 0)),
                    transformation(col("t1", "b"), out("sum", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_pairs_target_cols_positionally() {
        assert_column_ops(
            "INSERT INTO t1 (a, b) SELECT x, y FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2", "x"), read("t2", "y")],
                writes: vec![write("t1", "a"), write("t1", "b")],
                lineage: vec![
                    passthrough(col("t2", "x"), relation("t1", "a")),
                    passthrough(col("t2", "y"), relation("t1", "b")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_transformation_marks_kind_per_source() {
        assert_column_ops(
            "INSERT INTO t1 (a) SELECT x + y FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2", "x"), read("t2", "y")],
                writes: vec![write("t1", "a")],
                lineage: vec![
                    transformation(col("t2", "x"), relation("t1", "a")),
                    transformation(col("t2", "y"), relation("t1", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_union_pairs_both_branches_with_target_cols() {
        // Both UNION branches feed the same INSERT target positions,
        // so each branch's projection should pair `position N → t.col_N`.
        assert_column_ops(
            "INSERT INTO t1 (a, b) \
             SELECT x, y FROM t2 \
             UNION ALL \
             SELECT p, q FROM t3",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![
                    read("t2", "x"),
                    read("t2", "y"),
                    read("t3", "p"),
                    read("t3", "q"),
                ],
                writes: vec![write("t1", "a"), write("t1", "b")],
                lineage: vec![
                    passthrough(col("t2", "x"), relation("t1", "a")),
                    passthrough(col("t2", "y"), relation("t1", "b")),
                    passthrough(col("t3", "p"), relation("t1", "a")),
                    passthrough(col("t3", "q"), relation("t1", "b")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_without_explicit_cols_emits_no_lineage() {
        // Target column names would need catalog-driven positional mapping;
        // without a catalog the resolver can't pair them, so column writes /
        // lineage are dropped — flagged with `InsertColumnsUnresolved` so the
        // empty surfaces read as "couldn't analyze", not "nothing written".
        assert_column_ops(
            "INSERT INTO t1 SELECT x FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2", "x")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsUnresolved)],
            },
        );
    }

    #[test]
    fn insert_values_with_literals_emits_no_lineage() {
        assert_column_ops(
            "INSERT INTO t1 (a, b) VALUES (1, 2)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t1", "a"), write("t1", "b")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_set_literal_emits_no_lineage() {
        assert_column_ops(
            "UPDATE t1 SET a = 1",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![],
                writes: vec![write("t1", "a")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn delete_emits_no_lineage() {
        assert_column_ops(
            "DELETE FROM t1 WHERE id = 5",
            ColumnOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn wildcard_select_emits_no_lineage() {
        assert_column_ops(
            "SELECT * FROM t1",
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
    fn update_set_passthrough_lineage() {
        assert_column_ops(
            "UPDATE t1 SET a = b",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t1", "b")],
                writes: vec![write("t1", "a")],
                lineage: vec![passthrough(col("t1", "b"), relation("t1", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_set_transformation_lineage() {
        assert_column_ops(
            "UPDATE t1 SET a = b + 1",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t1", "b")],
                writes: vec![write("t1", "a")],
                lineage: vec![transformation(col("t1", "b"), relation("t1", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_set_with_qualified_rhs_resolves_to_other_table() {
        assert_column_ops(
            "UPDATE t1 SET a = t2.b FROM t2 WHERE t1.id = t2.id",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t2", "b"), read("t1", "id"), read("t2", "id")],
                writes: vec![write("t1", "a")],
                lineage: vec![passthrough(col("t2", "b"), relation("t1", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_call_in_projection_emits_transformation_edge() {
        assert_column_ops(
            "SELECT SUM(a) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "a"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_with_alias_carries_aliased_name() {
        assert_column_ops(
            "SELECT COUNT(b) AS n FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "b")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "b"), out("n", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_wrapped_in_expression_is_transformation() {
        // `SUM(a) + 1` is a value-changing expression, so the lineage edge
        // is Transformation — same kind a bare aggregate call would
        // produce, since the model no longer sub-classifies them.
        assert_column_ops(
            "SELECT SUM(a) + 1 FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "a"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_in_insert_select_propagates_transformation() {
        assert_column_ops(
            "INSERT INTO t2 (n) SELECT COUNT(a) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t1", "a")],
                writes: vec![write("t2", "n")],
                lineage: vec![transformation(col("t1", "a"), relation("t2", "n"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_aggregate_collapses_to_outer_as_transformation() {
        // CTE body's `s` is Transformation (SUM(a)); outer's bare `s`
        // would be Passthrough, but collapse keeps the chain a
        // Transformation (any transforming step dominates).
        assert_column_ops(
            "WITH cte AS (SELECT SUM(a) AS s FROM t1) SELECT s FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "a"), out("s", 0))],
                diagnostics: vec![],
            },
        );
    }
}

mod collapse {
    use super::*;

    #[test]
    fn cte_passthrough_collapses_to_real_table() {
        // The outer edge's source `id` resolves to cte, then collapses
        // through the CTE body's projection back to t1.id. No
        // intermediate cte.id → out edge survives.
        assert_column_ops(
            "WITH cte AS (SELECT id FROM t1) SELECT id FROM cte",
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
    fn cte_transformation_propagates_kind_after_collapse() {
        // CTE body's `sum` is a transformation of a, b. Outer's bare
        // `sum` collapses back into two edges, each Transformation
        // because the body item is (outer.bare && item.bare = false).
        assert_column_ops(
            "WITH cte AS (SELECT a + b AS sum FROM t1) SELECT sum FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out("sum", 0)),
                    transformation(col("t1", "b"), out("sum", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_to_insert_collapses_end_to_end() {
        // Composition reaches past the CTE boundary into the INSERT
        // target — t1.id → t2.x directly, no cte.id step.
        assert_column_ops(
            "INSERT INTO t2 (x) WITH cte AS (SELECT id FROM t1) SELECT id FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t1", "id")],
                writes: vec![write("t2", "x")],
                lineage: vec![passthrough(col("t1", "id"), relation("t2", "x"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_chain_collapses_through_all_levels() {
        // a → b → outer: outer's `b.id` collapses via b's body back to
        // a, then via a's body back to t1. Outer is qualified because
        // having both `a` and `b` in scope with the same column name
        // makes the unqualified form ambiguous under our scope model
        // (outer SELECT sees both CTE bindings, not just b).
        assert_column_ops(
            "WITH a AS (SELECT id FROM t1), b AS (SELECT id FROM a) SELECT b.id FROM b",
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
    fn derived_table_collapses_to_real_table() {
        // The outer projection's `col` collapses through derived `d`'s
        // body (a + b AS col) into two Transformation edges on t1.
        assert_column_ops(
            "SELECT col FROM (SELECT a + b AS col FROM t1) d",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out("col", 0)),
                    transformation(col("t1", "b"), out("col", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_referenced_twice_collapses_each_use() {
        // Each cte reference in the projection collapses independently
        // back to t1.id.
        assert_column_ops(
            "WITH cte AS (SELECT id FROM t1) SELECT cte.id AS a, cte.id AS b FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "id"), out("a", 0)),
                    passthrough(col("t1", "id"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_self_join_does_not_duplicate_lineage() {
        // A CTE joined to itself under two aliases (`c x JOIN c y`): the output
        // `x.id` is owned by exactly one reference, so it traces to a single
        // `base.id → id` edge. Previously the demand-driven origin trace expanded
        // the CTE body through *both* references — the `CteRef` node dropped its
        // alias, so it could not prune the non-owning side — and emitted the edge
        // twice, while `reads` (the body is walked once at the declaration) stayed
        // folded; that asymmetry was the bug. A real-table self-join never
        // duplicated (a base column is its own origin and never traces the join).
        assert_column_ops(
            "WITH c AS (SELECT id FROM base) SELECT x.id FROM c x JOIN c y ON x.id = y.id",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("base", "id")],
                writes: vec![],
                lineage: vec![passthrough(col("base", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_of_two_ctes_attributes_output_to_its_owner_only() {
        // `x.id` is owned by CTE `c` (alias `x`); the joined CTE `d` (alias `y`)
        // contributes `y.id` only in the ON predicate — filter position, a read
        // but never a lineage origin. So the output traces to `base.id` alone:
        // the qualifier prunes the non-owning `d` reference. (Before the
        // alias-aware prune the trace expanded *both* CTE bodies and spuriously
        // emitted `other.id → id`.) Both bodies are still walked once at the
        // declaration, so `reads` keeps `base.id` and `other.id`.
        assert_column_ops(
            "WITH c AS (SELECT id FROM base), d AS (SELECT id FROM other) \
             SELECT x.id FROM c x JOIN d y ON x.id = y.id",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("base", "id"), read("other", "id")],
                writes: vec![],
                lineage: vec![passthrough(col("base", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn recursive_cte_traces_through_to_the_real_table() {
        // A recursive CTE collapses through to the anchor's real table: the
        // outer `id` traces into the CTE body's set operation — the anchor
        // branch resolves `id` to `t1.id`, the recursive self-reference is
        // terminated by the active-set (it adds no new source). So lineage is a
        // single `t1.id → id` edge, and `t1.id` is read once (no infinite
        // recursion, no duplicate self-reference edge).
        assert_column_ops(
            "WITH RECURSIVE r AS (SELECT id FROM t1 UNION SELECT id FROM r) SELECT id FROM r",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![passthrough(read("t1", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }
}

mod cte_derived_rename {
    use super::*;

    #[test]
    fn cte_column_rename_collapses_through_renamed_name() {
        // Outer `a` refers to cte's renamed column at position 0,
        // which body-positionally is `x` from t. Composition follows
        // the renamed name back to the body item, then to t.x.
        // Reads surface only the real-table ref (CTE binding is
        // synthetic, dropped).
        assert_column_ops(
            "WITH cte (a) AS (SELECT x FROM t) SELECT a FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "x")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "x"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_column_alias_matched_case_insensitively() {
        // The CTE projects `x AS Foo`; the outer query references it
        // as unquoted `foo`. Composition's name-match folds both
        // sides to the same key, so `foo` collapses back to the real
        // source `t1.x`.
        assert_column_ops(
            "WITH cte AS (SELECT x AS Foo FROM t1) SELECT foo FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "x"), out("foo", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_column_rename_partial_keeps_remaining_body_names() {
        // Rename `(p)` covers position 0 only. Position 1's body name
        // `y` survives; outer can reference `p` or `y`.
        assert_column_ops(
            "WITH cte (p) AS (SELECT x, y FROM t) SELECT p, y FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "x"), read("t", "y")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t", "x"), out("p", 0)),
                    passthrough(col("t", "y"), out("y", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn derived_table_column_rename_collapses() {
        // `(SELECT x FROM t) AS d(a)` — outer `a` resolves via d's
        // renamed column at position 0 → body item x → t.x.
        assert_column_ops(
            "SELECT a FROM (SELECT x FROM t) d(a)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "x")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "x"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_column_rename_into_insert() {
        // `INSERT INTO t2 (col) WITH cte(a) AS (SELECT x FROM t1)
        //  SELECT a FROM cte` collapses through both the CTE rename
        //  and the INSERT pairing: t1.x → t2.col.
        assert_column_ops(
            "INSERT INTO t2 (col) WITH cte (a) AS (SELECT x FROM t1) \
             SELECT a FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t1", "x")],
                writes: vec![write("t2", "col")],
                lineage: vec![passthrough(col("t1", "x"), relation("t2", "col"))],
                diagnostics: vec![],
            },
        );
    }
}

mod lambda {
    use super::*;

    #[test]
    fn lambda_param_is_not_traced_into_a_same_named_derived_column() {
        // The lambda parameter `x` shadows the derived table's column `x`
        // within the body: it's a local, so it neither reads nor originates
        // `s.x`. Only the array argument (`arr` → `s.arr`) flows to the output.
        // (A `Derived` binding instead of a dedicated `Local` would mis-trace
        // `x` into the derived subquery and emit a spurious `s.x → r`.)
        assert_column_ops(
            "SELECT transform(arr, x -> x) AS r FROM (SELECT arr, x FROM s) d",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("s", "arr"), read("s", "x")],
                writes: vec![],
                lineage: vec![transformation(col("s", "arr"), out("r", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn lambda_param_does_not_shadow_a_subquery_own_column() {
        // A subquery in the body has its own innermost scope: a bare `x` in
        // `(SELECT x FROM t2)` resolves to the subquery's own `t2.x`, not the
        // (enclosing) lambda parameter — the parameter sits at its lexical depth
        // in the resolution stack and doesn't over-shadow an inner scope. Both
        // the array arg (`t1.arr`) and the body value (`t2.x`) flow to the
        // anonymous output as a transformation.
        assert_column_ops(
            "SELECT transform(arr, x -> (SELECT x FROM t2)) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "arr"), read("t2", "x")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "arr"), out_anon(0)),
                    transformation(col("t2", "x"), out_anon(0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn lambda_param_resolves_at_the_right_depth_when_deeply_nested() {
        // Two levels of nested subquery: the innermost (`t3`) is the innermost
        // scope, so a bare `x` resolves to `t3.x`. A flat "parameter checked
        // first" rule would have wrongly shadowed both `t2` and `t3`; the frame
        // stack places the parameter at its correct depth.
        assert_column_ops(
            "SELECT transform(arr, x -> (SELECT (SELECT x FROM t3) FROM t2)) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "arr"), read("t3", "x")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "arr"), out_anon(0)),
                    transformation(col("t3", "x"), out_anon(0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn lambda_param_shadowed_by_a_cte_column_in_the_body() {
        // Inside the body, the CTE `c` exposes a column `x` (renamed from `k`),
        // so the subquery `SELECT x FROM c` resolves `x` to the CTE column
        // (→ s.k), shadowing the lambda parameter — the parameter is not a false
        // read. (Asserting reads only: the body value's lineage to the output is
        // subject to a separate limitation — a scalar subquery with a leading
        // WITH currently drops its column lineage.)
        let op = extract(
            "SELECT transform(arr, x -> (WITH c AS (SELECT k AS x FROM s) SELECT x FROM c)) FROM t1",
        );
        assert_unordered_eq!(op.reads, vec![read("t1", "arr"), read("s", "k")]);
    }
}
