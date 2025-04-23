//! A Normalizer that converts SQL queries to a canonical form.
//!
//! See [`normalize`](crate::normalize()) as the entry point for normalizing SQL.

use std::ops::ControlFlow;

use crate::error::Error;
use sqlparser::ast::{Expr, VisitMut, VisitorMut};
use sqlparser::ast::{Query, SetExpr, Value};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;
use std::ops::DerefMut;

/// Convenience function to normalize SQL with default options.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
///
/// let dialect = GenericDialect {};
/// let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'";
/// let result = sql_insight::normalize(&dialect, sql).unwrap();
/// assert_eq!(result, ["SELECT a FROM t1 WHERE b = ? AND c IN (?, ?) AND d LIKE ?"]);
/// ```
pub fn normalize(dialect: &dyn Dialect, sql: &str) -> Result<Vec<String>, Error> {
    Normalizer::normalize(dialect, sql, NormalizerOptions::new())
}

/// Convenience function to normalize SQL with options.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
/// use sql_insight::NormalizerOptions;
///
/// let dialect = GenericDialect {};
/// let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3, 4)";
/// let result = sql_insight::normalize_with_options(&dialect, sql, NormalizerOptions::new().with_unify_in_list(true)).unwrap();
/// assert_eq!(result, ["SELECT a FROM t1 WHERE b = ? AND c IN (...)"]);
/// ```
pub fn normalize_with_options(
    dialect: &dyn Dialect,
    sql: &str,
    options: NormalizerOptions,
) -> Result<Vec<String>, Error> {
    Normalizer::normalize(dialect, sql, options)
}

/// Options for normalizing SQL.
#[derive(Default, Clone)]
pub struct NormalizerOptions {
    /// Unify IN lists to a single form when all elements are literal values.
    /// For example, `IN (1, 2, 3)` becomes `IN (...)`.
    pub unify_in_list: bool,
    /// Unify VALUES lists to a single form when all elements are literal values.
    /// For example, `VALUES (1, 2, 3), (4, 5, 6)` becomes `VALUES (...)`.
    pub unify_values: bool,
}

impl NormalizerOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_unify_in_list(mut self, unify_in_list: bool) -> Self {
        self.unify_in_list = unify_in_list;
        self
    }

    pub fn with_unify_values(mut self, unify_values: bool) -> Self {
        self.unify_values = unify_values;
        self
    }
}

/// A visitor for SQL AST nodes that normalizes SQL queries.
#[derive(Default)]
pub struct Normalizer {
    pub options: NormalizerOptions,
}

impl VisitorMut for Normalizer {
    type Break = ();

