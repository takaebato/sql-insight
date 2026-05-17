//! A Extractor that extracts CRUD tables from SQL queries.
//!
//! See [`extract_crud_tables`](crate::extract_crud_tables()) as the entry point for extracting CRUD tables from SQL.

use std::fmt;

use crate::error::Error;
use crate::relation::TableReference;
use crate::{StatementKind, TableOperationExtractor};
use sqlparser::ast::{MergeAction, Statement};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to extract CRUD tables from SQL.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
///
/// let dialect = GenericDialect {};
/// let sql = "INSERT INTO t1 (a) SELECT a FROM t2";
/// let result = sql_insight::extract_crud_tables(&dialect, sql).unwrap();
/// println!("{:#?}", result);
/// assert_eq!(result[0].as_ref().unwrap().to_string(), "Create: [t1], Read: [t2], Update: [], Delete: []");
/// ```
pub fn extract_crud_tables(
    dialect: &dyn Dialect,
    sql: &str,
) -> Result<Vec<Result<CrudTables, Error>>, Error> {
    CrudTableExtractor::extract(dialect, sql)
}

/// [`CrudTables`] represents the tables involved in CRUD operations.
#[derive(Default, Debug, PartialEq)]
pub struct CrudTables {
    pub create_tables: Vec<TableReference>,
    pub read_tables: Vec<TableReference>,
    pub update_tables: Vec<TableReference>,
    pub delete_tables: Vec<TableReference>,
}

impl fmt::Display for CrudTables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let create_tables = self.format_tables(&self.create_tables);
        let read_tables = self.format_tables(&self.read_tables);
        let update_tables = self.format_tables(&self.update_tables);
        let delete_tables = self.format_tables(&self.delete_tables);
        write!(
            f,
            "Create: [{}], Read: [{}], Update: [{}], Delete: [{}]",
            create_tables, read_tables, update_tables, delete_tables
        )
    }
}

impl CrudTables {
    fn format_tables(&self, tables: &[TableReference]) -> String {
        tables
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<String>>()
            .join(", ")
    }
}

/// Extracts CRUD tables from SQL. A thin shim over
/// [`TableOperationExtractor`] that buckets `reads`/`writes` into the
/// CRUD positions and consults the AST only for MERGE clauses (whose
/// target placement depends on WHEN actions).
#[derive(Default, Debug)]
pub struct CrudTableExtractor;

impl CrudTableExtractor {
    /// Extract CRUD tables from SQL.
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
    ) -> Result<Vec<Result<CrudTables, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        Ok(statements
            .iter()
            .map(Self::extract_from_statement)
            .collect())
    }

    fn extract_from_statement(statement: &Statement) -> Result<CrudTables, Error> {
        let ops = TableOperationExtractor::extract_from_statement(statement, None)?;
        let reads: Vec<_> = ops.reads.into_iter().map(|r| r.table).collect();
        let writes: Vec<_> = ops.writes.into_iter().map(|w| w.table).collect();

        let mut crud = CrudTables::default();
        match ops.statement_kind {
            StatementKind::Insert => {
                crud.create_tables = writes;
                crud.read_tables = reads;
            }
            StatementKind::Update => {
                crud.update_tables = writes;
                crud.read_tables = reads;
            }
            StatementKind::Delete => {
                crud.delete_tables = writes;
                crud.read_tables = reads;
            }
            StatementKind::Merge => {
                // MERGE target placement depends on which WHEN actions
                // appear; reach into the AST for that one detail. The
                // source comes from `reads` directly.
                if let Statement::Merge(merge) = statement {
                    let (mut inserted, mut updated, mut deleted) = (false, false, false);
                    for clause in &merge.clauses {
                        match &clause.action {
                            MergeAction::Insert(_) => inserted = true,
                            MergeAction::Update { .. } => updated = true,
                            MergeAction::Delete { .. } => deleted = true,
                        }
                    }
                    for target in &writes {
                        if inserted {
                            crud.create_tables.push(target.clone());
                        }
                        if updated {
                            crud.update_tables.push(target.clone());
                        }
                        if deleted {
                            crud.delete_tables.push(target.clone());
                        }
                    }
                }
                crud.read_tables = reads;
            }
            // SELECT, CreateTable, CreateView, AlterTable, AlterView,
            // Drop, Truncate, Unsupported — every touched table goes to
            // read_tables, matching the legacy catch-all behavior.
            _ => {
                crud.read_tables = reads;
                crud.read_tables.extend(writes);
            }
        }

        Ok(crud)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::all_dialects;
    use sqlparser::dialect::MySqlDialect;

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
                }),
                Ok(CrudTables {
                    create_tables: vec![],
                    read_tables: vec![table("t2")],
                    update_tables: vec![],
                    delete_tables: vec![],
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
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_statement_error_with_too_many_identifiers() {
            let sql = "INSERT INTO catalog.schema.table.extra (a) VALUES (1)";
            let expected = vec![Err(Error::AnalysisError(
                "Too many identifiers provided".to_string(),
            ))];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }
    }

    mod delete_statement {
        use crate::test_utils::all_dialects_except;

        use super::*;

        #[test]
        fn test_delete_statement() {
            let sql = "DELETE FROM t1";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![],
                update_tables: vec![],
                delete_tables: vec![table("t1")],
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
            })];
            // BigQuery and Generic do not support DELETE ... FROM
            assert_crud_table_extraction(
                sql,
                expected,
                all_dialects_except(&vec!["GenericDialect", "BigQueryDialect"]),
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
            })];
            // BigQuery and Generic do not support DELETE ... FROM
            assert_crud_table_extraction(
                sql,
                expected,
                all_dialects_except(&vec!["GenericDialect", "BigQueryDialect"]),
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
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
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
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
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
                }),]
            )
        }

        #[test]
        fn test_update_statement_with_alias() {
            // Behavior change vs the legacy implementation: joined tables
            // (`t2` here) are now classified as `read_tables` rather than
            // bundled into `update_tables`. This matches the SQL semantics
            // — only `t1` is being updated; `t2` is a join partner.
            let sql = "UPDATE t1 AS t1_alias INNER JOIN t2 ON t1_alias.a = t2.a SET t1_alias.b = t2.b WHERE t2.c = (SELECT c FROM t3)";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![table("t2"), table("t3")],
                update_tables: vec![table("t1")],
                delete_tables: vec![],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }
    }

    mod merge {
        use super::*;

        #[test]
        fn test_merge_statement() {
            let sql = "MERGE INTO t1 AS t1_alias USING t2 AS t2_alias ON t1_alias.a = t2_alias.a \
                         WHEN MATCHED AND t2_alias.b = 1 THEN DELETE \
                         WHEN MATCHED AND t2_alias.b = 2 THEN UPDATE SET t1_alias.b = t2_alias.b \
                         WHEN NOT MATCHED THEN INSERT (a, b) VALUES (t2_alias.a, t2_alias.b)";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![table("t1")],
                read_tables: vec![table("t2")],
                update_tables: vec![table("t1")],
                delete_tables: vec![table("t1")],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }
    }

    mod ddl {
        use super::*;

        #[test]
        fn test_create_table_statement() {
            let sql = "CREATE TABLE t1 (a INT)";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![table("t1")],
                update_tables: vec![],
                delete_tables: vec![],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }

        #[test]
        fn test_alters_table_statement() {
            let sql = "ALTER TABLE t1 ADD COLUMN a INT";
            let expected = vec![Ok(CrudTables {
                create_tables: vec![],
                read_tables: vec![table("t1")],
                update_tables: vec![],
                delete_tables: vec![],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }
    }
}
