use crate::error::Error;
use sqlparser::dialect::{dialect_from_str, Dialect};
use sqlparser::parser::Parser;

pub fn format(dialect: &dyn Dialect, subject: String) -> Result<Vec<String>, Error> {
    Formatter::format(dialect, subject)
}

pub fn format_cli(dialect_name: &str, subject: String) -> Result<Vec<String>, Error> {
    match dialect_from_str(dialect_name) {
        Some(dialect) => Ok(format(dialect.as_ref(), subject)?),
        None => Err(Error::ArgumentError(format!(
            "Dialect not found: {}",
            dialect_name
        ))),
    }
}

struct Formatter;

impl Formatter {
    pub fn format(dialect: &dyn Dialect, subject: String) -> Result<Vec<String>, Error> {
        let mut statements = Parser::parse_sql(dialect, &subject)?;
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
        let sql = "SELECT a from t1   WHERE b=1 AND c in (2, (select * from b)) AND d LIKE '%foo'";
        match Formatter::format(&MySqlDialect {}, sql.into()) {
            Ok(result) => assert_eq!(
                result,
                ["SELECT a FROM t1 WHERE b = 1 AND c IN (2, (SELECT * FROM b)) AND d LIKE '%foo'"]
            ),
            Err(error) => unreachable!("Should not have errored: {}", error),
        }
    }
}
