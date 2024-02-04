use sqlparser::parser::ParserError;

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Eq, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("{0}")]
    ArgumentError(String),
    #[error("{0}")]
    ParserError(#[from] ParserError),
    #[error("{0}")]
    AnalysisError(String),
}