    fn post_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        if let SetExpr::Values(values) = query.body.deref_mut() {
            if self.options.unify_values {
                let rows = &mut values.rows;
                if rows.is_empty()
                    || rows.iter().all(|row| {
                        row.is_empty() || row.iter().all(|expr| matches!(expr, Expr::Value(_)))
                    })
                {
                    *rows = vec![vec![Expr::Value(Value::Placeholder("...".into()))]];
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if let Expr::UnaryOp { op: _, expr: child } = expr {
            if matches!(**child, Expr::Value(_)) {
                *expr = Expr::Value(Value::Placeholder("?".into()));
            }
        } else if let Expr::Value(value) = expr {
            *value = Value::Placeholder("?".into());
        }
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        match expr {
            Expr::InList { list, .. } if self.options.unify_in_list => {
                if list.is_empty() || list.iter().all(|expr| matches!(expr, Expr::Value(_))) {
                    *list = vec![Expr::Value(Value::Placeholder("...".into()))];
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

impl Normalizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_options(mut self, options: NormalizerOptions) -> Self {
        self.options = options;
        self
    }

    /// Normalize SQL.
    pub fn normalize(
        dialect: &dyn Dialect,
        sql: &str,
        options: NormalizerOptions,
    ) -> Result<Vec<String>, Error> {
        let mut statements = Parser::parse_sql(dialect, sql)?;
        statements.visit(&mut Self::new().with_options(options));
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

    fn assert_normalize(
        sql: &str,
        expected: Vec<String>,
        dialects: Vec<Box<dyn Dialect>>,
        options: NormalizerOptions,
    ) {
        for dialect in dialects {
            let result = Normalizer::normalize(dialect.as_ref(), sql, options.clone()).unwrap();
            assert_eq!(result, expected, "Failed for dialect: {dialect:?}")
        }
    }

    #[test]
    fn test_single_sql() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, (select * from b)) AND d LIKE '%foo'";
        let expected = vec![
            "SELECT a FROM t1 WHERE b = ? AND c IN (?, (SELECT * FROM b)) AND d LIKE ?".into(),
        ];
        assert_normalize(sql, expected, all_dialects(), NormalizerOptions::new());
    }

    #[test]
    fn test_multiple_sql() {
        let sql = "INSERT INTO t2 (a) VALUES (4); UPDATE t1 SET a = 1 WHERE b = 2; DELETE FROM t3 WHERE c = 3";
        let expected = vec![
            "INSERT INTO t2 (a) VALUES (?)".into(),
            "UPDATE t1 SET a = ? WHERE b = ?".into(),
            "DELETE FROM t3 WHERE c = ?".into(),
        ];
        assert_normalize(sql, expected, all_dialects(), NormalizerOptions::new());
    }

    #[test]
    fn test_unary_operators_preceding_constants() {
        let sql = "SELECT * FROM t1 WHERE a=-9 AND b=+ 9 AND c=TRUE AND d=NOT TRUE AND e=NOT(TRUE) AND f IS NULL";
        let expected = vec![
            "SELECT * FROM t1 WHERE a = ? AND b = ? AND c = ? AND d = ? AND e = NOT (?) AND f IS NULL".into(),
        ];
        assert_normalize(sql, expected, all_dialects(), NormalizerOptions::new());
    }

    #[test]
    fn test_sql_with_in_list_without_unify_in_list_option() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3, 4)";
        let expected = vec!["SELECT a FROM t1 WHERE b = ? AND c IN (?, ?, ?)".into()];
        assert_normalize(sql, expected, all_dialects(), NormalizerOptions::new());
    }

    #[test]
    fn test_sql_with_in_list_with_unify_in_list_option() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3, NULL)";
        let expected = vec!["SELECT a FROM t1 WHERE b = ? AND c IN (...)".into()];
        assert_normalize(
            sql,
            expected,
            all_dialects(),
            NormalizerOptions::new().with_unify_in_list(true),
        );
    }

    #[test]
    fn test_sql_with_in_list_with_unify_in_list_option_when_not_all_elements_are_literal_values() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, (SELECT * FROM t2 WHERE d IN (3, COALESCE(e, 5))))";
        let expected = vec!["SELECT a FROM t1 WHERE b = ? AND c IN (?, (SELECT * FROM t2 WHERE d IN (?, COALESCE(e, ?))))".into()];
        assert_normalize(
            sql,
            expected,
            all_dialects(),
            NormalizerOptions::new().with_unify_in_list(true),
        );
    }

    #[test]
    fn test_sql_with_values_without_unify_values_option() {
        let sql = "INSERT INTO t1 (a, b, c) VALUES (1, 2, 3), (4, 5, 6), (7, 8, 9)";
        let expected =
            vec!["INSERT INTO t1 (a, b, c) VALUES (?, ?, ?), (?, ?, ?), (?, ?, ?)".into()];
        assert_normalize(sql, expected, all_dialects(), NormalizerOptions::new());
    }

    #[test]
    fn test_sql_with_values_with_unify_values_option() {
        let sql = "INSERT INTO t1 (a, b, c) VALUES (1, 2, 3), (4, 5, 6), (7, 8, 9)";
        let expected = vec!["INSERT INTO t1 (a, b, c) VALUES (...)".into()];
        assert_normalize(
            sql,
            expected,
            all_dialects(),
            NormalizerOptions::new().with_unify_values(true),
        );
    }

    #[test]
    fn test_sql_with_values_with_row_constructor_with_unify_values_option() {
        let sql = "INSERT INTO t1 (a, b, c) VALUES ROW(1, 2, 3), ROW(4, 5, 6), ROW(7, 8, 9)";
        let expected = vec!["INSERT INTO t1 (a, b, c) VALUES ROW(...)".into()];
        assert_normalize(
            sql,
            expected,
            all_dialects(),
            NormalizerOptions::new().with_unify_values(true),
        );
    }

    #[test]
    fn test_sql_with_values_with_unify_values_option_when_not_all_elements_are_literal_values() {
        let sql = "INSERT INTO t1 (a, b, c) VALUES (1, 2, 3), (4, 5, 6), (7, (SELECT * FROM t2 WHERE d = 9))";
        let expected = vec!["INSERT INTO t1 (a, b, c) VALUES (?, ?, ?), (?, ?, ?), (?, (SELECT * FROM t2 WHERE d = ?))".into()];
        assert_normalize(
            sql,
            expected,
            all_dialects(),
            NormalizerOptions::new().with_unify_values(true),
        );
    }
}
