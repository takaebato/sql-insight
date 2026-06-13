//! Table-level operation extraction.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example table_operations -p sql-insight
//! ```
//!
//! Shows how a single call yields the statement kind plus the
//! `reads` / `writes` / `lineage` surfaces for each parsed statement.

use sql_insight::extractor::{extract_table_operations, StatementKind};
use sql_insight::sqlparser::dialect::GenericDialect;

fn main() {
    let dialect = GenericDialect {};
    let sql = "\
        INSERT INTO orders (id, total) \
        SELECT order_id, amount FROM staging; \
        DELETE FROM staging WHERE processed = true;";

    let results = extract_table_operations(&dialect, sql, None).unwrap();

    for (i, result) in results.iter().enumerate() {
        let ops = result.as_ref().expect("parse + resolve succeeded");
        println!("--- statement {} ({:?}) ---", i + 1, ops.statement_kind);
        let reads: Vec<String> = ops
            .reads
            .iter()
            .map(|r| format!("{} [{:?}]", r.reference.name.value, r.resolution))
            .collect();
        let writes: Vec<&str> = ops.writes.iter().map(|w| w.name.value.as_str()).collect();
        println!("reads:  {:?}", reads);
        println!("writes: {:?}", writes);
        println!("lineage:  {} edge(s)", ops.lineage.len());
        for edge in &ops.lineage {
            println!(
                "  {} -> {}",
                edge.source.reference.name.value, edge.target.name.value
            );
        }
        if !ops.diagnostics.is_empty() {
            println!("diagnostics: {} non-fatal item(s)", ops.diagnostics.len());
        }
    }

    // Programmatic dispatch on StatementKind — count statements that
    // physically write to a relation.
    let writers = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter(|ops| {
            matches!(
                ops.statement_kind,
                StatementKind::Insert
                    | StatementKind::Update
                    | StatementKind::Delete
                    | StatementKind::Merge
            )
        })
        .count();
    println!("\n{} write statement(s) total", writers);
}
