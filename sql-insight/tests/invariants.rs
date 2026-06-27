//! Cross-API invariants: run the column- and table-level extractors over a
//! shared corpus and check the consistency properties that span both public
//! surfaces (e.g. column-level read tables ⊆ table-level reads ∪ writes).
//! These hold no single extractor accountable, so they live apart from the
//! per-extractor suites.

use sql_insight::extractor::{
    extract_column_operations, extract_table_operations, ColumnLineageEdge, ColumnOperation,
    ColumnTarget, StatementKind, TableOperation,
};
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::{ColumnReference, TableReference};
use std::collections::HashSet;

/// Curated corpus chosen to stress the major shapes the resolver
/// handles. New patterns should be added here as the resolver
/// grows, not as one-off tests scattered across the codebase.
fn corpus() -> &'static [&'static str] {
    &[
        // SELECT shapes
        "SELECT a FROM t1",
        "SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.id = t2.id",
        "SELECT a FROM t1 WHERE b > 0 GROUP BY a HAVING COUNT(*) > 1",
        "SELECT a FROM t1 ORDER BY b",
        "SELECT SUM(x) OVER (PARTITION BY p ORDER BY o) AS total FROM t1",
        "SELECT CASE WHEN a > 0 THEN b ELSE c END FROM t1",
        // CTE / derived / subquery
        "WITH cte AS (SELECT id FROM t1) SELECT id FROM cte",
        "SELECT x FROM (SELECT a + 1 AS x FROM t1) sub",
        "SELECT a FROM t1 WHERE id IN (SELECT id FROM t2)",
        // Set operations
        "SELECT a FROM t1 UNION SELECT b FROM t2",
        "SELECT a FROM t1 INTERSECT SELECT b FROM t2",
        // DML
        "INSERT INTO t1 (a, b) VALUES (1, 2)",
        "INSERT INTO t1 (a, b) SELECT x, y FROM s",
        "UPDATE t1 SET a = b + 1 WHERE id = 5",
        "UPDATE t1 SET a = (SELECT max(x) FROM s) WHERE id = 5",
        "DELETE FROM t1 WHERE id = 5",
        // DDL with body
        "CREATE TABLE dst AS SELECT a, b FROM src",
        "CREATE VIEW v AS SELECT a AS x FROM t1",
        // MERGE
        "MERGE INTO t1 USING t2 ON t1.id = t2.id \
         WHEN MATCHED THEN UPDATE SET a = t2.a \
         WHEN NOT MATCHED THEN INSERT (id, a) VALUES (t2.id, t2.a)",
    ]
}

/// Collected pair of outputs for the same statement — both
/// extractors run in lockstep so per-statement invariants can be
/// checked side by side.
struct StatementPair {
    col: ColumnOperation,
    tab: TableOperation,
}

fn extract_paired(sql: &str) -> Vec<StatementPair> {
    let col = extract_column_operations(&GenericDialect {}, sql).unwrap();
    let tab = extract_table_operations(&GenericDialect {}, sql).unwrap();
    assert_eq!(
        col.len(),
        tab.len(),
        "statement count mismatch between column_op and table_op for SQL: {sql}"
    );
    col.into_iter()
        .zip(tab)
        .map(|(c, t)| StatementPair {
            col: c.expect("column_op extraction succeeded"),
            tab: t.expect("table_op extraction succeeded"),
        })
        .collect()
}

fn table_set<I, T>(
    items: I,
    mut key: impl FnMut(&T) -> Option<TableReference>,
) -> HashSet<TableReference>
where
    I: IntoIterator<Item = T>,
{
    items.into_iter().filter_map(|i| key(&i)).collect()
}

fn column_read_table(r: &sql_insight::ColumnRead) -> Option<TableReference> {
    r.reference.table.clone()
}

fn column_write_table(w: &ColumnReference) -> Option<TableReference> {
    w.table.clone()
}

fn edge_relation_table(f: &ColumnLineageEdge) -> Option<TableReference> {
    match &f.target {
        ColumnTarget::Relation(c) => c.table.clone(),
        ColumnTarget::QueryOutput { .. } => None,
    }
}

