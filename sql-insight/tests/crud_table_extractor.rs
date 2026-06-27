use sql_insight::error::Error;
use sql_insight::extractor::*;
use sql_insight::sqlparser::dialect::{Dialect, MySqlDialect};
use sql_insight::test_utils::all_dialects;
use sql_insight::*;

fn assert_crud_table_extraction(
    sql: &str,
    expected: Vec<Result<CrudTables, Error>>,
    dialects: Vec<Box<dyn Dialect>>,
) {
    for dialect in dialects {
        let result = CrudTableExtractor::extract(dialect.as_ref(), sql)
            .unwrap_or_else(|_| panic!("parse failed for dialect: {dialect:?}"));
        assert_eq!(result, expected, "Failed for dialect: {dialect:?}")
    }
}

fn table(name: &str) -> TableReference {
    TableReference {
        catalog: None,
        schema: None,
        name: name.into(),
    }
}

fn catalog_schema_table(catalog: &str, schema: &str, name: &str) -> TableReference {
    TableReference {
        catalog: Some(catalog.into()),
        schema: Some(schema.into()),
        name: name.into(),
    }
}

mod basic {
    use super::*;

    #[test]
    fn test_single_statement() {
        let sql = "SELECT a FROM t1";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![table("t1")],
            update_tables: vec![],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_multiple_statements() {
        let sql = "SELECT a FROM t1; SELECT b FROM t2";
        let expected = vec![
            Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![table("t1")],
                update_tables: vec![],
                delete_tables: vec![],
                diagnostics: vec![],
            }),
            Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![table("t2")],
                update_tables: vec![],
                delete_tables: vec![],
                diagnostics: vec![],
            }),
        ];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_alias() {
        let sql = "SELECT a FROM t1 AS t1_alias";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![table("t1")],
            update_tables: vec![],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_table_identifier() {
        let sql = "SELECT a FROM catalog.schema.table";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![catalog_schema_table("catalog", "schema", "table")],
            update_tables: vec![],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_table_identifier_and_alias() {
        let sql = "SELECT a FROM catalog.schema.table AS table_alias";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![catalog_schema_table("catalog", "schema", "table")],
            update_tables: vec![],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_statement_with_cte() {
        let sql = "WITH t2 AS (SELECT id FROM t1) SELECT * FROM t2";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![table("t1")],
            update_tables: vec![],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_too_many_identifiers_drops_target_with_diagnostic() {
        // Behavior change vs the legacy resolver: a target with more
        // segments than `catalog.schema.name` can't be represented as a
        // `TableReference`. The resolver hard-errored ("Too many
        // identifiers provided"); the bound-plan engine is best-effort,
        // so it drops the unrepresentable target (empty surfaces) and
        // flags a `TooManyTableQualifiers` diagnostic instead of failing
        // the whole statement.
        use sql_insight::diagnostic::TableLevelDiagnosticKind;
        use sql_insight::sqlparser::dialect::GenericDialect;
        let sql = "INSERT INTO catalog.schema.table.extra (a) VALUES (1)";
        let result = CrudTableExtractor::extract(&GenericDialect {}, sql).unwrap();
        let crud = result.into_iter().next().unwrap().unwrap();
        assert_eq!(crud.create_tables, vec![]);
        assert_eq!(crud.read_tables, vec![]);
        assert_eq!(crud.update_tables, vec![]);
        assert_eq!(crud.delete_tables, vec![]);
        assert_eq!(crud.diagnostics.len(), 1);
        assert_eq!(
            crud.diagnostics[0].kind,
            TableLevelDiagnosticKind::TooManyTableQualifiers
        );
    }

    #[test]
    fn test_non_identifier_table_name_drops_with_diagnostic_not_panic() {
        // Snowflake `IDENTIFIER('t')` parses to an `ObjectNamePart::Function`,
        // not a plain identifier — `try_from_name` once `unwrap`-panicked on
        // it. It must now drop the unrepresentable target best-effort with a
        // diagnostic (never panic), the same path as an over-qualified name.
        use sql_insight::diagnostic::TableLevelDiagnosticKind;
        use sql_insight::sqlparser::dialect::SnowflakeDialect;
        let sql = "SELECT a FROM IDENTIFIER('t1')";
        let result = CrudTableExtractor::extract(&SnowflakeDialect {}, sql).unwrap();
        let crud = result.into_iter().next().unwrap().unwrap();
        assert_eq!(crud.create_tables, vec![]);
        assert_eq!(crud.read_tables, vec![]);
        assert_eq!(crud.update_tables, vec![]);
        assert_eq!(crud.delete_tables, vec![]);
        assert_eq!(crud.diagnostics.len(), 1);
        assert_eq!(
            crud.diagnostics[0].kind,
            TableLevelDiagnosticKind::TooManyTableQualifiers
        );
    }
}

mod delete_statement {
    use sql_insight::test_utils::{all_dialects_except, DialectName};

    use super::*;

    #[test]
    fn test_delete_statement() {
        let sql = "DELETE FROM t1";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![],
            update_tables: vec![],
            delete_tables: vec![table("t1")],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_delete_statement_with_table_identifier() {
        let sql = "DELETE FROM catalog.schema.t1";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![],
            update_tables: vec![],
            delete_tables: vec![catalog_schema_table("catalog", "schema", "t1")],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_delete_statement_with_alias() {
        let sql = "DELETE FROM t1 AS t1_alias";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![],
            update_tables: vec![],
            delete_tables: vec![table("t1")],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_delete_multiple_tables_syntax() {
        let sql = "DELETE t1, t2 FROM t1 INNER JOIN t2 INNER JOIN t3";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![table("t1"), table("t2"), table("t3")],
            update_tables: vec![],
            delete_tables: vec![table("t1"), table("t2")],
            diagnostics: vec![],
        })];
        // BigQuery and Generic do not support DELETE ... FROM
        assert_crud_table_extraction(
            sql,
            expected,
            all_dialects_except(&[
                DialectName::Generic,
                DialectName::BigQuery,
                DialectName::Oracle,
            ]),
        );
    }

    #[test]
    fn test_delete_multiple_tables_syntax_with_alias() {
        let sql =
            "DELETE t1_alias, t2_alias FROM t1 AS t1_alias INNER JOIN t2 AS t2_alias INNER JOIN t3";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![table("t1"), table("t2"), table("t3")],
            update_tables: vec![],
            delete_tables: vec![table("t1"), table("t2")],
            diagnostics: vec![],
        })];
        // BigQuery and Generic do not support DELETE ... FROM
        assert_crud_table_extraction(
            sql,
            expected,
            all_dialects_except(&[
                DialectName::Generic,
                DialectName::BigQuery,
                DialectName::Oracle,
            ]),
        );
    }

    #[test]
    fn test_delete_multiple_tables_syntax_with_using() {
        let sql = "DELETE FROM t1, t2 USING t1 INNER JOIN t2 INNER JOIN t3";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![table("t1"), table("t2"), table("t3")],
            update_tables: vec![],
            delete_tables: vec![table("t1"), table("t2")],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_delete_multiple_tables_syntax_with_using_with_alias() {
        let sql = "DELETE FROM t1_alias, t2_alias USING t1 AS t1_alias INNER JOIN t2 AS t2_alias INNER JOIN t3";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![table("t1"), table("t2"), table("t3")],
            update_tables: vec![],
            delete_tables: vec![table("t1"), table("t2")],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_parenthesized_delete_keeps_the_delete_verb() {
        // A parenthesised DML `(DELETE …)` parses as a Query wrapping the DML;
        // it must keep its verb so the target buckets as Delete, not misclassify
        // as a read-only Select (which dropped the verb and the table-lineage).
        use sql_insight::sqlparser::dialect::GenericDialect;
        let sql = "(DELETE FROM t WHERE id = 1)";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            // `t.id` is read by the WHERE — the target's own data is consumed,
            // so it reads as well as deletes.
            read_tables: vec![table("t")],
            update_tables: vec![],
            delete_tables: vec![table("t")],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, vec![Box::new(GenericDialect {})]);
    }
}

mod insert_statement {
    use super::*;

    #[test]
    fn test_insert_statement() {
        let sql = "INSERT INTO t1 (a) VALUES (1)";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![table("t1")],
            read_tables: vec![],
            update_tables: vec![],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_insert_select_statement() {
        let sql = "INSERT INTO t1 (a) SELECT a FROM t2 AS t2_alias INNER JOIN t3 USING (id)";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![table("t1")],
            read_tables: vec![table("t2"), table("t3")],
            update_tables: vec![],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_parenthesized_insert_keeps_the_insert_verb() {
        // `(INSERT … SELECT …)` keeps its verb → target buckets as Create,
        // source as Read (the misclassification dropped both into a Select).
        use sql_insight::sqlparser::dialect::GenericDialect;
        let sql = "(INSERT INTO t (a) SELECT b FROM src)";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![table("t")],
            read_tables: vec![table("src")],
            update_tables: vec![],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, vec![Box::new(GenericDialect {})]);
    }

    #[test]
    fn test_upsert_do_update_buckets_target_as_create_and_update() {
        use sql_insight::sqlparser::dialect::GenericDialect;
        // `ON CONFLICT DO UPDATE` is an upsert: it both inserts and updates the
        // target, so `t1` lands in both the create and update buckets.
        let sql = "INSERT INTO t1 (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET b = 2";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![table("t1")],
            read_tables: vec![],
            update_tables: vec![table("t1")],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, vec![Box::new(GenericDialect {})]);
    }

    #[test]
    fn test_upsert_do_nothing_is_create_only() {
        use sql_insight::sqlparser::dialect::GenericDialect;
        // `ON CONFLICT DO NOTHING` performs no update — create-only.
        let sql = "INSERT INTO t1 (a) VALUES (1) ON CONFLICT (a) DO NOTHING";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![table("t1")],
            read_tables: vec![],
            update_tables: vec![],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, vec![Box::new(GenericDialect {})]);
    }

    #[test]
    fn test_mysql_on_duplicate_key_update_is_upsert() {
        // MySQL `ON DUPLICATE KEY UPDATE` is the same upsert shape: create + update.
        let sql = "INSERT INTO t1 (a) VALUES (1) ON DUPLICATE KEY UPDATE b = 2";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![table("t1")],
            read_tables: vec![],
            update_tables: vec![table("t1")],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, vec![Box::new(MySqlDialect {})]);
    }
}

