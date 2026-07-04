use crate::support::*;

mod writes {
    use super::*;

    #[test]
    fn insert_with_explicit_columns_writes_those_columns_on_target() {
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
    fn insert_select_records_target_writes_and_qualified_source_reads() {
        assert_column_ops(
            "INSERT INTO t1 (a) SELECT t2.b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2", "b")],
                writes: vec![write("t1", "a")],
                lineage: vec![passthrough(col("t2", "b"), relation("t1", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_without_explicit_columns_yields_no_writes() {
        // Without an explicit column list AND without a catalog, the
        // resolver can't pair source projections to target columns;
        // writes / lineage stay empty and `InsertColumnsUnresolved` flags it.
        assert_column_ops(
            "INSERT INTO t1 SELECT t2.b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2", "b")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsUnresolved)],
            },
        );
    }

    #[test]
    fn update_set_targets_become_writes_on_update_table() {
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
    fn update_set_qualified_target_keeps_qualifier() {
        assert_column_ops(
            "UPDATE t1 SET t1.a = 1",
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
    fn update_set_rhs_qualified_ref_is_a_read() {
        // SET RHS is value-producing (Projection-like); WHERE refs are
        // Filter-tagged.
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
    fn multi_table_update_writes_each_set_target_to_its_own_table() {
        use sql_insight::sqlparser::dialect::MySqlDialect;
        // A multi-table `UPDATE t1 JOIN t2 SET t1.a = …, t2.b = …`: each SET
        // target writes the table its qualifier names, not the root. `t2.b`
        // used to be mis-attributed to `t1.b` (and its lineage to `t1.b`).
        assert_column_ops_with_dialect(
            &MySqlDialect {},
            "UPDATE t1 JOIN t2 ON t1.id = t2.id SET t1.a = 1, t2.b = t2.c",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t1", "id"), read("t2", "id"), read("t2", "c")],
                writes: vec![write("t1", "a"), write("t2", "b")],
                lineage: vec![passthrough(col("t2", "c"), relation("t2", "b"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn self_referencing_update_reads_and_self_lineages_the_target() {
        // `SET a = a + 1` reads `t.a` and writes `t.a`, with an intra-table
        // `t.a → t.a` edge (a transformation). The table surfaces mirror this
        // (table reads `t`, table lineage `t → t`).
        assert_column_ops(
            "UPDATE t SET a = a + 1",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t", "a")],
                writes: vec![write("t", "a")],
                lineage: vec![transformation(col("t", "a"), relation("t", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn multi_table_update_set_joined_table_from_root_column() {
        use sql_insight::sqlparser::dialect::MySqlDialect;
        // `SET t2.b = t1.c`: writes t2.b, lineage from t1.c (the root is the
        // source here, the joined table the write target).
        assert_column_ops_with_dialect(
            &MySqlDialect {},
            "UPDATE t1 JOIN t2 ON t1.id = t2.id SET t2.b = t1.c",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t1", "id"), read("t2", "id"), read("t1", "c")],
                writes: vec![write("t2", "b")],
                lineage: vec![passthrough(col("t1", "c"), relation("t2", "b"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_parenthesized_join_target_resolves_all_relations() {
        // `UPDATE (t1 JOIN t2 …) SET t1.b = t2.b`: the parenthesized join target
        // is flattened, so t2 is a joined read — the ON columns and the SET RHS
        // `t2.b` resolve (the join's second relation was previously dropped,
        // leaving `t2.b` unresolved).
        assert_column_ops(
            "UPDATE (t1 JOIN t2 ON t1.a = t2.a) SET t1.b = t2.b",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t1", "a"), read("t2", "a"), read("t2", "b")],
                writes: vec![write("t1", "b")],
                lineage: vec![passthrough(col("t2", "b"), relation("t1", "b"))],
                diagnostics: vec![],
            },
        );
    }
}

mod delete {
    use super::*;
    use sql_insight::sqlparser::dialect::MySqlDialect;

    #[test]
    fn delete_qualified_predicate_is_a_read() {
        assert_column_ops(
            "DELETE FROM t1 WHERE t1.id = 5",
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
    fn delete_order_by_keys_are_reads_constant_limit_is_not() {
        // MySQL `DELETE … ORDER BY … LIMIT`: ORDER BY keys reference the
        // target's columns (reads, filter position); the constant LIMIT adds
        // no read. Neither feeds lineage.
        assert_column_ops_with_dialect(
            &MySqlDialect {},
            "DELETE FROM t WHERE flag = 1 ORDER BY priority DESC LIMIT 5",
            ColumnOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("t", "flag"), read("t", "priority")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }
}

mod insert_inline_view_target {
    //! Oracle `INSERT INTO (SELECT … FROM t [WHERE …]) …`: an inline-view
    //! target resolves through to its single base table — the row lands there.
    //! The view projection names the target columns, the WHERE is filter reads
    //! against the target. A view over no single base table (a join) is
    //! flagged instead — see
    //! `diagnostics::reported::insert_into_join_view_target_reports_diagnostic`.
    use super::*;
    use sql_insight::sqlparser::dialect::OracleDialect;

    #[test]
    fn values_into_inline_view_writes_the_base_table_columns() {
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT a, b FROM emp) VALUES (100, 'x')",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("emp", "a"), write("emp", "b")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn view_where_predicate_is_a_filter_read_on_the_target() {
        // The WHERE restricts which rows the view exposes (what a
        // `WITH CHECK OPTION` would enforce) — its columns read against the
        // target, but never feed lineage (filter position).
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT a FROM emp WHERE dept = 10) VALUES (100)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("emp", "dept")],
                writes: vec![write("emp", "a")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn select_source_traces_through_the_view_to_the_base_column() {
        // Relation lineage pairs the source outputs with the view's projected
        // base columns positionally — `s.x → emp.a`.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT a FROM emp) SELECT x FROM s",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "x")],
                writes: vec![write("emp", "a")],
                lineage: vec![passthrough(col("s", "x"), relation("emp", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aliased_view_column_writes_the_underlying_base_column() {
        // `a AS x` renames the view column; the row still lands in base `a`.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT a AS x FROM emp) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("emp", "a")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn wildcard_view_projection_is_column_less() {
        // `SELECT *` names no target columns (wildcards aren't expanded) — the
        // base table still surfaces as the write target, but its column writes
        // drop with the usual column-less diagnostic (a catalog would fill
        // them).
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT * FROM emp) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsUnresolved)],
            },
        );
    }

    #[test]
    fn expression_view_projection_is_column_less() {
        // A non-column projection item (`a + 1`) makes the whole positional
        // pairing indeterminate — same column-less path as the wildcard.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT a + 1 FROM emp) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsUnresolved)],
            },
        );
    }

    #[test]
    fn view_columns_are_an_explicit_list_for_the_arity_check() {
        // The view projection acts as the explicit column list: a wider VALUES
        // row mismatches it exactly like `INSERT INTO emp (a) VALUES (1, 2)`.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT a FROM emp) VALUES (1, 2)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("emp", "a")],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsArityMismatch)],
            },
        );
    }

    #[test]
    fn aliased_base_table_resolves_the_view_predicate() {
        // The base table's alias (`FROM emp e`) is how the WHERE qualifies its
        // refs (`e.dept`) — they resolve through the alias to `emp`, and (as
        // usual) the alias shadows the bare table name.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT e.a FROM emp e WHERE e.dept = 10) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("emp", "dept")],
                writes: vec![write("emp", "a")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn view_with_other_clauses_is_flagged_not_resolved() {
        // Only the minimal insertable-view shape (projection + FROM + WHERE)
        // resolves through. Any other clause — GROUP BY here, likewise
        // DISTINCT / HAVING / ORDER BY / FETCH — makes the view
        // non-insertable, and could carry column refs that would otherwise
        // drop silently: flag + drop instead.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT a FROM emp GROUP BY a) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
        // A *query*-level clause (ORDER BY, outside the SELECT) rejects too.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT a FROM emp ORDER BY b) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }
}