#[test]
fn statement_kind_agrees_between_extractors() {
    for sql in corpus() {
        for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
            assert_eq!(
                pair.col.statement_kind, pair.tab.statement_kind,
                "column_op vs table_op kind disagrees \
                 for statement {idx} of SQL: {sql}"
            );
        }
    }
}

#[test]
fn column_op_read_tables_appear_in_table_op_reads_or_writes() {
    // Column-level reads include refs from the RHS of UPDATE SET,
    // the predicate of DELETE WHERE, etc. — even when those refs
    // point at the statement's *target* table. table_op's UPDATE
    // / DELETE conventions surface the target in `writes` only
    // (unless the statement also has a separate read source like
    // `DELETE ... USING t2` or `UPDATE ... FROM t2`). The
    // invariant relaxes accordingly: column_op read tables must
    // be in the union of table_op reads + writes.
    for sql in corpus() {
        for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
            let table_op_reads: HashSet<_> =
                table_set(pair.tab.reads.clone(), |r: &sql_insight::TableRead| {
                    Some(r.reference.clone())
                });
            let table_op_writes: HashSet<_> =
                table_set(pair.tab.writes.clone(), |w| Some(w.reference.clone()));
            let known: HashSet<_> = table_op_reads.union(&table_op_writes).cloned().collect();
            let column_op_read_tables = table_set(pair.col.reads.clone(), column_read_table);
            for t in &column_op_read_tables {
                assert!(
                    known.contains(t),
                    "column_op read table {t:?} missing from table_op reads ∪ writes \
                     for statement {idx} of SQL: {sql}\n\
                     table_op reads: {table_op_reads:?}\n\
                     table_op writes: {table_op_writes:?}"
                );
            }
        }
    }
}

#[test]
fn column_op_write_tables_appear_in_table_op_writes() {
    for sql in corpus() {
        for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
            let table_op_writes = table_set(pair.tab.writes.clone(), |w| Some(w.reference.clone()));
            let column_op_write_tables = table_set(pair.col.writes.clone(), column_write_table);
            for t in &column_op_write_tables {
                assert!(
                    table_op_writes.contains(t),
                    "column_op write table {t:?} missing from table_op writes \
                     for statement {idx} of SQL: {sql}\n\
                     table_op writes: {table_op_writes:?}"
                );
            }
        }
    }
}

#[test]
fn relation_lineage_targets_resolve_to_known_write_tables() {
    for sql in corpus() {
        for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
            let table_op_writes = table_set(pair.tab.writes.clone(), |w| Some(w.reference.clone()));
            for f in &pair.col.lineage {
                if let Some(target_table) = edge_relation_table(f) {
                    assert!(
                        table_op_writes.contains(&target_table),
                        "Relation lineage target {target_table:?} not in table_op writes \
                         for statement {idx} of SQL: {sql}\n\
                         table_op writes: {table_op_writes:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn select_statements_emit_no_writes() {
    for sql in corpus() {
        for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
            if pair.col.statement_kind == StatementKind::Select {
                assert!(
                    pair.col.writes.is_empty(),
                    "SELECT statement has non-empty column_op writes \
                     for statement {idx} of SQL: {sql}\n\
                     writes: {:?}",
                    pair.col.writes
                );
                assert!(
                    pair.tab.writes.is_empty(),
                    "SELECT statement has non-empty table_op writes \
                     for statement {idx} of SQL: {sql}\n\
                     writes: {:?}",
                    pair.tab.writes
                );
            }
        }
    }
}

#[test]
fn writing_statements_emit_writes() {
    for sql in corpus() {
        for (idx, pair) in extract_paired(sql).into_iter().enumerate() {
            let writes_expected = matches!(
                pair.col.statement_kind,
                StatementKind::Insert
                    | StatementKind::Update
                    | StatementKind::CreateTable
                    | StatementKind::CreateView
                    | StatementKind::Merge
            );
            if writes_expected {
                assert!(
                    !pair.tab.writes.is_empty(),
                    "writing statement has empty table_op writes \
                     for statement {idx} of SQL: {sql}"
                );
            }
        }
    }
}
