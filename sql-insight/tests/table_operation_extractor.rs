use sql_insight::diagnostic::{TableLevelDiagnostic, TableLevelDiagnosticKind};
use sql_insight::extractor::*;
use sql_insight::sqlparser::dialect::{Dialect, GenericDialect, MySqlDialect, PostgreSqlDialect};
use sql_insight::ResolutionKind;
use sql_insight::*;

/// Assert two collections are equal as multisets (order-independent).
/// `reads` / `lineage` are returned in source order, but these tests
/// compare as multisets to stay span-agnostic — pinning the *set* of
/// extracted facts. `writes` keeps a plain `assert_eq!` (source order,
/// order-sensitive).
macro_rules! assert_unordered_eq {
    ($actual:expr, $expected:expr, $msg:expr $(,)?) => {{
        let actual = $actual;
        let mut remaining = $expected;
        // Tie the element types so an empty-vs-empty comparison still infers.
        let _ = actual.iter().chain(remaining.iter()).count();
        assert_eq!(
            actual.len(),
            remaining.len(),
            "{}\n  actual:   {actual:#?}\n  expected: {remaining:#?}",
            $msg
        );
        for item in &actual {
            match remaining.iter().position(|e| e == item) {
                Some(i) => {
                    remaining.remove(i);
                }
                None => panic!(
                    "{}: unexpected item not in expected: {item:#?}\n  actual: {actual:#?}",
                    $msg
                ),
            }
        }
    }};
}

fn table(name: &str) -> TableReference {
    TableReference {
        catalog: None,
        schema: None,
        name: name.into(),
    }
}

/// A read occurrence. These tests run catalog-free, so every read
/// resolves to [`ResolutionKind::Inferred`].
fn read(name: &str) -> TableRead {
    TableRead {
        reference: table(name),
        resolution: ResolutionKind::Inferred,
    }
}

fn edge(source: &str, target: &str) -> TableLineageEdge {
    TableLineageEdge {
        source: read(source),
        target: table(target),
    }
}

/// Whole-value-ish assertion: pin down the full
/// `TableOperation` for `sql`, but compare diagnostics
/// by **kind sequence only** — message text and span coordinates
/// are ignored. This lets tests focus on "what was extracted"
/// without coupling to diagnostic wording or column offsets that
/// shift when SQL is reformatted.
///
/// Tests that genuinely care about the message / span shape
/// should fall back to per-field `assert_eq!`.
fn assert_ops(sql: &str, expected: TableOperation) {
    assert_nth_ops_with(sql, 0, &GenericDialect {}, expected);
}

fn assert_ops_with(sql: &str, dialect: &dyn Dialect, expected: TableOperation) {
    assert_nth_ops_with(sql, 0, dialect, expected);
}

/// Like `assert_ops`, but for multi-statement SQL — pins down the
/// statement at `index` in the parsed batch. Compose calls to pin
/// down every statement in a batch separately.
fn assert_nth_ops(sql: &str, index: usize, expected: TableOperation) {
    assert_nth_ops_with(sql, index, &GenericDialect {}, expected);
}

fn assert_nth_ops_with(sql: &str, index: usize, dialect: &dyn Dialect, expected: TableOperation) {
    let result = extract_table_operations(dialect, sql).unwrap();
    let actual = result
        .into_iter()
        .nth(index)
        .unwrap_or_else(|| panic!("statement {index} missing in result for SQL: {sql}"))
        .unwrap();
    let TableOperation {
        statement_kind,
        reads,
        writes,
        lineage,
        diagnostics,
    } = expected;
    assert_eq!(
        actual.statement_kind, statement_kind,
        "kind for SQL: {sql} (statement {index})"
    );
    assert_unordered_eq!(
        actual.reads,
        reads,
        format!("reads for SQL: {sql} (statement {index})")
    );
    assert_eq!(
        actual.writes, writes,
        "writes for SQL: {sql} (statement {index})"
    );
    assert_unordered_eq!(
        actual.lineage,
        lineage,
        format!("lineage for SQL: {sql} (statement {index})")
    );
    let actual_kinds: Vec<_> = actual.diagnostics.iter().map(|d| d.kind.clone()).collect();
    let expected_kinds: Vec<_> = diagnostics.iter().map(|d| d.kind.clone()).collect();
    assert_eq!(
        actual_kinds, expected_kinds,
        "diagnostic kinds for SQL: {sql} (statement {index})"
    );
}

