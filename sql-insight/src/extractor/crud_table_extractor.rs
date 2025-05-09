//! A Extractor that extracts CRUD tables from SQL queries.
//!
//! See [`extract_crud_tables`](crate::extract_crud_tables()) as the entry point for extracting CRUD tables from SQL.

use std::fmt;
use std::ops::ControlFlow;

use crate::error::Error;
use crate::extractor::table_extractor::TableReference;
use crate::{helper, TableExtractor};
use sqlparser::ast::{Delete, MergeAction, Statement, Visit, Visitor};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to extract CRUD tables from SQL.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
///
/// let dialect = GenericDialect {};
/// let sql = "INSERT INTO t1 (a) SELECT a FROM t2";
/// let result = sql_insight::extract_crud_tables(&dialect, sql).unwrap();
/// println!("{:#?}", result);
/// assert_eq!(result[0].as_ref().unwrap().to_string(), "Create: [t1], Read: [t2], Update: [], Delete: []");
/// ```
pub fn extract_crud_tables(
    dialect: &dyn Dialect,
    sql: &str,
) -> Result<Vec<Result<CrudTables, Error>>, Error> {
    CrudTableExtractor::extract(dialect, sql)
}

/// [`CrudTables`] represents the tables involved in CRUD operations.
#[derive(Default, Debug, PartialEq)]
pub struct CrudTables {
    pub create_tables: Vec<TableReference>,
    pub read_tables: Vec<TableReference>,
    pub update_tables: Vec<TableReference>,
    pub delete_tables: Vec<TableReference>,
}

impl fmt::Display for CrudTables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let create_tables = self.format_tables(&self.create_tables);
        let read_tables = self.format_tables(&self.read_tables);
        let update_tables = self.format_tables(&self.update_tables);
        let delete_tables = self.format_tables(&self.delete_tables);
        write!(
            f,
            "Create: [{}], Read: [{}], Update: [{}], Delete: [{}]",
            create_tables, read_tables, update_tables, delete_tables
        )
    }
}

impl CrudTables {
    fn format_tables(&self, tables: &[TableReference]) -> String {
        tables
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<String>>()
            .join(", ")
    }
}

/// A visitor to extract CRUD tables from SQL.
#[derive(Default, Debug)]
pub struct CrudTableExtractor {
    create_tables: Vec<TableReference>,
    read_tables: Vec<TableReference>,
    update_tables: Vec<TableReference>,
    delete_tables: Vec<TableReference>,
    possibly_aliased_delete_tables: Vec<TableReference>,
}