mod update_statement {
    use super::*;

    #[test]
    fn test_update_statement() {
        let sql = "UPDATE t1 SET a=1";
        let result = CrudTableExtractor::extract(&MySqlDialect {}, sql).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![],
                update_tables: vec![table("t1")],
                delete_tables: vec![],
                diagnostics: vec![],
            }),]
        )
    }

    #[test]
    fn test_update_statement_with_alias() {
        // `t1` is updated AND a join source — `t1_alias.a` is read by the ON, so
        // its own data is consumed. So `t1` is both a read and an update; `t2`
        // and `t3` are read-only sources.
        let sql = "UPDATE t1 AS t1_alias INNER JOIN t2 ON t1_alias.a = t2.a SET t1_alias.b = t2.b WHERE t2.c = (SELECT c FROM t3)";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![table("t1"), table("t2"), table("t3")],
            update_tables: vec![table("t1")],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_multi_table_update_buckets_set_target_and_reads_sources() {
        use sql_insight::sqlparser::dialect::GenericDialect;
        // `UPDATE t1 JOIN t2 SET t2.b = t1.c`: only `t2` is updated (the SET
        // target's resolved table); both `t1` and `t2` are read (their `id`
        // columns are consumed by the ON).
        let sql = "UPDATE t1 JOIN t2 ON t1.id = t2.id SET t2.b = t1.c";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![table("t1"), table("t2")],
            update_tables: vec![table("t2")],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, vec![Box::new(GenericDialect {})]);
    }

    #[test]
    fn test_parenthesized_update_keeps_the_update_verb() {
        // `(UPDATE …)` keeps its verb → the target buckets as Update.
        use sql_insight::sqlparser::dialect::GenericDialect;
        let sql = "(UPDATE t SET a = 1)";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![],
            update_tables: vec![table("t")],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, vec![Box::new(GenericDialect {})]);
    }
}

