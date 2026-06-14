//! Walking the bound [`Plan`] to recover the operation surfaces. For
//! now just `reads`; `lineage` / `writes` join as the binder grows. The
//! differential harness below is the strangler safety net — every
//! covered statement's reads must match the current resolver.

use super::ir::Plan;
use crate::reference::ColumnRead;

/// Every column read the bound plan expresses: each `Project` output
/// column's (pre-collapsed) provenance plus every `PassThrough`'s filter
/// reads. Occurrence-based and unordered relative to the old resolver —
/// the differential harness compares the *set*, so order / multiplicity
/// differences (inherent to pre-collapse) don't register as regressions.
pub(crate) fn extract_reads(plan: &Plan) -> Vec<ColumnRead> {
    let mut reads = Vec::new();
    collect_reads(plan, &mut reads);
    reads
}

fn collect_reads(plan: &Plan, out: &mut Vec<ColumnRead>) {
    match plan {
        Plan::Scan(_) | Plan::OpaqueLeaf(_) => {}
        Plan::PassThrough(pt) => {
            out.extend(pt.reads.iter().cloned());
            for input in &pt.inputs {
                collect_reads(input, out);
            }
        }
        Plan::Project(project) => {
            for column in &project.outputs {
                out.extend(column.provenance.iter().cloned());
            }
            collect_reads(&project.input, out);
        }
        Plan::SetOp(set) => {
            for operand in &set.operands {
                collect_reads(operand, out);
            }
        }
        Plan::Write(write) => collect_reads(&write.input, out),
    }
}

/// Differential harness (the strangler safety net): for SQL the binder
/// covers, the **set** of real column reads it produces must match the
/// current resolver-based `extract_column_operations`. As the binder
/// grows (lineage, writes, more clauses), this corpus and the compared
/// surfaces grow with it; a set mismatch flags a regression to classify.
#[cfg(test)]
mod differential {
    use super::*;
    use crate::catalog::{Catalog, CatalogTable};
    use crate::extractor::extract_column_operations;
    use crate::resolver::IdentifierCasing;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;
    use std::collections::HashSet;

    fn bind_one(sql: &str, catalog: Option<&Catalog>) -> Plan {
        let dialect = GenericDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let casing = IdentifierCasing::for_dialect(&dialect);
        crate::plan::binder::build(&statements[0], catalog, casing).expect("supported statement")
    }

    fn old_reads(sql: &str, catalog: Option<&Catalog>) -> Vec<ColumnRead> {
        let dialect = GenericDialect {};
        extract_column_operations(&dialect, sql, catalog)
            .unwrap()
            .remove(0)
            .unwrap()
            .reads
    }

    fn read_set(reads: &[ColumnRead]) -> HashSet<ColumnRead> {
        reads.iter().cloned().collect()
    }

    /// The binder's read-set must match the resolver's for every covered
    /// statement, under the same catalog.
    fn assert_parity(sql: &str, catalog: Option<&Catalog>) {
        let new_set = read_set(&extract_reads(&bind_one(sql, catalog)));
        let old_set = read_set(&old_reads(sql, catalog));
        assert_eq!(
            new_set, old_set,
            "read-set mismatch for: {sql}\n  plan: {new_set:?}\n  old:  {old_set:?}"
        );
    }

    /// SQL fully within the binder's current coverage (single SELECT,
    /// FROM / comma / `JOIN … ON`, WHERE, column / simple-expr
    /// projection).
    fn covered_corpus() -> &'static [&'static str] {
        &[
            "SELECT a FROM t",
            "SELECT a, b FROM t",
            "SELECT t.a FROM t",
            "SELECT a, b FROM t WHERE c > 0",
            "SELECT a + b AS s FROM t",
            "SELECT x.a, y.b FROM x JOIN y ON x.id = y.id",
            "SELECT a FROM x JOIN y ON x.id = y.id",
            "SELECT a FROM x JOIN y ON x.id = y.id WHERE x.c > 0",
            "SELECT p.a FROM p, q WHERE p.id = q.id",
        ]
    }

    #[test]
    fn catalog_free_reads_match_resolver() {
        for sql in covered_corpus() {
            assert_parity(sql, None);
        }
    }

    #[test]
    fn catalog_aware_reads_match_resolver() {
        // public.x = [id, a], public.y = [id, b]; `z` is in neither.
        let catalog: Catalog = [
            CatalogTable::new("public", "x").columns(["id", "a"]),
            CatalogTable::new("public", "y").columns(["id", "b"]),
            CatalogTable::new("public", "t").columns(["a", "b"]),
        ]
        .into_iter()
        .collect();
        for sql in [
            "SELECT a FROM x",                              // Cataloged
            "SELECT z FROM x",                              // Unresolved (catalog denies)
            "SELECT a, b FROM t WHERE a > 0",               // all Cataloged
            "SELECT x.a, y.b FROM x JOIN y ON x.id = y.id", // qualified Cataloged
            "SELECT a FROM x JOIN y ON x.id = y.id",        // a only in x → Known witness
            "SELECT id FROM x JOIN y ON x.id = y.id",       // id in both → Ambiguous
        ] {
            assert_parity(sql, Some(&catalog));
        }
    }
}
