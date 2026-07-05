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

    #[test]
    fn multi_table_unqualified_set_is_ambiguous_without_a_catalog() {
        // Catalog-free, both joined tables are Unknown suspects, so the
        // unqualified SET target can't be attributed — real MySQL rejects the
        // statement outright (error 1052) when both tables own the column, so
        // no side is fabricated. The write surfaces unattributed (`table:
        // None`, `Ambiguous`), mirroring the read side, and contributes no
        // table-level write. (Previously it silently pinned the root t1.)
        use sql_insight::sqlparser::dialect::MySqlDialect;
        assert_column_ops_with_dialect(
            &MySqlDialect {},
            "UPDATE t1 JOIN t2 ON t1.id = t2.id SET a = 1",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t1", "id"), read("t2", "id")],
                writes: vec![ColumnWrite {
                    reference: ColumnReference {
                        table: None,
                        name: "a".into(),
                    },
                    resolution: ResolutionKind::Ambiguous,
                }],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn lineage_still_targets_an_unattributed_set_column() {
        // A value RHS still traces to the unattributed write — the edge's
        // target carries `table: None` + `Ambiguous`, symmetric with an
        // ambiguous *source* read appearing in lineage. The value dependency
        // (`t2.c` flows somewhere) is real even when the sink table isn't
        // determinable.
        use sql_insight::sqlparser::dialect::MySqlDialect;
        let unattributed = ColumnWrite {
            reference: ColumnReference {
                table: None,
                name: "a".into(),
            },
            resolution: ResolutionKind::Ambiguous,
        };
        assert_column_ops_with_dialect(
            &MySqlDialect {},
            "UPDATE t1 JOIN t2 ON t1.id = t2.id SET a = t2.c",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t1", "id"), read("t2", "id"), read("t2", "c")],
                writes: vec![unattributed.clone()],
                lineage: vec![passthrough(
                    col("t2", "c"),
                    ColumnTarget::Relation(unattributed),
                )],
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
    fn join_view_qualified_projection_resolves_the_common_relation() {
        // A join view: every projected column is qualified to `e` — the row
        // lands in `emp` (text-only attribution, like a multi-table UPDATE's
        // `SET t2.col`). The companion `dept` is scanned context; the ON and
        // WHERE are filter reads over both relations.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT e.id, e.name FROM emp e JOIN dept d \
             ON e.dept_id = d.id WHERE d.active = 1) VALUES (100, 'x')",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![
                    read("emp", "dept_id"),
                    read("dept", "id"),
                    read("dept", "active"),
                ],
                writes: vec![write("emp", "id"), write("emp", "name")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_view_comma_form_resolves_too() {
        // The classic Oracle comma-join spelling of the same view.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT e.id FROM emp e, dept d WHERE e.dept_id = d.id) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("emp", "dept_id"), read("dept", "id")],
                writes: vec![write("emp", "id")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_view_select_source_traces_to_the_base_column() {
        // Relation lineage pairs the source outputs with the resolved base
        // columns — through the join view.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT e.id FROM emp e JOIN dept d ON e.dept_id = d.id) \
             SELECT x FROM s",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("emp", "dept_id"), read("dept", "id"), read("s", "x")],
                writes: vec![write("emp", "id")],
                lineage: vec![passthrough(col("s", "x"), relation("emp", "id"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_view_using_clause_adds_no_reads() {
        // `USING (dept_id)` names merge columns, not reads — consistent with
        // the SELECT path, where reads come from *reference* sites (a fan-in),
        // never the USING clause itself. The view still resolves to `emp`.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT e.id FROM emp e JOIN dept d USING (dept_id)) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("emp", "id")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_view_self_join_resolves_to_the_one_table() {
        // A self-join view: both instances are the same table, so columns
        // qualified through either alias agree on `emp` — the identity is
        // right even though key-preservedness is per-instance (unverifiable
        // without key metadata). The second instance stays a scanned
        // companion, so `emp` reads through it too.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT e1.id, e2.name FROM emp e1 JOIN emp e2 \
             ON e1.id = e2.id) VALUES (1, 'x')",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("emp", "id"), read("emp", "id")],
                writes: vec![write("emp", "id"), write("emp", "name")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_view_wildcard_projection_is_flagged() {
        // A wildcard projection names no attributable columns — the target
        // (and the positional pairing) is indeterminate: flag + drop.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT * FROM emp e JOIN dept d ON e.id = d.id) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }

    #[test]
    fn from_less_view_is_flagged() {
        // `(SELECT 1)` has no FROM — no base table exists: flag + drop.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT 1) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }

    #[test]
    fn derived_factor_in_view_is_flagged() {
        // A derived table as the view's FROM factor (single or joined) is not
        // a plain base table — the gate rejects both shapes.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT x.a FROM (SELECT a FROM t) x) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT e.id FROM emp e JOIN (SELECT 1 AS id) d ON e.id = d.id) \
             VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }

    #[test]
    fn single_table_view_over_a_cte_is_flagged() {
        // The single-table view path takes the same CTE-target check as a
        // plain-name INSERT — a CTE is read-only.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "WITH c AS (SELECT 1 AS a FROM x) INSERT INTO (SELECT a FROM c) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }

    #[test]
    fn join_view_expression_projection_is_flagged() {
        // A non-column projection item (`e.id + 1`) leaves the join view's
        // target indeterminate — unlike the single-table view (whose base is
        // shape-determined and falls to the column-less path), a join view
        // needs every column attributable: flag + drop.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT e.id + 1 FROM emp e JOIN dept d ON e.id = d.id) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }

    #[test]
    fn join_view_resolves_when_a_cte_is_only_a_companion() {
        // A declared CTE in the view's FROM is fine as long as the *target*
        // attributes to a real base table — the CTE-target check gates the
        // write, not the whole view. The companion CTE stays a best-effort
        // scan (its reference reads surface against the CTE's own name, not
        // through its body like a `CteRef` would — documented compromise),
        // and the CTE body's reads surface via the usual unreferenced-CTE
        // rule.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "WITH c AS (SELECT 1 AS id FROM x) \
             INSERT INTO (SELECT e.name FROM emp e JOIN c ON e.id = c.id) VALUES ('a')",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("emp", "id"), read("c", "id")],
                writes: vec![write("emp", "name")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_view_with_a_top_level_cte_source_traces_through_it() {
        // A top-level CTE consumed only by the *source* interacts with the
        // join view exactly like any source: the CTE reference resolves
        // through its body (`c.n → x.n`), and relation lineage lands on the
        // attributed base table.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "WITH c AS (SELECT n FROM x) \
             INSERT INTO (SELECT e.name FROM emp e JOIN dept d ON e.dept_id = d.id) \
             SELECT n FROM c",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("x", "n"), read("emp", "dept_id"), read("dept", "id")],
                writes: vec![write("emp", "name")],
                lineage: vec![passthrough(col("x", "n"), relation("emp", "name"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_inside_the_target_view_is_flagged() {
        // A WITH *inside* the target view is rejected by the shape gate
        // (`with: None`) — its CTE could shadow a FROM name, so resolving
        // through it would need full CTE machinery: flag + drop.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (WITH c AS (SELECT 1 AS id FROM x) \
             SELECT e.id FROM emp e JOIN dept d ON e.id = d.id) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }

    #[test]
    fn join_view_cte_target_is_flagged_not_written() {
        // The attributed target names a declared CTE — a read-only relation,
        // never a write target (the single-table view and plain-name INSERT
        // paths flag the same shape): flag + drop rather than fabricate a
        // write to `c`.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "WITH c AS (SELECT 1 AS id FROM x) \
             INSERT INTO (SELECT c2.id FROM c c2 JOIN emp e ON c2.id = e.id) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }

    #[test]
    fn join_view_straddling_columns_are_flagged() {
        // The projected columns attribute to *different* relations — no single
        // table receives the row (real Oracle rejects this shape: the INTO
        // columns must belong to one key-preserved table) — flag + drop.
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT e.id, d.active FROM emp e JOIN dept d \
             ON e.dept_id = d.id) VALUES (1, 1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }

    #[test]
    fn join_view_unqualified_without_catalog_is_flagged() {
        // Catalog-free, an unqualified projected column has no determinable
        // owner among the joined relations — flag + drop (the catalog-owner
        // rule resolves it when a catalog lists it in exactly one relation).
        assert_column_ops_with_dialect(
            &OracleDialect {},
            "INSERT INTO (SELECT id FROM emp JOIN dept ON emp.dept_id = dept.id) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
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
