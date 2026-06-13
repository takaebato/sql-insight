//! Flat table-identity extraction. See [`extract_tables`] as the
//! entry point.
//!
//! Returns the list of tables a statement references, with no
//! read/write distinction or lineage information. For those, see
//! [`extract_table_operations`](crate::extractor::extract_table_operations)
//! / [`extract_column_operations`](crate::extractor::extract_column_operations).

use core::fmt;

use crate::diagnostic::TableLevelDiagnostic;
use crate::error::Error;
use crate::reference::TableReference;
use crate::resolver::{IdentifierCasing, Resolver};
use sqlparser::ast::Statement;
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Parse `sql` under `dialect` and return one [`TableExtraction`] per
/// statement.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
///
/// let dialect = GenericDialect {};
/// let sql = "SELECT a FROM t1 INNER JOIN t2 ON t1.id = t2.id";
/// let result = sql_insight::extractor::extract_tables(&dialect, sql).unwrap();
/// println!("{:#?}", result);
/// assert_eq!(result[0].as_ref().unwrap().to_string(), "t1, t2");
/// ```
pub fn extract_tables(
    dialect: &dyn Dialect,
    sql: &str,
) -> Result<Vec<Result<TableExtraction, Error>>, Error> {
    TableExtractor::extract(dialect, sql)
}

/// Per-statement output of [`extract_tables`]: the table list plus
/// any non-fatal diagnostics surfaced during the walk. `Display`
/// renders just the comma-joined table list.
#[derive(Debug, PartialEq)]
pub struct TableExtraction {
    pub tables: Vec<TableReference>,
    pub diagnostics: Vec<TableLevelDiagnostic>,
}

impl fmt::Display for TableExtraction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", TableReference::format_list(&self.tables))
    }
}

/// Struct-style entry point. Equivalent to the free
/// [`extract_tables`] function.
#[derive(Default, Debug)]
pub struct TableExtractor;

