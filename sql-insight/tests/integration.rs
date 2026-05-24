//! Integration tests covering the public API surface end-to-end.
//!
//! `tests/integration.rs` is compiled as its own crate, so the
//! top-level items are equivalent to a `mod tests` in the library —
//! no extra wrapper module needed.

use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::test_utils::all_dialects;
use sql_insight::{
    extract_column_operations, extract_crud_tables, extract_table_operations, extract_tables,
    Catalog, ColumnLevelDiagnostic, ColumnLevelDiagnosticKind, ColumnLineageKind, ColumnSchema,
    ColumnTarget, CrudTables, NormalizerOptions, StatementKind, TableExtraction,
    TableLevelDiagnosticKind, TableReference, Tables,
};
use std::collections::HashMap;

mod format {
    use super::*;

    #[test]
    fn test_format() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'";
        for dialect in all_dialects() {
            let result = sql_insight::format(dialect.as_ref(), sql).unwrap();
            assert_eq!(
                result,
                ["SELECT a FROM t1 WHERE b = 1 AND c IN (2, 3) AND d LIKE '%foo'"],
                "Failed for dialect: {dialect:?}"
            )
        }
    }
}

mod normalize {
    use super::*;

    #[test]
    fn test_normalize() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'";
        for dialect in all_dialects() {
            let result = sql_insight::normalize(dialect.as_ref(), sql).unwrap();
            assert_eq!(
                result,
                ["SELECT a FROM t1 WHERE b = ? AND c IN (?, ?) AND d LIKE ?"],
                "Failed for dialect: {dialect:?}"
            )
        }
    }

    #[test]
    fn test_normalize_with_options() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3, 4); INSERT INTO t2 (a, b, c) VALUES (1, 2, 3), (4, 5, 6)";
        for dialect in all_dialects() {
            let result = sql_insight::normalize_with_options(
                dialect.as_ref(),
                sql,
                NormalizerOptions::new()
                    .with_unify_in_list(true)
                    .with_unify_values(true),
            )
            .unwrap();
            assert_eq!(
                result,
                [
                    "SELECT a FROM t1 WHERE b = ? AND c IN (...)",
                    "INSERT INTO t2 (a, b, c) VALUES (...)"
                ],
                "Failed for dialect: {dialect:?}"
            )
        }
    }
}

mod extract_crud_tables {
    use super::*;

    #[test]
    fn test_extract_crud_tables() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
        for dialect in all_dialects() {
            let result = extract_crud_tables(dialect.as_ref(), sql).unwrap();
            assert_eq!(
                result,
                vec![
                    Ok(CrudTables {
                        create_tables: vec![],
                        read_tables: vec![TableReference {
                            catalog: None,
                            schema: None,
                            name: "t1".into(),
                        }],
                        update_tables: vec![],
                        delete_tables: vec![],
                        diagnostics: vec![],
                    }),
                    Ok(CrudTables {
                        create_tables: vec![],
                        read_tables: vec![TableReference {
                            catalog: None,
                            schema: None,
                            name: "t2".into(),
                        }],
                        update_tables: vec![],
                        delete_tables: vec![],
                        diagnostics: vec![],
                    }),
                ],
                "Failed for dialect: {dialect:?}"
            )
        }
    }

    #[test]
    fn test_extract_crud_tables_with_cte() {
        let sql = "WITH t2 AS (SELECT id FROM t1) SELECT * FROM t2";
        for dialect in all_dialects() {
            let result = extract_crud_tables(dialect.as_ref(), sql).unwrap();
            assert_eq!(
                result,
                vec![Ok(CrudTables {
                    create_tables: vec![],
                    read_tables: vec![TableReference {
                        catalog: None,
                        schema: None,
                        name: "t1".into(),
                    }],
                    update_tables: vec![],
                    delete_tables: vec![],
                    diagnostics: vec![],
                })],
                "Failed for dialect: {dialect:?}"
            )
        }
    }
}

mod extract_tables {
    use super::*;

