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
/// Only one condition arises at table granularity: a whole statement the
/// extractor can't process. Column-resolution gaps (ambiguity, unresolved
/// names) and suppressed wildcards don't apply — a table's identity comes
/// straight from the FROM clause and is unaffected by them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableLevelDiagnosticKind {
    /// Statement variant the resolver / extractor does not understand well
    /// enough to extract operations from. `message` names the statement.
    UnsupportedStatement,
}

/// A non-fatal diagnostic from column-level extraction
/// ([`extract_column_operations`](crate::extract_column_operations)).
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

/// Why a column-level extraction is incomplete. Two flavours, by *which
/// side* the gap is on:
///
/// - **Tool-side coverage gap** — sql-insight didn't fully analyze this; a
///   more capable analyzer could do more.
///   [`UnsupportedStatement`](Self::UnsupportedStatement),
///   [`WildcardSuppressed`](Self::WildcardSuppressed).
/// - **Input-side resolution gap** — the SQL (+ catalog) doesn't determine
///   it, so the reference was left `table: None`. A real engine would also
///   reject these. [`AmbiguousColumn`](Self::AmbiguousColumn),
///   [`UnresolvedColumn`](Self::UnresolvedColumn).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnLevelDiagnosticKind {
    /// (tool-side) Statement variant the resolver / extractor does not
    /// understand well enough to extract operations from. `message` names
    /// the statement.
    UnsupportedStatement,
    /// (tool-side) `SELECT *` / `t.*` left unexpanded — the resolver does
    /// not perform wildcard expansion (see crate docs), so column lineage
    /// is incomplete for projections that include a wildcard.
    WildcardSuppressed,
    /// (input-side) Unqualified column reference matched multiple in-scope
    /// bindings whose schemas definitively contain the name. The reference
    /// is recorded with `table: None`. Only emitted in catalog-aware mode
    /// (i.e. when at least two `Known` schemas confirm the column); without
    /// catalog enrichment the resolver suppresses this to avoid false
    /// positives over `Unknown` schemas.
    AmbiguousColumn,
    /// (input-side) Unqualified column reference found no in-scope binding
    /// that contains the name. Only emitted in catalog-aware mode (i.e. when
    /// the scope has at least one `Known` schema and none of them holds the
    /// column); without catalog enrichment, every `Unknown` schema could
    /// contain anything and silence is the safer default.
    UnresolvedColumn,
}

impl ColumnLevelDiagnostic {
    /// Project to a [`TableLevelDiagnostic`] when this diagnostic is also
    /// meaningful at table granularity, else `None`.
    ///
    /// Only [`UnsupportedStatement`](ColumnLevelDiagnosticKind::UnsupportedStatement)
    /// carries over — wildcard suppression and column-resolution gaps don't
    /// affect table-level completeness. The `match` is exhaustive so a new
    /// `ColumnLevelDiagnosticKind` variant forces an explicit table-level
    /// decision here.
    pub(crate) fn to_table_level(&self) -> Option<TableLevelDiagnostic> {
        let kind = match self.kind {
            ColumnLevelDiagnosticKind::UnsupportedStatement => {
                TableLevelDiagnosticKind::UnsupportedStatement
            }
            ColumnLevelDiagnosticKind::WildcardSuppressed
            | ColumnLevelDiagnosticKind::AmbiguousColumn
            | ColumnLevelDiagnosticKind::UnresolvedColumn => return None,
        };
        Some(TableLevelDiagnostic {
            kind,
            message: self.message.clone(),
            span: self.span,
        })
    }
}
