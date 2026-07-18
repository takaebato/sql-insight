//! Dialect **semantics**: the binder's questions about what a construct
//! *means* under the parsing dialect — divergences that can't be folded into
//! identifier matching ([`crate::casing`]). Each question is one method over
//! [`Binder::dialect`]; a new dialect-dependent resolution rule is a new
//! method here, never an ad-hoc downcast inside bind code.

use sqlparser::dialect::{ClickHouseDialect, MsSqlDialect, MySqlDialect, SQLiteDialect};

use super::Binder;

impl Binder<'_> {
    /// Whether a dotted `SET` target can address a field inside a
    /// struct-typed column (`UPDATE t SET address.city = …` updating column
    /// `address`). PostgreSQL forbids relation-qualified SET targets
    /// outright — the leading segment is *always* a column there (verified
    /// on PostgreSQL 18: `SET t.col` errors with "SET target columns cannot
    /// be qualified with the relation name") — and BigQuery / Hive / Spark /
    /// DuckDB share the struct reading. The known table-qualifier-only
    /// dialects opt out: MySQL / MSSQL / SQLite / ClickHouse have no struct
    /// SET, so a qualifier there is a table path, and one that matches no
    /// relation is a mistake, never a field path. Everything else — custom
    /// dialects and `GenericDialect` — keeps the struct reading: struct-SET
    /// syntax under Generic almost always carries PostgreSQL-family intent,
    /// and the permissive side keeps a sole-assignment UPDATE visible in
    /// the write / CRUD surfaces (the unattributed alternative surfaces no
    /// table-level write).
    pub(super) fn struct_set_targets(&self) -> bool {
        !(self.dialect.is::<MySqlDialect>()
            || self.dialect.is::<MsSqlDialect>()
            || self.dialect.is::<SQLiteDialect>()
            || self.dialect.is::<ClickHouseDialect>())
    }
}
