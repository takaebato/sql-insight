use crate::support::*;

mod reported {
    use super::*;
    use sql_insight::sqlparser::dialect::BigQueryDialect;

    #[test]
    fn unsupported_statement_reports_diagnostic() {
        assert_column_ops(
            "CREATE INDEX idx ON t1 (a)",
            ColumnOperation {
                statement_kind: StatementKind::Unsupported,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }

    #[test]
    fn over_qualified_read_table_reports_diagnostic() {
        // A FROM table with more than `catalog.schema.name` segments can't
        // be represented, so it's dropped (the projected `a` is left
        // unresolved) and flagged with `TooManyTableQualifiers`.
        assert_column_ops(
            "SELECT a FROM catalog.schema.table.extra",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![unresolved("a")],
                writes: vec![],
                lineage: vec![passthrough(unresolved("a"), out("a", 0))],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::TooManyTableQualifiers)],
            },
        );
    }

    #[test]
    fn over_qualified_write_target_reports_diagnostic() {
        // An INSERT target with too many segments can't be represented, so
        // the whole statement's surfaces are empty and it's flagged.
        assert_column_ops(
            "INSERT INTO catalog.schema.table.extra (a) VALUES (1)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::TooManyTableQualifiers)],
            },
        );
    }

    #[test]
    fn wildcard_in_projection_reports_diagnostic() {
        // Whole-value pin-down on the structural shape; assert_column_ops
        // compares diagnostics by kind only. The message text and span
        // coordinates are verified separately below since this test's
        // *purpose* is to confirm both are populated.
        let ops = extract("SELECT * FROM t1");
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
        // Span info ("at L1:C8") is duplicated in message and surfaced
        // as structured data for programmatic consumers.
        assert!(
            ops.diagnostics[0].message.contains("at L1:C8"),
            "expected span suffix in message, got: {}",
            ops.diagnostics[0].message
        );
        let span = ops.diagnostics[0]
            .span
            .expect("wildcard token carries a span");
        assert_eq!(span.start.line, 1);
        assert_eq!(span.start.column, 8);
    }

    #[test]
    fn qualified_wildcard_in_projection_reports_diagnostic() {
        assert_column_ops(
            "SELECT t1.* FROM t1",
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
    fn wildcard_replace_expression_contributes_reads_and_lineage() {
        // The `*` stays suppressed, but a `REPLACE (expr AS col)` is a real
        // value-producing output: `products.price` reads and feeds `price`
        // (Transformation), exactly like a standalone `price * 1.1 AS price`.
        assert_column_ops_with_dialect(
            &BigQueryDialect {},
            "SELECT * REPLACE (price * 1.1 AS price) FROM products",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("products", "price")],
                writes: vec![],
                lineage: vec![transformation(col("products", "price"), out("price", 0))],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn multiple_statements_produce_multiple_results() {
        let sql = "SELECT t1.a FROM t1; SELECT t2.b FROM t2";
        assert_nth_column_ops(
            sql,
            0,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
        assert_nth_column_ops(
            sql,
            1,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t2", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t2", "b"), out("b", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn wildcard_select_yields_no_column_ops() {
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
    fn insert_column_count_mismatch_is_flagged() {
        // 3 target columns but the source projects 1: `writes` still lists all
        // three (they come from syntax), but lineage can only pair the first,
        // so the surplus is silently dropped — flagged with an arity mismatch
        // so the writes-vs-lineage gap reads as "couldn't pair", not "no source".
        assert_column_ops(
            "INSERT INTO t (a, b, c) SELECT x FROM s",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "x")],
                writes: vec![write("t", "a"), write("t", "b"), write("t", "c")],
                lineage: vec![passthrough(col("s", "x"), relation("t", "a"))],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsArityMismatch)],
            },
        );
    }

    #[test]
    fn insert_matching_column_count_is_not_flagged() {
        // The arity check must not fire on a well-formed INSERT.
        assert_column_ops(
            "INSERT INTO t (a, b) SELECT x, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![write("t", "a"), write("t", "b")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("t", "a")),
                    passthrough(col("s", "y"), relation("t", "b")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_values_arity_mismatch_is_flagged() {
        // An explicit column list against a VALUES row: 3 columns, 2 values →
        // flagged like the SELECT-source case. All three columns still surface
        // as writes (from syntax); a VALUES source has no traceable lineage.
        assert_column_ops(
            "INSERT INTO t (a, b, c) VALUES (1, 2)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t", "a"), write("t", "b"), write("t", "c")],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsArityMismatch)],
            },
        );
    }

    #[test]
    fn insert_with_wildcard_source_drops_lineage_and_skips_arity_diagnostic() {
        // A wildcard in the source projection (`SELECT *, y`) makes the column
        // count / positions indeterminate (wildcards aren't expanded), so the
        // positional pairing can't be trusted: relation lineage is dropped and
        // the arity check is skipped (no false `InsertColumnsArityMismatch`).
        // The target columns still surface as `writes`, with `WildcardSuppressed`
        // flagging the gap — matching a pure `SELECT *` source.
        assert_column_ops(
            "INSERT INTO t (a, b) SELECT *, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "y")],
                writes: vec![write("t", "a"), write("t", "b")],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn unaliased_ctas_expression_column_is_flagged() {
        // `a + 1` has no alias, so the created table's column can't be named
        // from the SQL text (engines auto-name it, e.g. `?column?`). It's
        // dropped from writes / lineage — but flagged, not silently lost. The
        // source `s.a` still surfaces as a read.
        assert_column_ops(
            "CREATE TABLE t AS SELECT a + 1 FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("s", "a")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::AnonymousColumnsSuppressed)],
            },
        );
    }

    #[test]
    fn aliased_ctas_expression_column_is_not_flagged() {
        // Aliasing the expression names the column, so it surfaces in writes /
        // lineage and nothing is flagged — the practical fix for the above.
        assert_column_ops(
            "CREATE TABLE t AS SELECT a + 1 AS x FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("s", "a")],
                writes: vec![write("t", "x")],
                lineage: vec![transformation(col("s", "a"), relation("t", "x"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unaliased_select_into_expression_column_is_flagged() {
        // `SELECT … INTO` lowers to a CTAS, so an unaliased expression column is
        // unnameable and dropped — and now flagged, like `CREATE TABLE … AS`
        // (the two are equivalent; the SELECT INTO path previously dropped it
        // silently).
        assert_column_ops(
            "SELECT a + 1 INTO t2 FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::AnonymousColumnsSuppressed)],
            },
        );
    }

    #[test]
    fn insert_columns_unresolved_message_reflects_the_cause() {
        // A `SELECT *` source can't be paired even with a catalog (its arity is
        // unknown), so the message blames the wildcard, not a missing catalog.
        let wildcard = extract("INSERT INTO dst SELECT * FROM src");
        let msg = wildcard
            .diagnostics
            .iter()
            .find(|d| d.kind == ColumnLevelDiagnosticKind::InsertColumnsUnresolved)
            .expect("expected InsertColumnsUnresolved")
            .message
            .clone();
        assert!(
            msg.contains("SELECT *"),
            "wildcard cause should mention `SELECT *`: {msg}"
        );
        assert!(
            !msg.contains("without a catalog"),
            "wildcard cause must not blame the catalog: {msg}"
        );

        // A determinate source with no catalog genuinely can't fill the target
        // columns — there the message does cite the missing catalog.
        let no_catalog = extract("INSERT INTO dst SELECT a FROM src");
        let msg2 = no_catalog
            .diagnostics
            .iter()
            .find(|d| d.kind == ColumnLevelDiagnosticKind::InsertColumnsUnresolved)
            .expect("expected InsertColumnsUnresolved")
            .message
            .clone();
        assert!(
            msg2.contains("without a catalog"),
            "no-catalog cause should say so: {msg2}"
        );
    }

    #[test]
    fn ctas_explicit_column_count_mismatch_is_flagged() {
        // 3 explicit columns but the source projects 1: like the INSERT form,
        // `writes` lists all three (from syntax) but lineage pairs only the
        // first, so the surplus is flagged with an arity mismatch.
        assert_column_ops(
            "CREATE TABLE t (a INT, b INT, c INT) AS SELECT x FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("s", "x")],
                writes: vec![write("t", "a"), write("t", "b"), write("t", "c")],
                lineage: vec![passthrough(col("s", "x"), relation("t", "a"))],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsArityMismatch)],
            },
        );
    }

    #[test]
    fn create_view_explicit_column_count_mismatch_is_flagged() {
        // CREATE VIEW with an explicit column list shares the arity check.
        assert_column_ops(
            "CREATE VIEW v (a, b, c) AS SELECT x FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateView,
                reads: vec![read("s", "x")],
                writes: vec![write("v", "a"), write("v", "b"), write("v", "c")],
                lineage: vec![passthrough(col("s", "x"), relation("v", "a"))],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::InsertColumnsArityMismatch)],
            },
        );
    }

    #[test]
    fn ctas_explicit_columns_with_wildcard_source_drops_lineage_no_arity() {
        // An explicit column list pairs positionally; a wildcard source makes
        // positions indeterminate, so lineage is dropped and the arity check
        // skipped (no false mismatch) — matching the INSERT form. Writes still
        // surface; `WildcardSuppressed` flags the gap.
        assert_column_ops(
            "CREATE TABLE t (a INT, b INT) AS SELECT *, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("s", "y")],
                writes: vec![write("t", "a"), write("t", "b")],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn ctas_implicit_columns_with_wildcard_source_keeps_name_followed_lineage() {
        // The implicit form takes each source output's own name, so a wildcard
        // there merely omits the unexpanded columns — the named `y` still maps
        // to its same-named target column (no misattribution), unlike the
        // explicit case above.
        assert_column_ops(
            "CREATE TABLE t AS SELECT *, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("s", "y")],
                writes: vec![write("t", "y")],
                lineage: vec![passthrough(col("s", "y"), relation("t", "y"))],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }
}

/// Coverage for the large "unsupported statement" dispatch arm in
/// `Resolver::visit_statement` (statement.rs:105-219), which folds ~110
/// DDL / session / transaction / show / utility `Statement` variants
/// into a single `record_unsupported_statement` call.
///
/// Each statement here parses under `GenericDialect` and must resolve to
/// `StatementKind::Unsupported` with exactly one
/// `UnsupportedStatement` diagnostic and no reads / writes / lineage.
/// Driving every variant individually means adding a new `Statement`
/// case to that arm (a compile-forced edit) also wants a line here,
/// keeping the arm honest. Variants that need a non-Generic dialect to
/// parse (`LOCK TABLES`, `LISTEN` / `NOTIFY`, `ALTER ROLE` / `SESSION`,
/// MySQL `LOAD DATA`, …) are out of scope for this Generic-only sweep.
#[cfg(test)]
mod unsupported_statement_coverage {
    use super::*;
    use sql_insight::sqlparser::dialect::GenericDialect;

    /// Statements that should all land in the unsupported arm: one
    /// `Unsupported` op carrying a single `UnsupportedStatement`
    /// diagnostic, with the three data surfaces empty.
    const UNSUPPORTED: &[&str] = &[
        "ANALYZE TABLE t",
        "SET x = 1",
        "MSCK REPAIR TABLE t",
        "INSTALL foo",
        "LOAD foo",
        "CALL foo()",
        "COPY t FROM 'f'",
        "OPEN cur",
        "CLOSE cur",
        "CREATE INDEX i ON t (a)",
        "CREATE ROLE r",
        "CREATE SERVER s FOREIGN DATA WRAPPER w",
        "CREATE POLICY p ON t",
        "CREATE EXTENSION ext",
        "DROP EXTENSION ext",
        "DROP FUNCTION f",
        "DROP DOMAIN d",
        "DROP PROCEDURE p",
        "DROP POLICY p ON t",
        "DECLARE c CURSOR FOR SELECT 1",
        "FETCH 1 IN c",
        "DISCARD ALL",
        "SHOW FUNCTIONS",
        "SHOW VARIABLE x",
        "SHOW STATUS",
        "SHOW VARIABLES",
        "SHOW CREATE TABLE t",
        "SHOW COLUMNS FROM t",
        "SHOW DATABASES",
        "SHOW SCHEMAS",
        "SHOW TABLES",
        "SHOW VIEWS",
        "SHOW COLLATION",
        "USE db",
        "START TRANSACTION",
        "BEGIN",
        "COMMENT ON TABLE t IS 'x'",
        "COMMIT",
        "ROLLBACK",
        "CREATE SCHEMA s",
        "CREATE DATABASE d",
        "CREATE FUNCTION f() RETURNS INT AS 'x' LANGUAGE SQL",
        "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
        "DROP TRIGGER tr ON t",
        "CREATE PROCEDURE p() AS BEGIN SELECT 1; END",
        "ASSERT 1 = 1",
        "GRANT SELECT ON t TO u",
        "DENY SELECT ON t TO u",
        "REVOKE SELECT ON t FROM u",
        "DEALLOCATE p",
        "EXECUTE p",
        "PREPARE p AS SELECT 1",
        "KILL 5",
        "EXPLAIN SELECT 1",
        "SAVEPOINT s",
        "RELEASE SAVEPOINT s",
        "CACHE TABLE t",
        "UNCACHE TABLE t",
        "CREATE SEQUENCE seq",
        "CREATE DOMAIN d AS INT",
        "CREATE TYPE ty AS (a INT)",
        "PRAGMA x",
        "UNLOAD (SELECT 1) TO 's'",
        "OPTIMIZE TABLE t",
        "RENAME TABLE t TO t2",
        "PRINT 'x'",
        "RETURN 1",
        "CREATE USER u",
        "ALTER USER u",
        "VACUUM",
        "RESET x",
        "FLUSH TABLES",
        "ALTER POLICY p ON t RENAME TO p2",
        "ALTER TYPE ty ADD VALUE 'z'",
        "DROP SECRET s",
        "ATTACH DATABASE 'f' AS d",
    ];

    #[test]
    fn unsupported_statements_report_only_a_diagnostic() {
        for sql in UNSUPPORTED {
            assert_unsupported(sql, &GenericDialect {});
        }
    }

    /// Statements that only parse under a specific dialect. Each entry
    /// exercises a distinct `Statement::*` pattern in the unsupported
    /// fold arm that `GenericDialect` cannot reach.
    const UNSUPPORTED_DIALECT_SPECIFIC: &[(&str, &str)] = &[
        ("mysql", "LOCK TABLES t READ"),
        ("mysql", "UNLOCK TABLES"),
        ("postgres", "LISTEN channel1"),
        ("postgres", "NOTIFY channel1"),
        ("postgres", "ALTER ROLE r WITH PASSWORD 'p'"),
    ];

    #[test]
    fn unsupported_statements_dialect_specific() {
        use sql_insight::sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};
        for (dialect_name, sql) in UNSUPPORTED_DIALECT_SPECIFIC {
            match *dialect_name {
                "mysql" => assert_unsupported(sql, &MySqlDialect {}),
                "postgres" => assert_unsupported(sql, &PostgreSqlDialect {}),
                other => panic!("unknown dialect tag in fixture: {other}"),
            }
        }
    }

    fn assert_unsupported(sql: &str, dialect: &dyn sql_insight::sqlparser::dialect::Dialect) {
        let op = extract_column_operations(dialect, sql)
            .unwrap()
            .remove(0)
            .unwrap();
        assert_eq!(
            op.statement_kind,
            StatementKind::Unsupported,
            "for SQL: {sql}"
        );
        assert!(op.reads.is_empty(), "for SQL: {sql}");
        assert!(op.writes.is_empty(), "for SQL: {sql}");
        assert!(op.lineage.is_empty(), "for SQL: {sql}");
        let kinds: Vec<_> = op.diagnostics.iter().map(|d| &d.kind).collect();
        assert_eq!(
            kinds,
            vec![&ColumnLevelDiagnosticKind::UnsupportedStatement],
            "for SQL: {sql}"
        );
    }
}

mod supported_kind_only_coverage {
    //! Statements that classify as a supported `StatementKind` but
    //! produce no column-level reads / writes / lineage / diagnostics.
    //! Pins that these don't accidentally fall through to
    //! `Unsupported`, and that the column-level path returns empty
    //! surfaces for DDL-only / DROP / TRUNCATE / bare VALUES shapes.

    use super::*;
    use sql_insight::sqlparser::dialect::GenericDialect;

    const KIND_ONLY: &[(&str, StatementKind)] = &[
        ("CREATE TABLE t (a INT)", StatementKind::CreateTable),
        ("DROP TABLE t", StatementKind::Drop),
        ("DROP VIEW v", StatementKind::Drop),
        ("DROP MATERIALIZED VIEW mv", StatementKind::Drop),
        ("TRUNCATE TABLE t", StatementKind::Truncate),
        ("VALUES (1, 2)", StatementKind::Select),
    ];

    #[test]
    fn supported_kind_only_statements_produce_empty_surfaces() {
        for (sql, expected_kind) in KIND_ONLY {
            let result = extract_column_operations(&GenericDialect {}, sql).unwrap();
            let op = result[0].as_ref().unwrap();
            assert_eq!(op.statement_kind, *expected_kind, "kind for SQL: {sql}");
            assert!(op.reads.is_empty(), "reads for SQL: {sql}");
            assert!(op.writes.is_empty(), "writes for SQL: {sql}");
            assert!(op.lineage.is_empty(), "lineage for SQL: {sql}");
            assert!(op.diagnostics.is_empty(), "diagnostics for SQL: {sql}");
        }
    }
}
