//! Diagnostics reported during SQL inspection.

/// A non-fatal diagnostic produced while inspecting SQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
}

/// The kind of diagnostic produced while inspecting SQL.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// Statement variant the resolver / extractor does not understand
    /// well enough to extract operations from. `message` names the
    /// statement.
    UnsupportedStatement,
    /// `SELECT *` / `t.*` left unexpanded — the resolver does not perform
    /// wildcard expansion (see crate docs), so lineage is incomplete for
    /// projections that include a wildcard.
    WildcardSuppressed,
    /// Unqualified column reference matched multiple in-scope bindings
    /// whose schemas definitively contain the name. The reference is
    /// recorded with `table: None`. Only emitted in catalog-aware mode
    /// (i.e. when at least two `Known` schemas confirm the column);
    /// without catalog enrichment the resolver suppresses this to avoid
    /// false positives over `Unknown` schemas.
    AmbiguousColumn,
    /// Unqualified column reference found no in-scope binding that
    /// contains the name. Only emitted in catalog-aware mode (i.e. when
    /// the scope has at least one `Known` schema and none of them holds
    /// the column); without catalog enrichment, every `Unknown` schema
    /// could contain anything and silence is the safer default.
    UnresolvedColumn,
}
