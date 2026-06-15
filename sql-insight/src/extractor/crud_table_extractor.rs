//! CRUD-bucketed table extraction. See [`extract_crud_tables`] as
//! the entry point.
//!
//! Buckets the tables touched by a statement into the four CRUD
//! positions (Create / Read / Update / Delete). For finer detail —
//! keeping the precise verb (Insert / Update / Delete / Merge),
//! separating reads from writes, and per-statement lineage — see
//! [`extract_table_operations`](crate::extractor::extract_table_operations).

use std::fmt;

use crate::casing::IdentifierCasing;
use crate::diagnostic::TableLevelDiagnostic;
use crate::error::Error;
use crate::extractor::{StatementKind, TableOperationExtractor};
use crate::reference::TableReference;
use sqlparser::ast::{MergeAction, Statement};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Parse `sql` under `dialect` and return one [`CrudTables`] per
/// statement.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
///
/// let dialect = GenericDialect {};
/// let sql = "INSERT INTO t1 (a) SELECT a FROM t2";
/// let result = sql_insight::extractor::extract_crud_tables(&dialect, sql).unwrap();
/// println!("{:#?}", result);
/// assert_eq!(result[0].as_ref().unwrap().to_string(), "Create: [t1], Read: [t2], Update: [], Delete: []");
/// ```
pub fn extract_crud_tables(
    dialect: &dyn Dialect,
    sql: &str,
) -> Result<Vec<Result<CrudTables, Error>>, Error> {
    CrudTableExtractor::extract(dialect, sql)
}

/// Per-statement output of [`extract_crud_tables`]: tables bucketed
/// by CRUD position plus non-fatal diagnostics. `Display` renders
/// `"Create: [...], Read: [...], Update: [...], Delete: [...]"`.
#[derive(Default, Debug, PartialEq)]
pub struct CrudTables {
    pub create_tables: Vec<TableReference>,
    pub read_tables: Vec<TableReference>,
    pub update_tables: Vec<TableReference>,
    pub delete_tables: Vec<TableReference>,
    /// Non-fatal diagnostics, forwarded from the underlying table-level
    /// extraction (only [`UnsupportedStatement`](crate::diagnostic::TableLevelDiagnosticKind::UnsupportedStatement)
    /// arises at this granularity).
    pub diagnostics: Vec<TableLevelDiagnostic>,
}

impl fmt::Display for CrudTables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Create: [{}], Read: [{}], Update: [{}], Delete: [{}]",
            TableReference::format_list(&self.create_tables),
            TableReference::format_list(&self.read_tables),
            TableReference::format_list(&self.update_tables),
            TableReference::format_list(&self.delete_tables),
        )
    }
}

/// Struct-style entry point. Equivalent to the free
/// [`extract_crud_tables`] function. A thin shim over
/// [`TableOperationExtractor`] that buckets `reads`/`writes` into the
/// CRUD positions and consults the AST only for MERGE clauses (whose
/// target placement depends on WHEN actions).
#[derive(Default, Debug)]
pub struct CrudTableExtractor;

impl CrudTableExtractor {
    /// Same as the free [`extract_crud_tables`] function — kept for
    /// users who prefer the struct-style API.
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
    ) -> Result<Vec<Result<CrudTables, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        let casing = IdentifierCasing::for_dialect(dialect);
        Ok(statements
            .iter()
            .map(|s| Self::extract_from_statement(s, casing))
            .collect())
    }

    fn extract_from_statement(
        statement: &Statement,
        casing: IdentifierCasing,
    ) -> Result<CrudTables, Error> {
        let ops = TableOperationExtractor::extract_from_statement(statement, None, casing)?;
        // CRUD buckets are identity-only — drop the per-read
        // `ResolutionKind` and keep the bare `TableReference`s.
        let reads: Vec<TableReference> = ops.reads.into_iter().map(|r| r.reference).collect();
        let writes = ops.writes;
        let diagnostics = ops.diagnostics;

        let mut crud = CrudTables {
            diagnostics,
            ..Default::default()
        };
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
            // Every touched table goes to `read_tables`, matching the
            // legacy catch-all behavior. Listed explicitly (rather
            // than `_ =>`) so a new `StatementKind` variant becomes a
            // compile error here and forces a placement decision.
            StatementKind::Select
            | StatementKind::CreateTable
            | StatementKind::CreateView
            | StatementKind::AlterTable
            | StatementKind::AlterView
            | StatementKind::Drop
            | StatementKind::Truncate
            | StatementKind::Unsupported => {
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
            use crate::diagnostic::TableLevelDiagnosticKind;
            use sqlparser::dialect::GenericDialect;
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
    }

    mod delete_statement {
        use crate::test_utils::{all_dialects_except, DialectName};

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
                diagnostics: vec![],
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
                diagnostics: vec![],
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
                diagnostics: vec![],
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
                diagnostics: vec![],
            })];
            assert_crud_table_extraction(sql, expected, all_dialects());
        }
    }
}