    #[test]
    fn test_extract_tables() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
        for dialect in all_dialects() {
            let result = extract_tables(dialect.as_ref(), sql).unwrap();
            let result = result
                .into_iter()
                .map(|result| result.map(TableExtraction::into_tables))
                .collect::<Vec<Result<Tables, sql_insight::error::Error>>>();
            assert_eq!(
                result,
                vec![
                    Ok(Tables(vec![TableReference {
                        catalog: None,
                        schema: None,
                        name: "t1".into(),
                    }])),
                    Ok(Tables(vec![TableReference {
                        catalog: None,
                        schema: None,
                        name: "t2".into(),
                    }])),
                ],
                "Failed for dialect: {dialect:?}"
            )
        }
    }

    #[test]
    fn test_extract_tables_with_cte() {
        let sql = "WITH t2 AS (SELECT id FROM t1) SELECT * FROM t2";
        for dialect in all_dialects() {
            let result = extract_tables(dialect.as_ref(), sql).unwrap();
            let result = result
                .into_iter()
                .map(|result| result.map(TableExtraction::into_tables))
                .collect::<Vec<Result<Tables, sql_insight::error::Error>>>();
            assert_eq!(
                result,
                vec![Ok(Tables(vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                }]))],
                "Failed for dialect: {dialect:?}"
            )
        }
    }

    #[test]
    fn test_extract_tables_reports_diagnostics() {
        let result = extract_tables(&GenericDialect {}, "SET x = 1").unwrap();
        let extraction = result.into_iter().next().unwrap().unwrap();
        assert_eq!(extraction.tables, vec![]);
        assert_eq!(extraction.diagnostics.len(), 1);
        assert_eq!(
            extraction.diagnostics[0].kind,
            TableLevelDiagnosticKind::UnsupportedStatement
        );
    }
}

mod extract_table_operations {
    use super::*;

    fn table(name: &str) -> TableReference {
        TableReference {
            catalog: None,
            schema: None,
            name: name.into(),
        }
    }

    #[test]
    fn select_classifies_kind_and_collects_reads() {
        let result =
            extract_table_operations(&GenericDialect {}, "SELECT a FROM t1", None).unwrap();
        let ops = result[0].as_ref().unwrap();
        assert_eq!(ops.statement_kind, StatementKind::Select);
        assert_eq!(ops.reads.len(), 1);
        assert_eq!(ops.reads[0], table("t1"));
        assert!(ops.writes.is_empty());
        assert!(ops.lineage.is_empty());
    }

    #[test]
    fn insert_select_emits_source_to_target_lineage() {
        let sql = "INSERT INTO orders (id, total) SELECT id, amount FROM staging";
        let result = extract_table_operations(&GenericDialect {}, sql, None).unwrap();
        let ops = result[0].as_ref().unwrap();
        assert_eq!(ops.statement_kind, StatementKind::Insert);
        assert_eq!(ops.reads, vec![table("staging")]);
        assert_eq!(ops.writes, vec![table("orders")]);
        assert_eq!(ops.lineage.len(), 1);
        assert_eq!(ops.lineage[0].source, table("staging"));
        assert_eq!(ops.lineage[0].target, table("orders"));
    }

    #[test]
    fn multi_statement_batch_returns_per_statement_results() {
        let sql = "SELECT * FROM t1; INSERT INTO t2 SELECT * FROM t3";
        let result = extract_table_operations(&GenericDialect {}, sql, None).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0].as_ref().unwrap().statement_kind,
            StatementKind::Select
        );
        assert_eq!(
            result[1].as_ref().unwrap().statement_kind,
            StatementKind::Insert
        );
    }

    #[test]
    fn unsupported_statement_surfaces_diagnostic() {
        let result =
            extract_table_operations(&GenericDialect {}, "CREATE INDEX idx ON t1 (a)", None)
                .unwrap();
        let ops = result[0].as_ref().unwrap();
        assert_eq!(ops.statement_kind, StatementKind::Unsupported);
        assert!(ops
            .diagnostics
            .iter()
            .any(|d| matches!(d.kind, TableLevelDiagnosticKind::UnsupportedStatement)));
    }
}

mod extract_column_operations {
    use super::*;

    fn col(table: &str, name: &str) -> sql_insight::ColumnReference {
        sql_insight::ColumnReference {
            table: Some(TableReference {
                catalog: None,
                schema: None,
                name: table.into(),
            }),
            name: name.into(),
        }
    }

