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
//! 2. Reads carry a `Confidence`: catalog-aware resolution surfaces
//!    `Confirmed` placements, ambiguity surfaces as `Ambiguous`, and
//!    columns the catalog actively denies surface as `Unresolved`.

use sql_insight::extractor::{extract_column_operations, ColumnTarget};
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::{
    catalog::{Catalog, ColumnSchema},
    Confidence, TableReference,
};
use std::collections::HashMap;

#[derive(Debug, Default)]
struct InMemoryCatalog {
    tables: HashMap<String, Vec<String>>,
}

impl InMemoryCatalog {
    fn with(mut self, name: &str, columns: &[&str]) -> Self {
        self.tables.insert(
            name.to_string(),
            columns.iter().map(|c| c.to_string()).collect(),
        );
        self
    }
}

impl Catalog for InMemoryCatalog {
    fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>> {
        self.tables.get(table.name.value.as_str()).map(|cols| {
            cols.iter()
                .map(|c| ColumnSchema { name: c.clone() })
                .collect()
        })
    }
}

fn main() {
    let dialect = GenericDialect {};
    let catalog = InMemoryCatalog::default()
        .with("orders", &["id", "total"])
        .with("staging", &["order_id", "amount"])
        .with("t1", &["a"])
        .with("t2", &["a"]);

    // 1) INSERT without explicit columns — the catalog supplies the
    //    target column list so source projections pair positionally.
    {
        let sql = "INSERT INTO orders SELECT order_id, amount FROM staging";
        let results = extract_column_operations(&dialect, sql, Some(&catalog)).unwrap();
        let ops = results[0].as_ref().unwrap();
        println!("--- 1. INSERT without explicit column list ---");
        for edge in &ops.lineage {
            if let ColumnTarget::Relation(target) = &edge.target {
                println!(
                    "  {} -> orders.{} ({:?})",
                    edge.source.reference.name.value, target.name.value, edge.kind
                );
            }
        }
    }

    // 2) Ambiguous reference — both `t1` and `t2` declare `a` via the
    //    catalog, so unqualified `a` can't be pinned to a single
    //    owner. The projection's `a` surfaces with `Confidence::Ambiguous`.
    //    Without the catalog the same SQL also yields `Ambiguous` (two
    //    Unknown suspects with no tiebreaker) — the difference is
    //    whether `t1.a` and `t2.a` themselves are `Confirmed` (with
    //    catalog) or `Inferred` (without).
    {
        let sql = "SELECT a FROM t1 JOIN t2 ON t1.a = t2.a";
        let with = extract_column_operations(&dialect, sql, Some(&catalog)).unwrap();
        let without = extract_column_operations(&dialect, sql, None).unwrap();
        println!("\n--- 2. ambiguous unqualified `a` ---");
        print_reads("with catalog", &with[0].as_ref().unwrap().reads);
        print_reads("without catalog", &without[0].as_ref().unwrap().reads);
    }

    // 3) Unresolved reference — `t1` catalog says columns are `[a]`;
    //    `z` is not in any in-scope Known schema. The read surfaces
    //    with `Confidence::Unresolved`. Without the catalog the same
    //    SQL surfaces as `Inferred` (t1 is Unknown, so `z` could
    //    plausibly belong to t1).
    {
        let sql = "SELECT z FROM t1";
        let with = extract_column_operations(&dialect, sql, Some(&catalog)).unwrap();
        let without = extract_column_operations(&dialect, sql, None).unwrap();
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
        let confidence_marker = match read.confidence {
            Confidence::Confirmed => "✓",
            Confidence::Inferred => "~",
            Confidence::Ambiguous => "?",
            Confidence::Unresolved => "✗",
        };
        println!(
            "    {confidence_marker} {table}.{name} ({confidence:?})",
            name = read.reference.name.value,
            confidence = read.confidence,
        );
    }
}
