//! Diagnostics reported during SQL inspection.
//!
//! Diagnostics are split by extraction granularity:
//! [`TableLevelDiagnostic`] for the table-level surfaces
//! (`extract_tables` / `extract_table_operations` / `extract_crud_tables`)
//! and [`ColumnLevelDiagnostic`] for `extract_column_operations`. The split
//! is by *type* so a table-level result cannot even represent a column-only
//! condition — e.g. a suppressed wildcard, which leaves column lineage
//! incomplete but doesn't affect table-level completeness at all.

use sqlparser::tokenizer::Span;

/// A non-fatal diagnostic from table-level extraction.
///
/// Carried by the table-level surfaces. `message` is human-readable and,
/// when a [`span`](Self::span) is available, also embeds the location for
/// log-line display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableLevelDiagnostic {
    pub kind: TableLevelDiagnosticKind,
    pub message: String,
    /// Source location of the offending token, when available. `None` when
    /// the originating AST node carries no span.
    pub span: Option<Span>,
}

/// Why a table-level extraction is incomplete.
///
/// Two conditions arise at table granularity: a whole statement the
/// extractor can't process, and a table name too qualified to represent.
/// Column-resolution gaps (ambiguity, unresolved names) and suppressed
/// wildcards don't apply — a table's identity comes straight from the FROM
/// clause and is unaffected by them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableLevelDiagnosticKind {
    /// Statement variant the extractor does not understand well enough to
    /// extract operations from. `message` names the statement.
    UnsupportedStatement,
    /// A table reference with more identifiers than `catalog.schema.name`
    /// (e.g. a SQL Server `server.db.schema.table`) that can't be
    /// represented as a [`TableReference`](crate::TableReference), so the
    /// relation is dropped from `reads` / `writes`. `message` names it.
    TooManyTableQualifiers,
}

/// A non-fatal diagnostic from column-level extraction
/// ([`extract_column_operations`](crate::extractor::extract_column_operations)).
///
/// Carries the same `message` / `span` shape as [`TableLevelDiagnostic`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnLevelDiagnostic {
    pub kind: ColumnLevelDiagnosticKind,
    pub message: String,
    /// Source location of the offending token, when available. `None` when
    /// the originating AST node carries no span (sqlparser-rs coverage is
    /// patchy outside `Ident` / `Value` / tokens), or when the resolver
    /// couldn't reasonably attribute the diagnostic to a single span.
    pub span: Option<Span>,
}

/// Why a column-level extraction is incomplete.
///
/// Both surviving variants are *tool-side coverage gaps*: sql-insight
/// chose not to (or couldn't) fully analyze the construct, and a more
/// capable analyzer could do more. Per-reference resolution outcomes
/// (ambiguous / unresolved columns) are *not* diagnostics — they
/// surface on each [`ColumnRead::resolution`](crate::ColumnRead) instead,
/// so the consumer reads them off the reference rather than
/// cross-referencing a parallel diagnostic stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnLevelDiagnosticKind {
    /// Statement variant the resolver / extractor does not understand
    /// well enough to extract operations from. `message` names the
    /// statement.
    UnsupportedStatement,
    /// `SELECT *` / `t.*` left unexpanded — the extractor does not
    /// perform wildcard expansion (see crate docs), so column lineage
    /// is incomplete for projections that include a wildcard.
    WildcardSuppressed,
    /// A table reference with more identifiers than `catalog.schema.name`
    /// (e.g. a SQL Server `server.db.schema.table`) that can't be
    /// represented as a [`TableReference`](crate::TableReference), so the
    /// relation — and any column read / write through it — is dropped.
    /// `message` names the offending identifier.
    TooManyTableQualifiers,
}

impl ColumnLevelDiagnostic {
    /// Project to a [`TableLevelDiagnostic`] when this diagnostic is also
    /// meaningful at table granularity, else `None`.
    ///
    /// [`UnsupportedStatement`](ColumnLevelDiagnosticKind::UnsupportedStatement)
    /// and [`TooManyTableQualifiers`](ColumnLevelDiagnosticKind::TooManyTableQualifiers)
    /// carry over (both drop a whole relation from the table surfaces);
    /// wildcard suppression and column-resolution gaps don't affect
    /// table-level completeness. The `match` is exhaustive so a new
    /// `ColumnLevelDiagnosticKind` variant forces an explicit table-level
    /// decision here.
    pub(crate) fn to_table_level(&self) -> Option<TableLevelDiagnostic> {
        let kind = match self.kind {
            ColumnLevelDiagnosticKind::UnsupportedStatement => {
                TableLevelDiagnosticKind::UnsupportedStatement
            }
            ColumnLevelDiagnosticKind::TooManyTableQualifiers => {
                TableLevelDiagnosticKind::TooManyTableQualifiers
            }
            ColumnLevelDiagnosticKind::WildcardSuppressed => return None,
        };
        Some(TableLevelDiagnostic {
            kind,
            message: self.message.clone(),
            span: self.span,
        })
    }
}
