//! Dialect **capabilities**: resolution-rule switches derived once from the
//! parsing dialect and threaded into the binder alongside the identifier
//! style. [`crate::casing`] covers how identifiers *match*; a capability
//! covers a divergence in what a construct *means* across dialects — the
//! kind of split that can't be folded into matching rules. Add a field per
//! divergence, derived in [`DialectCapabilities::for_dialect`], so each new
//! dialect-dependent resolution rule is a one-line switch here rather than
//! an ad-hoc dialect check inside the binder.

use sqlparser::dialect::{ClickHouseDialect, Dialect, MsSqlDialect, MySqlDialect, SQLiteDialect};

/// The resolution-rule switches the binder consults. Derived once per
/// extraction from the parsing dialect ([`Self::for_dialect`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DialectCapabilities {
    /// Whether a dotted `SET` target can address a field inside a
    /// struct-typed column (`UPDATE t SET address.city = …` updating column
    /// `address`). PostgreSQL forbids relation-qualified SET targets
    /// outright — the leading segment is *always* a column there (verified
    /// on PostgreSQL 18: `SET t.col` errors with "SET target columns cannot
    /// be qualified with the relation name") — and BigQuery / Hive / Spark /
    /// DuckDB share the struct reading. MySQL / MSSQL / SQLite / ClickHouse
    /// have no struct SET: a qualifier there is a table path, and one that
    /// matches no relation is a mistake, never a field path.
    pub(crate) struct_set_targets: bool,
}

impl DialectCapabilities {
    pub(crate) fn for_dialect(dialect: &dyn Dialect) -> Self {
        // Known table-qualifier-only dialects opt out; everything else —
        // PostgreSQL, BigQuery, Hive / Databricks, DuckDB, Redshift, custom
        // dialects, and GenericDialect — keeps the struct reading. Generic
        // is deliberately on the permissive side: a struct-field SET written
        // under it almost always carries PostgreSQL-family intent, and the
        // struct reading keeps a sole-assignment UPDATE visible in the
        // write / CRUD surfaces (the unattributed alternative surfaces no
        // table-level write).
        let table_qualifiers_only = dialect.is::<MySqlDialect>()
            || dialect.is::<MsSqlDialect>()
            || dialect.is::<SQLiteDialect>()
            || dialect.is::<ClickHouseDialect>();
        DialectCapabilities {
            struct_set_targets: !table_qualifiers_only,
        }
    }
}
