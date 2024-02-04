use std::collections::HashMap;
use std::fmt;
use std::ops::ControlFlow;

use crate::error::Error;
use crate::extractor::table_extractor::TableReference;
use crate::TableExtractor;
use sqlparser::ast::{Statement, Visit, Visitor};
use sqlparser::dialect::{dialect_from_str, Dialect};
use sqlparser::parser::Parser;

pub fn extract_crud_tables(
    dialect: &dyn Dialect,
    sql: String,
) -> Result<Vec<Result<CrudTables, Error>>, Error> {
    CrudTableExtractor::extract(dialect, sql)
}

pub fn extract_crud_tables_from_cli(
    dialect_name: Option<&str>,
    sql: String,
) -> Result<Vec<String>, Error> {
    let dialect_name = dialect_name.unwrap_or("generic");
    match dialect_from_str(dialect_name) {
        Some(dialect) => {
            let result = extract_crud_tables(dialect.as_ref(), sql)?;
            Ok(result
                .iter()
                .map(|r| match r {
                    Ok(crud_tables) => format!("{}", crud_tables),
                    Err(e) => format!("Error: {}", e),
                })
                .collect())
        }
        None => Err(Error::ArgumentError(format!(
            "Dialect not found: {}",
            dialect_name
        ))),
    }
}

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
            Statement::Insert { table_name, .. } => match TableReference::try_from(table_name) {
                Ok(table) => self.create_tables.push(table),
                Err(e) => return ControlFlow::Break(e),
            },
            Statement::Update { table, .. } => {
                match TableExtractor::extract_from_table_node(table) {
                    Ok(tables) => tables
                        .0
                        .into_iter()
                        .for_each(|table| self.update_tables.push(table)),
                    Err(e) => return ControlFlow::Break(e),
                }
                self.apply_difference_to_read_tables(self.update_tables.clone());
            }
            Statement::Delete { tables, from, .. } => {
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
                    for table_with_join in from {
                        match TableExtractor::extract_from_table_node(table_with_join) {
                            Ok(tables) => tables
                                .0
                                .into_iter()
                                .for_each(|table| self.possibly_aliased_delete_tables.push(table)),
                            Err(e) => return ControlFlow::Break(e),
                        }
                    }
                    self.apply_difference_to_read_tables(
                        self.possibly_aliased_delete_tables.clone(),
                    );
                }
                self.delete_tables = self.resolve_delete_tables();
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

impl CrudTableExtractor {
    pub fn extract(
        dialect: &dyn Dialect,
        sql: String,
    ) -> Result<Vec<Result<CrudTables, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, &sql)?;
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

    // Deletion sqls can be specified as aliases, so convert them to real table names if possible.
    fn resolve_delete_tables(&mut self) -> Vec<TableReference> {
        self.possibly_aliased_delete_tables
            .iter()
            .map(|delete_table| {
                if delete_table.has_qualifiers() || delete_table.has_alias() {
                    return delete_table.clone();
                }
                if let Some(resolved_table) = self.read_tables.iter().find_map(|read_table| {
                    read_table.alias.as_ref().and_then(|alias| {
                        if *alias == delete_table.name {
                            Some(read_table.clone())
                        } else {
                            None
                        }
                    })
                }) {
                    return resolved_table;
                }
                delete_table.clone()
            })
            .collect()
    }

    fn apply_difference_to_read_tables(&mut self, exclude_tables: Vec<TableReference>) {
        let mut exclude_tables_count = HashMap::new();
        for exclude_table in exclude_tables.iter() {
            *exclude_tables_count.entry(exclude_table).or_insert(0) += 1;
        }
        self.read_tables.retain(|base_table| {
            if let Some(count) = exclude_tables_count.get_mut(base_table) {
                if *count > 0 {
                    *count -= 1;
                }
                return false;
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::MySqlDialect;

    #[test]
    fn test_select_statement() {
        let sql = "SELECT a FROM t1";
        let result = CrudTableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                }],
                update_tables: vec![],
                delete_tables: vec![],
            }),]
        )
    }

    #[test]
    fn test_insert_statement() {
        let sql = "INSERT INTO t1 (a) VALUES (1)";
        let result = CrudTableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                }],
                read_tables: vec![],
                update_tables: vec![],
                delete_tables: vec![],
            }),]
        )
    }

    #[test]
    fn test_update_statement() {
        let sql = "UPDATE t1 SET a=1";
        let result = CrudTableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
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
    fn test_delete_statement() {
        let sql = "DELETE FROM t1";
        let result = CrudTableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![],
                update_tables: vec![],
                delete_tables: vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                }],
            }),]
        )
    }

    #[test]
    fn test_multiple_tables_update_statement_with_qualified_assignments() {
        let sql =
            "UPDATE t1 AS a JOIN t2 AS b USING (id) SET a.foo='bar', b.baz='bat' WHERE a.id=1";
        match CrudTableExtractor::extract(&MySqlDialect {}, sql.into()) {
            Ok(result) => assert_eq!(
                result,
                vec![Ok(CrudTables {
                    create_tables: vec![],
                    read_tables: vec![],
                    update_tables: vec![
                        TableReference {
                            catalog: None,
                            schema: None,
                            name: "t1".into(),
                            alias: Some("a".into()),
                        },
                        TableReference {
                            catalog: None,
                            schema: None,
                            name: "t2".into(),
                            alias: Some("b".into()),
                        },
                    ],
                    delete_tables: vec![],
                }),]
            ),
            Err(error) => unreachable!("Should not have errored: {}", error),
        }
    }

    #[test]
    fn test_multiple_tables_update_statement_with_qualified_assignments2() {
        let sql = "UPDATE t1 AS a JOIN t2 AS b USING (id) JOIN (select * from t3 JOIN t4) SET a.foo='bar', baz='bat' WHERE a.id=1";
        match CrudTableExtractor::extract(&MySqlDialect {}, sql.into()) {
            Ok(result) => assert_eq!(
                result,
                vec![Ok(CrudTables {
                    create_tables: vec![],
                    read_tables: vec![],
                    update_tables: vec![
                        TableReference {
                            catalog: None,
                            schema: None,
                            name: "t1".into(),
                            alias: Some("a".into()),
                        },
                        TableReference {
                            catalog: None,
                            schema: None,
                            name: "t2".into(),
                            alias: Some("b".into()),
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
                            name: "t4".into(),
                            alias: None,
                        },
                    ],
                    delete_tables: vec![],
                }),]
            ),
            Err(error) => unreachable!("Should not have errored: {}", error),
        }
    }
}
