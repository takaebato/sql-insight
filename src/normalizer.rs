use std::ops::ControlFlow;

use crate::error::Error;
use sqlparser::ast::Value;
use sqlparser::ast::{Expr, VisitMut, VisitorMut};
use sqlparser::dialect::{dialect_from_str, Dialect};
use sqlparser::parser::Parser;

pub fn normalize(dialect: &dyn Dialect, sql: String) -> Result<Vec<String>, Error> {
    Normalizer::normalize(dialect, sql)
}

pub fn normalize_from_cli(dialect_name: Option<&str>, sql: String) -> Result<Vec<String>, Error> {
    let dialect_name = dialect_name.unwrap_or("generic");
    match dialect_from_str(dialect_name) {
        Some(dialect) => Ok(normalize(dialect.as_ref(), sql)?),
        None => Err(Error::ArgumentError(format!(
            "Dialect not found: {}",
            dialect_name
        ))),
    }
}

struct Normalizer;

impl VisitorMut for Normalizer {
    type Break = ();

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if let Expr::Value(value) = expr {
            *value = Value::Placeholder("?".into());
        }
        ControlFlow::Continue(())
    }
}

impl Normalizer {
    pub fn normalize(dialect: &dyn Dialect, sql: String) -> Result<Vec<String>, Error> {
        let mut statements = Parser::parse_sql(dialect, &sql)?;
        statements.visit(&mut Self);
        Ok(statements
            .into_iter()
            .map(|statement| statement.to_string())
            .collect::<Vec<String>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::MySqlDialect;

    #[test]
    fn test_single_sql() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, (select * from b)) AND d LIKE '%foo'";

        match Normalizer::normalize(&MySqlDialect {}, sql.into()) {
            Ok(result) => assert_eq!(
                result,
                vec!["SELECT a FROM t1 WHERE b = ? AND c IN (?, (SELECT * FROM b)) AND d LIKE ?"]
            ),
            Err(error) => unreachable!("Should not have errored: {}", error),
        }
    }
}
