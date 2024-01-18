use std::collections::HashMap;
use std::ops::ControlFlow;

use crate::error::Error;
use sqlparser::ast::{Statement, TableFactor, Visit, Visitor};
use sqlparser::dialect::{dialect_from_str, Dialect};
use sqlparser::parser::Parser;
use tap::Tap;

pub fn extract_crud_tables(dialect: &dyn Dialect, subject: String) -> Result<CrudTables, Error> {
    CrudTableExtractor::extract(dialect, subject)
}

pub fn extract_crud_tables_cli(dialect_name: &str, subject: String) -> Result<CrudTables, Error> {
    match dialect_from_str(dialect_name) {
        Some(dialect) => Ok(extract_crud_tables(dialect.as_ref(), subject)?),
        None => Err(Error::ArgumentError(format!(
            "Dialect not found: {}",
            dialect_name
        ))),
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct CrudTables {
    pub create_tables: Vec<String>,
    pub read_tables: Vec<String>,
    pub update_tables: Vec<String>,
    pub delete_tables: Vec<String>,
}

impl CrudTables {
    pub fn create_tables(&self) -> Vec<String> {
        self.create_tables.clone()
    }
    pub fn read_tables(&self) -> Vec<String> {
        self.read_tables.clone()
    }
    pub fn update_tables(&self) -> Vec<String> {
        self.update_tables.clone()
    }
    pub fn delete_tables(&self) -> Vec<String> {
        self.delete_tables.clone()
    }
}

#[derive(Default, Debug)]
pub struct CrudTableExtractor {
    create_tables: Vec<String>,
    read_tables: Vec<String>,
    update_tables: Vec<String>,
    delete_tables: Vec<String>,
    aliases: HashMap<String, String>,
    to_subtract_from_read: Vec<String>,
}

impl Visitor for CrudTableExtractor {
    type Break = ();

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { name, alias, .. } = table_factor {
            self.read_tables.push(name.0[0].value.clone());
            if let Some(alias) = alias {
                self.aliases
                    .insert(alias.name.value.clone(), name.0[0].value.clone());
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        match statement {
            Statement::Insert { table_name, .. } => {
                self.create_tables.push(table_name.0[0].value.clone());
                self.to_subtract_from_read
                    .push(table_name.0[0].value.clone());
            }
            Statement::Update { table, .. } => {
                if let TableFactor::Table { name, .. } = &table.relation {
                    self.update_tables.push(name.0[0].value.clone());
                    self.to_subtract_from_read.push(name.0[0].value.clone());
                }
            }
            Statement::Delete {
                tables,
                from,
                using,
                ..
            } => {
                if !tables.is_empty() {
                    for obj_name in tables {
                        self.delete_tables.push(obj_name.0[0].value.clone());
                        self.to_subtract_from_read.push(obj_name.0[0].value.clone());
                    }
                } else {
                    for table_with_joins in from {
                        if let TableFactor::Table { name, .. } = &table_with_joins.relation {
                            self.delete_tables.push(name.0[0].value.clone());
                            self.to_subtract_from_read.push(name.0[0].value.clone());
                            // subtract again since using contains the same table
                            if using.is_some() {
                                self.to_subtract_from_read.push(name.0[0].value.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

impl CrudTableExtractor {
    pub fn extract(dialect: &dyn Dialect, subject: String) -> Result<CrudTables, Error> {
        let statements = Parser::parse_sql(dialect, &subject)?;
        let mut visitor = CrudTableExtractor::default();
        statements.visit(&mut visitor);
        let create_tables = visitor
            .convert_alias_to_original(visitor.create_tables.clone())
            .tap_mut(|vec| vec.sort());
        let read_tables = visitor
            .convert_alias_to_original(visitor.read_tables.clone())
            .tap_mut(|vec| vec.sort());
        let update_tables = visitor
            .convert_alias_to_original(visitor.update_tables.clone())
            .tap_mut(|vec| vec.sort());
        let delete_tables = visitor
            .convert_alias_to_original(visitor.delete_tables.clone())
            .tap_mut(|vec| vec.sort());
        Ok(CrudTables {
            read_tables: visitor.subtract(
                read_tables,
                visitor.convert_alias_to_original(visitor.to_subtract_from_read.clone()),
            ),
            create_tables,
            update_tables,
            delete_tables,
        })
    }

    fn convert_alias_to_original(&self, tables: Vec<String>) -> Vec<String> {
        tables
            .into_iter()
            .map(|table| {
                if let Some(original) = self.aliases.get(&table) {
                    original.clone()
                } else {
                    table
                }
            })
            .collect()
    }

    fn subtract(&self, read_tables: Vec<String>, mut to_subtracts: Vec<String>) -> Vec<String> {
        read_tables
            .into_iter()
            .filter(|read| {
                if let Some(pos) = to_subtracts.iter().position(|sub| sub == read) {
                    to_subtracts.remove(pos);
                    false
                } else {
                    true
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::MySqlDialect;

    #[test]
    fn test_select_statement() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
        match CrudTableExtractor::extract(&MySqlDialect {}, sql.into()) {
            Ok(result) => assert_eq!(
                result,
                CrudTables {
                    create_tables: vec![],
                    read_tables: vec!["t1".to_string(), "t2".to_string()],
                    update_tables: vec![],
                    delete_tables: vec![],
                }
            ),
            Err(error) => unreachable!("Should not have errored: {}", error),
        }
    }
}
