use sqlparser::parser::ParserError;

/// Top-level error type for sql-insight.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Eq, thiserror::Error, PartialEq)]
pub enum Error {
    /// Invalid CLI argument (unknown dialect, malformed option).
    /// Not emitted by the library.
    #[error("{0}")]
    ArgumentError(String),
    /// SQL parse failure surfaced from sqlparser.
    #[error("{0}")]
    ParserError(#[from] ParserError),
    /// Semantic analysis rejected the input (e.g. a qualified
    /// name with more than three parts).
    #[error("{0}")]
    AnalysisError(String),
    /// I/O failure. CLI only.
    #[error("{0}")]
    IOError(String),
}
