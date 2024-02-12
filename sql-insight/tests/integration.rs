#[cfg(test)]
mod integration {
    use sql_insight::test_utils::all_dialects;
    use sql_insight::{CrudTables, NormalizerOptions};
    use sql_insight::{TableReference, Tables};

    mod format {
        use super::*;

        #[test]
        fn test_format() {
            let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'";
            for dialect in all_dialects() {
                let result = sql_insight::format(dialect.as_ref(), sql).unwrap();
                assert_eq!(
                    result,
                    ["SELECT a FROM t1 WHERE b = 1 AND c IN (2, 3) AND d LIKE '%foo'"],
                    "Failed for dialect: {dialect:?}"
                )
            }
        }
    }

    mod normalize {
        use super::*;

        #[test]
        fn test_normalize() {
            let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'";
            for dialect in all_dialects() {
                let result =
                    sql_insight::normalize(dialect.as_ref(), sql, NormalizerOptions::new())
                        .unwrap();
                assert_eq!(
                    result,
                    ["SELECT a FROM t1 WHERE b = ? AND c IN (?, ?) AND d LIKE ?"],
                    "Failed for dialect: {dialect:?}"
                )
            }
        }
    }

    mod extract_crud_tables {
        use super::*;

        #[test]
        fn test_extract_crud_tables() {
            let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
            for dialect in all_dialects() {
                let result = sql_insight::extract_crud_tables(dialect.as_ref(), sql).unwrap();
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
                        }),
                    ],
                    "Failed for dialect: {dialect:?}"
                )
            }
        }
    }

    mod extract_tables {
        use super::*;

        #[test]
        fn test_extract_tables() {
            let sql = "SELECT a FROM t1 WHERE b = 1 AND c in (2, 3) AND d LIKE '%foo'; SELECT b FROM t2 WHERE c = 4";
            for dialect in all_dialects() {
                let result = sql_insight::extract_tables(dialect.as_ref(), sql).unwrap();
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
                    ],
                    "Failed for dialect: {dialect:?}"
                )
            }
        }
    }
}
