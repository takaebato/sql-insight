use sql_insight::normalizer::*;
use sql_insight::sqlparser::dialect::Dialect;
use sql_insight::test_utils::{all_dialects, all_dialects_except, DialectName};

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
    let expected =
        vec!["SELECT a FROM t1 WHERE b = ? AND c IN (?, (SELECT * FROM b)) AND d LIKE ?".into()];
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
    let sql = "SELECT * FROM t1 WHERE a=-9 AND b=+ 9 AND c IS NULL";
    let expected = vec!["SELECT * FROM t1 WHERE a = ? AND b = ? AND c IS NULL".into()];
    assert_normalize(sql, expected, all_dialects(), NormalizerOptions::new());
}

#[test]
fn test_nested_unary_operators_collapse_to_single_placeholder() {
    // A chain of unary ops over a literal collapses to one `?` (not `-?`), so
    // `- -9` matches `-9` and `9`. A parenthesised operand (`Expr::Nested`)
    // stops the chain — only its inner value is placeholdered.
    let sql = "SELECT * FROM t WHERE a = - -9 AND b = + -9 AND c = -(9)";
    let expected = vec!["SELECT * FROM t WHERE a = ? AND b = ? AND c = -(?)".into()];
    assert_normalize(sql, expected, all_dialects(), NormalizerOptions::new());
}

#[test]
fn test_unary_operators_preceding_booleans() {
    let sql = "SELECT * FROM t1 WHERE a=TRUE AND b=NOT TRUE AND c=NOT(TRUE)";
    let expected = vec!["SELECT * FROM t1 WHERE a = ? AND b = ? AND c = NOT (?)".into()];
    // The MsSQL / Oracle parsers consider "TRUE" and "FALSE" to be identifiers rather than constants
    assert_normalize(
        sql,
        expected,
        all_dialects_except(&[DialectName::MsSql, DialectName::Oracle]),
        NormalizerOptions::new(),
    );
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
fn test_sql_with_in_list_with_unify_in_list_option_with_tuples() {
    let sql = "SELECT a FROM t1 WHERE b = 1 AND (c, d) in ((2, 'a'), (3, 'b'), NULL)";
    let expected = vec!["SELECT a FROM t1 WHERE b = ? AND (c, d) IN (...)".into()];
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
    let expected = vec!["INSERT INTO t1 (a, b, c) VALUES (?, ?, ?), (?, ?, ?), (?, ?, ?)".into()];
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
    let sql =
        "INSERT INTO t1 (a, b, c) VALUES (1, 2, 3), (4, 5, 6), (7, (SELECT * FROM t2 WHERE d = 9))";
    let expected = vec![
        "INSERT INTO t1 (a, b, c) VALUES (?, ?, ?), (?, ?, ?), (?, (SELECT * FROM t2 WHERE d = ?))"
            .into(),
    ];
    assert_normalize(
        sql,
        expected,
        all_dialects(),
        NormalizerOptions::new().with_unify_values(true),
    );
}

#[test]
fn test_alphabetize_insert_columns() {
    let sql = "INSERT INTO t1 (c, b, a) VALUES (1, 2, 3)";
    let expected = vec!["INSERT INTO t1 (a, b, c) VALUES (...)".into()];
    assert_normalize(
        sql,
        expected,
        all_dialects(),
        NormalizerOptions::new()
            .with_unify_values(true)
            .with_alphabetize_insert_columns(true),
    );
}

#[test]
fn test_do_not_alphabetize_insert_columns_when_values_not_unified() {
    let sql = "INSERT INTO t1 (c, b, a) SELECT x, y, z FROM t2";
    let expected = vec!["INSERT INTO t1 (c, b, a) SELECT x, y, z FROM t2".into()];
    assert_normalize(
        sql,
        expected,
        all_dialects(),
        NormalizerOptions::new()
            .with_unify_values(true)
            .with_alphabetize_insert_columns(true),
    );
}

#[test]
fn test_typed_string_literal_value_is_normalized() {
    // A `DATE` / `TIMESTAMP '…'` literal is a `TypedString`, whose value is a
    // bare `Value` field (not an `Expr::Value`) — it must still normalize so
    // queries differing only in the date / timestamp collapse together.
    let sql = "SELECT * FROM t WHERE d = DATE '2020-01-01' AND ts > TIMESTAMP '2020-01-01 00:00:00'";
    let expected = vec!["SELECT * FROM t WHERE d = DATE ? AND ts > TIMESTAMP ?".into()];
    assert_normalize(sql, expected, all_dialects(), NormalizerOptions::new());
}

#[test]
fn test_like_escape_char_is_normalized() {
    // The `ESCAPE '!'` char is a bare `Value` on the `Like` node, alongside
    // the (already-normalized) pattern expression.
    let sql = "SELECT a FROM t WHERE c LIKE '%x%' ESCAPE '!'";
    let expected = vec!["SELECT a FROM t WHERE c LIKE ? ESCAPE ?".into()];
    assert_normalize(sql, expected, all_dialects(), NormalizerOptions::new());
}

#[test]
fn test_match_against_search_value_is_normalized() {
    // MySQL `MATCH(col) AGAINST ('…')`: the search string is a bare `Value`
    // on the `MatchAgainst` node.
    let sql = "SELECT MATCH(title, body) AGAINST ('foo') FROM t";
    let expected = vec!["SELECT MATCH (title, body) AGAINST (?) FROM t".into()];
    assert_normalize(
        sql,
        expected,
        vec![Box::new(sql_insight::sqlparser::dialect::MySqlDialect {})],
        NormalizerOptions::new(),
    );
}
