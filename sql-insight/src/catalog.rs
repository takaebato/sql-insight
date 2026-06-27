//! Optional schema provider plugged into the resolver.
//!
//! A [`Catalog`] is an *enrichment* input: structural resolution (CTE /
//! derived table schemas, FROM alias bindings) works catalog-free, and
//! a catalog only fills in the columns — and canonical identity — of
//! real tables the resolver could not derive from the SQL alone. With
//! no catalog those holes stay schema-unknown and surface as
//! [`Inferred`](crate::ResolutionKind::Inferred).
//!
//! It is a **concrete, eager registry**, not a callback: the consumer
//! builds it up front (typically from an `information_schema` dump,
//! migration files, or `CREATE TABLE` statements) and the resolver
//! matches query table references against it. The resolver — not the
//! consumer — owns identifier matching: a query reference matches a
//! registered table by **right-anchored, dialect-cased** comparison
//! (a bare `users` matches a registered `mydb.users`), so consumers
//! don't reimplement that subtlety.
//!
//! **Open-world.** A table the catalog doesn't contain is taken as
//! *schema unknown*, not *nonexistent* — it still surfaces as an
//! ordinary read / write, just `Inferred`. A misspelled / unregistered
//! table is never flagged at table granularity.
//!
//! **Identifiers are exact.** Registered names are the catalog's ground
//! truth (the stored identifiers), so they compare *exactly* under
//! case-sensitive dialect folds and fold only under case-insensitive
//! ones — i.e. they behave like quoted identifiers. Register the names
//! as actually stored (e.g. what `information_schema` reports); the
//! resolver's dialect-casing policy governs the comparison.

use crate::casing::IdentifierCasing;
use crate::error::Error;
use sqlparser::ast::{Ident, Statement};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;
use std::fmt;

/// A concrete, eager schema registry. Build it with [`Catalog::new`]
/// and [`Catalog::table`] (or collect an iterator of [`CatalogTable`]),
/// then hand `Some(&catalog)` to an extractor.
///
/// Internally a flat list of [`CatalogTable`]s — the resolver scans it
/// with right-anchored, cased matching, so there is no name-keyed
/// index (a bare `users` may match several `*.users` entries, which is
/// not a hashable equivalence). The optional default catalog / schema
/// fill a bare or partially-qualified query reference before matching
/// (like a single-entry search path); when unset, matching stays
/// best-effort right-anchored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    tables: Vec<CatalogTable>,
    default_catalog: Option<String>,
    default_schema: Option<String>,
}

impl Catalog {
    /// An empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one registered table. Returns `self` for chaining.
    pub fn table(mut self, table: CatalogTable) -> Self {
        self.tables.push(table);
        self
    }

    /// Set the default catalog used to fill a query reference that
    /// omits its catalog segment before matching. Returns `self`.
    pub fn default_catalog(mut self, catalog: impl Into<String>) -> Self {
        self.default_catalog = Some(catalog.into());
        self
    }

    /// Set the default schema used to fill a bare query reference
    /// before matching. Returns `self`.
    pub fn default_schema(mut self, schema: impl Into<String>) -> Self {
        self.default_schema = Some(schema.into());
        self
    }

    /// Build a catalog from SQL DDL: every `CREATE TABLE` with an explicit
    /// column list becomes a registered table — its `[catalog.]schema.name`
    /// path and column names. Column *types* and constraints are ignored
    /// (only names are needed), so dialect-specific type syntax doesn't
    /// matter as long as the statement parses. `ddl` is parsed with
    /// `dialect`, so pass the dialect matching your schema dump.
    ///
    /// An unqualified `CREATE TABLE t` registers *schema-less* (no schema
    /// is fabricated); right-anchored matching still lets a bare query `t`
    /// — or even a qualified `s.t` — resolve to it. Skipped (not
    /// registered): statements that aren't `CREATE TABLE`, `CREATE TABLE`s
    /// with no column definitions (`... AS SELECT`, `... LIKE ...`), and
    /// names with more than three `catalog.schema.name` segments. A parse
    /// failure (the DDL is invalid for `dialect`) returns `Err`.
    ///
    /// **Only `CREATE TABLE` is interpreted.** Session-default statements
    /// (`USE ...`, `SET search_path ...`) are *not* read — query-side
    /// defaults are the caller's responsibility via
    /// [`Catalog::default_schema`] / [`Catalog::default_catalog`].
    ///
    /// ```rust
    /// use sql_insight::catalog::Catalog;
    /// use sql_insight::sqlparser::dialect::GenericDialect;
    ///
    /// let ddl = "CREATE TABLE users (id INT, name TEXT); \
    ///            CREATE TABLE app.orders (id INT, total NUMERIC);";
    /// let catalog = Catalog::from_ddl(&GenericDialect {}, ddl).unwrap();
    /// // `users` registered schema-less; `app.orders` keeps its schema.
    /// ```
    pub fn from_ddl(dialect: &dyn Dialect, ddl: &str) -> Result<Self, Error> {
        Self::from_ddl_with_casing(dialect, ddl, IdentifierCasing::for_dialect(dialect))
    }

