//! Extraction APIs at four granularities of "what does this SQL touch?"
//!
//! Each sub-extractor is a thin wrapper around the resolver,
//! projecting the resolution into a different surface:
//!
//! - [`extract_tables`] — flat list of `TableReference`s per
//!   statement, no read/write distinction.
//! - [`extract_crud_tables`] — tables bucketed by CRUD verb
//!   (Create / Read / Update / Delete).
//! - [`extract_table_operations`] — per-statement
//!   `TableOperation` with `reads` / `writes` / `lineage` at table
//!   granularity.
//! - [`extract_column_operations`] — same shape at column
//!   granularity, plus per-column lineage kinds
//!   (Passthrough / Transformation).
//!
//! Each extractor returns `Vec<Result<X, Error>>` so one malformed
//! statement does not kill the rest of a multi-statement SQL
//! string. Sub-modules are private; the public items reach users
//! through the wildcard re-exports below.

mod column_operation_extractor;
mod crud_table_extractor;
mod table_extractor;
mod table_operation_extractor;

pub use column_operation_extractor::*;
pub use crud_table_extractor::*;
pub use table_extractor::*;
pub use table_operation_extractor::*;
