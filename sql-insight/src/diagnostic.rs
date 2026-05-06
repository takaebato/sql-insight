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
    UnsupportedStatement,
}
