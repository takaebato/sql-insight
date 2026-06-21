//! Flat table-identity extraction. See [`extract_tables`] as the
//! entry point.
//!
//! Returns the list of tables a statement references, with no
//! read/write distinction or lineage information. For those, see
//! [`extract_table_operations`](crate::extractor::extract_table_operations)
//! / [`extract_column_operations`](crate::extractor::extract_column_operations).

use core::fmt;

use crate::casing::IdentifierStyle;
use crate::catalog::Catalog;
use crate::diagnostic::{TableLevelDiagnostic, TableLevelDiagnosticKind};
use crate::error::Error;
use crate::extractor::{classify_statement, ExtractorOptions, StatementKind};
use crate::reference::TableReference;
use sqlparser::ast::Statement;
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Parse `sql` under `dialect` and return one [`TableExtraction`] per
/// statement.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
///
/// let dialect = GenericDialect {};
/// let sql = "SELECT a FROM t1 INNER JOIN t2 ON t1.id = t2.id";
/// let result = sql_insight::extractor::extract_tables(&dialect, sql).unwrap();
/// println!("{:#?}", result);
/// assert_eq!(result[0].as_ref().unwrap().to_string(), "t1, t2");
/// ```
pub fn extract_tables(
    dialect: &dyn Dialect,
    sql: &str,
) -> Result<Vec<Result<TableExtraction, Error>>, Error> {
    TableExtractor::extract(dialect, sql)
}

/// Like [`extract_tables`] but with [`ExtractorOptions`] — a catalog
/// and/or an identifier-casing override. With a catalog, matched tables
/// are canonicalized to their registered `catalog.schema.name` path, so
/// the surfaced identities differ from the catalog-free list.
pub fn extract_tables_with_options(
    dialect: &dyn Dialect,
    sql: &str,
    options: ExtractorOptions,
) -> Result<Vec<Result<TableExtraction, Error>>, Error> {
    TableExtractor::extract_with_options(dialect, sql, options)
}

/// Per-statement output of [`extract_tables`]: the table list plus
/// any non-fatal diagnostics surfaced during the walk. `Display`
/// renders just the comma-joined table list.
#[derive(Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TableExtraction {
    /// Every table the statement references, one entry per relation
    /// binding: a table that is both a write target and a row source
    /// appears once, while the same table reached through two separate
    /// FROM uses appears twice. **In source order** — by each table's written
    /// token span (`name.span`), a deterministic function of the SQL rather
    /// than the internal tree walk; occurrence count is preserved. For the
    /// distinct set, dedup via a `HashSet` (or, catalog-free, by
    /// [`TableReference::identity_key`](crate::TableReference::identity_key)
    /// to fold case-equivalent spellings).
    pub tables: Vec<TableReference>,
    pub diagnostics: Vec<TableLevelDiagnostic>,
}

impl fmt::Display for TableExtraction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", TableReference::format_list(&self.tables))
    }
}

/// Struct-style entry point. Equivalent to the free
/// [`extract_tables`] function.
#[derive(Default, Debug)]
pub struct TableExtractor;

impl TableExtractor {
    /// Same as the free [`extract_tables`] function — kept for
    /// users who prefer the struct-style API.
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
    ) -> Result<Vec<Result<TableExtraction, Error>>, Error> {
        Self::extract_with_options(dialect, sql, ExtractorOptions::new())
    }

    /// Like [`extract`](Self::extract) but with [`ExtractorOptions`] — a
    /// catalog and/or an identifier-casing override. `dialect` still
    /// drives parsing; the options govern only the analysis.
    pub fn extract_with_options(
        dialect: &dyn Dialect,
        sql: &str,
        options: ExtractorOptions,
    ) -> Result<Vec<Result<TableExtraction, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        let style = options.identifier_style(dialect);
        let results = statements
            .iter()
            .map(|s| Self::extract_from_statement(s, options.catalog, style))
            .collect::<Vec<Result<TableExtraction, Error>>>();
        Ok(results)
    }

    fn extract_from_statement(
        statement: &Statement,
        catalog: Option<&Catalog>,
        style: IdentifierStyle,
    ) -> Result<TableExtraction, Error> {
        // The flat table list is derived from the bound-plan engine. A
        // catalog changes no column data (this API surfaces no columns) but
        // does canonicalize matched table identities to their registered
        // path. An unsupported statement yields an empty list with a
        // diagnostic; otherwise walk the plan for every referenced table and
        // project the column diagnostics down.
        if classify_statement(statement) == StatementKind::Unsupported {
            return Ok(TableExtraction {
                tables: Vec::new(),
                diagnostics: vec![TableLevelDiagnostic {
                    kind: TableLevelDiagnosticKind::UnsupportedStatement,
                    message: format!("Unsupported statement while inspecting SQL: {statement}"),
                    span: None,
                }],
            });
        }
        let (plan, column_diagnostics) = crate::resolver::build(statement, catalog, style);
        Ok(TableExtraction {
            tables: crate::resolver::flat_tables(&plan),
            diagnostics: column_diagnostics
                .iter()
                .filter_map(|d| d.to_table_level())
                .collect(),
        })
    }
}
