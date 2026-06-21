//! Overriding identifier casing with `ExtractorOptions::with_casing`.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example casing_override -p sql-insight
//! ```
//!
//! Identifier matching is dialect-aware: each dialect folds identifiers a
//! certain way before comparing them (see [`CaseRule`]). That default can
//! be overridden — e.g. to model a deployment whose collation differs
//! from the dialect's nominal rule. Here the same SQL resolves
//! differently under the dialect default versus a case-sensitive
//! override, both for CTE binding (no catalog) and catalog matching.
//!
//! Note: the override governs only *matching*; surfaced identifier text is
//! never rewritten by it (only a catalog hit canonicalizes a table name).

use sql_insight::catalog::{Catalog, CatalogTable};
use sql_insight::extractor::{extract_table_operations_with_options, ExtractorOptions};
use sql_insight::sqlparser::dialect::{Dialect, GenericDialect};
use sql_insight::{CaseRule, IdentifierCasing};

fn main() {
    let dialect = GenericDialect {};

    // 1) CTE binding (no catalog). The CTE is declared `T2` but referenced
    //    as lowercase `t2`. Whether that reference binds the CTE — so only
    //    the CTE body's real table `t1` is read — or is treated as a
    //    distinct real table depends on the casing rule.
    {
        let sql = "WITH T2 AS (SELECT id FROM t1) SELECT * FROM t2";
        println!("=== 1. CTE reference `t2` vs CTE `T2` ===");

        // GenericDialect's default folds to lower case, so `t2` == `T2`:
        // the reference binds the CTE and only `t1` surfaces as a read.
        print_table_reads(
            "  dialect default (case-insensitive match): t2 binds the CTE",
            &dialect,
            sql,
            ExtractorOptions::new(),
        );

        // Case-sensitive override: `t2` != `T2`, so the reference does not
        // bind the CTE — it surfaces as its own real table beside `t1`.
        print_table_reads(
            "  Sensitive override: t2 is a distinct table",
            &dialect,
            sql,
            ExtractorOptions::new().with_casing(IdentifierCasing::uniform(CaseRule::Sensitive)),
        );
    }

    // 2) Catalog matching. The catalog registers `public.users` (lower
    //    case); the query writes `USERS` in upper case.
    {
        let catalog = Catalog::new().table(CatalogTable::new("public", "users").columns(["id"]));
        let sql = "SELECT id FROM USERS";
        println!("\n=== 2. `USERS` vs catalog `public.users` ===");

        // Default fold matches: the read is `Cataloged` and canonicalized
        // to the registered `public.users` path.
        print_table_reads(
            "  dialect default (matches): canonicalized + Cataloged",
            &dialect,
            sql,
            ExtractorOptions::new().with_catalog(&catalog),
        );

        // Case-sensitive override: `USERS` != `users`, so the catalog does
        // not match — the reference stays as written and resolves `Inferred`.
        print_table_reads(
            "  Sensitive override (no match): as-written + Inferred",
            &dialect,
            sql,
            ExtractorOptions::new()
                .with_catalog(&catalog)
                .with_casing(IdentifierCasing::uniform(CaseRule::Sensitive)),
        );
    }
}

/// Print each table read of the first statement as `schema.name
/// (ResolutionKind)` under the given options.
fn print_table_reads(label: &str, dialect: &dyn Dialect, sql: &str, options: ExtractorOptions) {
    let result = extract_table_operations_with_options(dialect, sql, options).unwrap();
    let ops = result[0].as_ref().unwrap();
    println!("{label}");
    for read in &ops.reads {
        let r = &read.reference;
        let path = match &r.schema {
            Some(schema) => format!("{}.{}", schema.value, r.name.value),
            None => r.name.value.clone(),
        };
        println!("    {path}  ({:?})", read.resolution);
    }
}
