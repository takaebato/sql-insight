//! Walking the bound [`Plan`] to recover the operation surfaces: the
//! `reads`, `writes`, and `lineage` an extractor exposes. The
//! differential harness below is the strangler safety net — every
//! covered statement's surfaces must match the current resolver.

use super::ir::{BoundColumn, Plan, Write};
use crate::extractor::{ColumnLineageEdge, ColumnTarget};
use crate::reference::{ColumnRead, ColumnReference};

/// Every physical column read the bound plan expresses: each `Project`
/// output column's non-synthetic provenance plus every `PassThrough`'s
/// filter reads. Occurrence-based, each carrying its source span;
/// synthetic-origin (collapse / alias) references are excluded so only
/// physical references are counted. The order is the tree walk's, which
/// the differential harness treats as incidental (it compares the
/// span-tagged multiset, not the sequence).
pub(crate) fn extract_reads(plan: &Plan) -> Vec<ColumnRead> {
    let mut reads = Vec::new();
    collect_reads(plan, &mut reads);
    reads
}

fn collect_reads(plan: &Plan, out: &mut Vec<ColumnRead>) {
    match plan {
        Plan::Scan(_) | Plan::OpaqueLeaf => {}
        Plan::PassThrough(pt) => {
            out.extend(pt.reads.iter().cloned());
            for input in &pt.inputs {
                collect_reads(input, out);
            }
        }
        Plan::Project(project) => {
            for column in &project.outputs {
                // A synthetic-origin source (referenced through a derived /
                // CTE relation or an output alias) is a lineage source but
                // not a physical read — that read is counted at the inner
                // producer, so it is excluded here.
                out.extend(
                    column
                        .provenance
                        .iter()
                        .filter(|source| !source.synthetic_origin)
                        .map(|source| source.read.clone()),
                );
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

/// The reads of a plan *below* its top output columns — a nested
/// subquery's filter-only reads. Its output provenance is taken
/// separately (as the enclosing value's lineage sources), so skipping it
/// here keeps the subquery's output from being counted twice.
pub(crate) fn input_reads(plan: &Plan) -> Vec<ColumnRead> {
    let mut reads = Vec::new();
    collect_input_reads(plan, &mut reads);
    reads
}

fn collect_input_reads(plan: &Plan, out: &mut Vec<ColumnRead>) {
    match plan {
        // Skip the top producer's output columns; walk everything beneath.
        Plan::Project(project) => collect_reads(&project.input, out),
        Plan::SetOp(set) => {
            for operand in &set.operands {
                collect_input_reads(operand, out);
            }
        }
        Plan::PassThrough(pt) => {
            out.extend(pt.reads.iter().cloned());
            for input in &pt.inputs {
                collect_input_reads(input, out);
            }
        }
        // No output columns to skip — every read is a filter read.
        Plan::Scan(_) | Plan::OpaqueLeaf | Plan::Write(_) => collect_reads(plan, out),
    }
}

/// Every column the statement writes: a [`Write`]'s target columns
/// qualified by its target relation. A bare query writes nothing.
pub(crate) fn extract_writes(plan: &Plan) -> Vec<ColumnReference> {
    match plan {
        Plan::Write(write) => write
            .target_columns
            .iter()
            .map(|column| ColumnReference {
                table: Some(write.target.clone()),
                name: column.clone(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The lineage edges the statement expresses: a [`Write`] pairs its
/// target columns with the source's output columns (`Relation` targets);
/// a bare query emits one `QueryOutput` edge group per projection.
/// Sources come straight from each output column's pre-collapsed
/// provenance (already real base columns carrying composed kind).
pub(crate) fn extract_lineage(plan: &Plan) -> Vec<ColumnLineageEdge> {
    let mut edges = Vec::new();
    match plan {
        Plan::Write(write) => write_lineage(write, &mut edges),
        _ => query_output_lineage(plan, &mut edges),
    }
    edges
}

/// `Write` lineage: pair each output operand's columns positionally with
/// the target columns (so a UNION-sourced INSERT pairs every branch), and
/// emit one `Relation` edge per provenance source.
fn write_lineage(write: &Write, out: &mut Vec<ColumnLineageEdge>) {
    for operand in output_operands(&write.input) {
        for (target_column, source_column) in write.target_columns.iter().zip(operand) {
            let target = ColumnTarget::Relation(ColumnReference {
                table: Some(write.target.clone()),
                name: target_column.clone(),
            });
            emit_edges(source_column, &target, out);
        }
    }
}

/// Bare-query lineage: each projection column becomes a `QueryOutput`
/// target at its position, with one edge per provenance source. Set
/// operands each restart positions from zero (mirroring the resolver's
/// per-operand emission), so a UNION emits an edge per branch column.
fn query_output_lineage(plan: &Plan, out: &mut Vec<ColumnLineageEdge>) {
    for operand in output_operands(plan) {
        for (position, column) in operand.iter().enumerate() {
            let target = ColumnTarget::QueryOutput {
                name: column.name.clone(),
                position,
            };
            emit_edges(column, &target, out);
        }
    }
}

/// One edge per provenance source of `column` into `target`, carrying
/// the source's composed kind. Ambiguous / unresolved sources (no
/// resolved table) are kept — the resolver surfaces them too, so a
/// consumer sees that the output has an unresolvable contributor.
fn emit_edges(column: &BoundColumn, target: &ColumnTarget, out: &mut Vec<ColumnLineageEdge>) {
    for source in &column.provenance {
        out.push(ColumnLineageEdge {
            source: source.read.clone(),
            target: target.clone(),
            kind: source.kind,
        });
    }
}

/// The output-column operands of a plan: one list per set-operation
/// branch (a plain `Project` has a single operand). Filter `PassThrough`
/// wrappers (clause reads / ORDER BY) are peeled to the producer beneath.
pub(crate) fn output_operands(plan: &Plan) -> Vec<&[BoundColumn]> {
    match plan {
        Plan::Project(project) => vec![&project.outputs],
        Plan::SetOp(set) => set.operands.iter().flat_map(output_operands).collect(),
        Plan::PassThrough(pt) => pt.inputs.first().map(output_operands).unwrap_or_default(),
        Plan::Scan(_) | Plan::OpaqueLeaf | Plan::Write(_) => Vec::new(),
    }
}

/// Differential harness (the strangler safety net): for SQL the binder
/// covers, its `reads` (as a span-tagged multiset — occurrence + source
/// span, order excepted), `writes`, and `lineage` must match the current
/// resolver-based `extract_column_operations`. The lazy-collapse binder
/// counts each physical reference once with its own span, so it matches
/// the resolver's occurrence + spans, not just the read set.
#[cfg(test)]
mod differential {
    use super::*;
    use crate::catalog::{Catalog, CatalogTable};
    use crate::extractor::{extract_column_operations, ColumnOperation};
    use crate::resolver::IdentifierCasing;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;
    use std::collections::HashSet;
    use std::hash::Hash;

    fn bind_one(sql: &str, catalog: Option<&Catalog>) -> Plan {
        let dialect = GenericDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let casing = IdentifierCasing::for_dialect(&dialect);
        crate::plan::binder::build(&statements[0], catalog, casing).expect("supported statement")
    }

    fn resolver_op(sql: &str, catalog: Option<&Catalog>) -> ColumnOperation {
        let dialect = GenericDialect {};
        extract_column_operations(&dialect, sql, catalog)
            .unwrap()
            .remove(0)
            .unwrap()
    }

    /// The plan-based column operation (the extractor switch's surface).
    fn plan_op(sql: &str, catalog: Option<&Catalog>) -> ColumnOperation {
        let dialect = GenericDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let casing = IdentifierCasing::for_dialect(&dialect);
        crate::plan::operation::column_operation(&statements[0], catalog, casing)
    }

    fn set<T: Clone + Eq + Hash>(items: &[T]) -> HashSet<T> {
        items.iter().cloned().collect()
    }

    /// Diagnostic *kinds* as a sorted multiset (messages are human-readable
    /// detail, not part of the compared contract).
    fn diag_kinds(op: &ColumnOperation) -> Vec<String> {
        let mut kinds: Vec<String> = op
            .diagnostics
            .iter()
            .map(|d| format!("{:?}", d.kind))
            .collect();
        kinds.sort();
        kinds
    }

    /// A span-free key for a read, so occurrences sort / compare by
    /// identity + resolution (sqlparser's `Ident` equality already ignores
    /// spans).
    fn read_key(r: &ColumnRead) -> String {
        let table = r
            .reference
            .table
            .as_ref()
            .map(|t| {
                format!(
                    "{}.{}.{}",
                    t.catalog.as_ref().map(|c| c.value.as_str()).unwrap_or(""),
                    t.schema.as_ref().map(|s| s.value.as_str()).unwrap_or(""),
                    t.name.value
                )
            })
            .unwrap_or_else(|| "?".into());
        format!("{}.{}|{:?}", table, r.reference.name.value, r.resolution)
    }

    /// Reads as a sorted, **span-tagged** multiset — proves the binder
    /// matches the resolver not just on identity + resolution + occurrence
    /// but on each reference's source span (the per-reference location that
    /// makes occurrences worth distinguishing). Order is *not* compared: a
    /// read's position is incidental walk order, not contractual.
    fn read_span_bag(reads: &[ColumnRead]) -> Vec<String> {
        let mut keys: Vec<String> = reads
            .iter()
            .map(|r| {
                let s = &r.reference.name.span;
                format!("{}@{}:{}", read_key(r), s.start.line, s.start.column)
            })
            .collect();
        keys.sort();
        keys
    }

    /// The plan-based column operation must match the resolver's for every
    /// covered statement: `statement_kind`, diagnostic kinds, the `reads`
    /// **span-tagged multiset** (occurrence + source span, order excepted),
    /// and the `writes` / `lineage` sets.
    fn assert_parity(sql: &str, catalog: Option<&Catalog>) {
        let plan = plan_op(sql, catalog);
        let old = resolver_op(sql, catalog);
        assert_eq!(
            plan.statement_kind, old.statement_kind,
            "statement-kind mismatch for: {sql}"
        );
        assert_eq!(
            diag_kinds(&plan),
            diag_kinds(&old),
            "diagnostic-kind mismatch for: {sql}"
        );
        assert_eq!(
            read_span_bag(&plan.reads),
            read_span_bag(&old.reads),
            "read-multiset mismatch for: {sql}"
        );
        assert_eq!(
            set(&plan.writes),
            set(&old.writes),
            "write-set mismatch for: {sql}"
        );
        assert_eq!(
            set(&plan.lineage),
            set(&old.lineage),
            "lineage-set mismatch for: {sql}\n  plan: {:?}\n  old:  {:?}",
            plan.lineage,
            old.lineage
        );
    }

    /// Like [`assert_parity`] but skips `lineage` — for statements whose
    /// lineage is a deferred refinement or a deliberate improvement (see
    /// [`reads_writes_only_corpus`]).
    fn assert_reads_writes_parity(sql: &str, catalog: Option<&Catalog>) {
        let plan = plan_op(sql, catalog);
        let old = resolver_op(sql, catalog);
        assert_eq!(
            read_span_bag(&plan.reads),
            read_span_bag(&old.reads),
            "read-multiset mismatch for: {sql}"
        );
        assert_eq!(
            set(&plan.writes),
            set(&old.writes),
            "write-set mismatch for: {sql}"
        );
    }

    /// Checks only `reads` — for statements whose `writes` / `lineage`
    /// deliberately differ (see [`reads_only_corpus`]).
    fn assert_reads_parity(sql: &str, catalog: Option<&Catalog>) {
        assert_eq!(
            read_span_bag(&plan_op(sql, catalog).reads),
            read_span_bag(&resolver_op(sql, catalog).reads),
            "read-multiset mismatch for: {sql}"
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
            // GROUP BY / HAVING / ORDER BY (clause-phase, alias visibility).
            "SELECT a, COUNT(*) FROM t GROUP BY a",
            "SELECT a FROM t GROUP BY a HAVING SUM(b) > 0",
            "SELECT a + b AS total FROM t ORDER BY total",
            "SELECT a AS x FROM t GROUP BY x",
            "SELECT a FROM t ORDER BY b",
            "SELECT a, b FROM t GROUP BY ROLLUP(a, b)",
            "SELECT a, b FROM t GROUP BY GROUPING SETS ((a, b), (a))",
            "SELECT x.a FROM x JOIN y ON x.id = y.id GROUP BY x.a ORDER BY x.a",
            // JOIN … USING (col): an unqualified merge-column ref fans in
            // to every joined side; a qualified one keeps its single owner;
            // a non-merge unqualified ref stays ambiguous.
            "SELECT a FROM x JOIN y USING (a)",
            "SELECT a, b FROM x JOIN y USING (a)",
            "SELECT x.a FROM x JOIN y USING (a)",
            "SELECT a FROM x JOIN y USING (a) WHERE a > 0",
            "SELECT a FROM x JOIN y USING (a) JOIN z USING (a)",
            // Derived tables (subquery in FROM): the outer ref resolves to
            // the subquery's output column, whose provenance is the inner
            // real column — collapse falls out of construction.
            "SELECT d.x FROM (SELECT a AS x FROM t) d",
            "SELECT x FROM (SELECT a AS x FROM t) d",
            "SELECT d.x, d.y FROM (SELECT a AS x, b AS y FROM t) d",
            "SELECT x FROM (SELECT a + b AS x FROM t) d",
            "SELECT x FROM (SELECT a AS x FROM t) d JOIN u ON d.x = u.id",
            "SELECT d.x FROM (SELECT a AS x FROM t WHERE b > 0) d WHERE d.x > 0",
            // CTEs (WITH): a reference resolves to the CTE's synthetic
            // relation, same as a derived table.
            "WITH c AS (SELECT id FROM t) SELECT id FROM c",
            "WITH c AS (SELECT a, b FROM t) SELECT a FROM c WHERE b > 0",
            "WITH c AS (SELECT id FROM t) SELECT c.id FROM c",
            "WITH c AS (SELECT id FROM t) SELECT d.id FROM c AS d",
            // NB: a chained `WITH a …, b AS (… FROM a) … FROM b` is an
            // intentional *improvement* (B resolves the body ref through
            // the chain to the real table; the resolver yields Ambiguous
            // because its flat scope leaks both CTEs), so it lives as a
            // binder unit test, not here in the strict-parity corpus.
            // Subqueries in expressions (uncorrelated): the subquery's
            // reads fold into the containing expression's position —
            // filter for WHERE / IN / EXISTS, value for a SELECT scalar.
            "SELECT a FROM t WHERE b IN (SELECT id FROM u)",
            "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.x > 0)",
            "SELECT a FROM t WHERE a > (SELECT avg(b) FROM u)",
            // Correlated subqueries: an inner reference to an outer
            // relation falls through the correlation stack to it.
            "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.x = t.a)",
            "SELECT a FROM t WHERE b > (SELECT avg(c) FROM u WHERE u.k = t.a)",
            // Scalar subquery in a projection (value position): only its
            // output is a lineage source of the column; its internal
            // predicate reads surface as reads, not lineage.
            "SELECT (SELECT max(x) FROM u) AS m FROM t",
            "SELECT (SELECT max(x) FROM u WHERE u.t_id = t.id) AS m FROM t",
            // Set operations (UNION / INTERSECT / EXCEPT): reads fan in
            // from every branch; a derived/CTE over a UNION traces each.
            "SELECT a FROM t UNION SELECT b FROM u",
            "SELECT a FROM t INTERSECT SELECT b FROM u",
            "SELECT x FROM (SELECT a AS x FROM t UNION SELECT b AS x FROM u) d",
            // DML / DDL: reads come from the source query, SET / predicate
            // expressions, and VALUES — the write targets aren't reads.
            "INSERT INTO target (a, b) SELECT x, y FROM source",
            "INSERT INTO target SELECT x, y FROM source",
            "INSERT INTO target (a, b) VALUES (1, 2)",
            "UPDATE t SET c = a + b WHERE d > 0",
            "UPDATE t SET c = a + b FROM s WHERE t.id = s.id",
            "DELETE FROM t WHERE d > 0",
            "CREATE TABLE dst AS SELECT a, b FROM src",
            "CREATE VIEW v AS SELECT a, b FROM src WHERE c > 0",
            // MERGE with an unqualified INSERT clause: writes / lineage
            // pair to the (canonical) target columns.
            "MERGE INTO target t USING source s ON t.id = s.id \
             WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, s.v)",
            // Unsupported statement: empty surfaces + an UnsupportedStatement
            // diagnostic, matching the resolver-based extractor.
            "CREATE INDEX idx ON t (a)",
            // Wildcards: left unexpanded, one WildcardSuppressed diagnostic
            // each (nested ones included). (A wildcard *before* a named
            // column would shift that column's QueryOutput position — a
            // separate refinement — so the corpus keeps wildcards last.)
            "SELECT * FROM t",
            "SELECT a, * FROM t",
            "SELECT *, * FROM t",
            // Nested wildcard (in a CTE body) is reported too. (Referencing
            // a column *out of* a wildcard relation — where the relation
            // should act open rather than empty — is a separate refinement,
            // so this case doesn't read through `c`.)
            "WITH c AS (SELECT * FROM t) SELECT 1 FROM c",
        ]
    }

    /// Statements whose `reads` / `writes` match the resolver but whose
    /// `lineage` is a deliberate **improvement**, so the harness checks
    /// only the first two surfaces: a recursive CTE — B collapses the
    /// body's output through to the real base table, while the resolver
    /// stops at the CTE binding as the lineage source (its documented
    /// deferred recursive-body collapse).
    fn reads_writes_only_corpus() -> &'static [&'static str] {
        &[
            "WITH RECURSIVE c AS (SELECT id FROM t UNION ALL SELECT id FROM c) SELECT id FROM c",
            "WITH RECURSIVE c(n) AS (SELECT 1 FROM t UNION ALL SELECT n + 1 FROM c WHERE n < 10) SELECT n FROM c",
        ]
    }

    /// Statements whose `reads` match but whose `writes` *and* `lineage`
    /// intentionally differ. A MERGE `WHEN MATCHED UPDATE SET t.col = …`
    /// writes through the target's *alias*: the resolver surfaces the
    /// write / lineage target as the alias-qualified `t.col`, while B
    /// canonicalizes it to the real `target.col` (consistent with every
    /// other write target). Reads are unaffected.
    fn reads_only_corpus() -> &'static [&'static str] {
        &["MERGE INTO target t USING source s ON t.id = s.id \
           WHEN MATCHED THEN UPDATE SET t.v = s.v \
           WHEN NOT MATCHED THEN INSERT (id, v) VALUES (s.id, s.v)"]
    }

    #[test]
    fn recursive_cte_lineage_reaches_real_table() {
        // Pins the improvement: B's recursive-CTE lineage collapses the
        // CTE body through to the real base table `t`, where the resolver
        // stops at the CTE binding `c`.
        let plan = bind_one(
            "WITH RECURSIVE c AS (SELECT id FROM t UNION ALL SELECT id FROM c) SELECT id FROM c",
            None,
        );
        let source_tables: HashSet<Option<String>> = extract_lineage(&plan)
            .iter()
            .map(|edge| {
                edge.source
                    .reference
                    .table
                    .as_ref()
                    .map(|t| t.name.value.clone())
            })
            .collect();
        assert_eq!(source_tables, set(&[Some("t".to_string())]));
    }

    #[test]
    fn projection_subquery_lineage_excludes_internal_filter() {
        // A scalar subquery in a projection: only its output (u.x) is a
        // lineage source of `m`; its internal predicate (u.t_id = t.id)
        // reads columns but is not lineage.
        let plan = bind_one(
            "SELECT (SELECT max(x) FROM u WHERE u.t_id = t.id) AS m FROM t",
            None,
        );
        let lineage_sources: Vec<(String, String)> = extract_lineage(&plan)
            .iter()
            .map(|edge| {
                let r = &edge.source.reference;
                (
                    r.table.as_ref().unwrap().name.value.clone(),
                    r.name.value.clone(),
                )
            })
            .collect();
        assert_eq!(lineage_sources, vec![("u".to_string(), "x".to_string())]);
        // The filter columns still surface as reads (u.x, u.t_id, t.id).
        assert_eq!(set(&extract_reads(&plan)).len(), 3);
    }

    #[test]
    fn catalog_free_reads_match_resolver() {
        for sql in covered_corpus() {
            assert_parity(sql, None);
        }
        for sql in reads_writes_only_corpus() {
            assert_reads_writes_parity(sql, None);
        }
        for sql in reads_only_corpus() {
            assert_reads_parity(sql, None);
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
            "SELECT a FROM x",                                                  // Cataloged
            "SELECT z FROM x",                // Unresolved (catalog denies)
            "SELECT a, b FROM t WHERE a > 0", // all Cataloged
            "SELECT x.a, y.b FROM x JOIN y ON x.id = y.id", // qualified Cataloged
            "SELECT a FROM x JOIN y ON x.id = y.id", // a only in x → Known witness
            "SELECT id FROM x JOIN y ON x.id = y.id", // id in both → Ambiguous
            "SELECT d.a FROM (SELECT a FROM x) d", // Cataloged through a derived table
            "SELECT a FROM (SELECT a FROM x) d", // unqualified, Cataloged through derived
            "WITH c AS (SELECT a FROM x) SELECT a FROM c", // Cataloged through a CTE
            "SELECT a FROM x WHERE id IN (SELECT id FROM y)", // subquery Cataloged
            "SELECT a FROM x WHERE EXISTS (SELECT 1 FROM y WHERE y.id = x.id)", // correlated Cataloged
            "SELECT a FROM x UNION SELECT b FROM y", // UNION Cataloged both branches
            "SELECT id FROM x JOIN y USING (id)",    // USING fan-in, both declare → Cataloged
            "SELECT a FROM x JOIN y USING (a)",      // USING fan-in narrows to x (y lacks a)
        ] {
            assert_parity(sql, Some(&catalog));
        }
    }
}