    /// Like [`from_ddl`](Self::from_ddl) but normalizes the stored identifiers
    /// with an explicit `casing` instead of the dialect default. Pass the same
    /// [`IdentifierCasing`] you give the extractor via
    /// [`ExtractorOptions::with_casing`](crate::extractor::ExtractorOptions::with_casing),
    /// so the catalog's canonical form matches what a query reference folds to:
    /// `from_ddl` (dialect default) only matches when the extraction also uses
    /// the default, so a case-sensitive override needs this to resolve.
    pub fn from_ddl_with_casing(
        dialect: &dyn Dialect,
        ddl: &str,
        casing: IdentifierCasing,
    ) -> Result<Self, Error> {
        let statements = Parser::parse_sql(dialect, ddl)?;
        // Normalize each DDL identifier to its stored form the way the dialect
        // would: an unquoted name folds (e.g. Postgres `Users` → `users`), a
        // quoted name stays exact. So the registered identity matches what a
        // query reference folds to — otherwise an unquoted mixed-case
        // `CREATE TABLE Users` would register `Users` and miss a folded query
        // `users` under a case-sensitive dialect. (Catalog names compare like
        // quoted identifiers, so they must already be in canonical form.)
        let mut catalog = Catalog::new();
        for statement in &statements {
            let Statement::CreateTable(create) = statement else {
                continue;
            };
            // No column definitions → CTAS / LIKE: nothing to register.
            if create.columns.is_empty() {
                continue;
            }
            let parts: Vec<&Ident> = create.name.0.iter().filter_map(|p| p.as_ident()).collect();
            let name = |id: &Ident| casing.table.normalize(id);
            let table = match parts.as_slice() {
                [n] => CatalogTable::unqualified(name(n)),
                [schema, n] => CatalogTable::new(name(schema), name(n)),
                [catalog_seg, schema, n] => {
                    CatalogTable::new(name(schema), name(n)).catalog(name(catalog_seg))
                }
                // 0 or 4+ segments — unrepresentable identity.
                _ => continue,
            };
            let columns = create
                .columns
                .iter()
                .map(|c| casing.column.normalize(&c.name));
            catalog = catalog.table(table.columns(columns));
        }
        Ok(catalog)
    }

    /// The registered tables, in registration order.
    pub(crate) fn tables(&self) -> &[CatalogTable] {
        &self.tables
    }

    /// The default catalog segment, if configured.
    pub(crate) fn default_catalog_segment(&self) -> Option<&str> {
        self.default_catalog.as_deref()
    }

    /// The default schema segment, if configured.
    pub(crate) fn default_schema_segment(&self) -> Option<&str> {
        self.default_schema.as_deref()
    }
}

impl FromIterator<CatalogTable> for Catalog {
    fn from_iter<I: IntoIterator<Item = CatalogTable>>(iter: I) -> Self {
        Self {
            tables: iter.into_iter().collect(),
            default_catalog: None,
            default_schema: None,
        }
    }
}

/// One table registered in a [`Catalog`]: a `(catalog?, schema?, name)`
/// identity plus its column names.
///
/// `name` is mandatory; `schema` and `catalog` are optional — a bare
/// table (e.g. from unqualified DDL) has neither, and engines without a
/// catalog layer omit the catalog. An omitted segment matches *any* query
/// value there (right-anchored, the same wildcard rule a query reference
/// gets for its own omitted qualifiers), so a schema-less `users` matches
/// both a bare `users` and a qualified `public.users`. Identifiers are
/// stored verbatim; a *present* segment compares exactly (folding only
/// under case-insensitive dialects — see the module docs). `columns` may
/// be empty when the table is known but its columns aren't.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogTable {
    catalog: Option<String>,
    schema: Option<String>,
    name: String,
    columns: Vec<String>,
}

impl CatalogTable {
    /// A table identified by `schema.name`, with no columns yet and no
    /// catalog segment. Add columns with [`Self::columns`] and a
    /// catalog with [`Self::catalog`].
    pub fn new(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            catalog: None,
            schema: Some(schema.into()),
            name: name.into(),
            columns: Vec::new(),
        }
    }

    /// A table with no schema (e.g. from unqualified DDL). The omitted
    /// schema is a wildcard, so it matches a query by name alone — bare
    /// `users` and qualified `public.users` both match. Add columns with
    /// [`Self::columns`].
    pub fn unqualified(name: impl Into<String>) -> Self {
        Self {
            catalog: None,
            schema: None,
            name: name.into(),
            columns: Vec::new(),
        }
    }

    /// Set the catalog segment (for engines with a catalog layer).
    pub fn catalog(mut self, catalog: impl Into<String>) -> Self {
        self.catalog = Some(catalog.into());
        self
    }

    /// Set the column names. Replaces any previously set columns.
    pub fn columns<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.columns = columns.into_iter().map(Into::into).collect();
        self
    }

    pub(crate) fn catalog_segment(&self) -> Option<&str> {
        self.catalog.as_deref()
    }

    pub(crate) fn schema_segment(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    pub(crate) fn name_segment(&self) -> &str {
        &self.name
    }

    pub(crate) fn column_names(&self) -> &[String] {
        &self.columns
    }
}

impl fmt::Display for CatalogTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = [
            self.catalog.as_deref(),
            self.schema.as_deref(),
            Some(self.name.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(".");
        write!(f, "{path}")
    }
}
