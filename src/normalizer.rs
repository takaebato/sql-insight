use std::ops::ControlFlow;

use crate::error::Error;
use sqlparser::ast::Value;
use sqlparser::ast::{Expr, VisitMut, VisitorMut};
use sqlparser::dialect::{dialect_from_str, Dialect};
use sqlparser::parser::Parser;

pub fn normalize(
    dialect: &dyn Dialect,
    sql: &str,
    options: NormalizerOptions,
) -> Result<Vec<String>, Error> {
    Normalizer::normalize(dialect, sql, options)
}

pub fn normalize_from_cli(
    dialect_name: Option<&str>,
    sql: &str,
    options: NormalizerOptions,
) -> Result<Vec<String>, Error> {
    let dialect_name = dialect_name.unwrap_or("generic");
    match dialect_from_str(dialect_name) {
        Some(dialect) => Ok(normalize(dialect.as_ref(), sql, options)?),
        None => Err(Error::ArgumentError(format!(
            "Dialect not found: {}",
            dialect_name
        ))),
    }
}

#[derive(Default, Clone)]
pub struct NormalizerOptions {
    pub unify_in_list: bool,
}

impl NormalizerOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_unify_in_list(mut self, unify_in_list: bool) -> Self {
        self.unify_in_list = unify_in_list;
        self
    }
}

#[derive(Default)]
pub struct Normalizer {
    pub options: NormalizerOptions,
}

impl VisitorMut for Normalizer {
    type Break = ();

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        if let Expr::Value(value) = expr {
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
}
