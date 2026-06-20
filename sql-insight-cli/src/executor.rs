use sql_insight::catalog::Catalog;
use sql_insight::error::Error;
use sql_insight::extractor::{
    extract_column_operations_with_options, extract_crud_tables_with_options,
    extract_table_operations_with_options, extract_tables_with_options, ColumnLineageKind,
    ColumnOperation, ColumnTarget, ExtractorOptions, TableOperation,
};
use sql_insight::normalizer::NormalizerOptions;
use sql_insight::sqlparser::dialect::{self, Dialect};
use sql_insight::{CaseRule, ColumnRead, IdentifierCasing, ResolutionKind, TableRead};

pub trait CliExecutable {
    fn execute(&self) -> Result<Vec<String>, Error>;
}

fn get_dialect(dialect_name: Option<&str>) -> Result<Box<dyn dialect::Dialect>, Error> {
    let dialect_name = dialect_name.unwrap_or("generic");
    dialect::dialect_from_str(dialect_name)
        .ok_or_else(|| Error::ArgumentError(format!("Dialect not found: {}", dialect_name)))
}

pub struct FormatExecutor {
    sql: String,
    dialect_name: Option<String>,
}

impl FormatExecutor {
    pub fn new(sql: String, dialect_name: Option<String>) -> Self {
        Self { sql, dialect_name }
    }
}

impl CliExecutable for FormatExecutor {
    fn execute(&self) -> Result<Vec<String>, Error> {
        sql_insight::formatter::format(
            get_dialect(self.dialect_name.as_deref())?.as_ref(),
            self.sql.as_ref(),
        )
    }
}

pub struct NormalizeExecutor {
    sql: String,
    dialect_name: Option<String>,
    options: NormalizerOptions,
}

impl NormalizeExecutor {
    pub fn new(sql: String, dialect_name: Option<String>) -> Self {
        Self {
            sql,
            dialect_name,
            options: NormalizerOptions::new(),
        }
    }

    pub fn with_options(mut self, options: NormalizerOptions) -> Self {
        self.options = options;
        self
    }
}

impl CliExecutable for NormalizeExecutor {
    fn execute(&self) -> Result<Vec<String>, Error> {
        sql_insight::normalizer::normalize_with_options(
            get_dialect(self.dialect_name.as_deref())?.as_ref(),
            self.sql.as_ref(),
            self.options.clone(),
        )
    }
}

/// Which extraction surface an [`ExtractExecutor`] produces.
pub enum ExtractKind {
    Tables,
    Crud,
    TableOps,
    ColumnOps,
}

/// Per-class identifier-casing overrides from the CLI. `all` is the
/// uniform base; the per-class fields patch individual classes on top.
/// All `None` = use the dialect default unchanged.
#[derive(Default)]
pub struct CasingOverride {
    pub all: Option<CaseRule>,
    pub table: Option<CaseRule>,
    pub table_alias: Option<CaseRule>,
    pub column: Option<CaseRule>,
}

impl CasingOverride {
    /// Resolve to an [`IdentifierCasing`], or `None` when no override was
    /// given (so the binder keeps the dialect default). Layering: dialect
    /// default → uniform `all` → per-class patches.
    fn resolve(&self, dialect: &dyn Dialect) -> Option<IdentifierCasing> {
        if self.all.is_none()
            && self.table.is_none()
            && self.table_alias.is_none()
            && self.column.is_none()
        {
            return None;
        }
        let mut casing = match self.all {
            Some(rule) => IdentifierCasing::uniform(rule),
            None => IdentifierCasing::for_dialect(dialect),
        };
        if let Some(rule) = self.table {
            casing.table = rule;
        }
        if let Some(rule) = self.table_alias {
            casing.table_alias = rule;
        }
        if let Some(rule) = self.column {
            casing.column = rule;
        }
        Some(casing)
    }
}

/// The single extractor entry point: builds the dialect, optional catalog
/// (from a DDL file), and casing override once, then dispatches on `kind`.
pub struct ExtractExecutor {
    pub kind: ExtractKind,
    pub sql: String,
    pub dialect_name: Option<String>,
    pub catalog_file: Option<String>,
    pub default_schema: String,
    pub casing: CasingOverride,
}

impl CliExecutable for ExtractExecutor {
    fn execute(&self) -> Result<Vec<String>, Error> {
        let dialect = get_dialect(self.dialect_name.as_deref())?;
        let dialect = dialect.as_ref();
        let catalog = self.load_catalog(dialect)?;
        let casing = self.casing.resolve(dialect);

        let mut options = ExtractorOptions::new();
        if let Some(catalog) = &catalog {
            options = options.with_catalog(catalog);
        }
        if let Some(casing) = casing {
            options = options.with_casing(casing);
        }

        let sql = self.sql.as_ref();
        match self.kind {
            ExtractKind::Tables => Ok(render_display(&extract_tables_with_options(
                dialect, sql, options,
            )?)),
            ExtractKind::Crud => Ok(render_display(&extract_crud_tables_with_options(
                dialect, sql, options,
            )?)),
            ExtractKind::TableOps => Ok(render_statements(
                &extract_table_operations_with_options(dialect, sql, options)?,
                format_table_operation,
            )),
            ExtractKind::ColumnOps => Ok(render_statements(
                &extract_column_operations_with_options(dialect, sql, options)?,
                format_column_operation,
            )),
        }
    }
}

