//! Per-dialect identifier case-folding policy.
//!
//! SQL engines disagree on how identifiers compare for equality:
//! whether an unquoted name folds to upper- or lower-case, whether a
//! quoted name is then case-sensitive, and whether quoting matters at
//! all. The binder matches identifiers (table qualifiers, column names,
//! aliases) by their folded key, so it needs the active dialect's rule
//! to decide e.g. whether `Users` and `users` are the same table.
//!
//! This folds the (well-surveyed) cross-dialect matrix down to a
//! [`CaseRule`] per identifier *class*. The six syntactic positions
//! (catalog / schema / table / table-alias / column / column-alias)
//! collapse to three classes, because every dialect models
//! catalog / schema / table alike, and column / column-alias alike:
//!
//! - [`IdentifierCasing::table`] — catalog / schema / table names (the
//!   qualified name path of a stored table or view).
//! - [`IdentifierCasing::table_alias`] — table aliases and the
//!   synthetic names of CTEs / derived tables / table functions. Its
//!   own class because BigQuery folds aliases case-insensitively while
//!   keeping tables case-sensitive, and MySQL ties aliases to the
//!   filesystem-dependent table rule — the two diverge, so neither
//!   `table` nor `column` can stand in for it.
//! - [`IdentifierCasing::column`] — column names and column aliases.
//!
//! Only matching uses the folded key; the surfaced
//! [`TableReference`](crate::reference::TableReference) /
//! [`ColumnReference`](crate::reference::ColumnReference) keep the
//! original identifier text.

use sqlparser::ast::Ident;
use sqlparser::dialect::{
    AnsiDialect, BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect, DuckDbDialect,
    HiveDialect, MsSqlDialect, MySqlDialect, OracleDialect, PostgreSqlDialect, RedshiftSqlDialect,
    SQLiteDialect, SnowflakeDialect,
};

/// How one identifier class folds before an equality comparison — the
/// per-class element of an [`IdentifierCasing`].
///
/// The four cases the cross-dialect matrix reduces to once
/// instance-specific models (filesystem-dependent, collation-dependent)
/// are resolved to a concrete choice. Only matching folds through this
/// rule; the surfaced identifier text is never rewritten by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseRule {
    /// Unquoted → upper-case; quoted → preserved (exact). ANSI /
    /// Oracle / Snowflake / DB2.
    Upper,
    /// Unquoted → lower-case; quoted → preserved (exact). PostgreSQL,
    /// and the generic default.
    Lower,
    /// Quoting ignored; comparison case-insensitive. DuckDB / Hive /
    /// Databricks; MySQL / BigQuery columns; BigQuery aliases.
    Insensitive,
    /// Quoting ignored; comparison case-sensitive (exact). ClickHouse
    /// (all identifiers) and BigQuery tables; also the safe fallback for
    /// filesystem-dependent real *table names*, where over-matching (a
    /// false merge of two distinct stored tables) is a data-correctness
    /// risk. Statement-local aliases don't use this *as a fallback* —
    /// they default lenient (see [`IdentifierCasing::table_alias`]) — but
    /// an engine that is definitively case-sensitive (ClickHouse) folds
    /// every class here.
    Sensitive,
}

impl CaseRule {
    /// Normalize `ident` to its comparison key under this fold.
    pub(crate) fn normalize(self, ident: &Ident) -> String {
        match self {
            CaseRule::Upper if ident.quote_style.is_none() => ident.value.to_ascii_uppercase(),
            CaseRule::Lower if ident.quote_style.is_none() => ident.value.to_ascii_lowercase(),
            // Quoted under Upper/Lower → preserved; Sensitive → always
            // preserved regardless of quoting.
            CaseRule::Upper | CaseRule::Lower | CaseRule::Sensitive => ident.value.clone(),
            // Quoting ignored; fold case away.
            CaseRule::Insensitive => ident.value.to_ascii_lowercase(),
        }
    }
}

