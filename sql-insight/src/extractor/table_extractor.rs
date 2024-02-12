//! A Extractor that extracts tables from SQL queries.
//!
//! See [`extract_tables`](crate::extract_tables()) as the entry point for extracting tables from SQL.

use core::fmt;
use std::ops::ControlFlow;

use crate::error::Error;
use crate::helper;
use sqlparser::ast::{Ident, ObjectName, Statement, TableFactor, TableWithJoins, Visit, Visitor};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to extract tables from SQL.
///
/// ## Example
///
/// ```rust
/// use sqlparser::dialect::GenericDialect;
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

impl TryFrom<&TableFactor> for TableReference {
    type Error = Error;

    fn try_from(table: &TableFactor) -> Result<Self, Self::Error> {
        match table {
            TableFactor::Table { name, alias, .. } => match name.0.len() {
                0 => unreachable!("Parser should not allow empty identifiers"),
                1 => Ok(TableReference {
                    catalog: None,
                    schema: None,
                    name: name.0[0].clone(),
                    alias: alias.as_ref().map(|a| a.name.clone()),
                }),
                2 => Ok(TableReference {
                    catalog: None,
                    schema: Some(name.0[0].clone()),
                    name: name.0[1].clone(),
                    alias: alias.as_ref().map(|a| a.name.clone()),
                }),
                3 => Ok(TableReference {
                    catalog: Some(name.0[0].clone()),
                    schema: Some(name.0[1].clone()),
                    name: name.0[2].clone(),
                    alias: alias.as_ref().map(|a| a.name.clone()),
                }),
                _ => Err(Error::AnalysisError(
                    "Too many identifiers provided".to_string(),
                )),
            },
            _ => unreachable!("TableFactor::Table expected"),
        }
    }
}

impl TryFrom<&ObjectName> for TableReference {
    type Error = Error;

    fn try_from(obj_name: &ObjectName) -> Result<Self, Self::Error> {
        match obj_name.0.len() {
            0 => unreachable!("Parser should not allow empty identifiers"),
            1 => Ok(TableReference {
                catalog: None,
                schema: None,
                name: obj_name.0[0].clone(),
                alias: None,
            }),
            2 => Ok(TableReference {
                catalog: None,
                schema: Some(obj_name.0[0].clone()),
                name: obj_name.0[1].clone(),
                alias: None,
            }),
            3 => Ok(TableReference {
                catalog: Some(obj_name.0[0].clone()),
                schema: Some(obj_name.0[1].clone()),
                name: obj_name.0[2].clone(),
                alias: None,
            }),
            _ => Err(Error::AnalysisError(
                "Too many identifiers provided".to_string(),
            )),
        }
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
pub struct TableExtractor {
    // All tables found in the SQL including aliases, must be resolved to original tables.
    all_tables: Vec<TableReference>,
    // Original tables found in the SQL, used to resolve aliases.
    original_tables: Vec<TableReference>,
    // Flag to indicate if the current relation is part of a `TableFactor::Table`
    relation_of_table: bool,
}

impl Visitor for TableExtractor {
    type Break = Error;

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<Self::Break> {
        // Skip if relation is part of a TableFactor::Table
        if self.relation_of_table {
            self.relation_of_table = false;
            return ControlFlow::Continue(());
        }
        match TableReference::try_from(relation) {
            Ok(table) => {
                self.all_tables.push(table.clone());
                self.original_tables.push(table)
            }
            Err(e) => return ControlFlow::Break(e),
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { .. } = table_factor {
            self.relation_of_table = true;
            match TableReference::try_from(table_factor) {
                Ok(table) => {
                    self.all_tables.push(table.clone());
                    self.original_tables.push(table)
                }
                Err(e) => return ControlFlow::Break(e),
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        if let Statement::Delete { tables, .. } = statement {
            // tables of delete statement are not visited by `pre_visit_table_factor` nor `pre_visit_relation`.
            for table in tables {
                match TableReference::try_from(table) {
                    Ok(table) => self.all_tables.push(table),
                    Err(e) => return ControlFlow::Break(e),
                }
            }
        }
        ControlFlow::Continue(())
    }
}

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
        let mut visitor = TableExtractor::default();
        match statement.visit(&mut visitor) {
            ControlFlow::Break(e) => Err(e),
            ControlFlow::Continue(()) => Ok(Tables(helper::resolve_aliased_tables(
                visitor.all_tables,
                visitor.original_tables,
            ))),
        }
    }

    // `Visit` trait object cannot be used since method `visit` has generic type parameters.
    // Concrete type `TableWithJoins` is used instead.
    pub fn extract_from_table_node(table: &TableWithJoins) -> Result<Tables, Error> {
        let mut visitor = TableExtractor::default();
        match table.visit(&mut visitor) {
            ControlFlow::Break(e) => Err(e),
            ControlFlow::Continue(()) => Ok(Tables(helper::resolve_aliased_tables(
                visitor.all_tables,
                visitor.original_tables,
            ))),
        }
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
            let result = TableExtractor::extract(dialect.as_ref(), sql).unwrap();
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
    fn test_statement_error_with_too_many_identifiers() {
        let sql = "SELECT a FROM catalog.schema.table.extra";
        let expected = vec![Err(Error::AnalysisError(
            "Too many identifiers provided".to_string(),
        ))];
        assert_table_extraction(sql, expected, all_dialects());
    }

    mod delete_statement {
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
            assert_table_extraction(sql, expected, all_dialects());
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
            assert_table_extraction(sql, expected, all_dialects());
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
            assert_table_extraction(sql, expected, all_dialects());
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
