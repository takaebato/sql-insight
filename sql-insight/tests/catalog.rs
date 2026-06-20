//! `Catalog::from_ddl` — verified end-to-end by building a catalog from
//! DDL and observing how a query resolves against it (the catalog's
//! contents aren't publicly inspectable, so behavior is the contract).

use sql_insight::catalog::Catalog;
use sql_insight::extractor::{extract_column_operations_with_options, ExtractorOptions};
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::{ColumnRead, ResolutionKind};

fn reads_with(ddl: &str, query: &str) -> Vec<ColumnRead> {
    let catalog = Catalog::from_ddl(&GenericDialect {}, ddl, "public").unwrap();
    extract_column_operations_with_options(
        &GenericDialect {},
        query,
        ExtractorOptions::new().with_catalog(&catalog),
    )
    .unwrap()
    .remove(0)
    .unwrap()
    .reads
}

fn find<'a>(reads: &'a [ColumnRead], col: &str) -> &'a ColumnRead {
    reads
        .iter()
        .find(|r| r.reference.name.value == col)
        .unwrap_or_else(|| panic!("no read for column `{col}` in {reads:#?}"))
}

#[test]
fn unqualified_ddl_registers_under_default_schema_and_canonicalizes() {
    // `CREATE TABLE users` (no schema) registers as `public.users`, so a
    // bare `users` query right-anchor-matches it: Cataloged + canonical.
    let read = {
        let reads = reads_with(
            "CREATE TABLE users (id INT, name TEXT)",
            "SELECT name FROM users",
        );
        find(&reads, "name").clone()
    };
    assert_eq!(read.resolution, ResolutionKind::Cataloged);
    let table = read.reference.table.unwrap();
    assert_eq!(table.schema.unwrap().value, "public");
    assert_eq!(table.name.value, "users");
}

#[test]
fn qualified_ddl_keeps_its_own_schema() {
    let reads = reads_with(
        "CREATE TABLE app.orders (id INT, total NUMERIC)",
        "SELECT total FROM app.orders",
    );
    let read = find(&reads, "total");
    assert_eq!(read.resolution, ResolutionKind::Cataloged);
    assert_eq!(
        read.reference
            .table
            .as_ref()
            .unwrap()
            .schema
            .as_ref()
            .unwrap()
            .value,
        "app"
    );
}

#[test]
fn column_resolution_is_strict_against_registered_columns() {
    // `missing` is not among the registered columns, so the catalog
    // rejects it: Unresolved (table dropped), not silently Inferred.
    let reads = reads_with(
        "CREATE TABLE users (id INT, name TEXT)",
        "SELECT missing FROM users",
    );
    let read = find(&reads, "missing");
    assert_eq!(read.resolution, ResolutionKind::Unresolved);
    assert!(read.reference.table.is_none());
}

#[test]
fn ctas_without_column_list_is_skipped() {
    // `CREATE TABLE ... AS SELECT` declares no column list, so `derived`
    // is not registered — a query against it stays open-world Inferred.
    let reads = reads_with(
        "CREATE TABLE derived AS SELECT id FROM src",
        "SELECT x FROM derived",
    );
    let read = find(&reads, "x");
    assert_eq!(read.resolution, ResolutionKind::Inferred);
}

#[test]
fn non_create_table_statements_are_ignored() {
    // A DDL file may interleave other statements; only CREATE TABLE with
    // columns registers. `users` still resolves Cataloged.
    let reads = reads_with(
        "INSERT INTO logs VALUES (1); CREATE TABLE users (id INT)",
        "SELECT id FROM users",
    );
    assert_eq!(find(&reads, "id").resolution, ResolutionKind::Cataloged);
}

#[test]
fn invalid_ddl_returns_error() {
    assert!(Catalog::from_ddl(&GenericDialect {}, "CREATE TABLE", "public").is_err());
}
