//! Column-level operation extraction.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example column_operations -p sql-insight
//! ```
//!
//! Demonstrates per-column flows: classification by `ColumnFlowKind`,
//! `Persisted` vs `QueryOutput` targets, and clause-role tagging on
//! reads.

use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::{extract_column_operations, ColumnFlowKind, ColumnTarget};

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
            .column
            .table
            .as_ref()
            .map(|t| t.name.value.as_str())
            .unwrap_or("<unresolved>");
        println!(
            "  {}.{} kinds={:?}",
            table, read.column.name.value, read.kinds
        );
    }

    println!("\nflows ({}):", ops.flows.len());
    for flow in &ops.flows {
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

    // Bucket flows by kind so consumers can answer questions like
    // "did any aggregation happen on the way to this column?".
    let mut passthrough = 0usize;
    let mut aggregation = 0usize;
    let mut computed = 0usize;
    for flow in &ops.flows {
        match flow.kind {
            ColumnFlowKind::Passthrough => passthrough += 1,
            ColumnFlowKind::Aggregation => aggregation += 1,
            ColumnFlowKind::Computed => computed += 1,
            // ColumnFlowKind is #[non_exhaustive] — future variants
            // fall here. Skipping is fine for the per-kind count.
            _ => {}
        }
    }
    println!(
        "\nflow kinds — Passthrough={}, Aggregation={}, Computed={}",
        passthrough, aggregation, computed
    );
}
