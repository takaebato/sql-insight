use core::fmt;
use std::ops::ControlFlow;

use crate::error::Error;
use sqlparser::ast::{Ident, ObjectName, Statement, TableFactor, TableWithJoins, Visit, Visitor};
use sqlparser::dialect::{dialect_from_str, Dialect};
use sqlparser::parser::Parser;

pub fn extract_tables(
    dialect: &dyn Dialect,
    sql: String,
) -> Result<Vec<Result<Tables, Error>>, Error> {
    TableExtractor::extract(dialect, sql)
}

pub fn extract_tables_from_cli(
    dialect_name: Option<&str>,
    sql: String,
) -> Result<Vec<String>, Error> {
    let dialect_name = dialect_name.unwrap_or("generic");
    match dialect_from_str(dialect_name) {
        Some(dialect) => {
            let result = extract_tables(dialect.as_ref(), sql)?;
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
        if let TableFactor::Table { name, alias, .. } = table {
            match name.0.len() {
                0 => Err(Error::AnalysisError("No identifiers provided".to_string())),
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
            }
        } else {
            Err(Error::AnalysisError("Not a table".to_string()))
        }
    }
}

impl TryFrom<&ObjectName> for TableReference {
    type Error = Error;

    fn try_from(obj_name: &ObjectName) -> Result<Self, Self::Error> {
        match obj_name.0.len() {
            0 => Err(Error::AnalysisError("No identifiers provided".to_string())),
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

#[derive(Default, Debug)]
pub struct TableExtractor(Vec<TableReference>);

impl Visitor for TableExtractor {
    type Break = Error;

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { .. } = table_factor {
            match TableReference::try_from(table_factor) {
                Ok(table) => self.0.push(table),
                Err(e) => return ControlFlow::Break(e),
            }
        }
        ControlFlow::Continue(())
    }
}

impl TableExtractor {
    pub fn extract(
        dialect: &dyn Dialect,
        sql: String,
    ) -> Result<Vec<Result<Tables, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, &sql)?;
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
            ControlFlow::Continue(()) => Ok(Tables(visitor.0)),
        }
    }

    // `Visit` trait object cannot be used since method `visit` has generic type parameters.
    // Concrete type `TableWithJoins` is used instead.
    pub fn extract_from_table_node(table: &TableWithJoins) -> Result<Tables, Error> {
        let mut visitor = TableExtractor::default();
        match table.visit(&mut visitor) {
            ControlFlow::Break(e) => Err(e),
            ControlFlow::Continue(()) => Ok(Tables(visitor.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::MySqlDialect;

    #[test]
    fn test_single_statement() {
        let sql = "SELECT a FROM t1";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![Ok(Tables(vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: None,
            },]))]
        )
    }

    #[test]
    fn test_multiple_statements() {
        let sql = "SELECT a FROM t1; SELECT b FROM t2";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![
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
            ]
        )
    }

    #[test]
    fn test_statement_with_alias() {
        let sql = "SELECT a FROM t1 AS t1_alias";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![Ok(Tables(vec![TableReference {
                catalog: None,
                schema: None,
                name: "t1".into(),
                alias: Some("t1_alias".into()),
            },]))]
        )
    }

    #[test]
    fn test_statement_with_table_identifier() {
        let sql = "SELECT a FROM catalog.schema.table";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![Ok(Tables(vec![TableReference {
                catalog: Some("catalog".into()),
                schema: Some("schema".into()),
                name: "table".into(),
                alias: None,
            }]))]
        )
    }

    #[test]
    fn test_statement_with_table_identifier_and_alias() {
        let sql = "SELECT a FROM catalog.schema.table AS table_alias";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![Ok(Tables(vec![TableReference {
                catalog: Some("catalog".into()),
                schema: Some("schema".into()),
                name: "table".into(),
                alias: Some("table_alias".into()),
            }]))]
        )
    }

    #[test]
    fn test_statement_where_same_tables_appear_multiple_times() {
        let sql = "SELECT a FROM t1 INNER JOIN t2 ON t1.id = t2.id WHERE b = ( SELECT c FROM t3 INNER JOIN t1 ON t3.id = t1.id )";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![Ok(Tables(vec![
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
            ]))]
        )
    }

    #[test]
    fn test_statement_error_with_too_many_identifiers() {
        let sql = "SELECT a FROM catalog.schema.table.extra";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![Err(Error::AnalysisError(
                "Too many identifiers provided".to_string()
            ))]
        )
    }

    #[test]
    fn test_delete_statement_with_aliases() {
        let sql = "DELETE t1_alias FROM t1 AS t1_alias JOIN t2 AS t2_alias ON t1_alias.a = t2_alias.a WHERE t2_alias.b = 1";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![Ok(Tables(vec![
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
            ]))]
        )
    }

    #[test]
    fn test_delete_multiple_tables_with_join() {
        let sql =
            "DELETE t1, t2 FROM t1 INNER JOIN t2 INNER JOIN t3 WHERE t1.a = t2.a AND t2.a = t3.a";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![Ok(Tables(vec![
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
            ]))]
        )
    }
}
