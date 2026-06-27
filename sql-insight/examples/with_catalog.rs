//! Operation extraction with a `Catalog`.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example with_catalog -p sql-insight
//! ```
//!
//! Shows how supplying a catalog changes resolver behaviour:
//!
//! 1. INSERT without an explicit column list pairs source projections
//!    with the target table's catalog-supplied columns.
//! 2. Reads carry a `ResolutionKind`: catalog-aware resolution surfaces
//!    `Cataloged` placements, ambiguity surfaces as `Ambiguous`, and
//!    columns the catalog actively denies surface as `Unresolved`.

use sql_insight::catalog::{Catalog, CatalogTable};
use sql_insight::extractor::{
    extract_column_operations, extract_column_operations_with_options, ColumnTarget,
    ExtractorOptions,
};
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::ResolutionKind;

fn main() {
    let dialect = GenericDialect {};
    // A concrete, eager registry: every table registered under a
    // `public` schema. The bare query references below resolve against
    // it by right-anchored matching.
    let catalog: Catalog = [
        CatalogTable::new("public", "orders").columns(["id", "total"]),
        CatalogTable::new("public", "staging").columns(["order_id", "amount"]),
        CatalogTable::new("public", "t1").columns(["a"]),
        CatalogTable::new("public", "t2").columns(["a"]),
    ]
    .into_iter()
    .collect();

    // 1) INSERT without explicit columns — the catalog supplies the
    //    target column list so source projections pair positionally.
    {
        let sql = "INSERT INTO orders SELECT order_id, amount FROM staging";
        let results = extract_column_operations_with_options(
            &dialect,
            sql,
            ExtractorOptions::new().with_catalog(&catalog),
        )
        .unwrap();
        let ops = results[0].as_ref().unwrap();
        println!("--- 1. INSERT without explicit column list ---");
        for edge in &ops.lineage {
            if let ColumnTarget::Relation(target) = &edge.target {
                println!(
                    "  {} -> orders.{} ({:?})",
                    edge.source.reference.name.value, target.reference.name.value, edge.kind
                );
            }
        }
    }

    // 2) Ambiguous reference — both `t1` and `t2` declare `a` via the
    //    catalog, so unqualified `a` can't be pinned to a single
    //    owner. The projection's `a` surfaces with `ResolutionKind::Ambiguous`.
    //    Without the catalog the same SQL also yields `Ambiguous` (two
    //    Unknown suspects with no tiebreaker) — the difference is
    //    whether `t1.a` and `t2.a` themselves are `Cataloged` (with
    //    catalog) or `Inferred` (without).
    {
        let sql = "SELECT a FROM t1 JOIN t2 ON t1.a = t2.a";
        let with = extract_column_operations_with_options(
            &dialect,
            sql,
            ExtractorOptions::new().with_catalog(&catalog),
        )
        .unwrap();
        let without = extract_column_operations(&dialect, sql).unwrap();
        println!("\n--- 2. ambiguous unqualified `a` ---");
        print_reads("with catalog", &with[0].as_ref().unwrap().reads);
        print_reads("without catalog", &without[0].as_ref().unwrap().reads);
    }

    // 3) Unresolved reference — `t1` catalog says columns are `[a]`;
    //    `z` is not in any in-scope Known schema. The read surfaces
    //    with `ResolutionKind::Unresolved`. Without the catalog the same
    //    SQL surfaces as `Inferred` (t1 is Unknown, so `z` could
    //    plausibly belong to t1).
    {
        let sql = "SELECT z FROM t1";
        let with = extract_column_operations_with_options(
            &dialect,
            sql,
            ExtractorOptions::new().with_catalog(&catalog),
        )
        .unwrap();
        let without = extract_column_operations(&dialect, sql).unwrap();
        println!("\n--- 3. catalog rejects unknown column `z` ---");
        print_reads("with catalog", &with[0].as_ref().unwrap().reads);
        print_reads("without catalog", &without[0].as_ref().unwrap().reads);
    }
}

fn print_reads(label: &str, reads: &[sql_insight::ColumnRead]) {
    println!("  {label}:");
    for read in reads {
        let table = read
            .reference
            .table
            .as_ref()
            .map(|t| t.name.value.as_str())
            .unwrap_or("<unresolved>");
        let confidence_marker = match read.resolution {
            ResolutionKind::Cataloged => "✓",
            ResolutionKind::Inferred => "~",
            ResolutionKind::Ambiguous => "?",
            ResolutionKind::Unresolved => "✗",
        };
        println!(
            "    {confidence_marker} {table}.{name} ({resolution:?})",
            name = read.reference.name.value,
            resolution = read.resolution,
        );
    }
}
