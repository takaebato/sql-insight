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
fn default_schema_is_matched_case_exactly_against_the_registration() {
    // A configured `default_schema` is a catalog-side schema *name* — the same
    // kind of case-exact stored identifier `CatalogTable` registers — so it is
    // matched case-exactly, not folded. Declaring `default_schema("public")`
    // against a registered `public` table resolves a bare `users` (Cataloged);
    // a mismatched-case default (`"PUBLIC"`) is a caller-side inconsistency and
    // correctly *fails* to match (Inferred), even though an inline unquoted
    // `PUBLIC.users` — query text, which folds — still hits. The two live on
    // different layers (catalog config vs query text), so they need not agree.
    use sql_insight::catalog::CatalogTable;
    use sql_insight::sqlparser::dialect::PostgreSqlDialect;

    let resolve = |default_schema: &str, query: &str| {
        let catalog = Catalog::new()
            .default_schema(default_schema)
            .table(CatalogTable::new("public", "users").columns(["id"]));
        let reads = extract_column_operations_with_options(
            &PostgreSqlDialect {},
            query,
            ExtractorOptions::new().with_catalog(&catalog),
        )
        .unwrap()
        .remove(0)
        .unwrap()
        .reads;
        find(&reads, "id").resolution
    };

    // Matching-case default fills + resolves the bare ref.
    assert_eq!(
        resolve("public", "SELECT id FROM users"),
        ResolutionKind::Cataloged
    );
    // Mismatched-case default is case-exact, so it doesn't match `public`.
    assert_eq!(
        resolve("PUBLIC", "SELECT id FROM users"),
        ResolutionKind::Inferred
    );
    // An inline unquoted qualifier is query text and folds, so it still hits —
    // regardless of the (here mismatched) default.
    assert_eq!(
        resolve("PUBLIC", "SELECT id FROM PUBLIC.users"),
        ResolutionKind::Cataloged
    );
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
fn three_segment_ddl_registers_catalog_schema_name() {
    // `CREATE TABLE c.s.t` registers the full catalog.schema.name path; a
    // matching three-part query resolves Cataloged and canonicalizes to it.
    let reads = reads_with("CREATE TABLE c.s.t (id INT)", "SELECT id FROM c.s.t");
    let read = find(&reads, "id");
    assert_eq!(read.resolution, ResolutionKind::Cataloged);
    let table = read.reference.table.as_ref().unwrap();
    assert_eq!(table.catalog.as_ref().unwrap().value, "c");
    assert_eq!(table.schema.as_ref().unwrap().value, "s");
    assert_eq!(table.name.value, "t");
}

#[test]
fn catalog_table_display_joins_present_segments() {
    use sql_insight::catalog::CatalogTable;
    assert_eq!(CatalogTable::unqualified("users").to_string(), "users");
    assert_eq!(
        CatalogTable::new("public", "users").to_string(),
        "public.users"
    );
    assert_eq!(
        CatalogTable::new("public", "users")
            .catalog("db")
            .to_string(),
        "db.public.users"
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

#[test]
fn columnless_insert_from_wildcard_source_drops_writes_and_flags() {
    // A column-list-less INSERT whose source carries a wildcard can't pair the
    // target's catalog columns positionally — the `*` count is indeterminate
    // (wildcards aren't expanded), so the visible-output count is too low.
    // Surface no writes and flag `InsertColumnsUnresolved` rather than
    // mis-truncating the catalog list to the undercounted outputs.
    use sql_insight::diagnostic::ColumnLevelDiagnosticKind;
    let ddl = "CREATE TABLE t (a INT, b INT, c INT, d INT); CREATE TABLE s (p INT, q INT, r INT)";
    let op = |q: &str| {
        let catalog = Catalog::from_ddl(&GenericDialect {}, ddl).unwrap();
        extract_column_operations_with_options(
            &GenericDialect {},
            q,
            ExtractorOptions::new().with_catalog(&catalog),
        )
        .unwrap()
        .remove(0)
        .unwrap()
    };

    let wild = op("INSERT INTO t SELECT *, now() FROM s");
    assert!(
        wild.writes.is_empty(),
        "wildcard source → no determinate writes, got {:?}",
        wild.writes
    );
    assert!(
        wild.diagnostics
            .iter()
            .any(|d| d.kind == ColumnLevelDiagnosticKind::InsertColumnsUnresolved),
        "should flag InsertColumnsUnresolved, got {:?}",
        wild.diagnostics
    );

    // A determinate projection still fills from the catalog (first two columns).
    let plain = op("INSERT INTO t SELECT p, q FROM s");
    let written: Vec<_> = plain
        .writes
        .iter()
        .map(|w| w.reference.name.value.as_str())
        .collect();
    assert_eq!(written, ["a", "b"]);
    assert!(
        plain.diagnostics.is_empty(),
        "determinate fill → no diagnostic, got {:?}",
        plain.diagnostics
    );
}

#[test]
fn from_ddl_folds_unquoted_identifiers_to_their_stored_form() {
    // `from_ddl` registers each identifier in its dialect-stored form: an
    // unquoted DDL name folds (Postgres lowercases), a quoted one stays exact.
    // So the registered identity matches what a query reference folds to —
    // previously an unquoted mixed-case `CREATE TABLE Users` registered `Users`
    // and missed a folded query `users` under a case-sensitive dialect.
    use sql_insight::sqlparser::dialect::PostgreSqlDialect;
    let resolution = |ddl: &str, query: &str| -> ResolutionKind {
        let catalog = Catalog::from_ddl(&PostgreSqlDialect {}, ddl).unwrap();
        extract_column_operations_with_options(
            &PostgreSqlDialect {},
            query,
            ExtractorOptions::new().with_catalog(&catalog),
        )
        .unwrap()
        .remove(0)
        .unwrap()
        .reads
        .first()
        .unwrap()
        .resolution
    };

    // Unquoted DDL folds to lowercase → a folded (unquoted) query matches,
    // however it was cased in the query text.
    assert_eq!(
        resolution("CREATE TABLE Users (Id INT)", "SELECT id FROM users"),
        ResolutionKind::Cataloged
    );
    assert_eq!(
        resolution("CREATE TABLE Users (Id INT)", "SELECT Id FROM Users"),
        ResolutionKind::Cataloged
    );
    // A quoted query is a distinct, case-exact identifier the folded DDL never
    // created.
    assert_eq!(
        resolution("CREATE TABLE Users (Id INT)", r#"SELECT "Id" FROM "Users""#),
        ResolutionKind::Inferred
    );

    // Quoted DDL stays exact → matches a quoted query, not an unquoted (folded)
    // one.
    assert_eq!(
        resolution(
            r#"CREATE TABLE "Users" ("Id" INT)"#,
            r#"SELECT "Id" FROM "Users""#
        ),
        ResolutionKind::Cataloged
    );
    assert_eq!(
        resolution(r#"CREATE TABLE "Users" ("Id" INT)"#, "SELECT id FROM users"),
        ResolutionKind::Inferred
    );
}

#[test]
fn from_ddl_with_casing_aligns_the_catalog_with_a_casing_override() {
    // Under a `with_casing(Sensitive)` extraction override, a default `from_ddl`
    // (folding with the dialect default) no longer matches — its canonical form
    // differs from what the Sensitive query folds to. `from_ddl_with_casing`
    // builds the catalog with the same casing, so it matches.
    use sql_insight::sqlparser::dialect::PostgreSqlDialect;
    use sql_insight::{CaseRule, IdentifierCasing};
    let sensitive = IdentifierCasing::uniform(CaseRule::Sensitive);
    let resolution = |catalog: &Catalog| -> ResolutionKind {
        extract_column_operations_with_options(
            &PostgreSqlDialect {},
            "SELECT a FROM MyTable",
            ExtractorOptions::new()
                .with_catalog(catalog)
                .with_casing(sensitive),
        )
        .unwrap()
        .remove(0)
        .unwrap()
        .reads
        .first()
        .unwrap()
        .resolution
    };

    // Default `from_ddl` folds `MyTable` → `mytable`; the Sensitive query keeps
    // `MyTable`, so it misses.
    let default = Catalog::from_ddl(&PostgreSqlDialect {}, "CREATE TABLE MyTable (a INT)").unwrap();
    assert_eq!(resolution(&default), ResolutionKind::Inferred);

    // Built with the same Sensitive casing, the catalog stores `MyTable` exact,
    // so the Sensitive query matches.
    let aligned = Catalog::from_ddl_with_casing(
        &PostgreSqlDialect {},
        "CREATE TABLE MyTable (a INT)",
        sensitive,
    )
    .unwrap();
    assert_eq!(resolution(&aligned), ResolutionKind::Cataloged);
}

#[test]
fn dml_target_resolves_to_the_base_table_not_a_shadowing_cte() {
    // A DML target name is resolved against the catalog, never the WITH list: a
    // base table sharing a CTE's name is the real (Cataloged) write target,
    // mirroring Postgres (which updates the base table, ignoring the CTE; only a
    // CTE-*only* name errors / is flagged).
    let catalog = Catalog::from_ddl(&GenericDialect {}, "CREATE TABLE c (x INT)").unwrap();
    let op = extract_column_operations_with_options(
        &GenericDialect {},
        "WITH c AS (SELECT 1 AS x) UPDATE c SET x = 1",
        ExtractorOptions::new().with_catalog(&catalog),
    )
    .unwrap()
    .remove(0)
    .unwrap();
    assert_eq!(op.diagnostics, vec![]);
    assert_eq!(op.writes.len(), 1);
    assert_eq!(op.writes[0].resolution, ResolutionKind::Cataloged);
    assert_eq!(op.writes[0].reference.name.value, "x");
}

#[test]
fn column_less_insert_fills_a_case_exact_column_quoted_and_cataloged() {
    // A column-less INSERT fills its column list from the catalog in the
    // canonical (quoted) form — like the table — so a case-exact column matches
    // the catalog (Cataloged, not Inferred) and a user's own `"MyCol"`
    // reference. The filled name previously surfaced unquoted and missed.
    use sql_insight::sqlparser::dialect::SnowflakeDialect;
    let catalog =
        Catalog::from_ddl(&SnowflakeDialect {}, r#"CREATE TABLE t ("MyCol" INT)"#).unwrap();
    let op = extract_column_operations_with_options(
        &SnowflakeDialect {},
        "INSERT INTO t SELECT 1",
        ExtractorOptions::new().with_catalog(&catalog),
    )
    .unwrap()
    .remove(0)
    .unwrap();
    assert_eq!(op.writes.len(), 1);
    assert_eq!(op.writes[0].reference.name.value, "MyCol");
    assert_eq!(op.writes[0].reference.name.quote_style, Some('"'));
    assert_eq!(op.writes[0].resolution, ResolutionKind::Cataloged);
}

#[test]
fn from_ddl_skips_a_table_with_a_non_identifier_name_segment() {
    // A name segment that isn't a plain identifier (Snowflake `IDENTIFIER('t')`)
    // makes the table's identity unrepresentable — the whole CREATE is skipped,
    // not mis-segmented into a phantom (`myschema.IDENTIFIER('t')` must not
    // register as table `myschema`). A valid CREATE in the same DDL still
    // registers.
    use sql_insight::sqlparser::dialect::SnowflakeDialect;
    let catalog = Catalog::from_ddl(
        &SnowflakeDialect {},
        "CREATE TABLE myschema.IDENTIFIER('t') (id INT); CREATE TABLE good (a INT)",
    )
    .unwrap();
    let resolution = |sql: &str| -> ResolutionKind {
        extract_column_operations_with_options(
            &SnowflakeDialect {},
            sql,
            ExtractorOptions::new().with_catalog(&catalog),
        )
        .unwrap()
        .remove(0)
        .unwrap()
        .reads
        .first()
        .unwrap()
        .resolution
    };
    // The mis-segmented `myschema` phantom is not registered.
    assert_eq!(
        resolution("SELECT id FROM myschema"),
        ResolutionKind::Inferred
    );
    // The valid sibling table still registers (the skip is per-statement).
    assert_eq!(resolution("SELECT a FROM good"), ResolutionKind::Cataloged);
}

#[test]
fn from_ddl_unquoted_registration_canonicalizes_the_surfaced_identity() {
    // The Cataloged read surfaces the catalog's stored (folded) identity, not
    // the query's written casing.
    use sql_insight::sqlparser::dialect::PostgreSqlDialect;
    let catalog = Catalog::from_ddl(&PostgreSqlDialect {}, "CREATE TABLE Users (Id INT)").unwrap();
    let reads = extract_column_operations_with_options(
        &PostgreSqlDialect {},
        "SELECT Id FROM Users",
        ExtractorOptions::new().with_catalog(&catalog),
    )
    .unwrap()
    .remove(0)
    .unwrap()
    .reads;
    let read = reads.first().unwrap();
    assert_eq!(read.reference.table.as_ref().unwrap().name.value, "users");
}
