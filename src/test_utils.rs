use sqlparser::dialect;
use sqlparser::dialect::Dialect;

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
    ]
}

pub static ALL_DIALECT_NAMES: [&str; 12] = [
    "Generic",
    "MySQL",
    "PostgreSQL",
    "Hive",
    "SQLite",
    "Snowflake",
    "Redshift",
    "MsSQL",
    "ClickHouse",
    "BigQuery",
    "ANSI",
    "DuckDB",
];