    #[test]
    fn select_collects_per_column_reads() {
        let sql = "SELECT a FROM t1 WHERE b > 0";
        let result = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
        let ops = result[0].as_ref().unwrap();
        // Both the projection `a` and the filter `b` surface as reads
        // (occurrence list, no clause tag). value-vs-filter is
        // recovered structurally: `a` is also a lineage source, `b` is not.
        let names: Vec<_> = ops.reads.iter().map(|r| r.name.value.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        let lineage_sources: Vec<_> = ops
            .lineage
            .iter()
            .map(|f| f.source.name.value.as_str())
            .collect();
        assert_eq!(lineage_sources, vec!["a"]); // `b` (filter) is not a lineage source
    }

    #[test]
    fn insert_select_emits_per_column_lineage() {
        let sql = "INSERT INTO orders (id, total) SELECT id, amount FROM staging";
        let result = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
        let ops = result[0].as_ref().unwrap();
        assert_eq!(ops.lineage.len(), 2);
        // Both lineage edges are Passthrough into Relation targets.
        for edge in &ops.lineage {
            assert!(matches!(edge.kind, ColumnLineageKind::Passthrough));
            assert!(matches!(edge.target, ColumnTarget::Relation(_)));
        }
    }

    #[test]
    fn aggregate_projection_marks_transformation() {
        let sql = "INSERT INTO summary (total) SELECT SUM(amount) FROM staging";
        let result = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
        let ops = result[0].as_ref().unwrap();
        assert_eq!(ops.lineage.len(), 1);
        assert_eq!(ops.lineage[0].source, col("staging", "amount"));
        // SUM changes the value → Transformation (the 2-way kind no
        // longer distinguishes aggregation from other transforms).
        assert!(matches!(
            ops.lineage[0].kind,
            ColumnLineageKind::Transformation
        ));
    }

    #[test]
    fn wildcard_in_projection_yields_wildcard_suppressed_diagnostic() {
        let result =
            extract_column_operations(&GenericDialect {}, "SELECT * FROM t1", None).unwrap();
        let ops = result[0].as_ref().unwrap();
        assert!(ops
            .diagnostics
            .iter()
            .any(|d| matches!(d.kind, ColumnLevelDiagnosticKind::WildcardSuppressed)));
    }
}

mod catalog {
    use super::*;

    #[derive(Debug, Default)]
    struct TestCatalog {
        tables: HashMap<String, Vec<&'static str>>,
    }

    impl TestCatalog {
        fn with(mut self, name: &str, cols: Vec<&'static str>) -> Self {
            self.tables.insert(name.to_string(), cols);
            self
        }
    }

    impl Catalog for TestCatalog {
        fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>> {
            self.tables.get(table.name.value.as_str()).map(|cols| {
                cols.iter()
                    .map(|c| ColumnSchema {
                        name: c.to_string(),
                    })
                    .collect()
            })
        }
    }

    fn count_kind(diagnostics: &[ColumnLevelDiagnostic], kind: ColumnLevelDiagnosticKind) -> usize {
        diagnostics.iter().filter(|d| d.kind == kind).count()
    }

    #[test]
    fn insert_without_explicit_columns_pairs_via_catalog() {
        // Without explicit `(a, b)`, the resolver needs the catalog to
        // know the target's columns and pair source projections.
        let catalog = TestCatalog::default()
            .with("orders", vec!["id", "total"])
            .with("staging", vec!["id", "amount"]);
        let sql = "INSERT INTO orders SELECT id, amount FROM staging";
        let result = extract_column_operations(&GenericDialect {}, sql, Some(&catalog)).unwrap();
        let ops = result[0].as_ref().unwrap();
        // Two lineage edges into Relation targets orders.id / orders.total.
        let relation_targets: Vec<_> = ops
            .lineage
            .iter()
            .filter_map(|f| match &f.target {
                ColumnTarget::Relation(c) => Some(c.name.value.as_str()),
                _ => None,
            })
            .collect();
        assert!(relation_targets.contains(&"id"));
        assert!(relation_targets.contains(&"total"));
    }