/// Construct a placeholder `TableLevelDiagnostic` for the
/// `expected.diagnostics` list in `assert_ops`. Only the kind is
/// compared; the message and span are placeholders.
fn diag(kind: TableLevelDiagnosticKind) -> TableLevelDiagnostic {
    TableLevelDiagnostic {
        kind,
        message: String::new(),
        span: None,
    }
}

mod select {
    use super::*;

    #[test]
    fn select_emits_reads_only() {
        assert_ops(
            "SELECT id FROM users",
            TableOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("users")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn select_with_join_emits_one_read_per_table() {
        // The `*` does not surface a diagnostic at table granularity —
        // WildcardSuppressed is a column-level concern and is filtered
        // out of table-level output (the table set is complete
        // regardless of wildcard expansion).
        assert_ops(
            "SELECT * FROM t1 JOIN t2 ON t1.id = t2.id",
            TableOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1"), read("t2")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn select_with_subquery_emits_read_for_every_table() {
        assert_ops(
            "SELECT t1.a FROM t1 WHERE id IN (SELECT id FROM t2)",
            TableOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1"), read("t2")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_body_tables_emit_reads_but_cte_name_does_not() {
        // Only t1 is a table reference; t2 is the CTE binding and stays out.
        assert_ops(
            "WITH t2 AS (SELECT id FROM t1) SELECT t2.id FROM t2",
            TableOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn bare_values_emits_nothing() {
        // `VALUES (1, 2)` parses as a query whose body is a VALUES
        // clause — no table references, no writes, no lineage.
        assert_ops(
            "VALUES (1, 2)",
            TableOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }
}

mod set_operations {
    use super::*;

    #[test]
    fn union_emits_read_for_each_branch_table() {
        // Each UNION branch walks its own FROM, so both tables
        // surface in reads. No lineage: bare SELECT statements
        // never produce table-level data movement.
        assert_ops(
            "SELECT a FROM t1 UNION SELECT b FROM t2",
            TableOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1"), read("t2")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn intersect_and_except_match_union_shape() {
        // SetOperator variant doesn't influence table-level
        // surfacing — INTERSECT and EXCEPT both walk both branches.
        for op in ["INTERSECT", "EXCEPT"] {
            let sql = format!("SELECT a FROM t1 {op} SELECT b FROM t2");
            assert_ops(
                &sql,
                TableOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1"), read("t2")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }
    }

    #[test]
    fn insert_select_union_emits_one_lineage_edge_per_branch() {
        // INSERT-SELECT-UNION moves data from each branch into the
        // target, so both source tables surface as lineage sources.
        assert_ops(
            "INSERT INTO dst SELECT a FROM t1 UNION SELECT b FROM t2",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t1"), read("t2")],
                writes: vec![table("dst")],
                lineage: vec![edge("t1", "dst"), edge("t2", "dst")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ctas_with_union_body_emits_lineage_per_branch() {
        assert_ops(
            "CREATE TABLE dst AS SELECT a FROM t1 UNION SELECT b FROM t2",
            TableOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("t1"), read("t2")],
                writes: vec![table("dst")],
                lineage: vec![edge("t1", "dst"), edge("t2", "dst")],
                diagnostics: vec![],
            },
        );
    }
}

mod diagnostics {
    use super::*;

    #[test]
    fn unsupported_statement_reports_diagnostic() {
        assert_ops(
            "CREATE INDEX idx ON t1 (a)",
            TableOperation {
                statement_kind: StatementKind::Unsupported,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(TableLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }

    #[test]
    fn multiple_statements_produce_multiple_results() {
        let sql = "SELECT * FROM t1; SELECT * FROM t2";
        assert_nth_ops(
            sql,
            0,
            TableOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
        assert_nth_ops(
            sql,
            1,
            TableOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t2")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }
}

mod insert {
    use super::*;

    #[test]
    fn insert_values_emits_write_only() {
        assert_ops(
            "INSERT INTO t1 (a, b) VALUES (1, 2)",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_emits_write_and_read() {
        assert_ops(
            "INSERT INTO t1 SELECT * FROM t2",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1")],
                diagnostics: vec![],
            },
        );
    }
}

mod update {
    use super::*;

    #[test]
    fn update_basic_emits_write_only() {
        assert_ops(
            "UPDATE t1 SET a = 1",
            TableOperation {
                statement_kind: StatementKind::Update,
                reads: vec![],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_with_subquery_predicate_emits_write_plus_read() {
        assert_ops(
            "UPDATE t1 SET a = 1 WHERE id IN (SELECT id FROM t2)",
            TableOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_with_from_clause_treats_from_as_read() {
        // FROM t2 contributes rows to the UPDATE target → t2 → t1
        // lineage edge. SET RHS scalar subquery from t3 feeds the new
        // value → t3 → t1 lineage edge. WHERE predicate subquery from
        // t4 is predicate-only → no lineage.
        assert_ops_with(
            "UPDATE t1 SET a = (SELECT b FROM t3) FROM t2 WHERE t1.id IN (SELECT id FROM t4)",
            &PostgreSqlDialect {},
            TableOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t2"), read("t3"), read("t4")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1"), edge("t3", "t1")],
                diagnostics: vec![],
            },
        );
    }
}

mod delete {
    use super::*;

    #[test]
    fn delete_from_emits_write_only() {
        assert_ops(
            "DELETE FROM t1",
            TableOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn delete_from_with_subquery_predicate_emits_write_plus_read() {
        assert_ops(
            "DELETE FROM t1 WHERE id IN (SELECT id FROM t2)",
            TableOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn delete_with_target_list_overlaps_writes_and_reads() {
        // `DELETE t1, t2 FROM t1 JOIN t2 JOIN t3` — t1 and t2 are both
        // deletion targets (writes) AND row sources (reads via FROM).
        assert_ops_with(
            "DELETE t1, t2 FROM t1 INNER JOIN t2 INNER JOIN t3",
            &MySqlDialect {},
            TableOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("t1"), read("t2"), read("t3")],
                writes: vec![table("t1"), table("t2")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn delete_with_using_lists_target_in_writes_and_source_in_reads() {
        assert_ops(
            "DELETE FROM t1, t2 USING t1 INNER JOIN t2 INNER JOIN t3",
            TableOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("t1"), read("t2"), read("t3")],
                writes: vec![table("t1"), table("t2")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn delete_resolves_target_alias_to_real_table() {
        assert_ops_with(
            "DELETE t1_alias FROM t1 AS t1_alias JOIN t2 ON t1_alias.a = t2.a",
            &MySqlDialect {},
            TableOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("t1"), read("t2")],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }
}

mod merge {
    use super::*;

    #[test]
    fn merge_emits_write_target_and_read_source() {
        assert_ops(
            "MERGE INTO t1 USING t2 ON t1.id = t2.id \
             WHEN MATCHED THEN UPDATE SET t1.b = t2.b",
            TableOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1")],
                diagnostics: vec![],
            },
        );
    }
}

mod ddl {
    use super::*;

    #[test]
    fn create_table_emits_write_only() {
        assert_ops(
            "CREATE TABLE t1 (a INT)",
            TableOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn create_table_as_select_emits_write_and_read() {
        assert_ops(
            "CREATE TABLE t1 AS SELECT * FROM t2",
            TableOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn create_view_emits_write_and_read() {
        assert_ops(
            "CREATE VIEW v1 AS SELECT * FROM t1",
            TableOperation {
                statement_kind: StatementKind::CreateView,
                reads: vec![read("t1")],
                writes: vec![table("v1")],
                lineage: vec![edge("t1", "v1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_table_emits_write_only() {
        assert_ops(
            "ALTER TABLE t1 ADD COLUMN a INT",
            TableOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_view_emits_write_and_read_with_lineage() {
        // ALTER VIEW ... AS SELECT replaces the view definition with
        // a new SELECT body — semantically the same shape as CREATE
        // VIEW, so it should emit lineage too.
        assert_ops(
            "ALTER VIEW v1 AS SELECT * FROM t1",
            TableOperation {
                statement_kind: StatementKind::AlterView,
                reads: vec![read("t1")],
                writes: vec![table("v1")],
                lineage: vec![edge("t1", "v1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn drop_view_emits_one_write_per_name() {
        assert_ops(
            "DROP VIEW v1, v2",
            TableOperation {
                statement_kind: StatementKind::Drop,
                reads: vec![],
                writes: vec![table("v1"), table("v2")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn drop_materialized_view_emits_one_write_per_name() {
        assert_ops(
            "DROP MATERIALIZED VIEW mv1",
            TableOperation {
                statement_kind: StatementKind::Drop,
                reads: vec![],
                writes: vec![table("mv1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn drop_table_emits_one_write_per_name() {
        assert_ops(
            "DROP TABLE t1, t2",
            TableOperation {
                statement_kind: StatementKind::Drop,
                reads: vec![],
                writes: vec![table("t1"), table("t2")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn truncate_emits_one_write_per_name() {
        assert_ops(
            "TRUNCATE TABLE t1, t2",
            TableOperation {
                statement_kind: StatementKind::Truncate,
                reads: vec![],
                writes: vec![table("t1"), table("t2")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn drop_function_still_unsupported() {
        // DROP variants that target non-relation objects don't carry a
        // meaningful table-level operation.
        assert_ops(
            "DROP FUNCTION my_fn",
            TableOperation {
                statement_kind: StatementKind::Unsupported,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(TableLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }
}

mod lineage {
    use super::*;

    #[test]
    fn insert_select_emits_lineage_from_source_to_target() {
        assert_ops(
            "INSERT INTO t1 SELECT * FROM t2",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_join_emits_one_lineage_edge_per_source() {
        assert_ops(
            "INSERT INTO t1 SELECT * FROM t2 JOIN t3 ON t2.id = t3.id",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2"), read("t3")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1"), edge("t3", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn predicate_subquery_does_not_feed_lineage() {
        // t3 is referenced only inside `WHERE id IN (SELECT id FROM t3)`,
        // so it must not appear as a lineage source even though it does
        // appear in `reads`.
        assert_ops(
            "INSERT INTO t1 SELECT * FROM t2 WHERE id IN (SELECT id FROM t3)",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2"), read("t3")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_source_feeds_lineage() {
        // The source projects an aggregate over a GROUP BY — feeding still
        // traces through the `Aggregate` to the scanned table.
        assert_ops(
            "INSERT INTO t SELECT SUM(s.x) FROM s GROUP BY s.y",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s")],
                writes: vec![table("t")],
                lineage: vec![edge("s", "t")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn scalar_subquery_in_projection_feeds_lineage() {
        // A value-position scalar subquery feeds: both its inner table and
        // the outer FROM are lineage sources of the target.
        assert_ops(
            "INSERT INTO t SELECT (SELECT max(u.z) FROM u) FROM s",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("u"), read("s")],
                writes: vec![table("t")],
                lineage: vec![edge("u", "t"), edge("s", "t")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn case_branches_feed_their_scalar_subqueries() {
        // A CASE in the projection: its `then` / `else` value branches feed,
        // so both branch subqueries' tables are lineage sources (the `when`
        // condition is filter position and does not).
        assert_ops(
            "INSERT INTO t SELECT CASE WHEN s.a > 0 THEN (SELECT z FROM u) \
             ELSE (SELECT w FROM v) END FROM s",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("u"), read("v"), read("s")],
                writes: vec![table("t")],
                lineage: vec![edge("u", "t"), edge("v", "t"), edge("s", "t")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_on_predicate_does_not_promote_to_lineage() {
        // t4 is in JOIN ON's predicate subquery — touches as read
        // but doesn't promote to a lineage edge (predicate position excluded
        // from data-feeding chain).
        assert_ops(
            "INSERT INTO t1 SELECT * FROM t2 JOIN t3 ON t2.id = t3.id \
             AND t2.id IN (SELECT id FROM t4)",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2"), read("t3"), read("t4")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1"), edge("t3", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn exists_subquery_in_projection_is_filter() {
        // EXISTS returns a boolean — its inner refs never carry
        // value into the projection. Even though the CASE
        // expression is lexically a projection (value position),
        // EXISTS itself is shape-based predicate and `x` must
        // not become a lineage source.
        assert_ops(
            "INSERT INTO t1 SELECT \
             CASE WHEN EXISTS (SELECT 1 FROM x) THEN 1 ELSE 0 END \
         FROM s",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s"), read("x")],
                writes: vec![table("t1")],
                lineage: vec![edge("s", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn in_subquery_rhs_in_projection_is_filter() {
        // IN's RHS subquery columns are match-test targets, not
        // value contributors — only the boolean result flows.
        // `x` stays out of lineage even when the IN sits inside
        // a projection-position CASE.
        assert_ops(
            "INSERT INTO t1 SELECT \
             CASE WHEN s.id IN (SELECT id FROM x) THEN 1 ELSE 0 END \
         FROM s",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s"), read("x")],
                writes: vec![table("t1")],
                lineage: vec![edge("s", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn exists_subquery_in_update_set_is_filter() {
        // Same shape-based rule applies inside UPDATE SET. The
        // SET RHS is value-position by default, but EXISTS still
        // acts as a predicate — `x` must not feed `t1`.
        assert_ops(
            "UPDATE t1 SET col = CASE WHEN EXISTS (SELECT 1 FROM x) THEN 1 ELSE 0 END",
            TableOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("x")],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn scalar_subquery_in_projection_still_feeds_lineage() {
        // Counterpoint to the EXISTS / IN cases above: a scalar
        // subquery returns a value, so its inner column does
        // flow into the projection and onto the target.
        assert_ops(
            "INSERT INTO t1 SELECT (SELECT v FROM x LIMIT 1) FROM s",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s"), read("x")],
                writes: vec![table("t1")],
                lineage: vec![edge("s", "t1"), edge("x", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn any_subquery_rhs_in_projection_is_filter() {
        // `x = ANY (SELECT col FROM y)` tests `x` against the
        // rows of y and returns boolean — y's column values
        // don't flow as values. Even when the ANY sits in a
        // projection-position CASE, `x` must not become a
        // lineage source.
        assert_ops(
            "INSERT INTO t1 SELECT \
             CASE WHEN s.id = ANY (SELECT id FROM x) THEN 1 ELSE 0 END \
         FROM s",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s"), read("x")],
                writes: vec![table("t1")],
                lineage: vec![edge("s", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn all_subquery_rhs_in_projection_is_filter() {
        // ALL has the same shape as ANY — RHS is filter, not
        // value contributor.
        assert_ops(
            "INSERT INTO t1 SELECT \
             CASE WHEN s.id > ALL (SELECT id FROM x) THEN 1 ELSE 0 END \
         FROM s",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s"), read("x")],
                writes: vec![table("t1")],
                lineage: vec![edge("s", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_scalar_subquery_in_set_feeds_lineage() {
        assert_ops(
            "UPDATE t1 SET col = (SELECT v FROM t2)",
            TableOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_predicate_subquery_does_not_feed_lineage() {
        assert_ops(
            "UPDATE t1 SET col = 1 WHERE id IN (SELECT id FROM t2)",
            TableOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn create_table_as_select_emits_lineage() {
        assert_ops(
            "CREATE TABLE t1 AS SELECT * FROM t2",
            TableOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn create_view_emits_lineage() {
        assert_ops(
            "CREATE VIEW v1 AS SELECT * FROM t1",
            TableOperation {
                statement_kind: StatementKind::CreateView,
                reads: vec![read("t1")],
                writes: vec![table("v1")],
                lineage: vec![edge("t1", "v1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_emits_lineage_from_source_to_target() {
        assert_ops(
            "MERGE INTO t1 USING t2 ON t1.id = t2.id \
             WHEN MATCHED THEN UPDATE SET t1.b = t2.b",
            TableOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_when_not_matched_insert_emits_lineage() {
        // WHEN NOT MATCHED THEN INSERT moves a value from the
        // source row into the target, same as WHEN MATCHED UPDATE.
        assert_ops(
            "MERGE INTO t1 USING t2 ON t1.id = t2.id \
             WHEN NOT MATCHED THEN INSERT (a) VALUES (t2.b)",
            TableOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![edge("t2", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_with_only_delete_action_emits_no_lineage() {
        // WHEN MATCHED THEN DELETE doesn't move data — the source
        // is only used to pick which target rows to delete. A
        // MERGE whose only WHEN clauses are DELETEs therefore
        // emits no lineage.
        assert_ops(
            "MERGE INTO t1 USING t2 ON t1.id = t2.id \
             WHEN MATCHED THEN DELETE",
            TableOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_data_reaches_write_target() {
        assert_ops(
            "INSERT INTO t1 WITH cte AS (SELECT * FROM s) SELECT * FROM cte",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s")],
                writes: vec![table("t1")],
                lineage: vec![edge("s", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_predicate_subquery_does_not_leak_into_lineage() {
        // x is in the CTE body's WHERE predicate subquery — touches
        // as read but doesn't promote to a lineage edge.
        assert_ops(
            "INSERT INTO t1 WITH cte AS (\
             SELECT * FROM s WHERE id IN (SELECT id FROM x)\
         ) SELECT * FROM cte",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s"), read("x")],
                writes: vec![table("t1")],
                lineage: vec![edge("s", "t1")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unreferenced_cte_body_tables_do_not_leak_into_lineage() {
        // `cte` is defined but never referenced — the INSERT pulls
        // its value from a constant projection. No data actually
        // flows from `s` to `t1` (an optimizer would prune the CTE
        // entirely). `s` should still appear in `reads` since the
        // SQL text touches it, but it must not produce a lineage
        // edge into `t1`.
        //
        // This pins down the boundary between "table is referenced
        // in the SQL" (reads) and "data actually moves from this
        // table to the target" (lineage). Driven by
        // `collapsed_feeding_table_sources`: real-table captures
        // sitting inside an unreferenced synthetic body sit under
        // that body's scope, get filtered by the synthetic-body
        // mask in the top loop, and never get reached by a
        // top-level `FROM cte` recursion either.
        assert_ops(
            "WITH cte AS (SELECT a FROM s) INSERT INTO t1 SELECT 1",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s")],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn recursive_cte_emits_lineage_from_anchor_real_table() {
        // The recursive CTE's anchor reads `x`; the recursive
        // branch's self-reference contributes no new real source.
        // External `FROM cte` use feeds the INSERT target, so
        // collapse should reach through the body to `x` and emit
        // a single `x → t` edge.
        assert_ops(
            "WITH RECURSIVE cte AS (\
             SELECT id FROM x UNION ALL \
             SELECT id + 1 FROM cte WHERE id < 10\
         ) INSERT INTO t SELECT id FROM cte",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("x")],
                writes: vec![table("t")],
                lineage: vec![edge("x", "t")],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn recursive_cte_self_only_emits_no_lineage() {
        // Pathological body — only a self-reference, no real
        // anchor source. The CTE has no real feeder, so no edge
        // is emitted even though the INSERT has a target. Pins
        // that the self-reference collapse terminates at the
        // pre-bind stub without traversing into the body scope
        // and re-emitting the self-cycle.
        assert_ops(
            "WITH RECURSIVE cte AS (SELECT id FROM cte) \
         INSERT INTO t SELECT id FROM cte",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![table("t")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn select_only_statement_emits_no_lineage() {
        assert_ops(
            "SELECT * FROM t1 JOIN t2 ON t1.id = t2.id",
            TableOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1"), read("t2")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_values_emits_no_lineage() {
        assert_ops(
            "INSERT INTO t1 VALUES (1, 2)",
            TableOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn delete_with_subquery_predicate_emits_no_lineage() {
        // DELETE doesn't move data — no lineage, even when a subquery
        // references another table.
        assert_ops(
            "DELETE FROM t1 WHERE id IN (SELECT id FROM t2)",
            TableOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("t2")],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn truncate_emits_no_lineage() {
        assert_ops(
            "TRUNCATE TABLE t1",
            TableOperation {
                statement_kind: StatementKind::Truncate,
                reads: vec![],
                writes: vec![table("t1")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }
}

/// Pins the table-level [`ResolutionKind`] carried on `reads` /
/// `lineage` sources once a catalog is supplied. Catalog-free
/// extraction is exercised throughout the rest of the file via the
/// `read()` helper (always `Inferred`); these cover the catalog-aware
/// arms (`Cataloged` / `Ambiguous`) the resolver can now produce.
mod catalog_resolution {
    use super::*;
    use sql_insight::catalog::{Catalog, CatalogTable};

    /// The canonical `public.<name>` identity a registered table
    /// surfaces with on the write / lineage-target side (bare
    /// `TableReference`). The canonical identity is case-exact, so its
    /// segments are quoted (the GenericDialect quote, `"`).
    fn pub_table(name: &str) -> TableReference {
        use sql_insight::sqlparser::ast::Ident;
        TableReference {
            catalog: None,
            schema: Some(Ident::with_quote('"', "public")),
            name: Ident::with_quote('"', name),
        }
    }

    /// A read the catalog uniquely identified. The tables here are
    /// registered under `public`, so a unique match canonicalizes
    /// the surfaced identity to `public.<name>`.
    fn cataloged(name: &str) -> TableRead {
        TableRead {
            reference: pub_table(name),
            resolution: ResolutionKind::Cataloged,
        }
    }

    /// A read whose (under-qualified) name the catalog matched to
    /// several registered tables. The reference is still surfaced.
    fn ambiguous(name: &str) -> TableRead {
        TableRead {
            reference: table(name),
            resolution: ResolutionKind::Ambiguous,
        }
    }

    fn ops_with_catalog(sql: &str, catalog: &Catalog) -> TableOperation {
        let options = ExtractorOptions::new().with_catalog(catalog);
        extract_table_operations_with_options(&GenericDialect {}, sql, options)
            .unwrap()
            .remove(0)
            .unwrap()
    }

    #[test]
    fn catalog_hit_marks_read_cataloged() {
        // A unique registered hit → Cataloged at table granularity,
        // independent of whether columns were registered.
        let catalog = Catalog::new().table(CatalogTable::new("public", "t1"));
        let ops = ops_with_catalog("SELECT a FROM t1", &catalog);
        assert_eq!(ops.reads, vec![cataloged("t1")]);
    }

    #[test]
    fn catalog_miss_marks_read_inferred() {
        // Empty catalog → no match → Inferred, same as catalog-less.
        let ops = ops_with_catalog("SELECT a FROM t1", &Catalog::new());
        assert_eq!(ops.reads, vec![read("t1")]);
    }

    #[test]
    fn lineage_source_carries_catalog_resolution() {
        // The cataloged INSERT source `t2` surfaces Cataloged in both
        // `reads` and the lineage source; the write target stays a
        // bare `TableReference`.
        let catalog = Catalog::new().table(CatalogTable::new("public", "t2"));
        let ops = ops_with_catalog("INSERT INTO t1 SELECT * FROM t2", &catalog);
        assert_eq!(ops.reads, vec![cataloged("t2")]);
        assert_eq!(ops.writes, vec![table("t1")]);
        assert_eq!(
            ops.lineage,
            vec![TableLineageEdge {
                source: cataloged("t2"),
                target: table("t1"),
            }]
        );
    }

    #[test]
    fn ambiguous_registration_marks_read_ambiguous() {
        // Bare `users` right-anchored-matches two registrations under
        // different schemas (no default schema to disambiguate) →
        // Ambiguous, with the reference still surfaced.
        let catalog: Catalog = [
            CatalogTable::new("s1", "users"),
            CatalogTable::new("s2", "users"),
        ]
        .into_iter()
        .collect();
        let ops = ops_with_catalog("SELECT a FROM users", &catalog);
        assert_eq!(ops.reads, vec![ambiguous("users")]);
    }

    #[test]
    fn canonicalizes_both_source_and_target_to_registered_path() {
        // Both tables are written bare but registered under
        // `public`; a unique match canonicalizes the surfaced
        // identity everywhere — the read, the write target, and both
        // ends of the lineage edge carry `public.<name>`.
        let catalog: Catalog = [
            CatalogTable::new("public", "orders"),
            CatalogTable::new("public", "staging"),
        ]
        .into_iter()
        .collect();
        let ops = ops_with_catalog("INSERT INTO orders SELECT id FROM staging", &catalog);
        assert_eq!(ops.reads, vec![cataloged("staging")]);
        assert_eq!(ops.writes, vec![pub_table("orders")]);
        assert_eq!(
            ops.lineage,
            vec![TableLineageEdge {
                source: cataloged("staging"),
                target: pub_table("orders"),
            }]
        );
    }
}
