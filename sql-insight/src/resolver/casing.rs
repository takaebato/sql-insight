//! Per-dialect identifier case-folding policy.
//!
//! SQL engines disagree on how identifiers compare for equality:
//! whether an unquoted name folds to upper- or lower-case, whether a
//! quoted name is then case-sensitive, and whether quoting matters at
//! all. The resolver matches identifiers (table qualifiers, column
//! names, aliases) through [`BindingKey`](super::binding::BindingKey),
//! so it needs to know
//! the active dialect's rule to decide e.g. whether `Users` and
//! `users` are the same table.
//!
//! This folds the (well-surveyed) cross-dialect matrix down to a
//! [`CaseFold`] per identifier *class*. The six syntactic positions
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
    AnsiDialect, BigQueryDialect, Dialect, DuckDbDialect, MsSqlDialect, MySqlDialect,
    PostgreSqlDialect, RedshiftSqlDialect, SQLiteDialect, SnowflakeDialect,
};

/// How a single identifier folds before an equality comparison.
///
/// The four cases the cross-dialect matrix reduces to once
/// instance-specific models (filesystem-dependent, collation-dependent)
/// are resolved to a concrete choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaseFold {
    /// Unquoted → upper-case; quoted → preserved (exact). ANSI /
    /// Oracle / Snowflake / DB2.
    Upper,
    /// Unquoted → lower-case; quoted → preserved (exact). PostgreSQL,
    /// and the generic default.
    Lower,
    /// Quoting ignored; comparison case-insensitive. DuckDB; MySQL /
    /// BigQuery columns; BigQuery aliases.
    Insensitive,
    /// Quoting ignored; comparison case-sensitive (exact). BigQuery
    /// tables; the safe fallback for filesystem-dependent real *table
    /// names*, where over-matching (a false merge of two distinct
    /// stored tables) is a data-correctness risk. Statement-local
    /// aliases don't use this — they default lenient (see
    /// [`IdentifierCasing::table_alias`]).
    Sensitive,
}

impl CaseFold {
    /// Normalize `ident` to its comparison key under this fold.
    pub(crate) fn normalize(self, ident: &Ident) -> String {
        match self {
            CaseFold::Upper if ident.quote_style.is_none() => ident.value.to_ascii_uppercase(),
            CaseFold::Lower if ident.quote_style.is_none() => ident.value.to_ascii_lowercase(),
            // Quoted under Upper/Lower → preserved; Sensitive → always
            // preserved regardless of quoting.
            CaseFold::Upper | CaseFold::Lower | CaseFold::Sensitive => ident.value.clone(),
            // Quoting ignored; fold case away.
            CaseFold::Insensitive => ident.value.to_ascii_lowercase(),
        }
    }
}

/// The active dialect's identifier-folding policy, split by class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IdentifierCasing {
    /// catalog / schema / table names.
    pub(crate) table: CaseFold,
    /// Table aliases and CTE / derived / table-function names.
    pub(crate) table_alias: CaseFold,
    /// Column names and column aliases.
    pub(crate) column: CaseFold,
}

impl IdentifierCasing {
    const fn uniform(fold: CaseFold) -> Self {
        Self {
            table: fold,
            table_alias: fold,
            column: fold,
        }
    }

    /// Map a parsed dialect to its default casing. Unrecognised
    /// dialects fall back to the generic policy ([`CaseFold::Lower`]
    /// everywhere), which preserves the resolver's historical
    /// behaviour.
    ///
    /// Filesystem-dependent (MySQL table names) and collation-dependent
    /// (SQL Server) models can't be known from the dialect alone, so
    /// they resolve to a fixed default here: SQL Server to the common
    /// case-insensitive collation, MySQL table names to the
    /// false-merge-avoiding [`CaseFold::Sensitive`]. A future override
    /// API can refine these per deployment.
    pub(crate) fn for_dialect(dialect: &dyn Dialect) -> Self {
        if dialect.is::<PostgreSqlDialect>() || dialect.is::<RedshiftSqlDialect>() {
            Self::uniform(CaseFold::Lower)
        } else if dialect.is::<AnsiDialect>() || dialect.is::<SnowflakeDialect>() {
            Self::uniform(CaseFold::Upper)
        } else if dialect.is::<DuckDbDialect>() || dialect.is::<SQLiteDialect>() {
            Self::uniform(CaseFold::Insensitive)
        } else if dialect.is::<MsSqlDialect>() {
            // Default install collation is case-insensitive (e.g.
            // `*_CI_AS`); a CS collation would flip every class.
            Self::uniform(CaseFold::Insensitive)
        } else if dialect.is::<MySqlDialect>() {
            // Table names are filesystem-dependent (Unix CS / Win-mac
            // CI) → Sensitive fallback (avoid merging distinct stored
            // tables). Aliases are *also* FS-dependent, but they're
            // statement-local: a mismatched-case alias reference almost
            // always intends the same alias, so default them to the
            // lenient Insensitive rather than introduce a phantom
            // reference. Columns are definitively CI.
            Self {
                table: CaseFold::Sensitive,
                table_alias: CaseFold::Insensitive,
                column: CaseFold::Insensitive,
            }
        } else if dialect.is::<BigQueryDialect>() {
            // Tables case-sensitive, but aliases and columns are
            // case-insensitive.
            Self {
                table: CaseFold::Sensitive,
                table_alias: CaseFold::Insensitive,
                column: CaseFold::Insensitive,
            }
        } else {
            // GenericDialect and anything unrecognised: preserve the
            // resolver's historical lower-fold behaviour.
            Self::uniform(CaseFold::Lower)
        }
    }
}

impl Default for IdentifierCasing {
    fn default() -> Self {
        Self::uniform(CaseFold::Lower)
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
    fn matches(fold: CaseFold, a: &Ident, b: &Ident) -> bool {
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
        use CaseFold::{Insensitive, Lower, Sensitive, Upper};

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
        assert_eq!(CaseFold::Lower.normalize(&quoted("Schema")), "Schema");
        assert_eq!(CaseFold::Lower.normalize(&unquoted("Table")), "table");
        // Under Sensitive: quoting ignored, both preserved.
        assert_eq!(CaseFold::Sensitive.normalize(&quoted("Schema")), "Schema");
        assert_eq!(CaseFold::Sensitive.normalize(&unquoted("Table")), "Table");
        // Under Insensitive: quoting ignored, both folded.
        assert_eq!(CaseFold::Insensitive.normalize(&quoted("Schema")), "schema");
        assert_eq!(CaseFold::Insensitive.normalize(&unquoted("Table")), "table");
    }
}
