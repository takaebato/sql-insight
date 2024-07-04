//! # sql-insight
//!
//! `sql-insight` is a utility designed for SQL query analysis, formatting, and transformation.
//!
//! ## Main Functionalities
//!
//! - **SQL Formatting**: Format SQL queries into a standardized format. See the [`formatter`] module for more information.
//! - **SQL Normalization**: Normalize SQL queries by abstracting literals. See the [`normalizer`] module for more information.
//! - **Table Extraction**: Extract tables within SQL queries. See the [`table_extractor`] module for more information.
//! - **CRUD Table Extraction**: Extract CRUD tables from SQL queries. See the [`crud_table_extractor`] module for more information.
//!
//! ## Quick Start
//!
//! Here's a quick example to get you started with SQL formatting:
//!
//! ```rust
//! use sql_insight::sqlparser::dialect::GenericDialect;
//!
//! let dialect = GenericDialect {};
//! let normalized_sql = sql_insight::format(&dialect, "SELECT * \n from users   WHERE id = 1").unwrap();
//! assert_eq!(normalized_sql, ["SELECT * FROM users WHERE id = 1"]);
//! ```
//!
//! For more comprehensive examples and usage, refer to [crates.io](https://crates.io/crates/sql-insight) or the documentation of each module.

pub mod error;
pub mod extractor;
pub mod formatter;
pub mod normalizer;

pub use extractor::*;
pub use formatter::*;
pub use normalizer::*;
pub use sqlparser;

#[doc(hidden)]
// Internal module for testing. Made public for use in integration tests.
pub mod test_utils;
