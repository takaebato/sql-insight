//! Basic SQL formatting — round-trips through sqlparser's AST
//! and emits its `Display`. See [`format()`] as the entry point.
//!
//! Output is a pass-through to [`sqlparser::ast::Statement`]'s
//! `Display` impl (keywords uppercase, single-space separators,
//! comments dropped). Default is single-line; opt into sqlparser's
//! multi-line pretty-print by setting [`FormatterOptions::pretty`]
//! and using [`format_with_options`].
//!
//! For value-normalization (collapsing `1` and `42` into the same
//! literal, etc.) see [`crate::normalizer`].

use crate::error::Error;
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Parse `sql` under `dialect` and re-emit one formatted string per
/// statement.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
///
/// let dialect = GenericDialect {};
/// let sql = "SELECT a FROM t1 \n WHERE b =   1";
/// let result = sql_insight::formatter::format(&dialect, sql).unwrap();
/// assert_eq!(result, ["SELECT a FROM t1 WHERE b = 1"]);
/// ```
pub fn format(dialect: &dyn Dialect, sql: &str) -> Result<Vec<String>, Error> {
    Formatter::format(dialect, sql, FormatterOptions::default())
}

/// Parse `sql` under `dialect` and re-emit one formatted string per
/// statement, with formatting controlled by `options`.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
/// use sql_insight::formatter::{format_with_options, FormatterOptions};
///
/// let dialect = GenericDialect {};
/// let sql = "SELECT a, b FROM t1";
/// let result = format_with_options(
///     &dialect,
///     sql,
///     FormatterOptions::new().with_pretty(true),
/// )
/// .unwrap();
/// assert_eq!(result[0], "SELECT\n  a,\n  b\nFROM\n  t1");
/// ```
pub fn format_with_options(
    dialect: &dyn Dialect,
    sql: &str,
    options: FormatterOptions,
) -> Result<Vec<String>, Error> {
    Formatter::format(dialect, sql, options)
}

/// Options controlling [`format_with_options()`] / [`Formatter::format`].
#[derive(Debug, Clone, Default)]
pub struct FormatterOptions {
    /// When `true`, emit the multi-line pretty-printed form via
    /// sqlparser's `{:#}` alternate `Display` (indented, one item
    /// per line). Defaults to `false` (single-line).
    pub pretty: bool,
}

impl FormatterOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_pretty(mut self, pretty: bool) -> Self {
        self.pretty = pretty;
        self
    }
}

/// Struct-style entry point. Used by both [`format()`] and
/// [`format_with_options()`].
#[derive(Debug, Default)]
pub struct Formatter;

impl Formatter {
    /// Parse `sql` under `dialect` and re-emit each statement,
    /// formatted according to `options`. [`format()`] / [`format_with_options()`]
    /// are thin free-function wrappers around this.
    pub fn format(
        dialect: &dyn Dialect,
        sql: &str,
        options: FormatterOptions,
    ) -> Result<Vec<String>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        Ok(statements
            .into_iter()
            .map(|statement| {
                if options.pretty {
                    format!("{statement:#}")
                } else {
                    statement.to_string()
                }
            })
            .collect::<Vec<String>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::all_dialects;

    fn assert_format(sql: &str, expected: Vec<String>, dialects: Vec<Box<dyn Dialect>>) {
        for dialect in dialects {
            let result =
                Formatter::format(dialect.as_ref(), sql, FormatterOptions::default()).unwrap();
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

    #[test]
    fn test_pretty_print_select() {
        let result = format_with_options(
            &sqlparser::dialect::GenericDialect {},
            "SELECT a, b FROM t1",
            FormatterOptions::new().with_pretty(true),
        )
        .unwrap();
        assert_eq!(result, vec!["SELECT\n  a,\n  b\nFROM\n  t1"]);
    }

    #[test]
    fn test_pretty_print_options_default_is_single_line() {
        // `FormatterOptions::default()` should match `format()`'s
        // single-line output — round-trip equality matters for the
        // builder's invariant.
        let single = format(
            &sqlparser::dialect::GenericDialect {},
            "SELECT a, b FROM t1",
        )
        .unwrap();
        let via_options = format_with_options(
            &sqlparser::dialect::GenericDialect {},
            "SELECT a, b FROM t1",
            FormatterOptions::default(),
        )
        .unwrap();
        assert_eq!(single, via_options);
    }
}
