use crate::support::*;

mod with_in_dml {
    //! `WITH cte AS (...) <DML>` — Postgres / Sqlite / standard
    //! SQL syntax for binding CTEs visible to a DML statement.
    //! sqlparser typically parses these as Query-with-WITH at the
    //! source level for INSERT, and wraps Update / Delete in
    //! various ways. These tests pin down what actually surfaces
    //! through the resolver.
    use super::*;

    #[test]
    fn with_in_insert_select_collapses_cte_to_target() {
        assert_column_ops(
            "WITH cte AS (SELECT x FROM s) INSERT INTO t (a) SELECT x FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "x")],
                writes: vec![write("t", "a")],
                lineage: vec![passthrough(col("s", "x"), relation("t", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn with_in_update_via_scalar_subquery_collapses() {
        // CTE referenced from the SET RHS scalar subquery. The
        // subquery emits no QueryOutput edge of its own (Option B);
        // the UPDATE SET assignment captures its source (collapsed
        // through cte to s.x) and emits the single Relation edge.
        // Transformation (the value is derived through max + the
        // subquery wrapping).
        assert_column_ops(
            "WITH cte AS (SELECT max(x) AS m FROM s) \
             UPDATE t SET a = (SELECT m FROM cte) WHERE id = 1",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("s", "x"), read("t", "id")],
                writes: vec![write("t", "a")],
                lineage: vec![transformation(col("s", "x"), relation("t", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn with_in_delete_via_predicate_subquery_keeps_cte_source_as_read() {
        // The DELETE target `t` lives in its own scope (the SetExpr
        // DML scope), so the outer predicate `id` resolves
        // unambiguously to `t`. The predicate subquery feeds a
        // filter, so it emits no lineage (Option B); its refs (s.id
        // via the cte) still surface in reads. DELETE has no column
        // lineage of its own — so lineage is empty.
        assert_column_ops(
            "WITH cte AS (SELECT id FROM s WHERE flag) \
             DELETE FROM t WHERE id IN (SELECT id FROM cte)",
            ColumnOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("s", "id"), read("s", "flag"), read("t", "id")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn with_multiple_ctes_chained_into_insert() {
        // Two CTEs where `b` references `a`. INSERT then pulls
        // from `b`. Composition walks back through both layers
        // to the real table.
        assert_column_ops(
            "WITH a AS (SELECT id FROM t1), \
                  b AS (SELECT id + 1 AS x FROM a) \
             INSERT INTO t2 (col) SELECT x FROM b",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t1", "id")],
                writes: vec![write("t2", "col")],
                lineage: vec![transformation(col("t1", "id"), relation("t2", "col"))],
                diagnostics: vec![],
            },
        );
    }
}

mod merge {
    use super::*;

    #[test]
    fn merge_when_matched_update_emits_lineage_and_write() {
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
    fn merge_when_not_matched_insert_emits_lineage_and_write() {
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a)",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![
                    read("t", "id"),
                    read("s", "id"),
                    read("s", "id"),
                    read("s", "a"),
                ],
                writes: vec![write("t", "id"), write("t", "a")],
                lineage: vec![
                    passthrough(col("s", "id"), relation("t", "id")),
                    passthrough(col("s", "a"), relation("t", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_column_less_insert_without_catalog_is_reads_only() {
        // Without a catalog the target's column names are unknown, so a
        // column-less MERGE INSERT can't pair its values — the values
        // surface as reads but there's no write / lineage, flagged with
        // `InsertColumnsUnresolved`. (With a catalog they pair positionally;
        // see the catalog-aware test.)
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.a)",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![
                    read("t", "id"),
                    read("s", "id"),
                    read("s", "id"),
                    read("s", "a"),
                ],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsUnresolved)],
            },
        );
    }

    #[test]
    fn merge_delete_action_emits_no_lineage_no_write() {
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![read("t", "id"), read("s", "id")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_combined_clauses_emit_per_clause_lineage_and_writes() {
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET t.a = s.a \
             WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a)",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![
                    read("t", "id"),
                    read("s", "id"),
                    read("s", "a"),
                    read("s", "id"),
                    read("s", "a"),
                ],
                writes: vec![write("t", "a"), write("t", "id"), write("t", "a")],
                lineage: vec![
                    passthrough(col("s", "a"), relation("t", "a")),
                    passthrough(col("s", "id"), relation("t", "id")),
                    passthrough(col("s", "a"), relation("t", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_update_transformation_kind_propagates() {
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET t.a = s.a + 1",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![read("t", "id"), read("s", "id"), read("s", "a")],
                writes: vec![write("t", "a")],
                lineage: vec![transformation(col("s", "a"), relation("t", "a"))],
                diagnostics: vec![],
            },
        );
    }
}

mod ctas_view {
    use super::*;

    #[test]
    fn ctas_pairs_source_projection_with_inferred_column_names() {
        // CREATE TABLE AS SELECT — no explicit column list, so target
        // columns follow the source projection's inferred names
        // (alias > bare ident).
        assert_column_ops(
            "CREATE TABLE t AS SELECT x AS a, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![write("t", "a"), write("t", "y")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("t", "a")),
                    passthrough(col("s", "y"), relation("t", "y")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ctas_with_explicit_columns_overrides_projection_names() {
        // Explicit column list wins over inferred names.
        assert_column_ops(
            "CREATE TABLE t (p INT, q INT) AS SELECT x, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![write("t", "p"), write("t", "q")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("t", "p")),
                    passthrough(col("s", "y"), relation("t", "q")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ctas_propagates_transformation_kind() {
        assert_column_ops(
            "CREATE TABLE t AS SELECT SUM(x) AS total FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("s", "x")],
                writes: vec![write("t", "total")],
                lineage: vec![transformation(col("s", "x"), relation("t", "total"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn create_view_pairs_source_projection() {
        assert_column_ops(
            "CREATE VIEW v AS SELECT x AS a, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateView,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![write("v", "a"), write("v", "y")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("v", "a")),
                    passthrough(col("s", "y"), relation("v", "y")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn create_view_with_explicit_columns_uses_list() {
        assert_column_ops(
            "CREATE VIEW v (a, b) AS SELECT x, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateView,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![write("v", "a"), write("v", "b")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("v", "a")),
                    passthrough(col("s", "y"), relation("v", "b")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_view_pairs_replacement_query_projection() {
        assert_column_ops(
            "ALTER VIEW v AS SELECT x AS a FROM s",
            ColumnOperation {
                statement_kind: StatementKind::AlterView,
                reads: vec![read("s", "x")],
                writes: vec![write("v", "a")],
                lineage: vec![passthrough(col("s", "x"), relation("v", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ctas_unnamed_projection_yields_no_paired_lineage() {
        // `SELECT 1` has no column ref and no inferable name, so the
        // CTAS source produces no lineage / no write for that slot.
        assert_column_ops(
            "CREATE TABLE t AS SELECT 1 FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_with_distinct_args_marker() {
        // COUNT(DISTINCT user_id) — an aggregate call, so the source
        // feeds into the output as a Transformation.
        assert_column_ops(
            "SELECT COUNT(DISTINCT user_id) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "user_id")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "user_id"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_with_filter_clause_marker() {
        // SUM(x) FILTER (WHERE y > 0) — `x` is the aggregated
        // value (lineage source). `y` is the filter predicate; same
        // disposition as a WHERE clause's column — it gates which
        // rows are summed but its value doesn't flow. `y` surfaces
        // in `reads` only.
        assert_column_ops(
            "SELECT SUM(x) FILTER (WHERE y > 0) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "y")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_aggregate_then_outer_expression_still_transformation() {
        // Outer wraps the CTE column in an expression (s + 1) —
        // collapse: outer Transformation × inner Transformation =
        // Transformation.
        assert_column_ops(
            "WITH cte AS (SELECT SUM(a) AS s FROM t1) SELECT s + 1 FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "a"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }
}

mod on_conflict {
    //! ON CONFLICT (Postgres / Sqlite) and ON DUPLICATE KEY UPDATE
    //! (MySQL) both sit in `Insert.on: Option<OnInsert>`. The
    //! resolver walks both, with subtle differences:
    //!
    //! - Postgres: `EXCLUDED.<col>` is a pseudo-table for the
    //!   would-be-inserted row. Bound as synthetic so refs
    //!   through it filter out of `reads` but still emit valid
    //!   Relation lineage edges into the target. The synthetic
    //!   binding's columns mirror the INSERT target's columns.
    //! - MySQL: `VALUES(<col>)` is a function-call form for the
    //!   same concept. No EXCLUDED binding (it would make
    //!   unqualified refs ambiguous against the INSERT target);
    //!   the inner ref resolves to the INSERT target like a
    //!   regular self-reference.
    //!
    //! DO UPDATE SET targets become writes on the INSERT target
    //! table — same role as a standalone UPDATE SET. The optional
    //! DO UPDATE WHERE clause walks in filter context.
    use super::*;
    use sql_insight::sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};

    fn assert_column_ops_with_dialect(
        sql: &str,
        dialect: &dyn sql_insight::sqlparser::dialect::Dialect,
        expected: ColumnOperation,
    ) {
        let actual = extract_column_operations(dialect, sql)
            .unwrap()
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("no statements in result for SQL: {sql}"))
            .unwrap();
        assert_column_ops_inner(sql, 0, actual, expected);
    }

    /// Construct a `ColumnRead` for the synthetic EXCLUDED
    /// pseudo-table — used only as a Source in lineage edges, not
    /// as a real table. The EXCLUDED binding inherits its
    /// `output_columns` from the INSERT source's per-operand
    /// projections; for VALUES sources (no projection captured) the
    /// binding ends up with `output_columns: None`, so refs against
    /// it can't be `Cataloged` and surface as `Inferred`.
    fn excluded(name: &str) -> ColumnRead {
        ColumnRead {
            reference: ColumnReference {
                table: Some(TableReference {
                    catalog: None,
                    schema: None,
                    name: "EXCLUDED".into(),
                }),
                name: name.into(),
            },
            resolution: ResolutionKind::Inferred,
        }
    }

    #[test]
    fn pg_on_conflict_do_update_set_excluded_emits_lineage_and_write() {
        // DO UPDATE SET b = EXCLUDED.b
        //   - writes: t.a, t.b from INSERT columns plus another
        //     t.b for the SET target.
        //   - reads: empty (EXCLUDED is synthetic-filtered;
        //     VALUES (1, 2) are literals).
        //   - lineage: EXCLUDED.b → Relation(t.b), Passthrough.
        assert_column_ops_with_dialect(
            "INSERT INTO t (a, b) VALUES (1, 2) ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                lineage: vec![passthrough(excluded("b"), relation("t", "b"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn pg_on_conflict_do_nothing_is_indistinguishable_from_plain_insert() {
        assert_column_ops_with_dialect(
            "INSERT INTO t (a, b) VALUES (1, 2) ON CONFLICT (a) DO NOTHING",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t", "a"), write("t", "b")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn pg_insert_select_with_on_conflict_collapses_excluded_to_source() {
        // EXCLUDED's output_columns come from the INSERT source
        // renamed to the target columns positionally. So
        // `EXCLUDED.b` collapses through to the source's position-1
        // projection (`y` from s) — the conflict-action lineage edge
        // bottoms out at the same real table as the
        // source-projection lineage edge.
        assert_column_ops_with_dialect(
            "INSERT INTO t (a, b) SELECT x, y FROM s \
             ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("t", "a")),
                    passthrough(col("s", "y"), relation("t", "b")),
                    passthrough(col("s", "y"), relation("t", "b")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn mysql_on_duplicate_key_update_values_func_self_references_target() {
        // MySQL `VALUES(<col>)` is the implicit-row form. Without
        // an EXCLUDED binding, the inner `b` ref resolves to t.b
        // (the INSERT target). Result: t.b shows up as a read
        // (the VALUES function call is a value-changing wrapper) and
        // the SET clause adds a Relation-target lineage edge t.b → t.b.
        assert_column_ops_with_dialect(
            "INSERT INTO t (a, b) VALUES (1, 2) \
             ON DUPLICATE KEY UPDATE b = VALUES(b)",
            &MySqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t", "b")],
                writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                lineage: vec![transformation(col("t", "b"), relation("t", "b"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn pg_insert_union_with_on_conflict_excluded_fans_out_to_each_branch() {
        // The source has TWO branches (one per UNION
        // branch), so EXCLUDED's output_columns also have two
        // groups — each with a position-0 item named after the
        // INSERT target column. `EXCLUDED.a` then collapses to
        // BOTH branches' position-0 source refs.
        assert_column_ops_with_dialect(
            "INSERT INTO t (a) SELECT x FROM s1 UNION SELECT y FROM s2 \
             ON CONFLICT (a) DO UPDATE SET a = EXCLUDED.a",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s1", "x"), read("s2", "y")],
                writes: vec![write("t", "a"), write("t", "a")],
                lineage: vec![
                    passthrough(col("s1", "x"), relation("t", "a")),
                    passthrough(col("s2", "y"), relation("t", "a")),
                    passthrough(col("s1", "x"), relation("t", "a")),
                    passthrough(col("s2", "y"), relation("t", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn pg_insert_aggregate_with_on_conflict_excluded_keeps_transformation_kind() {
        // SUM(x) makes the source projection a Transformation. When
        // EXCLUDED.total collapses back, collapse_lineage_kinds keeps the
        // transforming step → lineage kind stays Transformation even on
        // the conflict-action path.
        assert_column_ops_with_dialect(
            "INSERT INTO t (total) SELECT SUM(x) FROM s \
             ON CONFLICT (id) DO UPDATE SET total = EXCLUDED.total",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "x")],
                writes: vec![write("t", "total"), write("t", "total")],
                lineage: vec![
                    transformation(col("s", "x"), relation("t", "total")),
                    transformation(col("s", "x"), relation("t", "total")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn pg_on_conflict_do_update_with_where_clause_emits_read() {
        // DO UPDATE ... WHERE walks in filter context: `t.a` in the
        // WHERE expression surfaces as a read but not a lineage source.
        assert_column_ops_with_dialect(
            "INSERT INTO t (a, b) VALUES (1, 2) \
             ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b WHERE t.a > 0",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t", "a")],
                writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                lineage: vec![passthrough(excluded("b"), relation("t", "b"))],
                diagnostics: vec![],
            },
        );
    }

    // --- MySQL `INSERT … SET col = expr` (assignment form): like a
    // single-table UPDATE, each assignment writes its column and feeds
    // `RHS → target.col` lineage; the RHS resolves against the target.

    #[test]
    fn mysql_insert_set_literal_writes_only() {
        // A literal RHS has no source column → a write, no lineage.
        assert_column_ops_with_dialect(
            "INSERT INTO t SET a = 1",
            &MySqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t", "a")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn mysql_insert_set_expr_emits_transformation_lineage() {
        // `b = a + 1`: the RHS reads the target's own `a` (a value source,
        // so it also surfaces as a read) and transforms it into `b` →
        // `t.a → Relation(t.b)` transformation.
        assert_column_ops_with_dialect(
            "INSERT INTO t SET b = a + 1",
            &MySqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t", "a")],
                writes: vec![write("t", "b")],
                lineage: vec![transformation(col("t", "a"), relation("t", "b"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn mysql_insert_set_subquery_emits_lineage() {
        // `a = (SELECT v FROM s)`: the scalar subquery's output feeds the
        // target column; `s.v` surfaces as a read. The subquery wrapper
        // makes the edge a Transformation (not a bare passthrough).
        assert_column_ops_with_dialect(
            "INSERT INTO t SET a = (SELECT v FROM s)",
            &MySqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "v")],
                writes: vec![write("t", "a")],
                lineage: vec![transformation(col("s", "v"), relation("t", "a"))],
                diagnostics: vec![],
            },
        );
    }
}

mod returning {
    //! `RETURNING <select_items>` on INSERT / UPDATE / DELETE
    //! (Postgres / Sqlite extension) projects from the affected
    //! rows of the target table — treated like a top-level SELECT
    //! projection: each item contributes refs to `reads` and a
    //! `QueryOutput` lineage edge. Walked BEFORE the ON-clause for
    //! INSERT so any EXCLUDED binding doesn't ambify unqualified
    //! refs that collide with INSERT column names.
    use super::*;

    #[test]
    fn insert_values_with_returning_emits_target_reads_and_query_output() {
        assert_column_ops(
            "INSERT INTO t (a, b) VALUES (1, 2) RETURNING id",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t", "id")],
                writes: vec![write("t", "a"), write("t", "b")],
                lineage: vec![passthrough(col("t", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn returning_aliased_uses_alias_as_output_name() {
        assert_column_ops(
            "INSERT INTO t (a) VALUES (1) RETURNING id AS pk",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t", "id")],
                writes: vec![write("t", "a")],
                lineage: vec![passthrough(col("t", "id"), out("pk", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn returning_with_expression_marks_kind_transformation() {
        assert_column_ops(
            "INSERT INTO t (a) VALUES (1) RETURNING id + 1 AS bumped",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t", "id")],
                writes: vec![write("t", "a")],
                lineage: vec![transformation(col("t", "id"), out("bumped", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn returning_wildcard_records_wildcard_suppressed_diagnostic() {
        assert_column_ops(
            "INSERT INTO t (a) VALUES (1) RETURNING *",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t", "a")],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn update_returning_walks_target_columns() {
        assert_column_ops(
            "UPDATE t SET a = b + 1 WHERE id = 5 RETURNING id, a",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![
                    read("t", "b"),
                    read("t", "id"),
                    read("t", "id"),
                    read("t", "a"),
                ],
                writes: vec![write("t", "a")],
                lineage: vec![
                    transformation(col("t", "b"), relation("t", "a")),
                    passthrough(col("t", "id"), out("id", 0)),
                    passthrough(col("t", "a"), out("a", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn delete_returning_walks_target_columns() {
        assert_column_ops(
            "DELETE FROM t WHERE id = 5 RETURNING id, val",
            ColumnOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("t", "id"), read("t", "id"), read("t", "val")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t", "id"), out("id", 0)),
                    passthrough(col("t", "val"), out("val", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_with_returning_keeps_source_lineage_and_target_returning() {
        // Source SELECT's tables are out of scope by the time
        // RETURNING walks (their nested scope was popped after
        // resolve_query). So RETURNING refs resolve to the target
        // table alone, even when the bare name `id` exists in the
        // source too.
        assert_column_ops(
            "INSERT INTO t (a) SELECT x FROM s RETURNING id",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "x"), read("t", "id")],
                writes: vec![write("t", "a")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("t", "a")),
                    passthrough(col("t", "id"), out("id", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }
}

mod alter_table {
    //! ALTER TABLE produces column-level writes for column-naming
    //! operations: ADD COLUMN, DROP COLUMN, RENAME COLUMN, CHANGE
    //! COLUMN, MODIFY COLUMN, ALTER COLUMN. RENAME / CHANGE surface
    //! BOTH the old and new names — both ends of the rename are
    //! useful for downstream lineage consumers tracking column
    //! history. Schema-level operations (constraints, partitions,
    //! RENAME TABLE) contribute no column writes.
    use super::*;

    #[test]
    fn alter_table_add_column_emits_write() {
        assert_column_ops(
            "ALTER TABLE t ADD COLUMN c INT",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![write("t", "c")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_table_drop_column_emits_write() {
        assert_column_ops(
            "ALTER TABLE t DROP COLUMN c",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![write("t", "c")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_table_rename_column_emits_both_old_and_new() {
        // RENAME moves data from old to new; surface both for
        // downstream consumers tracking column history.
        assert_column_ops(
            "ALTER TABLE t RENAME COLUMN a TO b",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![write("t", "a"), write("t", "b")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_table_alter_column_emits_write_for_target_column() {
        assert_column_ops(
            "ALTER TABLE t ALTER COLUMN a SET NOT NULL",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![write("t", "a")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_table_multiple_ops_collects_all_target_columns() {
        // sqlparser parses multi-op ALTER as a single statement
        // with `operations: Vec<AlterTableOperation>`.
        assert_column_ops(
            "ALTER TABLE t ADD COLUMN c INT, DROP COLUMN d",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![write("t", "c"), write("t", "d")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_table_add_constraint_emits_no_column_writes() {
        // AddConstraint is schema-level — no column-level writes
        // surface (the table itself stays in table_op writes).
        assert_column_ops(
            "ALTER TABLE t ADD CONSTRAINT uq UNIQUE (a)",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }
}
