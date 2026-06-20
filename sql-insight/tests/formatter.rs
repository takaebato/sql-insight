use sql_insight::formatter::*;
use sql_insight::sqlparser::dialect::Dialect;
use sql_insight::test_utils::all_dialects;

fn assert_format(sql: &str, expected: Vec<String>, dialects: Vec<Box<dyn Dialect>>) {
    for dialect in dialects {
        let result = Formatter::format(dialect.as_ref(), sql, FormatterOptions::default()).unwrap();
        assert_eq!(result, expected, "Failed for dialect: {dialect:?}")
    }
}

#[test]
fn test_single_sql() {
    let sql = "SELECT a from t1   WHERE b=1 AND c in (2, (select * from b))\n  AND d LIKE '%foo'";
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
    let sql =
        "SELECT a FROM t1 WHERE b = 1; -- comment\nSELECT b FROM t2 WHERE c =  2  /* comment */";
    let expected = vec![
        "SELECT a FROM t1 WHERE b = 1".into(),
        "SELECT b FROM t2 WHERE c = 2".into(),
    ];
    assert_format(sql, expected, all_dialects());
}

#[test]
fn test_pretty_print_select() {
    let result = format_with_options(
        &sql_insight::sqlparser::dialect::GenericDialect {},
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
        &sql_insight::sqlparser::dialect::GenericDialect {},
        "SELECT a, b FROM t1",
    )
    .unwrap();
    let via_options = format_with_options(
        &sql_insight::sqlparser::dialect::GenericDialect {},
        "SELECT a, b FROM t1",
        FormatterOptions::default(),
    )
    .unwrap();
    assert_eq!(single, via_options);
}