    #[test]
    fn ambiguous_column_diagnostic_only_with_catalog() {
        let catalog = TestCatalog::default()
            .with("t1", vec!["a"])
            .with("t2", vec!["a"]);
        let sql = "SELECT a FROM t1 JOIN t2 ON t1.a = t2.a";

        let with = extract_column_operations(&GenericDialect {}, sql, Some(&catalog)).unwrap();
        let without = extract_column_operations(&GenericDialect {}, sql, None).unwrap();

        let with_count = count_kind(
            &with[0].as_ref().unwrap().diagnostics,
            ColumnLevelDiagnosticKind::AmbiguousColumn,
        );
        let without_count = count_kind(
            &without[0].as_ref().unwrap().diagnostics,
            ColumnLevelDiagnosticKind::AmbiguousColumn,
        );
        assert_eq!(with_count, 1, "with catalog should report AmbiguousColumn");
        assert_eq!(
            without_count, 0,
            "without catalog should stay silent (Unknown schemas)"
        );
    }

    #[test]
    fn unresolved_column_diagnostic_only_with_catalog() {
        let catalog = TestCatalog::default().with("t1", vec!["a", "b"]);
        let sql = "SELECT missing FROM t1";

        let with = extract_column_operations(&GenericDialect {}, sql, Some(&catalog)).unwrap();
        let without = extract_column_operations(&GenericDialect {}, sql, None).unwrap();

        let with_count = count_kind(
            &with[0].as_ref().unwrap().diagnostics,
            ColumnLevelDiagnosticKind::UnresolvedColumn,
        );
        let without_count = count_kind(
            &without[0].as_ref().unwrap().diagnostics,
            ColumnLevelDiagnosticKind::UnresolvedColumn,
        );
        assert_eq!(with_count, 1);
        assert_eq!(without_count, 0);
    }
}

mod diagnostics {
    use super::*;

    #[test]
    fn unsupported_statement_kind_surfaces_via_table_operations() {
        let result =
            extract_table_operations(&GenericDialect {}, "CREATE INDEX idx ON t (a)", None)
                .unwrap();
        let ops = result[0].as_ref().unwrap();
        assert!(ops
            .diagnostics
            .iter()
            .any(|d| matches!(d.kind, TableLevelDiagnosticKind::UnsupportedStatement)));
    }

    #[test]
    fn wildcard_diagnostic_carries_precise_span() {
        // Pin down line *and* column for the `*` token. The wildcard
        // sits at column 8 of `SELECT * FROM t1` (1-indexed,
        // immediately after `SELECT `). This pin-down means that if
        // span propagation regresses — e.g. the resolver starts using
        // the surrounding SELECT node's span instead of the wildcard
        // token's — this test will fail with a concrete diff.
        let result =
            extract_column_operations(&GenericDialect {}, "SELECT * FROM t1", None).unwrap();
        let ops = result[0].as_ref().unwrap();
        let wildcard = ops
            .diagnostics
            .iter()
            .find(|d| matches!(d.kind, ColumnLevelDiagnosticKind::WildcardSuppressed))
            .expect("WildcardSuppressed not found");
        assert!(
            wildcard.message.contains("at L1:"),
            "message should embed source location, got: {}",
            wildcard.message
        );
        let span = wildcard.span.expect("wildcard token carries a span");
        assert_eq!(span.start.line, 1, "wildcard line");
        assert_eq!(span.start.column, 8, "wildcard column");
    }