/// The identifier-casing policy for an analysis, split by identifier
/// class. Build one with [`IdentifierCasing::for_dialect`] (the dialect's
/// default), [`IdentifierCasing::uniform`] (one rule for every class), or
/// the field literal, and pass it via `ExtractorOptions::with_casing` to a
/// `*_with_options` extractor to override the dialect default — e.g. to
/// model a deployment-specific collation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdentifierCasing {
    /// catalog / schema / table names.
    pub table: CaseRule,
    /// Table aliases and CTE / derived / table-function names.
    pub table_alias: CaseRule,
    /// Column names and column aliases.
    pub column: CaseRule,
}

impl IdentifierCasing {
    /// One [`CaseRule`] applied to every identifier class.
    pub const fn uniform(fold: CaseRule) -> Self {
        Self {
            table: fold,
            table_alias: fold,
            column: fold,
        }
    }

    /// Map a parsed dialect to its default casing. Unrecognised
    /// dialects fall back to the generic policy ([`CaseRule::Lower`]
    /// everywhere), which preserves the resolver's historical
    /// behaviour.
    ///
    /// Filesystem-dependent (MySQL table names) and collation-dependent
    /// (SQL Server) models can't be known from the dialect alone, so
    /// they resolve to a fixed default here: SQL Server to the common
    /// case-insensitive collation, MySQL table names to the
    /// false-merge-avoiding [`CaseRule::Sensitive`]. A future override
    /// API can refine these per deployment.
    pub fn for_dialect(dialect: &dyn Dialect) -> Self {
        if dialect.is::<PostgreSqlDialect>() {
            Self::uniform(CaseRule::Lower)
        } else if dialect.is::<AnsiDialect>()
            || dialect.is::<SnowflakeDialect>()
            || dialect.is::<OracleDialect>()
        {
            // Oracle folds nonquoted identifiers to upper-case and keeps
            // quoted ones case-sensitive — the ANSI rule.
            Self::uniform(CaseRule::Upper)
        } else if dialect.is::<DuckDbDialect>()
            || dialect.is::<SQLiteDialect>()
            || dialect.is::<HiveDialect>()
            || dialect.is::<DatabricksDialect>()
            || dialect.is::<RedshiftSqlDialect>()
        {
            // Hive and Databricks / Spark SQL resolve identifiers
            // case-insensitively by default (`spark.sql.caseSensitive`
            // defaults to false). Redshift, by default, folds *both*
            // unquoted and double-quoted identifiers to lower case
            // (case-insensitive) — `enable_case_sensitive_identifier` is
            // `false` out of the box; setting it to `true` would make
            // quoted identifiers case-sensitive (a Lower rule), a
            // per-deployment override not modelled here.
            Self::uniform(CaseRule::Insensitive)
        } else if dialect.is::<MsSqlDialect>() {
            // Default install collation is case-insensitive (e.g.
            // `*_CI_AS`); a CS collation would flip every class.
            Self::uniform(CaseRule::Insensitive)
        } else if dialect.is::<ClickHouseDialect>() {
            // All identifiers — database / table / column and aliases —
            // are case-sensitive (aliases are identifiers too); quoting
            // handles special characters, not case.
            Self::uniform(CaseRule::Sensitive)
        } else if dialect.is::<MySqlDialect>() {
            // Table names are filesystem-dependent (Unix CS / Win-mac
            // CI) → Sensitive fallback (avoid merging distinct stored
            // tables). Aliases are *also* FS-dependent, but they're
            // statement-local: a mismatched-case alias reference almost
            // always intends the same alias, so default them to the
            // lenient Insensitive rather than introduce a phantom
            // reference. Columns are definitively CI.
            Self {
                table: CaseRule::Sensitive,
                table_alias: CaseRule::Insensitive,
                column: CaseRule::Insensitive,
            }
        } else if dialect.is::<BigQueryDialect>() {
            // Tables case-sensitive, but aliases and columns are
            // case-insensitive.
            Self {
                table: CaseRule::Sensitive,
                table_alias: CaseRule::Insensitive,
                column: CaseRule::Insensitive,
            }
        } else {
            // GenericDialect and anything unrecognised: preserve the
            // resolver's historical lower-fold behaviour.
            Self::uniform(CaseRule::Lower)
        }
    }
}

