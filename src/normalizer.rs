use std::ops::ControlFlow;

use crate::error::Error;
use sqlparser::ast::Value;
use sqlparser::ast::{Expr, VisitMut, VisitorMut};
use sqlparser::dialect::{dialect_from_str, Dialect};
use sqlparser::parser::Parser;

pub fn normalize(dialect: &dyn Dialect, subject: String) -> Result<Vec<String>, Error> {
    Normalizer::normalize(dialect, subject)
}

pub fn normalize_cli(dialect_name: &str, subject: String) -> Result<Vec<String>, Error> {
    match dialect_from_str(dialect_name) {
        Some(dialect) => Ok(normalize(dialect.as_ref(), subject)?),
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
    pub fn normalize(dialect: &dyn Dialect, subject: String) -> Result<Vec<String>, Error> {
        let mut statements = Parser::parse_sql(dialect, &subject)?;
        println!("{:?}", statements);
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
        // let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, (select * from b)) AND d LIKE '%foo'";
        let sql = "UPDATE t1 JOIN t2 USING (id) SET b.a.foo='bar', baz='bat' WHERE a.id=1";
        // let sql = "WITH updated_departments AS (UPDATE department SET budget = budget * 1.1 WHERE location = '東京' RETURNING id) UPDATE employees SET salary = salary * 1.05 FROM updated_departments WHERE employees.department_id = updated_departments.id;";

        match Normalizer::normalize(&MySqlDialect {}, sql.into()) {
            Ok(result) => assert_eq!(
                result,
                ["SELECT a FROM t1 WHERE b = ? AND c IN (?, ?) AND d LIKE ?"]
            ),
            Err(error) => unreachable!("Should not have errored: {}", error),
        }
    }
}