impl ExtractExecutor {
    /// Read `--catalog` DDL (if any) and build a catalog from it.
    fn load_catalog(&self, dialect: &dyn Dialect) -> Result<Option<Catalog>, Error> {
        let Some(path) = &self.catalog_file else {
            return Ok(None);
        };
        let ddl = std::fs::read_to_string(path).map_err(|e| {
            Error::ArgumentError(format!("Failed to read catalog file {path}: {e}"))
        })?;
        Ok(Some(Catalog::from_ddl(
            dialect,
            &ddl,
            &self.default_schema,
        )?))
    }
}

// ===== text rendering for the rich (operation) extractors =================
//
// One multi-line block per statement: `[N] <Kind>` header followed by the
// non-empty `reads` / `writes` / `lineage` surfaces and any diagnostics.
// Convention: a default is unmarked, a deviation is marked — `Inferred`
// resolution and `Passthrough` lineage carry no marker, so the common
// catalog-free SELECT stays clean while `(cataloged)` / `(ambiguous)` /
// `(unresolved)` and `[transform]` stand out.

/// Render each per-statement result, numbering from 1; an `Err` statement
/// becomes a one-line `[N] Error: ...` so one bad statement doesn't sink
/// the batch.
fn render_statements<T>(
    results: &[Result<T, Error>],
    format_one: impl Fn(usize, &T) -> String,
) -> Vec<String> {
    results
        .iter()
        .enumerate()
        .map(|(i, r)| match r {
            Ok(op) => format_one(i + 1, op),
            Err(e) => format!("[{}] Error: {}", i + 1, e),
        })
        .collect()
}

/// Render the `Display`-backed extractors (`tables` / `crud`), one
/// statement per line.
fn render_display<T: std::fmt::Display>(results: &[Result<T, Error>]) -> Vec<String> {
    results
        .iter()
        .map(|r| match r {
            Ok(value) => value.to_string(),
            Err(e) => format!("Error: {e}"),
        })
        .collect()
}

/// `  reads:    a, b` — labels pad to a fixed column so values align.
fn labeled(label: &str, value: String) -> String {
    format!("  {label:<8} {value}")
}

/// Marker for a non-default resolution; `Inferred` (the catalog-free
/// default) is unmarked.
fn resolution_marker(resolution: ResolutionKind) -> &'static str {
    match resolution {
        ResolutionKind::Inferred => "",
        ResolutionKind::Cataloged => " (cataloged)",
        ResolutionKind::Ambiguous => " (ambiguous)",
        ResolutionKind::Unresolved => " (unresolved)",
    }
}

fn table_read(read: &TableRead) -> String {
    format!("{}{}", read.reference, resolution_marker(read.resolution))
}

fn column_read(read: &ColumnRead) -> String {
    format!("{}{}", read.reference, resolution_marker(read.resolution))
}

fn column_target(target: &ColumnTarget) -> String {
    match target {
        ColumnTarget::Relation(reference) => reference.to_string(),
        ColumnTarget::QueryOutput { name, position } => match name {
            Some(name) => name.value.clone(),
            None => format!("#{position}"),
        },
    }
}

/// Join `lineage` edges one per line, the first after the `lineage:` label
/// and the rest aligned under it.
fn lineage_block(edges: Vec<String>) -> String {
    let pad = " ".repeat(labeled("lineage:", String::new()).len());
    edges
        .iter()
        .enumerate()
        .map(|(i, edge)| {
            if i == 0 {
                labeled("lineage:", edge.clone())
            } else {
                format!("{pad}{edge}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_table_operation(n: usize, op: &TableOperation) -> String {
    let mut lines = vec![format!("[{n}] {:?}", op.statement_kind)];
    if !op.reads.is_empty() {
        let reads = op.reads.iter().map(table_read).collect::<Vec<_>>();
        lines.push(labeled("reads:", reads.join(", ")));
    }
    if !op.writes.is_empty() {
        let writes = op.writes.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        lines.push(labeled("writes:", writes.join(", ")));
    }
    if !op.lineage.is_empty() {
        let edges = op
            .lineage
            .iter()
            .map(|e| format!("{} -> {}", table_read(&e.source), e.target))
            .collect();
        lines.push(lineage_block(edges));
    }
    for d in &op.diagnostics {
        lines.push(format!("  ! {}", d.message));
    }
    lines.join("\n")
}

fn format_column_operation(n: usize, op: &ColumnOperation) -> String {
    let mut lines = vec![format!("[{n}] {:?}", op.statement_kind)];
    if !op.reads.is_empty() {
        let reads = op.reads.iter().map(column_read).collect::<Vec<_>>();
        lines.push(labeled("reads:", reads.join(", ")));
    }
    if !op.writes.is_empty() {
        let writes = op.writes.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        lines.push(labeled("writes:", writes.join(", ")));
    }
    if !op.lineage.is_empty() {
        let edges = op
            .lineage
            .iter()
            .map(|e| {
                let transform = match e.kind {
                    ColumnLineageKind::Transformation => " [transform]",
                    ColumnLineageKind::Passthrough => "",
                };
                format!(
                    "{} -> {}{transform}",
                    column_read(&e.source),
                    column_target(&e.target)
                )
            })
            .collect();
        lines.push(lineage_block(edges));
    }
    for d in &op.diagnostics {
        lines.push(format!("  ! {}", d.message));
    }
    lines.join("\n")
}