    #[test]
    fn unresolved_column_diagnostic_carries_precise_span() {
        // The catalog is needed to fire UnresolvedColumn — without it
        // the resolver stays silent (Unknown schemas could contain
        // anything). With the catalog, `missing` is unambiguously
        // not a column of t1.
        //
        // `missing` starts at column 8 in `SELECT missing FROM t1`.
        // Pinning down the column here is the regression net for span
        // plumbing through the resolver's catalog-aware path —
        // separate from the wildcard path, which goes through
        // projection.rs.
        #[derive(Debug, Default)]
        struct C(HashMap<String, Vec<&'static str>>);
        impl Catalog for C {
            fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>> {
                self.0.get(table.name.value.as_str()).map(|cols| {
                    cols.iter()
                        .map(|c| ColumnSchema {
                            name: c.to_string(),
                        })
                        .collect()
                })
            }
        }
        let mut catalog = C::default();
        catalog.0.insert("t1".to_string(), vec!["a", "b"]);

        let result =
            extract_column_operations(&GenericDialect {}, "SELECT missing FROM t1", Some(&catalog))
                .unwrap();
        let ops = result[0].as_ref().unwrap();
        let unresolved = ops
            .diagnostics
            .iter()
            .find(|d| matches!(d.kind, ColumnLevelDiagnosticKind::UnresolvedColumn))
            .expect("UnresolvedColumn not found");
        let span = unresolved.span.expect("ident token carries a span");
        assert_eq!(span.start.line, 1);
        assert_eq!(span.start.column, 8);
    }
}

/// Cross-cutting properties that should hold for every parseable SQL
/// statement, regardless of shape. These are the safety net for
/// future resolver / extractor changes: a hand-written corpus walks
/// through both extractors and each statement is checked against a
/// handful of structural invariants.
///
/// On failure the assertion panics with the SQL + statement index +
/// which invariant tripped, so a single regression points straight at
/// what changed.
mod invariants {
    use super::*;
    use sql_insight::{ColumnLineageEdge, ColumnOperation, ColumnReference, TableOperation};
    use std::collections::HashSet;

    /// Curated corpus chosen to stress the major shapes the resolver
    /// handles. New patterns should be added here as the resolver
    /// grows, not as one-off tests scattered across the codebase.
    fn corpus() -> &'static [&'static str] {
        &[
            // SELECT shapes
            "SELECT a FROM t1",
            "SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.id = t2.id",
            "SELECT a FROM t1 WHERE b > 0 GROUP BY a HAVING COUNT(*) > 1",
            "SELECT a FROM t1 ORDER BY b",
            "SELECT SUM(x) OVER (PARTITION BY p ORDER BY o) AS total FROM t1",
            "SELECT CASE WHEN a > 0 THEN b ELSE c END FROM t1",
            // CTE / derived / subquery
            "WITH cte AS (SELECT id FROM t1) SELECT id FROM cte",
            "SELECT x FROM (SELECT a + 1 AS x FROM t1) sub",
            "SELECT a FROM t1 WHERE id IN (SELECT id FROM t2)",
            // Set operations
            "SELECT a FROM t1 UNION SELECT b FROM t2",
            "SELECT a FROM t1 INTERSECT SELECT b FROM t2",
            // DML
            "INSERT INTO t1 (a, b) VALUES (1, 2)",
            "INSERT INTO t1 (a, b) SELECT x, y FROM s",
            "UPDATE t1 SET a = b + 1 WHERE id = 5",
            "UPDATE t1 SET a = (SELECT max(x) FROM s) WHERE id = 5",
            "DELETE FROM t1 WHERE id = 5",
            // DDL with body
            "CREATE TABLE dst AS SELECT a, b FROM src",
            "CREATE VIEW v AS SELECT a AS x FROM t1",
            // MERGE
            "MERGE INTO t1 USING t2 ON t1.id = t2.id \
             WHEN MATCHED THEN UPDATE SET a = t2.a \
             WHEN NOT MATCHED THEN INSERT (id, a) VALUES (t2.id, t2.a)",
        ]
    }

    /// Collected pair of outputs for the same statement — both
    /// extractors run in lockstep so per-statement invariants can be
    /// checked side by side.
    struct StatementPair {
        col: ColumnOperation,
        tab: TableOperation,
    }

    fn extract_paired(sql: &str) -> Vec<StatementPair> {
        let col = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
        let tab = extract_table_operations(&GenericDialect {}, sql, None).unwrap();
        assert_eq!(
            col.len(),
            tab.len(),
            "statement count mismatch between column_op and table_op for SQL: {sql}"
        );
        col.into_iter()
            .zip(tab)
            .map(|(c, t)| StatementPair {
                col: c.expect("column_op extraction succeeded"),
                tab: t.expect("table_op extraction succeeded"),
            })
            .collect()
    }

    fn table_set<I, T>(
        items: I,
        mut key: impl FnMut(&T) -> Option<TableReference>,
    ) -> HashSet<TableReference>
    where
        I: IntoIterator<Item = T>,
    {
        items.into_iter().filter_map(|i| key(&i)).collect()
    }

    fn column_read_table(r: &ColumnReference) -> Option<TableReference> {
        r.table.clone()
    }

