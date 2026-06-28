//! CRUD-bucketed table extraction. See [`extract_crud_tables`] as
//! the entry point.
//!
//! Buckets the tables touched by a statement into the four CRUD
//! positions (Create / Read / Update / Delete). For finer detail —
//! keeping the precise verb (Insert / Update / Delete / Merge),
//! separating reads from writes, and per-statement lineage — see
//! [`extract_table_operations`](crate::extractor::extract_table_operations).
//!
//! Write targets bucket by verb, so DDL lands where its effect is, not in
//! `Read`: `INSERT`, `CREATE TABLE` / `CREATE VIEW`, and `SELECT … INTO` →
//! Create (an upsert `INSERT … ON CONFLICT DO UPDATE` / `ON DUPLICATE KEY
//! UPDATE` also → Update; `REPLACE INTO` / `INSERT OVERWRITE`, which delete the
//! conflicting / existing rows first, also → Delete), `UPDATE` and `ALTER` →
//! Update, `DELETE` / `DROP` / `TRUNCATE` → Delete, and `MERGE` to each bucket
//! its WHEN actions imply. A
//! statement's
//! read-role tables (a `SELECT`, a CTAS / view source, an `UPDATE … FROM`)
//! always go to Read. A `WITH … <DML>` parses as a `Query`-wrapped DML, so
//! the verb is recovered through that wrapper.

use std::fmt;

use crate::casing::IdentifierStyle;
use crate::catalog::Catalog;
use crate::diagnostic::TableLevelDiagnostic;
use crate::error::Error;
use crate::extractor::{ExtractorOptions, StatementKind, TableOperationExtractor};
use crate::reference::{TableRead, TableReference, TableWrite};
use sqlparser::ast::{Insert, SetExpr, Statement};
use sqlparser::dialect::Dialect;

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

/// Like [`extract_crud_tables`] but with [`ExtractorOptions`] — a catalog
/// and/or an identifier-casing override. With a catalog, the bucketed
/// tables are canonicalized to their registered path.
pub fn extract_crud_tables_with_options(
    dialect: &dyn Dialect,
    sql: &str,
    options: ExtractorOptions,
) -> Result<Vec<Result<CrudTables, Error>>, Error> {
    CrudTableExtractor::extract_with_options(dialect, sql, options)
}

/// Per-statement output of [`extract_crud_tables`]: tables bucketed
/// by CRUD position plus non-fatal diagnostics. `Display` renders
/// `"Create: [...], Read: [...], Update: [...], Delete: [...]"`.
#[derive(Default, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct CrudTables {
    /// Tables created (`INSERT` / `CREATE` / a MERGE INSERT action), each paired
    /// with its catalog-match [`ResolutionKind`](crate::ResolutionKind).
    pub create_tables: Vec<TableWrite>,
    /// Tables read, each paired with its [`ResolutionKind`](crate::ResolutionKind).
    pub read_tables: Vec<TableRead>,
    /// Tables updated (`UPDATE` / `ALTER` / a MERGE UPDATE action / an upsert).
    pub update_tables: Vec<TableWrite>,
    /// Tables deleted (`DELETE` / `DROP` / `TRUNCATE` / a MERGE DELETE action).
    pub delete_tables: Vec<TableWrite>,
    /// Non-fatal diagnostics, forwarded from the underlying table-level
    /// extraction (only [`UnsupportedStatement`](crate::diagnostic::TableLevelDiagnosticKind::UnsupportedStatement)
    /// arises at this granularity).
    pub diagnostics: Vec<TableLevelDiagnostic>,
}

impl fmt::Display for CrudTables {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let write_refs = |ws: &[TableWrite]| -> Vec<TableReference> {
            ws.iter().map(|w| w.reference.clone()).collect()
        };
        let read_refs = |rs: &[TableRead]| -> Vec<TableReference> {
            rs.iter().map(|r| r.reference.clone()).collect()
        };
        write!(
            f,
            "Create: [{}], Read: [{}], Update: [{}], Delete: [{}]",
            TableReference::format_list(&write_refs(&self.create_tables)),
            TableReference::format_list(&read_refs(&self.read_tables)),
            TableReference::format_list(&write_refs(&self.update_tables)),
            TableReference::format_list(&write_refs(&self.delete_tables)),
        )
    }
}

/// Struct-style entry point. Equivalent to the free
/// [`extract_crud_tables`] function. A thin shim over
/// [`TableOperationExtractor`] that buckets `reads`/`writes` into the
/// CRUD positions, consulting the bind's normalized MERGE clause summary
/// (rather than re-walking the raw AST) for the one verb-aware case —
/// whose target placement depends on the WHEN actions.
#[derive(Default, Debug)]
pub struct CrudTableExtractor;

