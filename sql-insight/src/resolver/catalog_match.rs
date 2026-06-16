//! Catalog matching: turning a written table reference into a registered
//! table's canonical identity + column list, under the dialect's table
//! casing. [`TableMatch`] is the one-shape outcome; the free functions are
//! the (default-fill, right-anchored, cased) matching mechanics. The
//! [`Binder::table_match`](super::binder) method drives them.

use sqlparser::ast::Ident;

use crate::casing::CaseFold;
use crate::catalog::{Catalog, CatalogTable};
use crate::reference::{ResolutionKind, TableReference};

/// The outcome of matching a written table reference against the catalog —
/// one shape for every case, so a `Scan` gets its identity, table-level
/// [`ResolutionKind`], and (when known) column list from a single match.
pub(super) struct TableMatch {
    /// Canonical identity for a unique hit; the written reference for an
    /// ambiguous / missing / catalog-free match.
    pub(super) table: TableReference,
    /// `Cataloged` (unique hit), `Ambiguous` (several), or `Inferred`
    /// (no hit / no catalog).
    pub(super) resolution: ResolutionKind,
    /// The registered column names for a unique hit that declared them;
    /// empty otherwise (schema-known-columns-unknown, or no hit).
    pub(super) columns: Vec<Ident>,
}

/// Fill a query reference's missing prefix segments from the catalog's
/// defaults before matching (bare → schema then catalog; catalog only
/// once a schema is present). Filled segments are quoted (exact).
pub(super) fn fill_query_defaults(written: &TableReference, catalog: &Catalog) -> TableReference {
    let mut filled = written.clone();
    if filled.schema.is_none() {
        if let Some(schema) = catalog.default_schema_segment() {
            filled.schema = Some(Ident::with_quote('"', schema));
        }
    }
    if filled.catalog.is_none() && filled.schema.is_some() {
        if let Some(catalog_segment) = catalog.default_catalog_segment() {
            filled.catalog = Some(Ident::with_quote('"', catalog_segment));
        }
    }
    filled
}

/// Right-anchored, dialect-cased match of a (default-filled) query
/// reference against a registered table. Catalog identifiers are
/// compared as exact (quoted) — see the casing notes.
pub(super) fn catalog_table_matches(
    query: &TableReference,
    table: &CatalogTable,
    fold: CaseFold,
) -> bool {
    if fold.normalize(&query.name) != normalize_catalog(table.name_segment(), fold) {
        return false;
    }
    if let Some(schema) = &query.schema {
        if fold.normalize(schema) != normalize_catalog(table.schema_segment(), fold) {
            return false;
        }
    }
    match (&query.catalog, table.catalog_segment()) {
        (Some(query_catalog), Some(table_catalog)) => {
            fold.normalize(query_catalog) == normalize_catalog(table_catalog, fold)
        }
        _ => true,
    }
}

/// Fold a catalog-side string as an exact (quoted) identifier.
fn normalize_catalog(segment: &str, fold: CaseFold) -> String {
    fold.normalize(&Ident::with_quote('"', segment))
}

/// The surfaced canonical identity of a matched table: plain (unquoted)
/// idents so reads / writes compare naturally.
pub(super) fn canonical_ref(table: &CatalogTable) -> TableReference {
    TableReference {
        catalog: table.catalog_segment().map(Ident::new),
        schema: Some(Ident::new(table.schema_segment())),
        name: Ident::new(table.name_segment()),
    }
}
