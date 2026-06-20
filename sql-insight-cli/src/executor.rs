use sql_insight::error::Error;
use sql_insight::extractor::{ColumnLineageKind, ColumnOperation, ColumnTarget, TableOperation};
use sql_insight::normalizer::NormalizerOptions;
use sql_insight::sqlparser::dialect;
use sql_insight::{ColumnRead, ResolutionKind, TableRead};

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

pub struct TableExtractExecutor {
    pub sql: String,
    pub dialect_name: Option<String>,
}

impl TableExtractExecutor {
    pub fn new(sql: String, dialect_name: Option<String>) -> Self {
        Self { sql, dialect_name }
    }
}

impl CliExecutable for TableExtractExecutor {
    fn execute(&self) -> Result<Vec<String>, Error> {
        let result = sql_insight::extractor::extract_tables(
            get_dialect(self.dialect_name.as_deref())?.as_ref(),
            self.sql.as_ref(),
        )?;
        Ok(result
            .iter()
            .map(|r| match r {
                Ok(tables) => format!("{}", tables),
                Err(e) => format!("Error: {}", e),
            })
            .collect())
    }
}

pub struct CrudTableExtractExecutor {
    sql: String,
    dialect_name: Option<String>,
}

impl CrudTableExtractExecutor {
    pub fn new(sql: String, dialect_name: Option<String>) -> Self {
        Self { sql, dialect_name }
    }
}

impl CliExecutable for CrudTableExtractExecutor {
    fn execute(&self) -> Result<Vec<String>, Error> {
        let result = sql_insight::extractor::extract_crud_tables(
            get_dialect(self.dialect_name.as_deref())?.as_ref(),
            self.sql.as_ref(),
        )?;
        Ok(result
            .iter()
            .map(|r| match r {
                Ok(crud_tables) => format!("{}", crud_tables),
                Err(e) => format!("Error: {}", e),
            })
            .collect())
    }
}

pub struct TableOperationExtractExecutor {
    sql: String,
    dialect_name: Option<String>,
}

impl TableOperationExtractExecutor {
    pub fn new(sql: String, dialect_name: Option<String>) -> Self {
        Self { sql, dialect_name }
    }
}

impl CliExecutable for TableOperationExtractExecutor {
    fn execute(&self) -> Result<Vec<String>, Error> {
        let result = sql_insight::extractor::extract_table_operations(
            get_dialect(self.dialect_name.as_deref())?.as_ref(),
            self.sql.as_ref(),
        )?;
        Ok(render_statements(&result, format_table_operation))
    }
}

pub struct ColumnOperationExtractExecutor {
    sql: String,
    dialect_name: Option<String>,
}

impl ColumnOperationExtractExecutor {
    pub fn new(sql: String, dialect_name: Option<String>) -> Self {
        Self { sql, dialect_name }
    }
}

impl CliExecutable for ColumnOperationExtractExecutor {
    fn execute(&self) -> Result<Vec<String>, Error> {
        let result = sql_insight::extractor::extract_column_operations(
            get_dialect(self.dialect_name.as_deref())?.as_ref(),
            self.sql.as_ref(),
        )?;
        Ok(render_statements(&result, format_column_operation))
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
