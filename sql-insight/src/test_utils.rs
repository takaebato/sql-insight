//! Dialect fixtures for tests across this crate and downstream
//! integration tests. `pub` so external `tests/` crates can reach
//! in too.

use sqlparser::dialect;
use sqlparser::dialect::Dialect;

/// Each sqlparser dialect this crate exercises in tests. Pair with
/// [`all_dialects_except`] to skip a known-fail dialect from a
/// per-dialect sweep without typo-prone string literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialectName {
    Generic,
    MySql,
    PostgreSql,
    Hive,
    SQLite,
    Snowflake,
    Redshift,
    MsSql,
    ClickHouse,
    BigQuery,
    Ansi,
    DuckDb,
    Databricks,
    Oracle,
}

impl DialectName {
    /// Every variant, in a fixed order. Used by [`all_dialects`] /
    /// [`all_dialects_except`]; exposed in case a caller wants to
    /// drive the iteration themselves.
    pub fn all() -> [DialectName; 14] {
        [
            Self::Generic,
            Self::MySql,
            Self::PostgreSql,
            Self::Hive,
            Self::SQLite,
            Self::Snowflake,
            Self::Redshift,
            Self::MsSql,
            Self::ClickHouse,
            Self::BigQuery,
            Self::Ansi,
            Self::DuckDb,
            Self::Databricks,
            Self::Oracle,
        ]
    }

    /// Construct a fresh boxed dialect instance for this variant.
    pub fn instance(self) -> Box<dyn Dialect> {
        match self {
            Self::Generic => Box::new(dialect::GenericDialect {}),
            Self::MySql => Box::new(dialect::MySqlDialect {}),
            Self::PostgreSql => Box::new(dialect::PostgreSqlDialect {}),
            Self::Hive => Box::new(dialect::HiveDialect {}),
            Self::SQLite => Box::new(dialect::SQLiteDialect {}),
            Self::Snowflake => Box::new(dialect::SnowflakeDialect {}),
            Self::Redshift => Box::new(dialect::RedshiftSqlDialect {}),
            Self::MsSql => Box::new(dialect::MsSqlDialect {}),
            Self::ClickHouse => Box::new(dialect::ClickHouseDialect {}),
            Self::BigQuery => Box::new(dialect::BigQueryDialect {}),
            Self::Ansi => Box::new(dialect::AnsiDialect {}),
            Self::DuckDb => Box::new(dialect::DuckDbDialect {}),
            Self::Databricks => Box::new(dialect::DatabricksDialect {}),
            Self::Oracle => Box::new(dialect::OracleDialect {}),
        }
    }
}

/// Every dialect in [`DialectName::all`], boxed.
pub fn all_dialects() -> Vec<Box<dyn Dialect>> {
    DialectName::all()
        .into_iter()
        .map(|n| n.instance())
        .collect()
}

/// [`all_dialects`] minus the named exclusions.
pub fn all_dialects_except(exclude: &[DialectName]) -> Vec<Box<dyn Dialect>> {
    DialectName::all()
        .into_iter()
        .filter(|n| !exclude.contains(n))
        .map(|n| n.instance())
        .collect()
}
