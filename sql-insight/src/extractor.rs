//! Extraction APIs at four granularities of "what does this SQL touch?"
//!
//! Each sub-extractor is a thin wrapper around the bound-plan analysis
//! engine, projecting the resolved plan into a different surface:
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

// The statement classifier is shared by the column / flat extractors to
// pick the statement verb before assembling their surfaces.
pub(crate) use table_operation_extractor::classify_statement;

use crate::casing::IdentifierCasing;
use crate::catalog::Catalog;
use sqlparser::dialect::Dialect;

/// Optional inputs shared by every `*_with_options` extractor. Defaults
/// to no catalog and the dialect-derived identifier casing — i.e. the
/// plain `extract_*(dialect, sql)` behaviour.
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
/// use sql_insight::extractor::{extract_table_operations_with_options, ExtractorOptions};
/// use sql_insight::{CaseRule, IdentifierCasing};
///
/// let dialect = GenericDialect {};
/// let options = ExtractorOptions::new().with_casing(IdentifierCasing::uniform(CaseRule::Sensitive));
/// let result = extract_table_operations_with_options(&dialect, "SELECT * FROM users", options).unwrap();
/// assert_eq!(result[0].as_ref().unwrap().reads.len(), 1);
/// ```
#[derive(Default, Clone, Debug)]
pub struct ExtractorOptions<'a> {
    /// The schema to resolve against. With a catalog, matched tables are
    /// canonicalized to their registered path and column resolution is
    /// strict; without one (the default), references stay as written and
    /// resolution is inferred.
    pub catalog: Option<&'a Catalog>,
    /// Override the dialect-derived identifier casing. `None` (the
    /// default) derives it from the dialect via
    /// [`IdentifierCasing::for_dialect`] — set this to model a
    /// deployment-specific collation.
    pub casing: Option<IdentifierCasing>,
}

impl<'a> ExtractorOptions<'a> {
    /// Default options: no catalog, dialect-derived casing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve against `catalog`.
    pub fn with_catalog(mut self, catalog: &'a Catalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// Override the identifier casing (otherwise derived from the dialect).
    pub fn with_casing(mut self, casing: IdentifierCasing) -> Self {
        self.casing = Some(casing);
        self
    }

    /// The effective casing: the override if set, else the dialect default.
    pub(crate) fn casing_for(&self, dialect: &dyn Dialect) -> IdentifierCasing {
        self.casing
            .unwrap_or_else(|| IdentifierCasing::for_dialect(dialect))
    }
}
