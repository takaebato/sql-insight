//! Column-level operation extraction.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example column_operations -p sql-insight
//! ```
//!
//! Demonstrates per-column lineage: classification by `ColumnLineageKind`,
//! `Persisted` vs `QueryOutput` targets, and occurrence-based reads.

use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::{extract_column_operations, ColumnLineageKind, ColumnTarget};

fn main() {
    let dialect = GenericDialect {};
    let sql = "INSERT INTO orders (id, total) \
               SELECT order_id, SUM(amount) FROM staging GROUP BY order_id";

    let results = extract_column_operations(&dialect, sql, None).unwrap();
    let ops = results[0].as_ref().expect("ok");

    println!("--- {:?} ---", ops.statement_kind);

    println!("\nreads ({}):", ops.reads.len());
    for read in &ops.reads {
        let table = read
            .table
            .as_ref()
            .map(|t| t.name.value.as_str())
            .unwrap_or("<unresolved>");
        println!("  {}.{}", table, read.name.value);
    }

    println!("\nlineage ({}):", ops.lineage.len());
    for flow in &ops.lineage {
        let source = format!(
            "{}.{}",
            flow.source
                .table
                .as_ref()
                .map(|t| t.name.value.as_str())
                .unwrap_or("?"),
            flow.source.name.value
        );
        let target = match &flow.target {
            ColumnTarget::Persisted(c) => format!(
                "{}.{}",
                c.table
                    .as_ref()
                    .map(|t| t.name.value.as_str())
                    .unwrap_or("?"),
                c.name.value
            ),
            ColumnTarget::QueryOutput { name, position } => format!(
                "<output #{} {}>",
                position,
                name.as_ref().map(|n| n.value.as_str()).unwrap_or("anon")
            ),
        };
        println!("  {} -> {} ({:?})", source, target, flow.kind);
    }

    // Bucket lineage by kind: is the value forwarded unchanged, or
    // derived? (`direct copy` vs `transformed`).
    let mut passthrough = 0usize;
    let mut transformation = 0usize;
    for flow in &ops.lineage {
        match flow.kind {
            ColumnLineageKind::Passthrough => passthrough += 1,
            ColumnLineageKind::Transformation => transformation += 1,
        }
    }
    println!(
        "\nlineage kinds — Passthrough={}, Transformation={}",
        passthrough, transformation
    );
}
