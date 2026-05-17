//! Integration tests covering the public API surface end-to-end.
//!
//! `tests/integration.rs` is compiled as its own crate, so the
//! top-level items are equivalent to a `mod tests` in the library —
//! no extra wrapper module needed.

use sql_insight::sqlparser::ast::Ident;
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::test_utils::all_dialects;
use sql_insight::{
    extract_column_operations, extract_crud_tables, extract_table_operations, extract_tables,
    Catalog, ColumnFlowKind, ColumnSchema, ColumnTarget, CrudTables, Diagnostic, DiagnosticKind,
    NormalizerOptions, StatementKind, TableExtraction, TableReference, Tables,
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
            DiagnosticKind::UnsupportedStatement
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
        assert_eq!(ops.reads[0].table, table("t1"));
        assert!(ops.writes.is_empty());
        assert!(ops.flows.is_empty());
    }

    #[test]
    fn insert_select_emits_source_to_target_flow() {
        let sql = "INSERT INTO orders (id, total) SELECT id, amount FROM staging";
        let result = extract_table_operations(&GenericDialect {}, sql, None).unwrap();
        let ops = result[0].as_ref().unwrap();
        assert_eq!(ops.statement_kind, StatementKind::Insert);
        assert_eq!(
            ops.reads.iter().map(|r| &r.table).collect::<Vec<_>>(),
            vec![&table("staging")]
        );
        assert_eq!(
            ops.writes.iter().map(|w| &w.table).collect::<Vec<_>>(),
            vec![&table("orders")]
        );
        assert_eq!(ops.flows.len(), 1);
        assert_eq!(ops.flows[0].source, table("staging"));
        assert_eq!(ops.flows[0].target, table("orders"));
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
            .any(|d| matches!(d.kind, DiagnosticKind::UnsupportedStatement)));
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
    fn select_collects_per_column_reads_with_clause_role() {
        let sql = "SELECT a FROM t1 WHERE b > 0";
        let result = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
        let ops = result[0].as_ref().unwrap();
        // a → Projection, b → Filter
        let by_name: HashMap<_, _> = ops
            .reads
            .iter()
            .map(|r| (r.column.name.value.as_str(), r.kinds.clone()))
            .collect();
        assert_eq!(
            by_name.get("a"),
            Some(&vec![sql_insight::ReadKind::Projection])
        );
        assert_eq!(by_name.get("b"), Some(&vec![sql_insight::ReadKind::Filter]));
    }

    #[test]
    fn insert_select_emits_per_column_flows() {
        let sql = "INSERT INTO orders (id, total) SELECT id, amount FROM staging";
        let result = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
        let ops = result[0].as_ref().unwrap();
        assert_eq!(ops.flows.len(), 2);
        // Both flows are Passthrough into Persisted targets.
        for flow in &ops.flows {
            assert!(matches!(flow.kind, ColumnFlowKind::Passthrough));
            assert!(matches!(flow.target, ColumnTarget::Persisted(_)));
        }
    }

    #[test]
    fn aggregate_projection_marks_flow_aggregation() {
        let sql = "INSERT INTO summary (total) SELECT SUM(amount) FROM staging";
        let result = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
        let ops = result[0].as_ref().unwrap();
        assert_eq!(ops.flows.len(), 1);
        assert_eq!(ops.flows[0].source, col("staging", "amount"));
        assert!(matches!(ops.flows[0].kind, ColumnFlowKind::Aggregation));
    }

    #[test]
    fn wildcard_in_projection_yields_wildcard_suppressed_diagnostic() {
        let result =
            extract_column_operations(&GenericDialect {}, "SELECT * FROM t1", None).unwrap();
        let ops = result[0].as_ref().unwrap();
        assert!(ops
            .diagnostics
            .iter()
            .any(|d| matches!(d.kind, DiagnosticKind::WildcardSuppressed)));
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
                        name: Ident::new(*c),
                    })
                    .collect()
            })
        }
    }

    fn count_kind(diagnostics: &[Diagnostic], kind: DiagnosticKind) -> usize {
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
        // Two flows into Persisted orders.id / orders.total.
        let persisted_targets: Vec<_> = ops
            .flows
            .iter()
            .filter_map(|f| match &f.target {
                ColumnTarget::Persisted(c) => Some(c.name.value.as_str()),
                _ => None,
            })
            .collect();
        assert!(persisted_targets.contains(&"id"));
        assert!(persisted_targets.contains(&"total"));
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
            DiagnosticKind::AmbiguousColumn,
        );
        let without_count = count_kind(
            &without[0].as_ref().unwrap().diagnostics,
            DiagnosticKind::AmbiguousColumn,
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
            DiagnosticKind::UnresolvedColumn,
        );
        let without_count = count_kind(
            &without[0].as_ref().unwrap().diagnostics,
            DiagnosticKind::UnresolvedColumn,
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
            .any(|d| matches!(d.kind, DiagnosticKind::UnsupportedStatement)));
    }

    #[test]
    fn wildcard_diagnostic_carries_span_info() {
        let result =
            extract_column_operations(&GenericDialect {}, "SELECT * FROM t1", None).unwrap();
        let ops = result[0].as_ref().unwrap();
        let wildcard = ops
            .diagnostics
            .iter()
            .find(|d| matches!(d.kind, DiagnosticKind::WildcardSuppressed))
            .expect("WildcardSuppressed not found");
        // Message contains the source location.
        assert!(
            wildcard.message.contains("at L1:"),
            "got: {}",
            wildcard.message
        );
        // Structured span is also populated.
        let span = wildcard.span.expect("wildcard token carries a span");
        assert_eq!(span.start.line, 1);
    }
}
