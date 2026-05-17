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
//! 2. `AmbiguousColumn` fires when two `Known` schemas both confirm an
//!    unqualified column; it stays silent without a catalog.
//! 3. `UnresolvedColumn` fires when a `Known` schema has the column
//!    not in any in-scope binding; same silence rule applies without
//!    a catalog.

use sql_insight::sqlparser::ast::Ident;
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::{
    extract_column_operations, Catalog, ColumnSchema, ColumnTarget, DiagnosticKind, TableReference,
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
                .map(|c| ColumnSchema {
                    name: Ident::new(c.as_str()),
                })
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
        for flow in &ops.flows {
            if let ColumnTarget::Persisted(target) = &flow.target {
                println!(
                    "  {} -> orders.{} ({:?})",
                    flow.source.name.value, target.name.value, flow.kind
                );
            }
        }
    }

    // 2) Ambiguous column — both `t1` and `t2` declare `a` via the
    //    catalog, so `SELECT a FROM t1 JOIN t2 ...` is genuinely
    //    ambiguous and the diagnostic fires.
    {
        let sql = "SELECT a FROM t1 JOIN t2 ON t1.a = t2.a";
        let with = extract_column_operations(&dialect, sql, Some(&catalog)).unwrap();
        let without = extract_column_operations(&dialect, sql, None).unwrap();
        let with_count = count_kind(
            &with[0].as_ref().unwrap().diagnostics,
            DiagnosticKind::AmbiguousColumn,
        );
        let without_count = count_kind(
            &without[0].as_ref().unwrap().diagnostics,
            DiagnosticKind::AmbiguousColumn,
        );
        println!(
            "\n--- 2. ambiguous column: with catalog={}, without={} ---",
            with_count, without_count
        );
        for diag in &with[0].as_ref().unwrap().diagnostics {
            if matches!(diag.kind, DiagnosticKind::AmbiguousColumn) {
                println!("  {}", diag.message);
            }
        }
    }

    // 3) Unresolved column — `t1` catalog says columns are [a]; `z`
    //    does not exist in any in-scope Known schema.
    {
        let sql = "SELECT z FROM t1";
        let with = extract_column_operations(&dialect, sql, Some(&catalog)).unwrap();
        let without = extract_column_operations(&dialect, sql, None).unwrap();
        let with_count = count_kind(
            &with[0].as_ref().unwrap().diagnostics,
            DiagnosticKind::UnresolvedColumn,
        );
        let without_count = count_kind(
            &without[0].as_ref().unwrap().diagnostics,
            DiagnosticKind::UnresolvedColumn,
        );
        println!(
            "\n--- 3. unresolved column: with catalog={}, without={} ---",
            with_count, without_count
        );
        for diag in &with[0].as_ref().unwrap().diagnostics {
            if matches!(diag.kind, DiagnosticKind::UnresolvedColumn) {
                println!("  {}", diag.message);
            }
        }
    }
}

fn count_kind(diagnostics: &[sql_insight::Diagnostic], kind: DiagnosticKind) -> usize {
    diagnostics.iter().filter(|d| d.kind == kind).count()
}