impl Visitor for CrudTableExtractor {
    type Break = Error;

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        match statement {
            Statement::Insert(insert) => {
                match TableReference::try_from(insert) {
                    Ok(table) => self.create_tables.push(table),
                    Err(e) => return ControlFlow::Break(e),
                }
                self.read_tables = helper::calc_difference_of_tables(
                    self.read_tables.clone(),
                    self.create_tables.clone(),
                );
            }
            Statement::Update { table, .. } => {
                match TableExtractor::extract_from_table_node(table) {
                    Ok(tables) => tables
                        .0
                        .into_iter()
                        .for_each(|table| self.update_tables.push(table)),
                    Err(e) => return ControlFlow::Break(e),
                }
                self.read_tables = helper::calc_difference_of_tables(
                    self.read_tables.clone(),
                    self.update_tables.clone(),
                );
            }
            Statement::Delete(Delete { tables, from, .. }) => {
                // When tables are present, deletion sqls are these tables,
                // and from clause is used as a data source.
                if !tables.is_empty() {
                    for table in tables {
                        match TableReference::try_from(table) {
                            Ok(table) => self.possibly_aliased_delete_tables.push(table),
                            Err(e) => return ControlFlow::Break(e),
                        }
                    }
                } else {
                    let from = match from {
                        sqlparser::ast::FromTable::WithFromKeyword(items) => items,
                        sqlparser::ast::FromTable::WithoutKeyword(items) => items,
                    };
                    for table_with_join in from {
                        match TableExtractor::extract_from_table_node(table_with_join) {
                            Ok(tables) => tables
                                .0
                                .into_iter()
                                .for_each(|table| self.possibly_aliased_delete_tables.push(table)),
                            Err(e) => return ControlFlow::Break(e),
                        }
                    }
                }
                self.delete_tables = helper::resolve_aliased_tables(
                    self.possibly_aliased_delete_tables.clone(),
                    self.read_tables.clone(),
                );
                self.read_tables = helper::calc_difference_of_tables(
                    self.read_tables.clone(),
                    self.delete_tables.clone(),
                );
            }
            Statement::Merge { table, clauses, .. } => {
                let target_table = match TableReference::try_from(table) {
                    Ok(table) => table,
                    Err(e) => return ControlFlow::Break(e),
                };
                let (mut inserted, mut updated, mut deleted) = (false, false, false);
                clauses.iter().for_each(|clause| match clause.action {
                    MergeAction::Update { .. } => updated = true,
                    MergeAction::Delete => deleted = true,
                    MergeAction::Insert(_) => inserted = true,
                });
                if inserted {
                    self.create_tables.push(target_table.clone());
                }
                if updated {
                    self.update_tables.push(target_table.clone());
                }
                if deleted {
                    self.delete_tables.push(target_table.clone());
                }
                self.read_tables =
                    helper::calc_difference_of_tables(self.read_tables.clone(), vec![target_table]);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

impl CrudTableExtractor {
    /// Extract CRUD tables from SQL.
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
    ) -> Result<Vec<Result<CrudTables, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        let results = statements
            .iter()
            .map(Self::extract_from_statement)
            .collect::<Vec<Result<CrudTables, Error>>>();
        Ok(results)
    }

    fn extract_from_statement(statement: &Statement) -> Result<CrudTables, Error> {
        let mut visitor = CrudTableExtractor {
            read_tables: TableExtractor::extract_from_statement(statement)?.0,
            ..Default::default()
        };
        match statement.visit(&mut visitor) {
            ControlFlow::Break(e) => Err(e),
            ControlFlow::Continue(()) => Ok(CrudTables {
                create_tables: visitor.create_tables,
                read_tables: visitor.read_tables,
                update_tables: visitor.update_tables,
                delete_tables: visitor.delete_tables,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::all_dialects;
    use sqlparser::dialect::MySqlDialect;

    fn assert_crud_table_extraction(
        sql: &str,
        expected: Vec<Result<CrudTables, Error>>,
        dialects: Vec<Box<dyn Dialect>>,
    ) {
        for dialect in dialects {
            let result = CrudTableExtractor::extract(dialect.as_ref(), sql)
                .unwrap_or_else(|_| panic!("parse failed for dialect: {dialect:?}"));
            assert_eq!(result, expected, "Failed for dialect: {dialect:?}")
        }
    }

    #[test]
    fn test_single_statement() {
        let sql = "SELECT a FROM t1";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            }],
            update_tables: vec![],
            delete_tables: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_multiple_statements() {
        let sql = "SELECT a FROM t1; SELECT b FROM t2";
        let expected = vec![
            Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
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
                    alias: None,
                }],
                update_tables: vec![],
                delete_tables: vec![],
            }),
        ];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_alias() {
        let sql = "SELECT a FROM t1 AS t1_alias";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: Some("t1_alias".into()),
            }],
            update_tables: vec![],
            delete_tables: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_table_identifier() {
        let sql = "SELECT a FROM catalog.schema.table";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![TableReference {
                catalog: Some("catalog".into()),
                schema: Some("schema".into()),
                name: "table".into(),
                alias: None,
            }],
            update_tables: vec![],
            delete_tables: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_table_identifier_and_alias() {
        let sql = "SELECT a FROM catalog.schema.table AS table_alias";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![TableReference {
                catalog: Some("catalog".into()),
                schema: Some("schema".into()),
                name: "table".into(),
                alias: Some("table_alias".into()),
            }],
            update_tables: vec![],
            delete_tables: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_error_with_too_many_identifiers() {
        let sql = "INSERT INTO catalog.schema.table.extra (a) VALUES (1)";
        let expected = vec![Err(Error::AnalysisError(
            "Too many identifiers provided".to_string(),
        ))];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    mod delete_statement {
        use crate::test_utils::all_dialects_except;

        use super::*;

        #[test]
        fn test_delete_statement() {
            let sql = "DELETE FROM t1";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![],
                update_tables: vec![],
                delete_tables: vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                }],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_delete_statement_with_table_identifier() {
            let sql = "DELETE FROM catalog.schema.t1";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![],
                update_tables: vec![],
                delete_tables: vec![TableReference {
                    catalog: Some("catalog".into()),
                    schema: Some("schema".into()),
                    name: "t1".into(),
                    alias: None,
                }],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_delete_statement_with_alias() {
            let sql = "DELETE FROM t1 AS t1_alias";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![],
                update_tables: vec![],
                delete_tables: vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: Some("t1_alias".into()),
                }],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_delete_multiple_tables_syntax() {
            let sql = "DELETE t1, t2 FROM t1 INNER JOIN t2 INNER JOIN t3";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![
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
                ],
                update_tables: vec![],
                delete_tables: vec![
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
                ],
            })];
            // BigQuery and Generic do not support DELETE ... FROM
            assert_crud_table_extraction(
                sql,
                expected,
                all_dialects_except(&vec!["GenericDialect", "BigQueryDialect"]),
            );
        }

