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