impl TableExtractor {
    /// Same as the free [`extract_tables`] function — kept for
    /// users who prefer the struct-style API.
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
    ) -> Result<Vec<Result<TableExtraction, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        let casing = IdentifierCasing::for_dialect(dialect);
        let results = statements
            .iter()
            .map(|s| Self::extract_from_statement(s, casing))
            .collect::<Vec<Result<TableExtraction, Error>>>();
        Ok(results)
    }

    fn extract_from_statement(
        statement: &Statement,
        casing: IdentifierCasing,
    ) -> Result<TableExtraction, Error> {
        // The legacy table-extraction API does not surface columns, so a
        // catalog would not influence its output; pass `None`.
        let resolution = Resolver::resolve_statement(None, statement, casing)?;
        Ok(TableExtraction {
            tables: resolution.tables(),
            // Project resolver diagnostics to table granularity; column
            // resolution / wildcard gaps don't affect the table list.
            diagnostics: resolution
                .diagnostics
                .iter()
                .filter_map(|d| d.to_table_level())
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::all_dialects;
    use sqlparser::dialect::GenericDialect;

    fn table(name: &str) -> TableReference {
        TableReference {
            catalog: None,
            schema: None,
            name: name.into(),
        }
    }

    fn schema_table(schema: &str, name: &str) -> TableReference {
        TableReference {
            catalog: None,
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    fn catalog_schema_table(catalog: &str, schema: &str, name: &str) -> TableReference {
        TableReference {
            catalog: Some(catalog.into()),
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    fn ok_tables(tables: Vec<TableReference>) -> Result<Vec<TableReference>, Error> {
        Ok(tables)
    }

    fn generic_dialect() -> Vec<Box<dyn Dialect>> {
        vec![Box::new(GenericDialect {})]
    }

    fn one_dialect(dialect: impl Dialect + 'static) -> Vec<Box<dyn Dialect>> {
        vec![Box::new(dialect)]
    }

    fn assert_table_extraction(
        sql: &str,
        expected: Vec<Result<Vec<TableReference>, Error>>,
        dialects: Vec<Box<dyn Dialect>>,
    ) {
        for dialect in dialects {
            let result = TableExtractor::extract(dialect.as_ref(), sql).unwrap_or_else(|e| {
                panic!("parse failed for dialect: {dialect:?}, sql: {sql}, error: {e}")
            });
            let result = result
                .into_iter()
                .map(|result| result.map(|extraction| extraction.tables))
                .collect::<Vec<Result<Vec<TableReference>, Error>>>();
            assert_eq!(result, expected, "Failed for dialect: {dialect:?}")
        }
    }

    mod basic {
        use super::*;

        #[test]
        fn test_single_statement() {
            let sql = "SELECT a FROM t1";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_multiple_statements() {
            let sql = "SELECT a FROM t1; SELECT b FROM t2";
            let expected = vec![ok_tables(vec![table("t1")]), ok_tables(vec![table("t2")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_table_extraction_display() {
            let extraction = TableExtraction {
                tables: vec![schema_table("s1", "t1"), table("t2")],
                diagnostics: Vec::new(),
            };

            assert_eq!(extraction.to_string(), "s1.t1, t2");
        }

        fn assert_unsupported_statement(sql: &str) {
            let result = TableExtractor::extract(&GenericDialect {}, sql).unwrap();
            let extraction = result.into_iter().next().unwrap().unwrap();
            assert_eq!(extraction.tables, vec![]);
            assert_eq!(extraction.diagnostics.len(), 1);
            assert_eq!(
                extraction.diagnostics[0].kind,
                crate::diagnostic::TableLevelDiagnosticKind::UnsupportedStatement
            );
            assert!(extraction.diagnostics[0]
                .message
                .contains("Unsupported statement while inspecting SQL"));
        }

        #[test]
        fn test_unsupported_statements_are_reported_as_diagnostics() {
            for sql in [
                "SET x = 1",
                "ANALYZE TABLE t1",
                "SHOW TABLES",
                "SHOW COLUMNS FROM t1",
                "SHOW DATABASES",
                "SHOW SCHEMAS",
                "USE mydb",
                "START TRANSACTION",
                "COMMIT",
                "ROLLBACK",
                "EXPLAIN SELECT * FROM t1",
                "CREATE INDEX idx ON t1 (a)",
                "CREATE SCHEMA s",
                "CREATE DATABASE db",
                "DEALLOCATE PREPARE stmt",
                "PREPARE stmt AS SELECT 1",
                "SAVEPOINT sp",
                "RELEASE SAVEPOINT sp",
                "RESET ALL",
            ] {
                assert_unsupported_statement(sql);
            }
        }
    }

    mod resolver_traversal {
        use super::*;

        #[test]
        fn test_subqueries_inside_predicate_expressions() {
            for (sql, expected_tables) in [
                (
                    "SELECT * FROM t1 WHERE EXISTS (SELECT 1 FROM t2)",
                    vec![table("t1"), table("t2")],
                ),
                (
                    "SELECT * FROM t1 WHERE a IN (SELECT a FROM t2)",
                    vec![table("t1"), table("t2")],
                ),
                (
                    "SELECT * FROM t1 WHERE a BETWEEN (SELECT b FROM t2) AND (SELECT c FROM t3)",
                    vec![table("t1"), table("t2"), table("t3")],
                ),
                (
                    "SELECT * FROM t1 WHERE a LIKE (SELECT pattern FROM t2)",
                    vec![table("t1"), table("t2")],
                ),
            ] {
                assert_table_extraction(sql, vec![ok_tables(expected_tables)], generic_dialect());
            }
        }

        #[test]
        fn test_subqueries_inside_projection_expressions() {
            for (sql, expected_tables) in [
                (
                    "SELECT CASE WHEN a > 0 THEN (SELECT b FROM t2) ELSE (SELECT c FROM t3) END FROM t1",
                    vec![table("t1"), table("t2"), table("t3")],
                ),
                (
                    "SELECT CAST((SELECT b FROM t2) AS INT) FROM t1",
                    vec![table("t1"), table("t2")],
                ),
                (
                    "SELECT ((SELECT b FROM t2)) FROM t1",
                    vec![table("t1"), table("t2")],
                ),
                (
                    "SELECT ARRAY[(SELECT b FROM t2)] FROM t1",
                    vec![table("t1"), table("t2")],
                ),
                (
                    "SELECT STRUCT((SELECT b FROM t2) AS b) FROM t1",
                    vec![table("t1"), table("t2")],
                ),
            ] {
                assert_table_extraction(sql, vec![ok_tables(expected_tables)], generic_dialect());
            }
        }

        #[test]
        fn test_subqueries_inside_query_clauses() {
            for (sql, expected_tables) in [
                (
                    "SELECT a FROM t1 GROUP BY (SELECT b FROM t2)",
                    vec![table("t1"), table("t2")],
                ),
                (
                    "SELECT a FROM t1 HAVING (SELECT b FROM t2) > 0",
                    vec![table("t1"), table("t2")],
                ),
                (
                    "SELECT a FROM t1 ORDER BY (SELECT b FROM t2)",
                    vec![table("t1"), table("t2")],
                ),
            ] {
                assert_table_extraction(sql, vec![ok_tables(expected_tables)], generic_dialect());
            }
        }

        #[test]
        fn test_subqueries_inside_function_clauses() {
            for (sql, expected_tables) in [
                (
                    "SELECT COUNT(*) FILTER (WHERE EXISTS (SELECT 1 FROM t2)) FROM t1",
                    vec![table("t1"), table("t2")],
                ),
                (
                    "SELECT ARRAY_AGG(a ORDER BY (SELECT b FROM t2)) FROM t1",
                    vec![table("t1"), table("t2")],
                ),
                (
                    "SELECT SUM(a) OVER (PARTITION BY (SELECT b FROM t2) ORDER BY (SELECT c FROM t3)) FROM t1",
                    vec![table("t1"), table("t2"), table("t3")],
                ),
            ] {
                assert_table_extraction(sql, vec![ok_tables(expected_tables)], generic_dialect());
            }
        }

        #[test]
        fn test_nested_join_and_join_constraints() {
            let sql = "SELECT * FROM (t1 JOIN t2 ON t1.id = t2.id) AS t12 JOIN t3 USING (id)";
            let expected = vec![ok_tables(vec![table("t1"), table("t2"), table("t3")])];
            assert_table_extraction(sql, expected, generic_dialect());
        }

        #[test]
        fn test_derived_table_and_lateral_sources() {
            // Outer scope's tables (t2 via JOIN) come before nested
            // scopes (LATERAL subquery's t1).
            let sql = "SELECT * FROM LATERAL (SELECT id FROM t1) AS d JOIN t2 ON d.id = t2.id";
            let expected = vec![ok_tables(vec![table("t2"), table("t1")])];
            assert_table_extraction(sql, expected, generic_dialect());
        }

        #[test]
        fn test_table_function_sources() {
            for (sql, expected_tables) in [
                (
                    "SELECT * FROM UNNEST(ARRAY[(SELECT id FROM t1)]) AS u",
                    vec![table("t1")],
                ),
                (
                    "SELECT * FROM generate_series((SELECT min_id FROM t1), 10) AS g",
                    vec![table("generate_series"), table("t1")],
                ),
            ] {
                assert_table_extraction(sql, vec![ok_tables(expected_tables)], generic_dialect());
            }
        }

        #[test]
        fn test_query_set_expr_forms() {
            for (sql, expected_tables) in [
                (
                    "SELECT * FROM t1 UNION SELECT * FROM t2",
                    vec![table("t1"), table("t2")],
                ),
                ("VALUES ((SELECT id FROM t1))", vec![table("t1")]),
                (
                    "CREATE TABLE t2 AS TABLE t1",
                    vec![table("t2"), table("t1")],
                ),
            ] {
                assert_table_extraction(sql, vec![ok_tables(expected_tables)], generic_dialect());
            }
        }

        #[test]
        fn test_query_clauses_with_subqueries() {
            for (sql, expected_tables) in [
                (
                    "SELECT * FROM t1 LIMIT (SELECT n FROM t2)",
                    vec![table("t1"), table("t2")],
                ),
                (
                    "SELECT * FROM t1 FETCH FIRST 10 ROWS ONLY",
                    vec![table("t1")],
                ),
                (
                    "SELECT SUM(a) OVER w FROM t1 WINDOW w AS (PARTITION BY (SELECT b FROM t2))",
                    vec![table("t1"), table("t2")],
                ),
            ] {
                assert_table_extraction(sql, vec![ok_tables(expected_tables)], generic_dialect());
            }
        }

        #[test]
        fn test_dialect_specific_query_clauses_with_subqueries() {
            // DISTINCT ON / TOP exprs are walked before FROM, but the outer
            // scope's tables (t1) still come before the nested
            // subquery's (t2) under scope-order traversal.
            assert_table_extraction(
                "SELECT DISTINCT ON ((SELECT id FROM t2)) id FROM t1",
                vec![ok_tables(vec![table("t1"), table("t2")])],
                one_dialect(sqlparser::dialect::PostgreSqlDialect {}),
            );
            assert_table_extraction(
                "SELECT TOP ((SELECT n FROM t2)) id FROM t1",
                vec![ok_tables(vec![table("t1"), table("t2")])],
                one_dialect(sqlparser::dialect::MsSqlDialect {}),
            );
            assert_table_extraction(
                "SELECT * INTO t2 FROM t1",
                vec![ok_tables(vec![table("t1"), table("t2")])],
                one_dialect(sqlparser::dialect::MsSqlDialect {}),
            );
            assert_table_extraction(
                "SELECT * FROM t1 SETTINGS max_threads = (SELECT n FROM t2)",
                vec![ok_tables(vec![table("t1"), table("t2")])],
                one_dialect(sqlparser::dialect::ClickHouseDialect {}),
            );
        }

        #[test]
        fn test_join_variants() {
            for sql in [
                "SELECT * FROM t1 LEFT JOIN t2 ON t1.id = t2.id",
                "SELECT * FROM t1 RIGHT JOIN t2 ON t1.id = t2.id",
                "SELECT * FROM t1 FULL OUTER JOIN t2 ON t1.id = t2.id",
                "SELECT * FROM t1 CROSS JOIN t2",
            ] {
                assert_table_extraction(
                    sql,
                    vec![ok_tables(vec![table("t1"), table("t2")])],
                    generic_dialect(),
                );
            }
        }

        #[test]
        fn test_table_factor_extensions() {
            assert_table_extraction(
                "SELECT * FROM t1 TABLESAMPLE (10)",
                vec![ok_tables(vec![table("t1")])],
                generic_dialect(),
            );
            assert_table_extraction(
                "SELECT * FROM monthly_sales PIVOT(SUM(amount) FOR month IN ('JAN')) AS p",
                vec![ok_tables(vec![table("monthly_sales")])],
                generic_dialect(),
            );
        }

        #[test]
        fn test_pipe_operator_sources() {
            // Outer scope's tables (t1 from FROM, t3 from |> JOIN) come
            // before the WHERE subquery's nested scope (t2).
            let sql =
                "SELECT * FROM t1 |> WHERE id IN (SELECT id FROM t2) |> JOIN t3 ON id = t3.id";
            let expected = vec![ok_tables(vec![table("t1"), table("t3"), table("t2")])];
            assert_table_extraction(
                sql,
                expected,
                one_dialect(sqlparser::dialect::BigQueryDialect {}),
            );
        }
    }

    mod query_shapes {
        use super::*;

        #[test]
        fn test_statement_with_alias() {
            let sql = "SELECT a FROM t1 AS t1_alias";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_schema_identifier() {
            let sql = "SELECT a FROM schema.table; INSERT INTO schema.table (a) VALUES (1)";
            let expected = vec![
                ok_tables(vec![schema_table("schema", "table")]),
                ok_tables(vec![schema_table("schema", "table")]),
            ];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_full_identifier() {
            let sql =
            "SELECT a FROM catalog.schema.table; INSERT INTO catalog.schema.table (a) VALUES (1)";
            let expected = vec![
                ok_tables(vec![catalog_schema_table("catalog", "schema", "table")]),
                ok_tables(vec![catalog_schema_table("catalog", "schema", "table")]),
            ];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_table_identifier_and_alias() {
            let sql = "SELECT a FROM catalog.schema.table AS table_alias";
            let expected = vec![ok_tables(vec![catalog_schema_table(
                "catalog", "schema", "table",
            )])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_where_same_tables_appear_multiple_times() {
            let sql = "SELECT a FROM t1 INNER JOIN t2 ON t1.id = t2.id WHERE b = ( SELECT c FROM t3 INNER JOIN t1 ON t3.id = t1.id )";
            let expected = vec![ok_tables(vec![
                table("t1"),
                table("t2"),
                table("t3"),
                table("t1"),
            ])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_subquery_inside_function_expression() {
            let sql = "SELECT COALESCE((SELECT b FROM t2), a) FROM t1";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_subquery_in_order_by() {
            let sql = "SELECT a FROM t1 ORDER BY (SELECT b FROM t2)";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(sql, expected, all_dialects());
        }
    }

    mod cte {
        use super::*;

        #[test]
        fn test_statement_with_cte() {
            let sql = "WITH t2 AS (SELECT id FROM t1) SELECT * FROM t2";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_case_insensitive_cte_reference() {
            let sql = "WITH T2 AS (SELECT id FROM t1) SELECT * FROM t2";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_quoted_cte_does_not_match_unquoted_reference() {
            let sql = r#"WITH "T2" AS (SELECT id FROM t1) SELECT * FROM t2"#;
            // Outer scope's t2 (CTE didn't match the unquoted reference)
            // precedes the nested CTE body's t1.
            let expected = vec![ok_tables(vec![table("t2"), table("t1")])];
            assert_table_extraction(
                sql,
                expected,
                vec![Box::new(sqlparser::dialect::GenericDialect {})],
            );
        }

        #[test]
        fn test_statement_with_quoted_cte_exact_reference() {
            let sql = r#"WITH "T2" AS (SELECT id FROM t1) SELECT * FROM "T2""#;
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(
                sql,
                expected,
                vec![Box::new(sqlparser::dialect::GenericDialect {})],
            );
        }

        #[test]
        fn test_statement_with_cte_referencing_previous_cte() {
            let sql = "WITH t2 AS (SELECT id FROM t1), t3 AS (SELECT id FROM t2) SELECT * FROM t3";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_cte_does_not_resolve_forward_reference() {
            let sql = "WITH t2 AS (SELECT id FROM t3), t3 AS (SELECT id FROM t1) SELECT * FROM t2";
            let expected = vec![ok_tables(vec![table("t3"), table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_cte_shadows_real_table_after_definition() {
            let sql = "WITH t2 AS (SELECT id FROM t3), t3 AS (SELECT id FROM t1) SELECT * FROM t3";
            let expected = vec![ok_tables(vec![table("t3"), table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_qualified_table_not_shadowed_by_cte() {
            let sql =
                "WITH t2 AS (SELECT id FROM t4), t3 AS (SELECT id FROM t1) SELECT * FROM s.t3";
            // Outer scope's s.t3 comes first; CTE bodies (t4, t1) follow in
            // creation order.
            let expected = vec![ok_tables(vec![
                schema_table("s", "t3"),
                table("t4"),
                table("t1"),
            ])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_qualified_table_not_shadowed_by_previous_cte_inside_cte_body() {
            let sql =
                "WITH t2 AS (SELECT id FROM t1), t3 AS (SELECT id FROM s.t2) SELECT * FROM t3";
            let expected = vec![ok_tables(vec![table("t1"), schema_table("s", "t2")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_with_recursive_cte_self_reference() {
            let sql = "WITH RECURSIVE t2 AS (SELECT id FROM t2) SELECT * FROM t2";
            let expected = vec![ok_tables(vec![])];
            assert_table_extraction(
                sql,
                expected,
                vec![Box::new(sqlparser::dialect::GenericDialect {})],
            );
        }

        #[test]
        fn test_statement_with_cte_shadowing_real_table() {
            let sql =
                "WITH t1 AS (SELECT id FROM t2) SELECT * FROM t1 JOIN s1.t1 AS t3 ON t1.id = t3.id";
            // Outer scope's s1.t1 AS t3 (from JOIN) is recorded before the CTE
            // body's t2 in the nested scope.
            let expected = vec![ok_tables(vec![schema_table("s1", "t1"), table("t2")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_nested_statement_with_cte_scope() {
            let sql = "WITH t1 AS (SELECT id FROM t2) SELECT * FROM (WITH t1 AS (SELECT id FROM t3) SELECT * FROM t1) AS t4 JOIN t1 ON t4.id = t1.id";
            let expected = vec![ok_tables(vec![table("t2"), table("t3")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_nested_cte_does_not_leak_to_outer_query() {
            let sql = "SELECT * FROM (WITH t2 AS (SELECT id FROM t1) SELECT * FROM t2) AS t3 JOIN t2 ON t3.id = t2.id";
            // Outer scope's t2 (from JOIN, real table) comes before the nested
            // CTE body's t1.
            let expected = vec![ok_tables(vec![table("t2"), table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_insert_select_with_cte_source() {
            let sql = "INSERT INTO t1 WITH t3 AS (SELECT id FROM t2) SELECT * FROM t3";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_error_with_too_many_identifiers() {
            let sql = "SELECT a FROM catalog.schema.table.extra";
            let expected = vec![Err(Error::AnalysisError(
                "Too many identifiers provided".to_string(),
            ))];
            assert_table_extraction(sql, expected, all_dialects());
        }
    }

    mod delete_statement {
        use crate::test_utils::{all_dialects_except, DialectName};

        use super::*;

        #[test]
        fn test_delete_statement() {
            // Targets used to be spliced into the output; now only scope-bound
            // sources appear, so the target reference no longer duplicates.
            let sql = "DELETE t1 FROM t1";
            let expected = vec![ok_tables(vec![table("t1")])];
            // BigQuery / Generic / Oracle do not support DELETE ... FROM
            assert_table_extraction(
                sql,
                expected,
                all_dialects_except(&[
                    DialectName::Generic,
                    DialectName::BigQuery,
                    DialectName::Oracle,
                ]),
            );
        }

        #[test]
        fn test_delete_statement_with_aliases() {
            let sql = "DELETE t1_alias FROM t1 AS t1_alias JOIN t2 AS t2_alias ON t1_alias.a = t2_alias.a WHERE t2_alias.b = 1";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            // BigQuery / Generic / Oracle do not support DELETE ... FROM
            assert_table_extraction(
                sql,
                expected,
                all_dialects_except(&[
                    DialectName::Generic,
                    DialectName::BigQuery,
                    DialectName::Oracle,
                ]),
            );
        }

        #[test]
        fn test_delete_statement_with_alias_target() {
            // The DELETE target references the FROM alias. With matching
            // case it merges into the alias's binding on every dialect
            // (only t1, t2 surface — the alias is not a separate table).
            let sql = "DELETE t1_alias FROM t1 AS t1_alias JOIN t2 ON t1_alias.a = t2.a";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            // BigQuery / Generic / Oracle do not support DELETE ... FROM
            assert_table_extraction(
                sql,
                expected,
                all_dialects_except(&[
                    DialectName::Generic,
                    DialectName::BigQuery,
                    DialectName::Oracle,
                ]),
            );
        }

        #[test]
        fn test_delete_target_alias_mismatched_case() {
            // Mismatched case (`T1_ALIAS` target vs `t1_alias` alias).
            // Dialects whose alias fold is case-insensitive
            // (lower / upper / insensitive) still merge it into the
            // alias binding. MySQL is excluded: its real-table names
            // are case-sensitive while aliases default case-insensitive,
            // but a multi-table DELETE target is bound by the *table*
            // (relation) fold, so a mismatched-case alias target doesn't
            // merge — a known limitation of the relation / table-alias
            // fold split at DELETE-target binding sites.
            let sql = "DELETE T1_ALIAS FROM t1 AS t1_alias JOIN t2 ON t1_alias.a = t2.a";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(
                sql,
                expected,
                all_dialects_except(&[
                    DialectName::Generic,
                    DialectName::BigQuery,
                    DialectName::Oracle,
                    DialectName::MySql,
                ]),
            );
        }

        #[test]
        fn test_delete_multiple_tables_with_join() {
            let sql =
                "DELETE t1, t2 FROM t1 INNER JOIN t2 INNER JOIN t3 WHERE t1.a = t2.a AND t2.a = t3.a";
            let expected = vec![ok_tables(vec![table("t1"), table("t2"), table("t3")])];
            // BigQuery / Generic / Oracle do not support DELETE ... FROM
            assert_table_extraction(
                sql,
                expected,
                all_dialects_except(&[
                    DialectName::Generic,
                    DialectName::BigQuery,
                    DialectName::Oracle,
                ]),
            );
        }

        #[test]
        fn test_delete_from_statement() {
            let sql = "DELETE FROM t1";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_delete_from_statement_with_selection() {
            let sql = "DELETE FROM t1 WHERE id IN (SELECT id FROM t2)";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_delete_from_statement_with_alias() {
            let sql = "DELETE FROM t1_alias, t2_alias USING t1 AS t1_alias INNER JOIN t2 AS t2_alias INNER JOIN t3";
            let expected = vec![ok_tables(vec![table("t1"), table("t2"), table("t3")])];
            assert_table_extraction(sql, expected, all_dialects());
        }
    }

    mod insert_statement {
        use super::*;

        #[test]
        fn test_insert_statement() {
            let sql = "INSERT INTO t1 (a, b) VALUES (1, 2)";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_insert_select_statement() {
            let sql = "INSERT INTO t1 SELECT * FROM t2";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_insert_set_statement() {
            let sql = "INSERT INTO t1 SET a = (SELECT b FROM t2)";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(
                sql,
                expected,
                one_dialect(sqlparser::dialect::MySqlDialect {}),
            );
        }

        #[test]
        fn test_insert_table_function_statement() {
            let sql = "INSERT INTO FUNCTION remote('localhost', default.t1) SELECT * FROM t2";
            let expected = vec![ok_tables(vec![table("remote"), table("t2")])];
            assert_table_extraction(
                sql,
                expected,
                one_dialect(sqlparser::dialect::ClickHouseDialect {}),
            );
        }
    }

    mod update_statement {
        use super::*;

        #[test]
        fn test_update_statement() {
            let sql = "UPDATE t1 SET a = 1";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_update_statement_with_alias() {
            let sql = "UPDATE t1 AS t1_alias INNER JOIN t2 ON t1_alias.a = t2.a SET t1_alias.b = t2.b WHERE t2.c = (SELECT c FROM t3)";
            let expected = vec![ok_tables(vec![table("t1"), table("t2"), table("t3")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_update_statement_with_from_and_subqueries() {
            let sql =
                "UPDATE t1 SET a = (SELECT b FROM t3) FROM t2 WHERE t1.id IN (SELECT id FROM t4)";
            let expected = vec![ok_tables(vec![
                table("t1"),
                table("t2"),
                table("t3"),
                table("t4"),
            ])];
            assert_table_extraction(
                sql,
                expected,
                one_dialect(sqlparser::dialect::PostgreSqlDialect {}),
            );
        }
    }

    mod merge {
        use super::*;

        #[test]
        fn test_merge_statement() {
            let sql = "MERGE INTO t1 USING t2 ON t1.a = t2.a \
                         WHEN MATCHED THEN UPDATE SET t1.b = t2.b \
                         WHEN NOT MATCHED THEN INSERT (a, b) VALUES (t2.a, t2.b)";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_merge_statement_with_alias() {
            let sql = "MERGE INTO t1 AS t1_alias USING (SELECT a, b FROM t2) AS t2_alias(a, b) ON t1_alias.a = t2_alias.a \
                         WHEN MATCHED THEN UPDATE SET t1_alias.b = t2_alias.b \
                         WHEN NOT MATCHED THEN INSERT (a, b) VALUES (t2_alias.a, t2_alias.b)";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_merge_statement_with_clause_predicate() {
            let sql = "MERGE INTO t1 USING t2 ON t1.id = t2.id \
                         WHEN MATCHED AND EXISTS (SELECT 1 FROM t3) THEN DELETE";
            let expected = vec![ok_tables(vec![table("t1"), table("t2"), table("t3")])];
            assert_table_extraction(sql, expected, generic_dialect());
        }
    }

    mod ddl {
        use super::*;

        #[test]
        fn test_create_table_statement() {
            let sql = "CREATE TABLE t1 (a INT)";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_create_table_as_select_statement() {
            let sql = "CREATE TABLE t1 AS SELECT * FROM t2";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(sql, expected, generic_dialect());
        }

        #[test]
        fn test_create_view_statement() {
            let sql = "CREATE VIEW t1 AS SELECT * FROM t2";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(sql, expected, generic_dialect());
        }

        #[test]
        fn test_create_virtual_table_statement() {
            let sql = "CREATE VIRTUAL TABLE t1 USING fts5(a)";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(
                sql,
                expected,
                one_dialect(sqlparser::dialect::SQLiteDialect {}),
            );
        }

        #[test]
        fn test_alters_table_statement() {
            let sql = "ALTER TABLE t1 ADD COLUMN a INT";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_drop_table_statement() {
            let sql = "DROP TABLE t1, t2";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(sql, expected, generic_dialect());
        }

        #[test]
        fn test_drop_index_statement_records_parent_table() {
            let sql = "DROP INDEX idx1 ON t1";
            let expected = vec![ok_tables(vec![table("t1")])];
            assert_table_extraction(sql, expected, generic_dialect());
        }

        #[test]
        fn test_truncate_table_statement() {
            let sql = "TRUNCATE TABLE t1, t2";
            let expected = vec![ok_tables(vec![table("t1"), table("t2")])];
            assert_table_extraction(sql, expected, generic_dialect());
        }
    }
}
