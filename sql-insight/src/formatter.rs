//! A Formatter that formats SQL into a standardized format.
//!
//! See [`format`](crate::format) as the entry point for formatting SQL.

use crate::error::Error;
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to format SQL.
///
/// ## Example
///
/// ```rust
/// use sqlparser::dialect::GenericDialect;
///
/// let dialect = GenericDialect {};
/// let sql = "SELECT a FROM t1 \n WHERE b =   1";
/// let result = sql_insight::format(&dialect, sql).unwrap();
/// assert_eq!(result, ["SELECT a FROM t1 WHERE b = 1"]);
/// ```
pub fn format(dialect: &dyn Dialect, sql: &str) -> Result<Vec<String>, Error> {
    Formatter::format(dialect, sql)
}

/// Formatter for SQL.
#[derive(Debug, Default)]
pub struct Formatter;

impl Formatter {
    /// Format SQL.
    pub fn format(dialect: &dyn Dialect, sql: &str) -> Result<Vec<String>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        Ok(statements
            .into_iter()
            .map(|statement| statement.to_string())
            .collect::<Vec<String>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::all_dialects;

    fn assert_format(sql: &str, expected: Vec<String>, dialects: Vec<Box<dyn Dialect>>) {
        for dialect in dialects {
            let result = Formatter::format(dialect.as_ref(), sql).unwrap();
            assert_eq!(result, expected, "Failed for dialect: {dialect:?}")
        }
    }

    #[test]
    fn test_single_sql() {
        let sql =
            "SELECT a from t1   WHERE b=1 AND c in (2, (select * from b))\n  AND d LIKE '%foo'";
        let expected = vec![
            "SELECT a FROM t1 WHERE b = 1 AND c IN (2, (SELECT * FROM b)) AND d LIKE '%foo'".into(),
        ];
        assert_format(sql, expected, all_dialects());
    }

    #[test]
    fn test_multiple_sql() {
        let sql = "INSERT INTO   t2  \n (a) VALUES (4); UPDATE t1   SET b  = 2 \n WHERE a = 1; DELETE \n FROM t3   WHERE c = 3";
        let expected = vec![
            "INSERT INTO t2 (a) VALUES (4)".into(),
            "UPDATE t1 SET b = 2 WHERE a = 1".into(),
            "DELETE FROM t3 WHERE c = 3".into(),
        ];
        assert_format(sql, expected, all_dialects());
    }

    #[test]
    fn test_sql_with_comments() {
        let sql = "SELECT a FROM t1 WHERE b = 1; -- comment\nSELECT b FROM t2 WHERE c =  2  /* comment */";
        let expected = vec![
            "SELECT a FROM t1 WHERE b = 1".into(),
            "SELECT b FROM t2 WHERE c = 2".into(),
        ];
        assert_format(sql, expected, all_dialects());
    }
}
