use sql_insight::{CrudTables, Tables};
use sqlparser::dialect::MySqlDialect;
use std::collections::HashMap;

#[test]
fn test_normalize() {
    let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'";
    let result = sql_insight::normalize(&MySqlDialect {}, sql.into()).unwrap();
    assert_eq!(
        result,
        ["SELECT a FROM t1 WHERE b = ? AND c IN (?, ?) AND d LIKE ?"]
    )
}

#[test]
fn test_normalize_cli() {
    let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'";
    let result = sql_insight::normalize_cli("mysql", sql.into()).unwrap();
    assert_eq!(
        result,
        ["SELECT a FROM t1 WHERE b = ? AND c IN (?, ?) AND d LIKE ?"]
    )
}

#[test]
fn test_extract_crud_tables() {
    let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
    let result = sql_insight::extract_crud_tables(&MySqlDialect {}, sql.into()).unwrap();
    assert_eq!(
        result,
        CrudTables {
            create_tables: vec![],
            read_tables: vec!["t1".to_string(), "t2".to_string()],
            update_tables: vec![],
            delete_tables: vec![],
        }
    )
}

#[test]
fn test_extract_crud_tables_cli() {
    let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
    let result = sql_insight::extract_crud_tables_cli("mysql", sql.into()).unwrap();
    assert_eq!(
        result,
        CrudTables {
            create_tables: vec![],
            read_tables: vec!["t1".to_string(), "t2".to_string()],
            update_tables: vec![],
            delete_tables: vec![],
        }
    )
}

#[test]
fn test_extract_tables() {
    let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
    let result = sql_insight::extract_tables(&MySqlDialect {}, sql.into()).unwrap();
    assert_eq!(
        result,
        Tables {
            tables: vec!["t1".to_string(), "t2".to_string()],
            aliases: HashMap::new(),
        }
    )
}

#[test]
fn test_extract_tables_cli() {
    let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
    let result = sql_insight::extract_tables_cli("mysql", sql.into()).unwrap();
    assert_eq!(
        result,
        Tables {
            tables: vec!["t1".to_string(), "t2".to_string()],
            aliases: HashMap::new(),
        }
    )
}
