//! SQL normalization — rewrite the AST so structurally identical
//! queries hash to the same string. See [`normalize`] as the entry
//! point.
//!
//! The base pass replaces every literal `Value` with a `?`
//! placeholder, so queries that differ only in their parameter
//! values collapse to the same string.
//!
//! "Every literal" is meant literally: it includes `Value`s in
//! structurally significant positions, not just bound-parameter slots.
//! A JSON path (`JSON_TABLE(data, '$.a')`, `JSON_EXTRACT(data, '$.a')`),
//! a `CAST(x AS DATE FORMAT 'YYYY-MM-DD')` format string, the
//! `TABLESAMPLE (BUCKET 3 OUT OF 10)` / `(10 PERCENT)` counts, and
//! `LIMIT` / `OFFSET` are all rewritten to `?`. So two queries differing
//! only in such a literal — e.g. selecting a different JSON field or
//! sampling a different bucket — collapse to the same normalized string.
//!
//! Three opt-in toggles ([`NormalizerOptions`]) further collapse
//! repetitive shapes:
//!
//! - [`unify_in_list`](NormalizerOptions::unify_in_list):
//!   `IN (1, 2, 3)` → `IN (...)`.
//! - [`unify_values`](NormalizerOptions::unify_values):
//!   `VALUES (1, 2, 3), (4, 5, 6)` → `VALUES (...)`.
//! - [`alphabetize_insert_columns`](NormalizerOptions::alphabetize_insert_columns):
//!   `INSERT INTO t (c, b, a) VALUES (...)` →
//!   `INSERT INTO t (a, b, c) VALUES (...)`, only when VALUES is
//!   unified.
//!
//! Output is one `String` per parsed statement, formatted by
//! sqlparser's `Display` after the rewrite.

use std::ops::{ControlFlow, Deref};

use crate::error::Error;
use sqlparser::ast::{Expr, Insert, Statement, VisitMut, VisitorMut};
use sqlparser::ast::{Query, SetExpr, TopQuantity, Value};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;
use std::ops::DerefMut;

/// Parse `sql` under `dialect` and normalize each statement with
/// default options (literal-to-`?` placeholder substitution only).
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
///
/// let dialect = GenericDialect {};
/// let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'";
/// let result = sql_insight::normalizer::normalize(&dialect, sql).unwrap();
/// assert_eq!(result, ["SELECT a FROM t1 WHERE b = ? AND c IN (?, ?) AND d LIKE ?"]);
/// ```
pub fn normalize(dialect: &dyn Dialect, sql: &str) -> Result<Vec<String>, Error> {
    Normalizer::normalize(dialect, sql, NormalizerOptions::new())
}

/// Parse `sql` under `dialect` and normalize each statement,
/// applying any extra collapses enabled in `options`.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
/// use sql_insight::normalizer::{normalize_with_options, NormalizerOptions};
///
/// let dialect = GenericDialect {};
/// let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3, 4)";
/// let result = normalize_with_options(&dialect, sql, NormalizerOptions::new().with_unify_in_list(true)).unwrap();
/// assert_eq!(result, ["SELECT a FROM t1 WHERE b = ? AND c IN (...)"]);
/// ```
pub fn normalize_with_options(
    dialect: &dyn Dialect,
    sql: &str,
    options: NormalizerOptions,
) -> Result<Vec<String>, Error> {
    Normalizer::normalize(dialect, sql, options)
}

/// Toggles for [`normalize_with_options`]. Defaults to all `false`
/// (placeholder substitution only).
#[derive(Default, Clone)]
pub struct NormalizerOptions {
    /// Unify IN lists to a single form when all elements are literal values.
    /// For example, `IN (1, 2, 3)` becomes `IN (...)`.
    pub unify_in_list: bool,
    /// Unify VALUES lists to a single form when all elements are literal values.
    /// For example, `VALUES (1, 2, 3), (4, 5, 6)` becomes `VALUES (...)`.
    pub unify_values: bool,
    /// Alphabetize column lists for INSERT statements with a VALUES expression
    /// that gets unified.
    /// For example, `INSERT INTO t(c, b, a)` becomes `INSERT INTO t(a, b, c)`.
    pub alphabetize_insert_columns: bool,
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

    pub fn with_alphabetize_insert_columns(mut self, alphabetize_insert_columns: bool) -> Self {
        self.alphabetize_insert_columns = alphabetize_insert_columns;
        self
    }
}

/// `VisitorMut` impl that performs the normalization rewrite.
/// Most callers go through [`normalize`] / [`normalize_with_options`]
/// or [`Normalizer::normalize`] (which constructs and drives this
/// visitor internally). Use the struct directly only when you want
/// to integrate the rewrite into a larger AST traversal.
#[derive(Default)]
pub struct Normalizer {
    pub options: NormalizerOptions,
}

impl VisitorMut for Normalizer {
    type Break = ();

