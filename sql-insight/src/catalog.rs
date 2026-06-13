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
//! don't reimplement that subtlety. The resolver performs the matching
//! itself when it walks a statement.
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

/// One table registered in a [`Catalog`]: a `(catalog?, schema, name)`
/// identity plus its column names.
///
/// Registration requires the full identity — `schema` and `name` are
/// mandatory (`catalog` is optional, for engines without a catalog
/// layer). This keeps the catalog unambiguous ground truth: query
/// references may omit qualifiers (resolved by right-anchoring /
/// defaults), but a registered table never does. All identifiers are
/// stored verbatim and matched exactly (see the module docs); `columns`
/// may be empty when the schema is known but its columns aren't.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogTable {
    catalog: Option<String>,
    schema: String,
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
            schema: schema.into(),
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

    pub(crate) fn schema_segment(&self) -> &str {
        &self.schema
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
        match &self.catalog {
            Some(c) => write!(f, "{c}.{}.{}", self.schema, self.name),
            None => write!(f, "{}.{}", self.schema, self.name),
        }
    }
}
