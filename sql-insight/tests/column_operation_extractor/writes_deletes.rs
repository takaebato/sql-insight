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