impl CrudTableExtractor {
    /// Same as the free [`extract_crud_tables`] function — kept for
    /// users who prefer the struct-style API.
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
    ) -> Result<Vec<Result<CrudTables, Error>>, Error> {
        Self::extract_with_options(dialect, sql, ExtractorOptions::new())
    }

    /// Like [`extract`](Self::extract) but with [`ExtractorOptions`] — a
    /// catalog and/or an identifier-casing override. `dialect` still
    /// drives parsing; the options govern only the analysis.
    pub fn extract_with_options(
        dialect: &dyn Dialect,
        sql: &str,
        options: ExtractorOptions,
    ) -> Result<Vec<Result<CrudTables, Error>>, Error> {
        crate::extractor::extract_each(dialect, sql, options, Self::extract_from_statement)
    }

    fn extract_from_statement(
        statement: &Statement,
        catalog: Option<&Catalog>,
        style: IdentifierStyle,
    ) -> Result<CrudTables, Error> {
        let (ops, merge_actions, insert_updates, cte_crud) =
            TableOperationExtractor::extract_inner(statement, catalog, style)?;
        // CRUD buckets carry the same `ResolutionKind` as the table operation:
        // reads as `TableRead`, the create / update / delete buckets as
        // `TableWrite`.
        let reads = ops.reads;
        // With data-modifying CTEs the flat write list spans several roots with
        // different verbs, so the outer-kind match below must bucket only the
        // outer root's *own* writes; each CTE's writes are added afterwards by
        // that CTE's verb. Without them, the flat list is the outer's writes.
        let writes = match &cte_crud {
            Some(c) => c.outer_writes.clone(),
            None => ops.writes,
        };
        let diagnostics = ops.diagnostics;

        let mut crud = CrudTables {
            diagnostics,
            ..Default::default()
        };
        match ops.statement_kind {
            StatementKind::Insert => {
                // An upsert (`INSERT … ON CONFLICT DO UPDATE` / MySQL
                // `ON DUPLICATE KEY UPDATE`) both inserts and updates the
                // target, so it lands in both buckets; a plain INSERT (or
                // `DO NOTHING`) is create-only.
                if insert_updates {
                    crud.update_tables = writes.clone();
                }
                // `REPLACE INTO` / `INSERT OVERWRITE` delete the conflicting /
                // existing rows of the target before inserting, so the target is
                // also a delete (unlike an upsert, which updates in place). Peel
                // a `WITH … INSERT OVERWRITE …` wrapper (parsed as a Query-
                // wrapped Insert) so the flags are read off the real insert.
                if peel_to_insert(statement).is_some_and(|i| i.replace_into || i.overwrite) {
                    crud.delete_tables = writes.clone();
                }
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
                // appear — read that off the IR-derived `MergeActions` the
                // bind produced, so this stays in step with the binder's
                // model (and handles `WITH … MERGE` transparently; the
                // facade peels the wrapper).
                let actions = merge_actions.unwrap_or_default();
                for target in &writes {
                    if actions.has_insert {
                        crud.create_tables.push(target.clone());
                    }
                    if actions.has_update {
                        crud.update_tables.push(target.clone());
                    }
                    if actions.has_delete {
                        crud.delete_tables.push(target.clone());
                    }
                }
                crud.read_tables = reads;
            }
            // DDL write targets bucket by verb: CREATE → Create (a new
            // object), ALTER → Update (modifies an existing one), DROP /
            // TRUNCATE → Delete (removes it). A CTAS / CREATE-VIEW source
            // still feeds `reads` (e.g. `CREATE TABLE t AS SELECT … FROM src`
            // → Create: [t], Read: [src]).
            StatementKind::CreateTable | StatementKind::CreateView => {
                crud.create_tables = writes;
                crud.read_tables = reads;
            }
            StatementKind::AlterTable | StatementKind::AlterView => {
                crud.update_tables = writes;
                crud.read_tables = reads;
            }
            StatementKind::Drop | StatementKind::Truncate => {
                crud.delete_tables = writes;
                crud.read_tables = reads;
            }
            // A plain `SELECT` writes nothing, so Create stays empty and the
            // read-role tables go to Read. (`SELECT … INTO t` is *not* here — it
            // classifies as `CreateTable`, handled above; its target surfaces in
            // `writes` → Create. The `writes` passthrough here is harmless: a
            // plain query has none.)
            StatementKind::Select => {
                crud.create_tables = writes;
                crud.read_tables = reads;
            }
            // An unsupported statement has no reliable write placement — fold
            // everything into `read_tables` (best-effort). Listed explicitly
            // (rather than `_ =>`) so a new `StatementKind` variant becomes a
            // compile error here and forces a bucket decision.
            StatementKind::Unsupported => {
                crud.read_tables = reads;
                // Best-effort: fold any write targets in as reads (an
                // unsupported statement builds no plan, so this is normally
                // empty). Same fields, write → read role.
                crud.read_tables
                    .extend(writes.into_iter().map(|w| TableRead {
                        reference: w.reference,
                        resolution: w.resolution,
                    }));
            }
        }

        // Each data-modifying CTE's write target lands in its own verb's bucket
        // (an INSERT CTE → create, DELETE → delete, UPDATE → update), regardless
        // of the outer kind handled above.
        if let Some(c) = cte_crud {
            crud.create_tables.extend(c.create);
            crud.update_tables.extend(c.update);
            crud.delete_tables.extend(c.delete);
        }

        Ok(crud)
    }
}

/// The underlying `INSERT` of a statement, peeling a `WITH` / parenthesis
/// wrapper. `WITH … INSERT OVERWRITE …` parses as a `Query`-wrapped `Insert`
/// (the verb rides `query.body`), so the REPLACE / OVERWRITE flags must be read
/// off the real insert, not the outer `Statement::Query`. `None` for any
/// non-INSERT statement.
fn peel_to_insert(statement: &Statement) -> Option<&Insert> {
    match statement {
        Statement::Insert(insert) => Some(insert),
        Statement::Query(query) => {
            let mut body = query.body.as_ref();
            loop {
                match body {
                    SetExpr::Insert(inner) => return peel_to_insert(inner),
                    SetExpr::Query(inner) => body = inner.body.as_ref(),
                    _ => return None,
                }
            }
        }
        _ => None,
    }
}
