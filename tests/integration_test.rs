use sql_insight::{self, CrudTables, MySqlDialect};

#[test]
fn test_normalize() {
    let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'";
    match sql_insight::normalize(&MySqlDialect {}, sql.into()) {
        Ok(result) => assert_eq!(
            result,
            ["SELECT a FROM t1 WHERE b = ? AND c IN (?, ?) AND d LIKE ?"]
        ),
        Err(error) => unreachable!("Should not have errored: {}", error),
    }
}

#[test]
fn test_extract_crud_tables() {
    let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
    match sql_insight::extract_crud_tables(&MySqlDialect {}, sql.into()) {
        Ok(result) => assert_eq!(
            result,
            CrudTables {
                create_tables: vec![],
                read_tables: vec!["t1".to_string(), "t2".to_string()],
                update_tables: vec![],
                delete_tables: vec![],
            }
        ),
        Err(error) => unreachable!("Should not have errored: {}", error),
    }
}