impl Default for IdentifierCasing {
    fn default() -> Self {
        Self::uniform(CaseRule::Lower)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unquoted(s: &str) -> Ident {
        Ident::new(s)
    }

    fn quoted(s: &str) -> Ident {
        Ident::with_quote('"', s)
    }

    /// Two identifiers match under `fold` iff their normalized keys
    /// are equal.
    fn matches(fold: CaseRule, a: &Ident, b: &Ident) -> bool {
        fold.normalize(a) == fold.normalize(b)
    }

    /// The quoting matrix: each `(binding, reference)` pair is folded
    /// independently by its own quote flag, then compared. Columns are
    /// the four folds; `true` = the two identifiers match.
    ///
    /// | binding   | reference | Upper | Lower | Insensitive | Sensitive |
    /// |-----------|-----------|-------|-------|-------------|-----------|
    /// | `Users`   | `users`   | ✓     | ✓     | ✓           | ✗         |
    /// | `Users`   | `"users"` | ✗     | ✓     | ✓           | ✗         |
    /// | `Users`   | `"Users"` | ✗     | ✗     | ✓           | ✓         |
    /// | `"Users"` | `users`   | ✗     | ✗     | ✓           | ✗         |
    /// | `"Users"` | `"Users"` | ✓     | ✓     | ✓           | ✓         |
    /// | `"Users"` | `"users"` | ✗     | ✗     | ✓           | ✗         |
    #[test]
    fn quoting_matrix() {
        use CaseRule::{Insensitive, Lower, Sensitive, Upper};

        // (binding, reference) → [Upper, Lower, Insensitive, Sensitive]
        let cases: &[(Ident, Ident, [bool; 4])] = &[
            (
                unquoted("Users"),
                unquoted("users"),
                [true, true, true, false],
            ),
            (
                unquoted("Users"),
                quoted("users"),
                [false, true, true, false],
            ),
            (
                unquoted("Users"),
                quoted("Users"),
                [false, false, true, true],
            ),
            (
                quoted("Users"),
                unquoted("users"),
                [false, false, true, false],
            ),
            (quoted("Users"), quoted("Users"), [true, true, true, true]),
            (
                quoted("Users"),
                quoted("users"),
                [false, false, true, false],
            ),
        ];

        for (binding, reference, [up, lo, ci, cs]) in cases {
            assert_eq!(
                matches(Upper, binding, reference),
                *up,
                "Upper: {binding:?} vs {reference:?}"
            );
            assert_eq!(
                matches(Lower, binding, reference),
                *lo,
                "Lower: {binding:?} vs {reference:?}"
            );
            assert_eq!(
                matches(Insensitive, binding, reference),
                *ci,
                "Insensitive: {binding:?} vs {reference:?}"
            );
            assert_eq!(
                matches(Sensitive, binding, reference),
                *cs,
                "Sensitive: {binding:?} vs {reference:?}"
            );
        }
    }

    /// Each segment carries its own quote flag, so a mixed qualifier
    /// like `"Schema".table` folds segment-by-segment.
    #[test]
    fn per_segment_quoting_is_independent() {
        // Under Lower: quoted segment preserved, unquoted folded.
        assert_eq!(CaseRule::Lower.normalize(&quoted("Schema")), "Schema");
        assert_eq!(CaseRule::Lower.normalize(&unquoted("Table")), "table");
        // Under Sensitive: quoting ignored, both preserved.
        assert_eq!(CaseRule::Sensitive.normalize(&quoted("Schema")), "Schema");
        assert_eq!(CaseRule::Sensitive.normalize(&unquoted("Table")), "Table");
        // Under Insensitive: quoting ignored, both folded.
        assert_eq!(CaseRule::Insensitive.normalize(&quoted("Schema")), "schema");
        assert_eq!(CaseRule::Insensitive.normalize(&unquoted("Table")), "table");
    }

    /// `for_dialect` maps every recognised dialect to its
    /// identifier-casing policy. Homogeneous dialects fold every class
    /// alike; MySQL and BigQuery *split* — real table names are stricter
    /// (`Sensitive`) than statement-local aliases / columns
    /// (`Insensitive`). Anything unrecognised falls back to the generic
    /// lower-fold. Covers all `sqlparser` dialect structs (an `is::<…>()`
    /// ladder has no compile-time exhaustiveness, so the table stands in
    /// as arm coverage).
    ///
    /// | dialect    | table       | table_alias | column      |
    /// |------------|-------------|-------------|-------------|
    /// | PostgreSQL | Lower       | Lower       | Lower       |
    /// | Redshift   | Insensitive | Insensitive | Insensitive |
    /// | ANSI       | Upper       | Upper       | Upper       |
    /// | Snowflake  | Upper       | Upper       | Upper       |
    /// | Oracle     | Upper       | Upper       | Upper       |
    /// | DuckDB     | Insensitive | Insensitive | Insensitive |
    /// | SQLite     | Insensitive | Insensitive | Insensitive |
    /// | SQL Server | Insensitive | Insensitive | Insensitive |
    /// | Hive       | Insensitive | Insensitive | Insensitive |
    /// | Databricks | Insensitive | Insensitive | Insensitive |
    /// | ClickHouse | Sensitive   | Sensitive   | Sensitive   |
    /// | MySQL      | Sensitive   | Insensitive | Insensitive |
    /// | BigQuery   | Sensitive   | Insensitive | Insensitive |
    /// | Generic    | Lower       | Lower       | Lower       |
    #[test]
    fn dialect_casing_matrix() {
        use sqlparser::dialect::GenericDialect;
        use CaseRule::{Insensitive, Lower, Sensitive, Upper};

        let uniform = |fold| IdentifierCasing {
            table: fold,
            table_alias: fold,
            column: fold,
        };
        // MySQL / BigQuery: real tables stricter than aliases / columns.
        let split = IdentifierCasing {
            table: Sensitive,
            table_alias: Insensitive,
            column: Insensitive,
        };

        let cases: Vec<(&str, Box<dyn Dialect>, IdentifierCasing)> = vec![
            ("PostgreSQL", Box::new(PostgreSqlDialect {}), uniform(Lower)),
            (
                "Redshift",
                Box::new(RedshiftSqlDialect {}),
                uniform(Insensitive),
            ),
            ("ANSI", Box::new(AnsiDialect {}), uniform(Upper)),
            ("Snowflake", Box::new(SnowflakeDialect {}), uniform(Upper)),
            ("Oracle", Box::new(OracleDialect {}), uniform(Upper)),
            ("DuckDB", Box::new(DuckDbDialect {}), uniform(Insensitive)),
            ("SQLite", Box::new(SQLiteDialect {}), uniform(Insensitive)),
            (
                "SQL Server",
                Box::new(MsSqlDialect {}),
                uniform(Insensitive),
            ),
            ("Hive", Box::new(HiveDialect {}), uniform(Insensitive)),
            (
                "Databricks",
                Box::new(DatabricksDialect {}),
                uniform(Insensitive),
            ),
            (
                "ClickHouse",
                Box::new(ClickHouseDialect {}),
                uniform(Sensitive),
            ),
            ("MySQL", Box::new(MySqlDialect {}), split),
            ("BigQuery", Box::new(BigQueryDialect {}), split),
            ("Generic", Box::new(GenericDialect {}), uniform(Lower)),
        ];

        for (name, dialect, expected) in cases {
            assert_eq!(
                IdentifierCasing::for_dialect(dialect.as_ref()),
                expected,
                "{name}"
            );
        }
    }
}