        #[test]
        fn test_delete_multiple_tables_syntax_with_alias() {
            let sql =
                "DELETE t1_alias, t2_alias FROM t1 AS t1_alias INNER JOIN t2 AS t2_alias INNER JOIN t3";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![
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
                ],
                update_tables: vec![],
                delete_tables: vec![
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
                ],
            })];
            // BigQuery and Generic do not support DELETE ... FROM
            assert_crud_table_extraction(
                sql,
                expected,
                all_dialects_except(&vec!["GenericDialect", "BigQueryDialect"]),
            );
        }

        #[test]
        fn test_delete_multiple_tables_syntax_with_using() {
            let sql = "DELETE FROM t1, t2 USING t1 INNER JOIN t2 INNER JOIN t3";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![
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
                ],
                update_tables: vec![],
                delete_tables: vec![
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
                ],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_delete_multiple_tables_syntax_with_using_with_alias() {
            let sql = "DELETE FROM t1_alias, t2_alias USING t1 AS t1_alias INNER JOIN t2 AS t2_alias INNER JOIN t3";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![
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
                ],
                update_tables: vec![],
                delete_tables: vec![
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
                ],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }
    }

    mod insert_statement {
        use super::*;

        #[test]
        fn test_insert_statement() {
            let sql = "INSERT INTO t1 (a) VALUES (1)";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                }],
                read_tables: vec![],
                update_tables: vec![],
                delete_tables: vec![],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_insert_select_statement() {
            let sql = "INSERT INTO t1 (a) SELECT a FROM t2 AS t2_alias INNER JOIN t3 USING (id)";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                }],
                read_tables: vec![
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
                ],
                update_tables: vec![],
                delete_tables: vec![],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }
    }

    mod update_statemnet {
        use super::*;

        #[test]
        fn test_update_statement() {
            let sql = "UPDATE t1 SET a=1";
            let result = CrudTableExtractor::extract(&MySqlDialect {}, sql).unwrap();
            assert_eq!(
                result,
                vec![Ok(CrudTables {
                    create_tables: vec![],
                    read_tables: vec![],
                    update_tables: vec![TableReference {
                        catalog: None,
                        schema: None,
                        name: "t1".into(),
                        alias: None,
                    }],
                    delete_tables: vec![],
                }),]
            )
        }

        #[test]
        fn test_update_statement_with_alias() {
            let sql = "UPDATE t1 AS t1_alias INNER JOIN t2 ON t1_alias.a = t2.a SET t1_alias.b = t2.b WHERE t2.c = (SELECT c FROM t3)";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t3".into(),
                    alias: None,
                }],
                update_tables: vec![
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
                ],
                delete_tables: vec![],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }
    }

    #[test]
    fn test_merge_statement() {
        let sql = "MERGE INTO t1 AS t1_alias USING t2 AS t2_alias ON t1_alias.a = t2_alias.a \
                         WHEN MATCHED AND t2_alias.b = 1 THEN DELETE \
                         WHEN MATCHED AND t2_alias.b = 2 THEN UPDATE SET t1_alias.b = t2_alias.b \
                         WHEN NOT MATCHED THEN INSERT (a, b) VALUES (t2_alias.a, t2_alias.b)";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: Some("t1_alias".into()),
            }],
            read_tables: vec![TableReference {
                catalog: None,
                schema: None,
                name: "t2".into(),
                alias: Some("t2_alias".into()),
            }],
            update_tables: vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: Some("t1_alias".into()),
            }],
            delete_tables: vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: Some("t1_alias".into()),
            }],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_create_table_statement() {
        let sql = "CREATE TABLE t1 (a INT)";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            }],
            update_tables: vec![],
            delete_tables: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_alters_table_statement() {
        let sql = "ALTER TABLE t1 ADD COLUMN a INT";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            }],
            update_tables: vec![],
            delete_tables: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }
}
