//! A Extractor that extracts tables from SQL queries.
//!
//! See [`extract_tables`](crate::extract_tables()) as the entry point for extracting tables from SQL.

use core::fmt;

use crate::error::Error;
use crate::extractor::relation_binder::RelationBinder;
use sqlparser::ast::{
    Ident, Insert, ObjectName, Statement, TableFactor, TableObject, TableWithJoins,
};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to extract tables from SQL.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
///
/// let dialect = GenericDialect {};
/// let sql = "SELECT a FROM t1 INNER JOIN t2 ON t1.id = t2.id";
/// let result = sql_insight::extract_tables(&dialect, sql).unwrap();
/// println!("{:#?}", result);
/// assert_eq!(result[0].as_ref().unwrap().to_string(), "t1, t2");
/// ```
pub fn extract_tables(
    dialect: &dyn Dialect,
    sql: &str,
) -> Result<Vec<Result<Tables, Error>>, Error> {
    TableExtractor::extract(dialect, sql)
}

/// [`TableReference`] represents a qualified table with alias.
/// In this crate, this is the canonical representation of a table.
/// Tables found during analyzing an AST are stored as `TableReference`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TableReference {
    pub catalog: Option<Ident>,
    pub schema: Option<Ident>,
    pub name: Ident,
    pub alias: Option<Ident>,
}

impl TableReference {
    pub fn has_alias(&self) -> bool {
        self.alias.is_some()
    }
    pub fn has_qualifiers(&self) -> bool {
        self.catalog.is_some() || self.schema.is_some()
    }
    pub fn try_from_name_and_alias(
        name: &ObjectName,
        alias: &Option<Ident>,
    ) -> Result<Self, Error> {
        match name.0.len() {
            0 => unreachable!("Parser should not allow empty identifiers"),
            1 => Ok(TableReference {
                catalog: None,
                schema: None,
                name: name.0[0].as_ident().unwrap().clone(),
                alias: alias.clone(),
            }),
            2 => Ok(TableReference {
                catalog: None,
                schema: Some(name.0[0].as_ident().unwrap().clone()),
                name: name.0[1].as_ident().unwrap().clone(),
                alias: alias.clone(),
            }),
            3 => Ok(TableReference {
                catalog: Some(name.0[0].as_ident().unwrap().clone()),
                schema: Some(name.0[1].as_ident().unwrap().clone()),
                name: name.0[2].as_ident().unwrap().clone(),
                alias: alias.clone(),
            }),
            _ => Err(Error::AnalysisError(
                "Too many identifiers provided".to_string(),
            )),
        }
    }
    pub fn try_from_name(name: &ObjectName) -> Result<Self, Error> {
        Self::try_from_name_and_alias(name, &None)
    }
}

impl fmt::Display for TableReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let Some(catalog) = &self.catalog {
            parts.push(catalog.to_string());
        }
        if let Some(schema) = &self.schema {
            parts.push(schema.to_string());
        }
        parts.push(self.name.to_string());
        let table = parts.join(".");
        if let Some(alias) = &self.alias {
            write!(f, "{} AS {}", table, alias)
        } else {
            write!(f, "{}", table)
        }
    }
}

impl TryFrom<&Insert> for TableReference {
    type Error = Error;

    fn try_from(value: &Insert) -> Result<Self, Self::Error> {
        let name = match &value.table {
            TableObject::TableName(object_name) => object_name,
            TableObject::TableFunction(function) => &function.name,
        };
        Self::try_from_name_and_alias(name, &value.table_alias)
    }
}

impl TryFrom<&TableFactor> for TableReference {
    type Error = Error;

    fn try_from(table: &TableFactor) -> Result<Self, Self::Error> {
        match table {
            TableFactor::Table { name, alias, .. } => {
                Self::try_from_name_and_alias(name, &alias.as_ref().map(|a| a.name.clone()))
            }
            _ => unreachable!("TableFactor::Table expected"),
        }
    }
}

impl TryFrom<&ObjectName> for TableReference {
    type Error = Error;

    fn try_from(obj_name: &ObjectName) -> Result<Self, Self::Error> {
        Self::try_from_name(obj_name)
    }
}

/// [`Tables`] represents a list of [`TableReference`] that found in SQL.
#[derive(Debug, PartialEq)]
pub struct Tables(pub Vec<TableReference>);

impl fmt::Display for Tables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tables = self
            .0
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<String>>()
            .join(", ");
        write!(f, "{}", tables)
    }
}

/// A visitor to extract tables from SQL.
#[derive(Default, Debug)]
pub struct TableExtractor;

