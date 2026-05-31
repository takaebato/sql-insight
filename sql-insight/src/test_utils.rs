//! Dialect fixtures for tests across this crate and downstream
//! integration tests. `pub` so external `tests/` crates can reach
//! in too.

use sqlparser::dialect;
use sqlparser::dialect::Dialect;

/// Every sqlparser dialect this crate exercises, in a fixed order.
pub fn all_dialects() -> Vec<Box<dyn Dialect>> {
    vec![
        Box::new(dialect::GenericDialect {}),
        Box::new(dialect::MySqlDialect {}),
        Box::new(dialect::PostgreSqlDialect {}),
        Box::new(dialect::HiveDialect {}),
        Box::new(dialect::SQLiteDialect {}),
        Box::new(dialect::SnowflakeDialect {}),
        Box::new(dialect::RedshiftSqlDialect {}),
        Box::new(dialect::MsSqlDialect {}),
        Box::new(dialect::ClickHouseDialect {}),
        Box::new(dialect::BigQueryDialect {}),
        Box::new(dialect::AnsiDialect {}),
        Box::new(dialect::DuckDbDialect {}),
        Box::new(dialect::DatabricksDialect {}),
        Box::new(dialect::OracleDialect {}),
    ]
}

/// [`all_dialects`] minus dialects whose struct name appears in
/// `exclude`. Names compare against the `{:?}` form of each dialect
/// (e.g. `"GenericDialect"`, `"MsSqlDialect"`).
pub fn all_dialects_except(exclude: &[&'static str]) -> Vec<Box<dyn Dialect>> {
    all_dialects()
        .into_iter()
        .filter(|d| !exclude.contains(&format!("{:?}", d).as_str()))
        .collect()
}