    fn column_write_table(w: &ColumnReference) -> Option<TableReference> {
        w.table.clone()
    }

    fn edge_relation_table(f: &ColumnLineageEdge) -> Option<TableReference> {
        match &f.target {
            ColumnTarget::Relation(c) => c.table.clone(),
            ColumnTarget::QueryOutput { .. } => None,
        }
    }

    #[test]
    fn statement_kind_agrees_between_extractors() {
        for sql in corpus() {
            for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
                assert_eq!(
                    pair.col.statement_kind, pair.tab.statement_kind,
                    "column_op vs table_op kind disagrees \
                     for statement {idx} of SQL: {sql}"
                );
            }
        }
    }

    #[test]
    fn column_op_read_tables_appear_in_table_op_reads_or_writes() {
        // Column-level reads include refs from the RHS of UPDATE SET,
        // the predicate of DELETE WHERE, etc. — even when those refs
        // point at the statement's *target* table. table_op's UPDATE
        // / DELETE conventions surface the target in `writes` only
        // (unless the statement also has a separate read source like
        // `DELETE ... USING t2` or `UPDATE ... FROM t2`). The
        // invariant relaxes accordingly: column_op read tables must
        // be in the union of table_op reads + writes.
        for sql in corpus() {
            for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
                let table_op_reads: HashSet<_> =
                    table_set(pair.tab.reads.clone(), |r| Some(r.clone()));
                let table_op_writes: HashSet<_> =
                    table_set(pair.tab.writes.clone(), |w| Some(w.clone()));
                let known: HashSet<_> = table_op_reads.union(&table_op_writes).cloned().collect();
                let column_op_read_tables = table_set(pair.col.reads.clone(), column_read_table);
                for t in &column_op_read_tables {
                    assert!(
                        known.contains(t),
                        "column_op read table {t:?} missing from table_op reads ∪ writes \
                         for statement {idx} of SQL: {sql}\n\
                         table_op reads: {table_op_reads:?}\n\
                         table_op writes: {table_op_writes:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn column_op_write_tables_appear_in_table_op_writes() {
        for sql in corpus() {
            for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
                let table_op_writes = table_set(pair.tab.writes.clone(), |w| Some(w.clone()));
                let column_op_write_tables = table_set(pair.col.writes.clone(), column_write_table);
                for t in &column_op_write_tables {
                    assert!(
                        table_op_writes.contains(t),
                        "column_op write table {t:?} missing from table_op writes \
                         for statement {idx} of SQL: {sql}\n\
                         table_op writes: {table_op_writes:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn relation_lineage_targets_resolve_to_known_write_tables() {
        for sql in corpus() {
            for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
                let table_op_writes = table_set(pair.tab.writes.clone(), |w| Some(w.clone()));
                for f in &pair.col.lineage {
                    if let Some(target_table) = edge_relation_table(f) {
                        assert!(
                            table_op_writes.contains(&target_table),
                            "Relation lineage target {target_table:?} not in table_op writes \
                             for statement {idx} of SQL: {sql}\n\
                             table_op writes: {table_op_writes:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn select_statements_emit_no_writes() {
        for sql in corpus() {
            for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
                if pair.col.statement_kind == StatementKind::Select {
                    assert!(
                        pair.col.writes.is_empty(),
                        "SELECT statement has non-empty column_op writes \
                         for statement {idx} of SQL: {sql}\n\
                         writes: {:?}",
                        pair.col.writes
                    );
                    assert!(
                        pair.tab.writes.is_empty(),
                        "SELECT statement has non-empty table_op writes \
                         for statement {idx} of SQL: {sql}\n\
                         writes: {:?}",
                        pair.tab.writes
                    );
                }
            }
        }
    }

    #[test]
    fn writing_statements_emit_writes() {
        for sql in corpus() {
            for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
                let writes_expected = matches!(
                    pair.col.statement_kind,
                    StatementKind::Insert
                        | StatementKind::Update
                        | StatementKind::CreateTable
                        | StatementKind::CreateView
                        | StatementKind::Merge
                );
                if writes_expected {
                    assert!(
                        !pair.tab.writes.is_empty(),
                        "writing statement has empty table_op writes \
                         for statement {idx} of SQL: {sql}"
                    );
                }
            }
        }
    }
}
