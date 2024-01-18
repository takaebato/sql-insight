use sqlparser::parser::ParserError;

#[derive(Debug, Eq, thiserror::Error, PartialEq)]
pub enum Error {
    #[error("{0}")]
    ArgumentError(String),
    #[error("{0}")]
    ParseError(#[from] ParserError),
}
