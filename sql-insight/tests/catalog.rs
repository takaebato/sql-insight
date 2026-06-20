//! `Catalog::from_ddl` — verified end-to-end by building a catalog from
//! DDL and observing how a query resolves against it (the catalog's
//! contents aren't publicly inspectable, so behavior is the contract).

use sql_insight::catalog::Catalog;
use sql_insight::extractor::{extract_column_operations_with_options, ExtractorOptions};
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::{ColumnRead, ResolutionKind};

fn reads_with(ddl: &str, query: &str) -> Vec<ColumnRead> {
    let catalog = Catalog::from_ddl(&GenericDialect {}, ddl).unwrap();
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
fn unqualified_ddl_registers_schema_less_and_resolves_bare_query() {
    // `CREATE TABLE users` (no schema) registers schema-less — no schema
    // is fabricated. A bare `users` query still matches (Cataloged) and
    // the surfaced identity stays bare (no injected `public`).
    let reads = reads_with(
        "CREATE TABLE users (id INT, name TEXT)",
        "SELECT name FROM users",
    );
    let read = find(&reads, "name");
    assert_eq!(read.resolution, ResolutionKind::Cataloged);
    let table = read.reference.table.as_ref().unwrap();
    assert!(
        table.schema.is_none(),
        "schema-less stays bare, got {table:?}"
    );
    assert_eq!(table.name.value, "users");
}

#[test]
fn schema_less_table_matches_a_qualified_query_by_wildcard() {
    // The omitted schema on the registered side is a wildcard, so a
    // qualified `public.users` reference still matches the schema-less
    // `users` and canonicalizes to the registered (bare) identity.
    let reads = reads_with(
        "CREATE TABLE users (id INT)",
        "SELECT public.users.id FROM users",
    );
    let read = find(&reads, "id");
    assert_eq!(read.resolution, ResolutionKind::Cataloged);
    let table = read.reference.table.as_ref().unwrap();
    assert!(table.schema.is_none());
    assert_eq!(table.name.value, "users");
}

#[test]
fn catalog_table_unqualified_matches_bare_and_qualified_queries() {
    // `CatalogTable::unqualified` directly (not via DDL): a schema-less
    // entry matches both a bare and a qualified query by name.
    use sql_insight::catalog::CatalogTable;
    let catalog = Catalog::new().table(CatalogTable::unqualified("users").columns(["id"]));
    for query in ["SELECT id FROM users", "SELECT id FROM public.users"] {
        let reads = extract_column_operations_with_options(
            &GenericDialect {},
            query,
            ExtractorOptions::new().with_catalog(&catalog),
        )
        .unwrap()
        .remove(0)
        .unwrap()
        .reads;
        let read = find(&reads, "id");
        assert_eq!(read.resolution, ResolutionKind::Cataloged, "for `{query}`");
        assert!(
            read.reference.table.as_ref().unwrap().schema.is_none(),
            "for `{query}`"
        );
    }
}

#[test]
fn default_schema_qualifies_bare_ref_without_registered_tables() {
    // No registered tables, just `default_schema = public`: a bare `users`
    // surfaces qualified as `public.users` (Inferred) — the declared
    // default is applied to the resolved identity, not just to matching.
    let catalog = Catalog::new().default_schema("public");
    let reads = extract_column_operations_with_options(
        &GenericDialect {},
        "SELECT id FROM users",
        ExtractorOptions::new().with_catalog(&catalog),
    )
    .unwrap()
    .remove(0)
    .unwrap()
    .reads;
    let read = find(&reads, "id");
    assert_eq!(read.resolution, ResolutionKind::Inferred);
    let table = read.reference.table.as_ref().unwrap();
    assert_eq!(table.schema.as_ref().unwrap().value, "public");
    assert_eq!(table.name.value, "users");
}

#[test]
fn default_schema_discriminates_a_mismatched_qualifier() {
    // `FROM users` resolves as `public.users` (via the default), so a column
    // qualified with a *different* schema (`db.users`) no longer matches the
    // relation — it surfaces Unresolved instead of silently binding.
    let catalog = Catalog::new().default_schema("public");
    let reads = extract_column_operations_with_options(
        &GenericDialect {},
        "SELECT db.users.id FROM users",
        ExtractorOptions::new().with_catalog(&catalog),
    )
    .unwrap()
    .remove(0)
    .unwrap()
    .reads;
    let read = find(&reads, "id");
    assert_eq!(read.resolution, ResolutionKind::Unresolved);
    assert!(read.reference.table.is_none());
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
    assert!(Catalog::from_ddl(&GenericDialect {}, "CREATE TABLE").is_err());
}
