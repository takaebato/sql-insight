use std::collections::HashMap;
use std::ops::ControlFlow;

use crate::error::Error;
use sqlparser::ast::{TableFactor, Visit, Visitor};
use sqlparser::dialect::{dialect_from_str, Dialect};
use sqlparser::parser::Parser;

pub fn extract_tables(dialect: &dyn Dialect, subject: String) -> Result<Tables, Error> {
    TableExtractor::extract(dialect, subject)
}

pub fn extract_tables_cli(dialect_name: &str, subject: String) -> Result<Tables, Error> {
    match dialect_from_str(dialect_name) {
        Some(dialect) => Ok(extract_tables(dialect.as_ref(), subject)?),
        None => Err(Error::ArgumentError(format!(
            "Dialect not found: {}",
            dialect_name
        ))),
    }
}

type Original = String;
type Alias = String;

#[derive(Debug, PartialEq)]
pub struct Tables {
    pub tables: Vec<Original>,
    pub aliases: HashMap<Original, Alias>,
}

#[derive(Default, Debug)]
pub struct TableExtractor {
    tables: Vec<Original>,
    aliases: HashMap<Original, Alias>,
}

impl Visitor for TableExtractor {
    type Break = ();

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { name, alias, .. } = table_factor {
            self.tables.push(name.0[0].value.clone());
            if let Some(alias) = alias {
                self.aliases
                    .insert(name.0[0].value.clone(), alias.name.value.clone());
            }
        }
        ControlFlow::Continue(())
    }
}

impl TableExtractor {
    pub fn extract(dialect: &dyn Dialect, subject: String) -> Result<Tables, Error> {
        let statements = Parser::parse_sql(dialect, &subject)?;
        let mut visitor = TableExtractor::default();
        statements.visit(&mut visitor);
        Ok(Tables {
            tables: visitor.tables,
            aliases: visitor.aliases,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::MySqlDialect;

    #[test]
    fn test_select_statement() {
        let sql = "SELECT a FROM t1 INNER JOIN t2 ON t1.id = t2.id WHERE b = ( SELECT c FROM t3 )";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            Tables {
                tables: vec!["t1".into(), "t2".into(), "t3".into()],
                aliases: HashMap::new(),
            }
        )
    }

    #[test]
    fn test_select_statement_with_aliases() {
        let sql = "SELECT a FROM t1 AS t1_alias INNER JOIN t2 AS t2_alias ON t1_alias.id = t2_alias.id WHERE b = ( SELECT c FROM t3 )";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            Tables {
                tables: vec!["t1".into(), "t2".into(), "t3".into()],
                aliases: HashMap::from([
                    ("t1".into(), "t1_alias".into()),
                    ("t2".into(), "t2_alias".into())
                ]),
            }
        )
    }

    #[test]
    fn test_delete_statement_with_aliases() {
        let sql = "DELETE t1_alias FROM t1 AS t1_alias JOIN t2 AS t2_alias ON t1_alias.a = t2_alias.a WHERE t2_alias.b = 1";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            Tables {
                tables: vec!["t1".into(), "t2".into()],
                aliases: HashMap::from([
                    ("t1".into(), "t1_alias".into()),
                    ("t2".into(), "t2_alias".into())
                ]),
            }
        )
    }

    #[test]
    fn test_delete_multiple_tables_with_join() {
        let sql =
            "DELETE t1, t2 FROM t1 INNER JOIN t2 INNER JOIN t3 WHERE t1.a = t2.a AND t2.a = t3.a";
        let result = TableExtractor::extract(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            Tables {
                tables: vec!["t1".into(), "t2".into(), "t3".into()],
                aliases: HashMap::new(),
            }
        )
    }
}