mod merge {
    use super::*;
    use sql_insight::sqlparser::dialect::GenericDialect;

    #[test]
    fn test_with_merge_unwraps_query_wrapper_to_bucket_target() {
        // `WITH … MERGE` parses as `Statement::Query(SetExpr::Merge(..))`. The
        // bucketing must unwrap that to reach the WHEN actions — otherwise the
        // target silently falls out of every bucket (the bug this guards).
        let sql = "WITH s AS (SELECT id, v FROM src) \
                   MERGE INTO tgt USING s ON tgt.id = s.id \
                   WHEN MATCHED THEN UPDATE SET v = s.v";
        let result = CrudTableExtractor::extract(&GenericDialect {}, sql).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                // `tgt.id` is read by the ON (to match), so the target reads as
                // well as updates; `src` is the read source.
                create_tables: vec![],
                read_tables: vec![table("src"), table("tgt")],
                update_tables: vec![table("tgt")],
                delete_tables: vec![],
                diagnostics: vec![],
            })]
        );
    }

    #[test]
    fn test_merge_insert_row_buckets_target_as_create() {
        // BigQuery `WHEN NOT MATCHED THEN INSERT ROW`: the target is an insert
        // target (Create), the source a Read. Regression — the unhandled ROW
        // kind left `has_insert` false, so the target fell out of every bucket.
        // The column-level coverage gap is flagged on the column surface only,
        // so no table-level diagnostic surfaces here.
        let sql = "MERGE INTO tgt USING src ON tgt.id = src.id WHEN NOT MATCHED THEN INSERT ROW";
        let result = CrudTableExtractor::extract(&GenericDialect {}, sql).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                // `tgt.id` is read by the ON, so the target reads as well as
                // (insert-)creates; `src` is the read source.
                create_tables: vec![table("tgt")],
                read_tables: vec![table("tgt"), table("src")],
                update_tables: vec![],
                delete_tables: vec![],
                diagnostics: vec![],
            })]
        );
    }

    #[test]
    fn test_merge_statement() {
        let sql = "MERGE INTO t1 AS t1_alias USING t2 AS t2_alias ON t1_alias.a = t2_alias.a \
                     WHEN MATCHED AND t2_alias.b = 1 THEN DELETE \
                     WHEN MATCHED AND t2_alias.b = 2 THEN UPDATE SET t1_alias.b = t2_alias.b \
                     WHEN NOT MATCHED THEN INSERT (a, b) VALUES (t2_alias.a, t2_alias.b)";
        let expected = vec![Ok(CrudTables {
            // `t1_alias.a` is read by the ON, so the target `t1` reads as well
            // as create/update/delete; `t2` is the read source.
            create_tables: vec![table("t1")],
            read_tables: vec![table("t1"), table("t2")],
            update_tables: vec![table("t1")],
            delete_tables: vec![table("t1")],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }
}

mod ddl {
    //! DDL write targets bucket by verb, not into `Read`: CREATE → Create,
    //! ALTER → Update, DROP / TRUNCATE → Delete. A CTAS / CREATE-VIEW source
    //! still feeds Read. (Bucketing is dialect-independent, so the source-only
    //! cases use a single dialect; the universal CREATE / ALTER TABLE forms
    //! stay on the full dialect matrix.)
    use super::*;
    use sql_insight::sqlparser::dialect::GenericDialect;

    #[test]
    fn test_create_table_buckets_as_create() {
        let sql = "CREATE TABLE t1 (a INT)";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![table("t1")],
            read_tables: vec![],
            update_tables: vec![],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_create_table_as_select_buckets_target_as_create_source_as_read() {
        // CTAS: the new table is a Create, the SELECT source a Read.
        let sql = "CREATE TABLE new AS SELECT a FROM src";
        let result = CrudTableExtractor::extract(&GenericDialect {}, sql).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![table("new")],
                read_tables: vec![table("src")],
                update_tables: vec![],
                delete_tables: vec![],
                diagnostics: vec![],
            })]
        );
    }

    #[test]
    fn test_create_view_buckets_target_as_create_source_as_read() {
        let sql = "CREATE VIEW v AS SELECT a FROM src";
        let result = CrudTableExtractor::extract(&GenericDialect {}, sql).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![table("v")],
                read_tables: vec![table("src")],
                update_tables: vec![],
                delete_tables: vec![],
                diagnostics: vec![],
            })]
        );
    }

    #[test]
    fn test_alter_table_buckets_as_update() {
        let sql = "ALTER TABLE t1 ADD COLUMN a INT";
        let expected = vec![Ok(CrudTables {
            create_tables: vec![],
            read_tables: vec![],
            update_tables: vec![table("t1")],
            delete_tables: vec![],
            diagnostics: vec![],
        })];
        assert_crud_table_extraction(sql, expected, all_dialects());
    }

    #[test]
    fn test_alter_view_buckets_target_as_update_source_as_read() {
        // ALTER VIEW redefines an existing object → Update; its new SELECT
        // body still supplies the Read source.
        let sql = "ALTER VIEW v AS SELECT a FROM src";
        let result = CrudTableExtractor::extract(&GenericDialect {}, sql).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![table("src")],
                update_tables: vec![table("v")],
                delete_tables: vec![],
                diagnostics: vec![],
            })]
        );
    }

    #[test]
    fn test_drop_table_buckets_as_delete() {
        // The destructive verb must not read as a harmless Read.
        let sql = "DROP TABLE doomed";
        let result = CrudTableExtractor::extract(&GenericDialect {}, sql).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![],
                update_tables: vec![],
                delete_tables: vec![table("doomed")],
                diagnostics: vec![],
            })]
        );
    }

    #[test]
    fn test_truncate_table_buckets_as_delete() {
        let sql = "TRUNCATE TABLE logs";
        let result = CrudTableExtractor::extract(&GenericDialect {}, sql).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![],
                update_tables: vec![],
                delete_tables: vec![table("logs")],
                diagnostics: vec![],
            })]
        );
    }

    #[test]
    fn test_select_into_buckets_target_as_create_source_as_read() {
        // `SELECT … INTO new_t` classifies as `StatementKind::CreateTable`
        // (the binder lowers it to a CTAS), so the created table is a Create
        // (not a Read) and the source a Read — a plain `SELECT` writes nothing,
        // keeping Create empty.
        let sql = "SELECT a INTO new_t FROM src";
        let result = CrudTableExtractor::extract(&GenericDialect {}, sql).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![table("new_t")],
                read_tables: vec![table("src")],
                update_tables: vec![],
                delete_tables: vec![],
                diagnostics: vec![],
            })]
        );
    }

    #[test]
    fn test_select_into_over_union_creates_one_table_from_both_branches() {
        // `INTO` rides the leading SELECT but targets the whole query: over a
        // UNION it creates one table fed by both branches. The create surfaces
        // (it used to vanish from the write surfaces while the flat table list
        // still showed it).
        let sql = "SELECT a INTO new_t FROM src UNION SELECT b FROM other";
        let result = CrudTableExtractor::extract(&GenericDialect {}, sql).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![table("new_t")],
                read_tables: vec![table("src"), table("other")],
                update_tables: vec![],
                delete_tables: vec![],
                diagnostics: vec![],
            })]
        );
    }

    #[test]
    fn test_select_into_nested_in_cte_body_is_ignored() {
        // `INTO` is only a create at the statement root; nested in a CTE body
        // (not valid SQL there) it's ignored rather than leaking a mid-tree
        // create that only some surfaces would see. `t1` appears nowhere.
        let sql = "WITH x AS (SELECT a INTO t1 FROM s) SELECT * FROM x";
        let result = CrudTableExtractor::extract(&GenericDialect {}, sql).unwrap();
        assert_eq!(
            result,
            vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![table("s")],
                update_tables: vec![],
                delete_tables: vec![],
                diagnostics: vec![],
            })]
        );
    }
}
