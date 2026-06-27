//! Column-level operation extraction.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example column_operations -p sql-insight
//! ```
//!
//! Demonstrates per-column lineage: classification by `ColumnLineageKind`,
//! `Relation` vs `QueryOutput` targets, and occurrence-based reads.

use sql_insight::extractor::{extract_column_operations, ColumnLineageKind, ColumnTarget};
use sql_insight::sqlparser::dialect::GenericDialect;

fn main() {
    let dialect = GenericDialect {};
    let sql = "INSERT INTO orders (id, total) \
               SELECT order_id, SUM(amount) FROM staging GROUP BY order_id";

    let results = extract_column_operations(&dialect, sql).unwrap();
    let ops = results[0].as_ref().expect("ok");

    println!("--- {:?} ---", ops.statement_kind);

    println!("\nreads ({}):", ops.reads.len());
    for read in &ops.reads {
        let table = read
            .reference
            .table
            .as_ref()
            .map(|t| t.name.value.as_str())
            .unwrap_or("<unresolved>");
        println!(
            "  {}.{} [{:?}]",
            table, read.reference.name.value, read.resolution
        );
    }

    println!("\nlineage ({}):", ops.lineage.len());
    for edge in &ops.lineage {
        let source = format!(
            "{}.{}",
            edge.source
                .reference
                .table
                .as_ref()
                .map(|t| t.name.value.as_str())
                .unwrap_or("?"),
            edge.source.reference.name.value
        );
        let target = match &edge.target {
            ColumnTarget::Relation(c) => format!(
                "{}.{}",
                c.reference
                    .table
                    .as_ref()
                    .map(|t| t.name.value.as_str())
                    .unwrap_or("?"),
                c.reference.name.value
            ),
            ColumnTarget::QueryOutput { name, position } => format!(
                "<output #{} {}>",
                position,
                name.as_ref().map(|n| n.value.as_str()).unwrap_or("anon")
            ),
        };
        println!("  {} -> {} ({:?})", source, target, edge.kind);
    }

    // Bucket lineage by kind: is the value forwarded unchanged, or
    // derived? (`direct copy` vs `transformed`).
    let mut passthrough = 0usize;
    let mut transformation = 0usize;
    for edge in &ops.lineage {
        match edge.kind {
            ColumnLineageKind::Passthrough => passthrough += 1,
            ColumnLineageKind::Transformation => transformation += 1,
        }
    }
    println!(
        "\nlineage kinds — Passthrough={}, Transformation={}",
        passthrough, transformation
    );
}