impl TableExtractor {
    /// Extract tables from SQL.
    pub fn extract(dialect: &dyn Dialect, sql: &str) -> Result<Vec<Result<Tables, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        let results = statements
            .iter()
            .map(Self::extract_from_statement)
            .collect::<Vec<Result<Tables, Error>>>();
        Ok(results)
    }

    pub fn extract_from_statement(statement: &Statement) -> Result<Tables, Error> {
        Ok(Tables(
            RelationBinder::bind_statement(statement)?.into_tables(),
        ))
    }

    // `Visit` trait object cannot be used since method `visit` has generic type parameters.
    // Concrete type `TableWithJoins` is used instead.
    pub fn extract_from_table_node(table: &TableWithJoins) -> Result<Tables, Error> {
        Ok(Tables(
            RelationBinder::bind_table_node(table)?.into_tables(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::all_dialects;

    fn assert_table_extraction(
        sql: &str,
        expected: Vec<Result<Tables, Error>>,
        dialects: Vec<Box<dyn Dialect>>,
    ) {
        for dialect in dialects {
            let result = TableExtractor::extract(dialect.as_ref(), sql)
                .unwrap_or_else(|_| panic!("parse failed for dialect: {dialect:?}"));
            assert_eq!(result, expected, "Failed for dialect: {dialect:?}")
        }
    }

    #[test]
    fn test_single_statement() {
        let sql = "SELECT a FROM t1";
        let expected = vec![Ok(Tables(vec![TableReference {
            catalog: None,
            schema: None,
            name: "t1".into(),
            alias: None,
        }]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_multiple_statements() {
        let sql = "SELECT a FROM t1; SELECT b FROM t2";
        let expected = vec![
            Ok(Tables(vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            }])),
            Ok(Tables(vec![TableReference {
                catalog: None,
                schema: None,
                name: "t2".into(),
                alias: None,
            }])),
        ];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_alias() {
        let sql = "SELECT a FROM t1 AS t1_alias";
        let expected = vec![Ok(Tables(vec![TableReference {
            catalog: None,
            schema: None,
            name: "t1".into(),
            alias: Some("t1_alias".into()),
        }]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_schema_identifier() {
        let sql = "SELECT a FROM schema.table; INSERT INTO schema.table (a) VALUES (1)";
        let expected = vec![
            Ok(Tables(vec![TableReference {
                catalog: None,
                schema: Some("schema".into()),
                name: "table".into(),
                alias: None,
            }])),
            Ok(Tables(vec![TableReference {
                catalog: None,
                schema: Some("schema".into()),
                name: "table".into(),
                alias: None,
            }])),
        ];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_full_identifier() {
        let sql =
            "SELECT a FROM catalog.schema.table; INSERT INTO catalog.schema.table (a) VALUES (1)";
        let expected = vec![
            Ok(Tables(vec![TableReference {
                catalog: Some("catalog".into()),
                schema: Some("schema".into()),
                name: "table".into(),
                alias: None,
            }])),
            Ok(Tables(vec![TableReference {
                catalog: Some("catalog".into()),
                schema: Some("schema".into()),
                name: "table".into(),
                alias: None,
            }])),
        ];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_table_identifier_and_alias() {
        let sql = "SELECT a FROM catalog.schema.table AS table_alias";
        let expected = vec![Ok(Tables(vec![TableReference {
            catalog: Some("catalog".into()),
            schema: Some("schema".into()),
            name: "table".into(),
            alias: Some("table_alias".into()),
        }]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_where_same_tables_appear_multiple_times() {
        let sql = "SELECT a FROM t1 INNER JOIN t2 ON t1.id = t2.id WHERE b = ( SELECT c FROM t3 INNER JOIN t1 ON t3.id = t1.id )";
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t2".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t3".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            },
        ]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_cte() {
        let sql = "WITH t2 AS (SELECT id FROM t1) SELECT * FROM t2";
        let expected = vec![Ok(Tables(vec![TableReference {
            catalog: None,
            schema: None,
            name: "t1".into(),
            alias: None,
        }]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_case_insensitive_cte_reference() {
        let sql = "WITH T2 AS (SELECT id FROM t1) SELECT * FROM t2";
        let expected = vec![Ok(Tables(vec![TableReference {
            catalog: None,
            schema: None,
            name: "t1".into(),
            alias: None,
        }]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_quoted_cte_does_not_match_unquoted_reference() {
        let sql = r#"WITH "T2" AS (SELECT id FROM t1) SELECT * FROM t2"#;
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t2".into(),
                alias: None,
            },
        ]))];
        assert_table_extraction(
            sql,
            expected,
            vec![Box::new(sqlparser::dialect::GenericDialect {})],
        );
    }

    #[test]
    fn test_statement_with_quoted_cte_exact_reference() {
        let sql = r#"WITH "T2" AS (SELECT id FROM t1) SELECT * FROM "T2""#;
        let expected = vec![Ok(Tables(vec![TableReference {
            catalog: None,
            schema: None,
            name: "t1".into(),
            alias: None,
        }]))];
        assert_table_extraction(
            sql,
            expected,
            vec![Box::new(sqlparser::dialect::GenericDialect {})],
        );
    }

    #[test]
    fn test_statement_with_cte_referencing_previous_cte() {
        let sql = "WITH t2 AS (SELECT id FROM t1), t3 AS (SELECT id FROM t2) SELECT * FROM t3";
        let expected = vec![Ok(Tables(vec![TableReference {
            catalog: None,
            schema: None,
            name: "t1".into(),
            alias: None,
        }]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_cte_does_not_resolve_forward_reference() {
        let sql = "WITH t2 AS (SELECT id FROM t3), t3 AS (SELECT id FROM t1) SELECT * FROM t2";
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t3".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            },
        ]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_cte_shadows_base_table_after_definition() {
        let sql = "WITH t2 AS (SELECT id FROM t3), t3 AS (SELECT id FROM t1) SELECT * FROM t3";
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t3".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            },
        ]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_qualified_table_not_shadowed_by_cte() {
        let sql = "WITH t2 AS (SELECT id FROM t4), t3 AS (SELECT id FROM t1) SELECT * FROM s.t3";
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t4".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: Some("s".into()),
                name: "t3".into(),
                alias: None,
            },
        ]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_qualified_table_not_shadowed_by_previous_cte_inside_cte_body() {
        let sql = "WITH t2 AS (SELECT id FROM t1), t3 AS (SELECT id FROM s.t2) SELECT * FROM t3";
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: Some("s".into()),
                name: "t2".into(),
                alias: None,
            },
        ]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_recursive_cte_self_reference() {
        let sql = "WITH RECURSIVE t2 AS (SELECT id FROM t2) SELECT * FROM t2";
        let expected = vec![Ok(Tables(vec![]))];
        assert_table_extraction(
            sql,
            expected,
            vec![Box::new(sqlparser::dialect::GenericDialect {})],
        );
    }

    #[test]
    fn test_statement_with_cte_shadowing_base_table() {
        let sql =
            "WITH t1 AS (SELECT id FROM t2) SELECT * FROM t1 JOIN s1.t1 AS t3 ON t1.id = t3.id";
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t2".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: Some("s1".into()),
                name: "t1".into(),
                alias: Some("t3".into()),
            },
        ]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_nested_statement_with_cte_scope() {
        let sql = "WITH t1 AS (SELECT id FROM t2) SELECT * FROM (WITH t1 AS (SELECT id FROM t3) SELECT * FROM t1) AS t4 JOIN t1 ON t4.id = t1.id";
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t2".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t3".into(),
                alias: None,
            },
        ]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_nested_cte_does_not_leak_to_outer_query() {
        let sql = "SELECT * FROM (WITH t2 AS (SELECT id FROM t1) SELECT * FROM t2) AS t3 JOIN t2 ON t3.id = t2.id";
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t2".into(),
                alias: None,
            },
        ]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_insert_select_with_cte_source() {
        let sql = "INSERT INTO t1 WITH t3 AS (SELECT id FROM t2) SELECT * FROM t3";
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t2".into(),
                alias: None,
            },
        ]))];
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

    mod delete_statement {
        use crate::test_utils::all_dialects_except;

        use super::*;

        #[test]
        fn test_delete_statement() {
            let sql = "DELETE t1 FROM t1";
            let expected = vec![Ok(Tables(vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                },
            ]))];
            // BigQuery and Generic do not support DELETE ... FROM
            assert_table_extraction(
                sql,
                expected,
                all_dialects_except(&vec!["GenericDialect", "BigQueryDialect"]),
            );
        }

        #[test]
        fn test_delete_statement_with_aliases() {
            let sql = "DELETE t1_alias FROM t1 AS t1_alias JOIN t2 AS t2_alias ON t1_alias.a = t2_alias.a WHERE t2_alias.b = 1";
            let expected = vec![Ok(Tables(vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: Some("t1_alias".into()),
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: Some("t1_alias".into()),
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t2".into(),
                    alias: Some("t2_alias".into()),
                },
            ]))];
            // BigQuery and Generic do not support DELETE ... FROM
            assert_table_extraction(
                sql,
                expected,
                all_dialects_except(&vec!["GenericDialect", "BigQueryDialect"]),
            );
        }

        #[test]
        fn test_delete_statement_with_case_insensitive_alias_target() {
            let sql = "DELETE T1_ALIAS FROM t1 AS t1_alias JOIN t2 ON t1_alias.a = t2.a";
            let expected = vec![Ok(Tables(vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: Some("t1_alias".into()),
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: Some("t1_alias".into()),
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t2".into(),
                    alias: None,
                },
            ]))];
            // BigQuery and Generic do not support DELETE ... FROM
            assert_table_extraction(
                sql,
                expected,
                all_dialects_except(&vec!["GenericDialect", "BigQueryDialect"]),
            );
        }

        #[test]
        fn test_delete_multiple_tables_with_join() {
            let sql =
                "DELETE t1, t2 FROM t1 INNER JOIN t2 INNER JOIN t3 WHERE t1.a = t2.a AND t2.a = t3.a";
            let expected = vec![Ok(Tables(vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t2".into(),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t2".into(),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t3".into(),
                    alias: None,
                },
            ]))];
            // BigQuery and Generic do not support DELETE ... FROM
            assert_table_extraction(
                sql,
                expected,
                all_dialects_except(&vec!["GenericDialect", "BigQueryDialect"]),
            );
        }

        #[test]
        fn test_delete_from_statement() {
            let sql = "DELETE FROM t1";
            let expected = vec![Ok(Tables(vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            }]))];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_delete_from_statement_with_alias() {
            let sql = "DELETE FROM t1_alias, t2_alias USING t1 AS t1_alias INNER JOIN t2 AS t2_alias INNER JOIN t3";
            let expected = vec![Ok(Tables(vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: Some("t1_alias".into()),
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t2".into(),
                    alias: Some("t2_alias".into()),
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: Some("t1_alias".into()),
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t2".into(),
                    alias: Some("t2_alias".into()),
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t3".into(),
                    alias: None,
                },
            ]))];
            assert_table_extraction(sql, expected, all_dialects());
        }
    }

    mod insert_statement {
        use super::*;

        #[test]
        fn test_insert_statement() {
            let sql = "INSERT INTO t1 (a, b) VALUES (1, 2)";
            let expected = vec![Ok(Tables(vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            }]))];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_insert_select_statement() {
            let sql = "INSERT INTO t1 SELECT * FROM t2";
            let expected = vec![Ok(Tables(vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t2".into(),
                    alias: None,
                },
            ]))];
            assert_table_extraction(sql, expected, all_dialects());
        }
    }

    mod update_statement {
        use super::*;

        #[test]
        fn test_update_statement() {
            let sql = "UPDATE t1 SET a = 1";
            let expected = vec![Ok(Tables(vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            }]))];
            assert_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_update_statement_with_alias() {
            let sql = "UPDATE t1 AS t1_alias INNER JOIN t2 ON t1_alias.a = t2.a SET t1_alias.b = t2.b WHERE t2.c = (SELECT c FROM t3)";
            let expected = vec![Ok(Tables(vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: Some("t1_alias".into()),
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t2".into(),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: "t3".into(),
                    alias: None,
                },
            ]))];
            assert_table_extraction(sql, expected, all_dialects());
        }
    }

    #[test]
    fn test_merge_statement() {
        let sql = "MERGE INTO t1 USING t2 ON t1.a = t2.a \
                         WHEN MATCHED THEN UPDATE SET t1.b = t2.b \
                         WHEN NOT MATCHED THEN INSERT (a, b) VALUES (t2.a, t2.b)";
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t2".into(),
                alias: None,
            },
        ]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_merge_statement_with_alias() {
        let sql = "MERGE INTO t1 AS t1_alias USING (SELECT a, b FROM t2) AS t2_alias(a, b) ON t1_alias.a = t2_alias.a \
                         WHEN MATCHED THEN UPDATE SET t1_alias.b = t2_alias.b \
                         WHEN NOT MATCHED THEN INSERT (a, b) VALUES (t2_alias.a, t2_alias.b)";
        let expected = vec![Ok(Tables(vec![
            TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: Some("t1_alias".into()),
            },
            TableReference {
                catalog: None,
                schema: None,
                name: "t2".into(),
                alias: None,
            },
        ]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_create_table_statement() {
        let sql = "CREATE TABLE t1 (a INT)";
        let expected = vec![Ok(Tables(vec![TableReference {
            catalog: None,
            schema: None,
            name: "t1".into(),
            alias: None,
        }]))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_alters_table_statement() {
        let sql = "ALTER TABLE t1 ADD COLUMN a INT";
        let expected = vec![Ok(Tables(vec![TableReference {
            catalog: None,
            schema: None,
            name: "t1".into(),
            alias: None,
        }]))];
        assert_table_extraction(sql, expected, all_dialects());
    }
}
