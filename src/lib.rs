mod error;
mod extractor;
pub use extractor::{extract_crud_tables, CrudTables};
pub mod normalizer;
pub use normalizer::normalize;
pub use sqlparser::dialect::*;