    fn post_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
        // `SELECT TOP 10`'s quantity is a bare `u64` (`TopQuantity::Constant`),
        // not a `Value`, so the visitor's literal pass doesn't reach it — unlike
        // `TOP (expr)` / `LIMIT`. Normalize it here so `TOP 10` becomes `TOP ?`.
        normalize_top(query.body.deref_mut());
        if let SetExpr::Values(values) = query.body.deref_mut() {
            if self.options.unify_values {
                let rows = &mut values.rows;
                if rows.is_empty()
                    || rows.iter().all(|row| {
                        row.is_empty() || row.iter().all(|expr| matches!(expr, Expr::Value(_)))
                    })
                {
                    *rows = vec![vec![Expr::Value(
                        Value::Placeholder("...".into()).with_empty_span(),
                    )]];
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn post_visit_statement(
        &mut self,
        stmt: &mut sqlparser::ast::Statement,
    ) -> ControlFlow<Self::Break> {
        if self.options.alphabetize_insert_columns {
            if let Statement::Insert(Insert {
                columns,
                after_columns,
                source,
                ..
            }) = stmt
            {
                if let Some(Query { body, .. }) = source.as_deref() {
                    if let SetExpr::Values(v) = body.deref() {
                        if v.rows
                            == vec![vec![Expr::Value(
                                Value::Placeholder("...".into()).with_empty_span(),
                            )]]
                        {
                            if columns.len() > 1 {
                                columns.sort_by_key(|s| s.value.to_lowercase());
                            }
                            if after_columns.len() > 1 {
                                after_columns.sort_by_key(|s| s.value.to_lowercase());
                            }
                        }
                    }
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        // A unary op over a literal — directly (`-9`) or through a chain of
        // unary ops (`- -9`, `+ -9`) — collapses to a *single* placeholder
        // (`?`, not `-?`). A parenthesised operand (`NOT (TRUE)`) is an
        // `Expr::Nested`, not a chain, so it isn't collapsed — only its inner
        // value is, by `pre_visit_value` on descent. Every other literal —
        // including a plain `Expr::Value` — is normalized by `pre_visit_value`,
        // so it needs no arm here.
        if let Expr::UnaryOp { op: _, expr: child } = expr {
            if Self::is_unary_chain_over_value(child) {
                *expr = Expr::Value(Value::Placeholder("?".into()).with_empty_span());
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_value(&mut self, value: &mut Value) -> ControlFlow<Self::Break> {
        // The base contract: *every* literal `Value` becomes `?`, wherever the
        // AST holds it. `pre_visit_expr` only catches an `Expr::Value`; a
        // literal kept in a bare `Value` field — `DATE '…'` / `TIMESTAMP '…'`
        // (`TypedString`), a `LIKE … ESCAPE '!'` char, a `MATCH … AGAINST '…'`
        // search string — is reached only through this hook.
        *value = Value::Placeholder("?".into());
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<Self::Break> {
        match expr {
            Expr::InList { list, .. }
                if self.options.unify_in_list
                    && list.iter().all(Self::contains_only_tuples_of_values) =>
            {
                *list = vec![Expr::Value(
                    Value::Placeholder("...".into()).with_empty_span(),
                )];
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

    /// Parse and normalize `sql`. [`normalize`] / [`normalize_with_options`]
    /// are thin free-function wrappers around this.
    pub fn normalize(
        dialect: &dyn Dialect,
        sql: &str,
        options: NormalizerOptions,
    ) -> Result<Vec<String>, Error> {
        let mut statements = Parser::parse_sql(dialect, sql)?;
        let _ = statements.visit(&mut Self::new().with_options(options));
        Ok(statements
            .into_iter()
            .map(|statement| statement.to_string())
            .collect::<Vec<String>>())
    }

    /// Whether `expr` is a literal `Value`, or a chain of unary ops bottoming
    /// out in one (`-9`, `- -9`, `+ -9`). Such a chain collapses to a single
    /// `?`; a parenthesised operand (`Expr::Nested`) is *not* a chain, so it
    /// stops the recursion and its inner value is placeholdered separately.
    fn is_unary_chain_over_value(expr: &Expr) -> bool {
        match expr {
            Expr::Value(_) => true,
            Expr::UnaryOp { expr: child, .. } => Self::is_unary_chain_over_value(child),
            _ => false,
        }
    }

    /// Check if an expression contains only tuples of constants, recursively.
    fn contains_only_tuples_of_values(expr: &Expr) -> bool {
        match expr {
            Expr::Value(_) => true,
            Expr::Tuple(v) => v.iter().all(Self::contains_only_tuples_of_values),
            _ => false,
        }
    }
}

/// Replace a `SELECT TOP 10`-style constant quantity with a `?` placeholder
/// (rendered `TOP ?`), recursing into set-operation branches. A bare
/// `TopQuantity::Constant(u64)` isn't a `Value`, so the visitor's literal pass
/// misses it; `TopQuantity::Expr` is already normalized by that pass.
fn normalize_top(body: &mut SetExpr) {
    match body {
        SetExpr::Select(select) => {
            if let Some(top) = &mut select.top {
                if matches!(top.quantity, Some(TopQuantity::Constant(_))) {
                    top.quantity = Some(TopQuantity::Expr(Expr::Value(
                        Value::Placeholder("?".into()).with_empty_span(),
                    )));
                }
            }
        }
        SetExpr::SetOperation { left, right, .. } => {
            normalize_top(left);
            normalize_top(right);
        }
        _ => {}
    }
}
