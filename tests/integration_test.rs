#[cfg(test)]
mod tests {
    use sql_insight::CrudTables;
    use sql_insight::{TableReference, Tables};
    use sqlparser::dialect::MySqlDialect;

    #[test]
    fn test_format() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'";
        let result = sql_insight::format(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            ["SELECT a FROM t1 WHERE b = 1 AND c IN (2, 3) AND d LIKE '%foo'"]
        )
    }

    #[test]
    fn test_format_cli() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'";
        let result = sql_insight::format_cli("mysql", sql.into()).unwrap();
        assert_eq!(
            result,
            ["SELECT a FROM t1 WHERE b = 1 AND c IN (2, 3) AND d LIKE '%foo'"]
        )
    }

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
            vec![
                Ok(CrudTables {
                    create_tables: vec![],
                    read_tables: vec![TableReference {
                        catalog: None,
                        schema: None,
                        name: "t1".into(),
                        alias: None,
                    }],
                    update_tables: vec![],
                    delete_tables: vec![],
                }),
                Ok(CrudTables {
                    create_tables: vec![],
                    read_tables: vec![TableReference {
                        catalog: None,
                        schema: None,
                        name: "t2".into(),
                        alias: None,
                    }],
                    update_tables: vec![],
                    delete_tables: vec![],
                })
            ]
        )
    }

    #[test]
    fn test_extract_crud_tables_cli() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
        let result = sql_insight::extract_crud_tables_cli("mysql", sql.into()).unwrap();
        assert_eq!(
            result,
            vec![
                Ok(CrudTables {
                    create_tables: vec![],
                    read_tables: vec![TableReference {
                        catalog: None,
                        schema: None,
                        name: "t1".into(),
                        alias: None,
                    }],
                    update_tables: vec![],
                    delete_tables: vec![],
                }),
                Ok(CrudTables {
                    create_tables: vec![],
                    read_tables: vec![TableReference {
                        catalog: None,
                        schema: None,
                        name: "t2".into(),
                        alias: None,
                    }],
                    update_tables: vec![],
                    delete_tables: vec![],
                })
            ]
        )
    }

    #[test]
    fn test_extract_tables() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
        let result = sql_insight::extract_tables(&MySqlDialect {}, sql.into()).unwrap();
        assert_eq!(
            result,
            vec![
                Ok(Tables(vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                }])),
                Ok(Tables(vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t2".into(),
                    alias: None,
                }]))
            ]
        )
    }

    #[test]
    fn test_extract_tables_cli() {
        let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
        let result = sql_insight::extract_tables_cli("mysql", sql.into()).unwrap();
        assert_eq!(
            result,
            vec![
                Ok(Tables(vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t1".into(),
                    alias: None,
                }])),
                Ok(Tables(vec![TableReference {
                    catalog: None,
                    schema: None,
                    name: "t2".into(),
                    alias: None,
                }])),
            ]
        )
    }
}
