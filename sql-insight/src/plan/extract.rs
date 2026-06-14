//! Walking the bound [`Plan`] to recover the operation surfaces: the
//! `reads`, `writes`, and `lineage` an extractor exposes. The
//! differential harness below is the strangler safety net — every
//! covered statement's surfaces must match the current resolver.

use super::ir::{BoundColumn, CtePlan, Plan, ScanRole, Write};
use crate::extractor::{ColumnLineageEdge, ColumnTarget, TableLineageEdge};
use crate::reference::{ColumnRead, ColumnReference, TableRead, TableReference};

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
        // A `CteRef` is resolved through the scope at bind time; its body's
        // reads are counted once at the `With` node below, not per reference.
        Plan::Scan(_) | Plan::OpaqueLeaf | Plan::CteRef(_) => {}
        Plan::PassThrough(pt) => {
            out.extend(pt.reads.iter().cloned());
            for input in &pt.inputs {
                collect_reads(input, out);
            }
            for subquery in &pt.subqueries {
                collect_reads(subquery, out);
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
            for subquery in &project.subqueries {
                collect_reads(subquery, out);
            }
        }
        Plan::SetOp(set) => {
            for operand in &set.operands {
                collect_reads(operand, out);
            }
        }
        Plan::Write(write) => {
            collect_reads(&write.input, out);
            // RETURNING reads the target's columns (value-position
            // provenance), like a projection over the written relation.
            for column in &write.returning {
                out.extend(
                    column
                        .provenance
                        .iter()
                        .filter(|source| !source.synthetic_origin)
                        .map(|source| source.read.clone()),
                );
            }
        }
        // Each declared CTE body is walked once here regardless of how many
        // references consume it (or whether any do) — its reads count once.
        Plan::With(with) => {
            collect_reads(&with.body, out);
            for cte in &with.ctes {
                collect_reads(&cte.plan, out);
            }
        }
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
        // A statement-level `WITH … INSERT/UPDATE …` wraps the DML `Write`.
        Plan::With(with) => extract_writes(&with.body),
        _ => Vec::new(),
    }
}

/// Every table the statement reads from, occurrence-based: one
/// [`TableRead`] per read-role `Scan` in the tree (a table scanned more
/// than once appears more than once), carrying the scan's table-level
/// [`ResolutionKind`](crate::reference::ResolutionKind). Write-target
/// scans are skipped — they surface through [`extract_table_writes`]. CTE
/// bodies and predicate / scalar subqueries are walked, so the real
/// tables they read surface too. Order is the tree walk's, incidental
/// like the column-level reads.
pub(crate) fn extract_table_reads(plan: &Plan) -> Vec<TableRead> {
    let mut reads = Vec::new();
    collect_table_reads(plan, &mut reads);
    reads
}

fn collect_table_reads(plan: &Plan, out: &mut Vec<TableRead>) {
    match plan {
        // A read-role scan is a table read; a write-target scan is in the
        // tree only for resolution scope and surfaces via `Write.target`.
        Plan::Scan(scan) => {
            if scan.role == ScanRole::Read {
                out.push(TableRead {
                    reference: scan.table.clone(),
                    resolution: scan.resolution,
                });
            }
        }
        Plan::OpaqueLeaf | Plan::CteRef(_) => {}
        Plan::PassThrough(pt) => {
            for input in &pt.inputs {
                collect_table_reads(input, out);
            }
            for subquery in &pt.subqueries {
                collect_table_reads(subquery, out);
            }
        }
        Plan::Project(project) => {
            collect_table_reads(&project.input, out);
            for subquery in &project.subqueries {
                collect_table_reads(subquery, out);
            }
        }
        Plan::SetOp(set) => {
            for operand in &set.operands {
                collect_table_reads(operand, out);
            }
        }
        Plan::Write(write) => collect_table_reads(&write.input, out),
        Plan::With(with) => {
            collect_table_reads(&with.body, out);
            for cte in &with.ctes {
                collect_table_reads(&cte.plan, out);
            }
        }
    }
}

/// The table the statement writes to: a [`Write`]'s target, else nothing.
/// Occurrence-based like [`extract_table_reads`]; one target per `Write`.
pub(crate) fn extract_table_writes(plan: &Plan) -> Vec<TableReference> {
    match plan {
        Plan::Write(write) => vec![write.target.clone()],
        // A statement-level `WITH … INSERT/UPDATE …` wraps the DML `Write`.
        Plan::With(with) => extract_table_writes(&with.body),
        _ => Vec::new(),
    }
}

/// Table-level lineage: one `source → target` edge per real table that
/// **feeds data** into the [`Write`] target, occurrence-based (a source
/// used twice emits two edges). Feeding sources are the read-role scans on
/// the value / data path of the source — FROM / JOIN relations, value
/// (projection / SET) subqueries, and the bodies of referenced CTEs —
/// **not** predicate (filter) subqueries, and never the write target
/// itself (its scan is write-role). A bare query, or a `Write` that moves
/// no data (`DELETE`), has no lineage; the caller gates on the statement
/// kind, since a column-less INSERT and a DELETE are structurally alike.
pub(crate) fn extract_table_lineage(plan: &Plan) -> Vec<TableLineageEdge> {
    // Peel leading WITHs, keeping their CTE bodies so a `CteRef` on the
    // feeding path can be resolved to the body's feeding scans.
    let mut ctes: Vec<&CtePlan> = Vec::new();
    let mut node = plan;
    while let Plan::With(with) = node {
        ctes.extend(with.ctes.iter());
        node = &with.body;
    }
    let Plan::Write(write) = node else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    feeding_scans(&write.input, &mut ctes, &mut sources);
    sources
        .into_iter()
        .map(|source| TableLineageEdge {
            source,
            target: write.target.clone(),
        })
        .collect()
}

/// Collect the read-role scans that feed data up through `plan` (a value /
/// data path): joins and filters pass their inputs through, a projection
/// also pulls its value subqueries, but a filter's predicate subqueries do
/// not feed. A `CteRef` resolves to the referenced CTE body's feeding
/// scans (innermost declaration shadows).
fn feeding_scans<'a>(plan: &'a Plan, ctes: &mut Vec<&'a CtePlan>, out: &mut Vec<TableRead>) {
    match plan {
        Plan::Scan(scan) => {
            if scan.role == ScanRole::Read {
                out.push(TableRead {
                    reference: scan.table.clone(),
                    resolution: scan.resolution,
                });
            }
        }
        Plan::OpaqueLeaf => {}
        Plan::CteRef(cteref) => {
            // A reference on the feeding path pulls the CTE body's sources.
            if let Some(cte) = ctes.iter().rev().find(|c| c.name == cteref.name).copied() {
                feeding_scans(&cte.plan, ctes, out);
            }
        }
        Plan::PassThrough(pt) => {
            // Inputs feed; predicate (filter) subqueries do not.
            for input in &pt.inputs {
                feeding_scans(input, ctes, out);
            }
        }
        Plan::Project(project) => {
            feeding_scans(&project.input, ctes, out);
            // Value-position subqueries (scalar projection / SET RHS) feed.
            for subquery in &project.subqueries {
                feeding_scans(subquery, ctes, out);
            }
        }
        Plan::SetOp(set) => {
            for operand in &set.operands {
                feeding_scans(operand, ctes, out);
            }
        }
        Plan::Write(write) => feeding_scans(&write.input, ctes, out),
        Plan::With(with) => {
            let added = with.ctes.len();
            ctes.extend(with.ctes.iter());
            feeding_scans(&with.body, ctes, out);
            ctes.truncate(ctes.len() - added);
        }
    }
}

/// The lineage edges the statement expresses: a [`Write`] pairs its
/// target columns with the source's output columns (`Relation` targets);
/// a bare query emits one `QueryOutput` edge group per projection.
/// Sources come straight from each output column's pre-collapsed
/// provenance (already real base columns carrying composed kind).
pub(crate) fn extract_lineage(plan: &Plan) -> Vec<ColumnLineageEdge> {
    // A leading statement-level `WITH` wraps the real root (a query or a
    // DML `Write`); peel it — the CTE bodies feed that root through
    // collapsed provenance, they are not lineage roots themselves.
    if let Plan::With(with) = plan {
        return extract_lineage(&with.body);
    }
    let mut edges = Vec::new();
    match plan {
        Plan::Write(write) => {
            write_lineage(write, &mut edges);
            // RETURNING projects the written relation: each column emits a
            // `QueryOutput` edge, so the statement both writes and returns.
            for (position, column) in write.returning.iter().enumerate() {
                let target = ColumnTarget::QueryOutput {
                    name: column.name.clone(),
                    position,
                };
                emit_edges(column, &target, &mut edges);
            }
        }
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
        // A `With` exposes its body's outputs; its CTE bodies feed
        // references (via collapsed provenance), never the query output. A
        // `CteRef` has no inspectable outputs of its own (resolved at bind).
        Plan::With(with) => output_operands(&with.body),
        Plan::Scan(_) | Plan::OpaqueLeaf | Plan::Write(_) | Plan::CteRef(_) => Vec::new(),
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
    use crate::extractor::{
        extract_column_operations, extract_table_operations, ColumnOperation, TableOperation,
    };
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
            // Expression-arm coverage: every sub-expression's column refs
            // surface as reads; the value / filter split (CASE conditions,
            // window PARTITION / ORDER keys, aggregate FILTER) keeps
            // predicate refs out of lineage while values flow.
            "SELECT a FROM t WHERE c BETWEEN lo AND hi",
            "SELECT CASE WHEN a > 0 THEN b ELSE c END AS m FROM t",
            "SELECT CASE d WHEN 1 THEN b ELSE c END AS m FROM t",
            "SELECT a FROM t WHERE x LIKE y",
            "SELECT a FROM t WHERE b IN (1, c)",
            "SELECT a FROM t WHERE b IS NOT NULL AND NOT (c > 0)",
            "SELECT COALESCE(a, b) AS m FROM t",
            "SELECT substring(a FROM 1 FOR b) AS m FROM t",
            "SELECT sum(a) OVER (PARTITION BY b ORDER BY c) AS m FROM t",
            "SELECT sum(a) FILTER (WHERE b > 0) AS m FROM t GROUP BY c",
            // Relation-arm coverage: table functions / UNNEST / PIVOT /
            // nested join / LATERAL. A table-function output is opaque (a
            // qualified ref to its alias yields nothing), but its argument
            // expressions read against the surrounding (LATERAL-visible)
            // scope; PIVOT / UNPIVOT read the inner table's columns; a
            // nested join exposes its inner tables directly.
            "SELECT g.v FROM TABLE(gen(t.a)) g",
            "SELECT u.x FROM UNNEST(t.arr) u",
            "SELECT * FROM t PIVOT(SUM(t.amt) FOR t.mon IN ('a', 'b'))",
            "SELECT * FROM t UNPIVOT(v FOR n IN (t.a, t.b))",
            "SELECT t1.a FROM (t1 JOIN t2 ON t1.id = t2.id)",
            "SELECT f.value FROM t, LATERAL FLATTEN(input => t.arr) AS f",
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
            // Shared-node CTE: the body is walked once regardless of
            // reference count, so its reads are counted once (not per
            // reference) and an unreferenced CTE's body still surfaces —
            // both matching the resolver.
            "WITH c AS (SELECT a FROM t) SELECT c1.a FROM c c1 JOIN c c2 ON c1.a = c2.a",
            "WITH c AS (SELECT a FROM t) SELECT b FROM other",
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
            // Statement-level WITH over DML: sqlparser nests the INSERT as
            // the WITH-query's body, so writes / lineage must peel the With.
            "WITH c AS (SELECT a FROM s) INSERT INTO t (col) SELECT a FROM c",
            // RETURNING: a projection over the written relation — reads the
            // target's columns and emits QueryOutput lineage, alongside the
            // statement's write / write-lineage.
            "INSERT INTO t (a, b) VALUES (1, 2) RETURNING id",
            "INSERT INTO t (a) SELECT x FROM s RETURNING id AS pk",
            "UPDATE t SET a = b + 1 WHERE id = 5 RETURNING id, a",
            "DELETE FROM t WHERE id = 5 RETURNING id, val",
            "INSERT INTO t (a) VALUES (1) RETURNING id + 1 AS bumped",
            "INSERT INTO t (a) VALUES (1) RETURNING *",
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
    fn table_lineage_feeds_values_not_predicates() {
        // At table granularity a source feeds the target iff it is on the
        // value / data path: the FROM source and a value-position subquery
        // feed; a predicate-position subquery does not (it's a read only).
        let plan = bind_one(
            "INSERT INTO target SELECT (SELECT max(v) FROM u) FROM source \
             WHERE id IN (SELECT id FROM x)",
            None,
        );
        let edges: Vec<(String, String)> = extract_table_lineage(&plan)
            .iter()
            .map(|e| {
                (
                    e.source.reference.name.value.clone(),
                    e.target.name.value.clone(),
                )
            })
            .collect();
        assert_eq!(
            set(&edges),
            set(&[
                ("source".to_string(), "target".to_string()),
                ("u".to_string(), "target".to_string()),
            ])
        );
        // `x` (predicate subquery) is excluded from lineage but is a read.
        assert!(extract_table_reads(&plan)
            .iter()
            .any(|r| r.reference.name.value == "x"));
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

    // ---- Table-level differential ----

    fn table_plan_op(sql: &str, catalog: Option<&Catalog>) -> TableOperation {
        let dialect = GenericDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let casing = IdentifierCasing::for_dialect(&dialect);
        crate::plan::table_operation::table_operation(&statements[0], catalog, casing)
    }

    fn table_resolver_op(sql: &str, catalog: Option<&Catalog>) -> TableOperation {
        let dialect = GenericDialect {};
        extract_table_operations(&dialect, sql, catalog)
            .unwrap()
            .remove(0)
            .unwrap()
    }

    /// A table read as a `path|resolution` key. Table references carry no
    /// span, so occurrence is the only multiplicity signal — the bag is a
    /// sorted multiset (order non-contractual, like column reads).
    fn table_read_bag(reads: &[TableRead]) -> Vec<String> {
        let mut keys: Vec<String> = reads
            .iter()
            .map(|r| format!("{}|{:?}", table_path(&r.reference), r.resolution))
            .collect();
        keys.sort();
        keys
    }

    fn table_ref_bag(refs: &[TableReference]) -> Vec<String> {
        let mut keys: Vec<String> = refs.iter().map(table_path).collect();
        keys.sort();
        keys
    }

    fn table_path(t: &TableReference) -> String {
        format!(
            "{}.{}.{}",
            t.catalog.as_ref().map(|c| c.value.as_str()).unwrap_or(""),
            t.schema.as_ref().map(|s| s.value.as_str()).unwrap_or(""),
            t.name.value,
        )
    }

    fn table_diag_kinds(op: &TableOperation) -> Vec<String> {
        let mut kinds: Vec<String> = op
            .diagnostics
            .iter()
            .map(|d| format!("{:?}", d.kind))
            .collect();
        kinds.sort();
        kinds
    }

    /// Lineage as a sorted multiset of `source|resolution → target` keys
    /// (occurrence-based, order non-contractual).
    fn table_lineage_bag(edges: &[TableLineageEdge]) -> Vec<String> {
        let mut keys: Vec<String> = edges
            .iter()
            .map(|e| {
                format!(
                    "{}|{:?}->{}",
                    table_path(&e.source.reference),
                    e.source.resolution,
                    table_path(&e.target),
                )
            })
            .collect();
        keys.sort();
        keys
    }

    /// Full table-level parity: `statement_kind` + diagnostic kinds +
    /// `reads` (path|resolution multiset) + `writes` (path multiset) +
    /// `lineage` (source|resolution → target multiset) must match the
    /// resolver.
    fn assert_table_parity(sql: &str, catalog: Option<&Catalog>) {
        let plan = table_plan_op(sql, catalog);
        let old = table_resolver_op(sql, catalog);
        assert_eq!(
            plan.statement_kind, old.statement_kind,
            "table statement-kind mismatch for: {sql}"
        );
        assert_eq!(
            table_diag_kinds(&plan),
            table_diag_kinds(&old),
            "table diagnostic-kind mismatch for: {sql}"
        );
        assert_eq!(
            table_read_bag(&plan.reads),
            table_read_bag(&old.reads),
            "table read-multiset mismatch for: {sql}"
        );
        assert_eq!(
            table_ref_bag(&plan.writes),
            table_ref_bag(&old.writes),
            "table write-multiset mismatch for: {sql}"
        );
        assert_eq!(
            table_lineage_bag(&plan.lineage),
            table_lineage_bag(&old.lineage),
            "table lineage-multiset mismatch for: {sql}"
        );
    }

    /// Table-level cases not already in the column corpus: the write-target
    /// role split (USING source is a read, the target isn't; a no-FROM
    /// UPDATE has no reads / lineage), a no-column read (`SELECT 1 FROM t`),
    /// and lineage feeding rules — value (projection) subqueries feed while
    /// predicate subqueries don't, joins feed every side, a self FROM feeds
    /// `t → t`, CTE transitivity feeds the inner real table, and an
    /// unreferenced CTE / a DELETE feed nothing.
    fn table_only_corpus() -> &'static [&'static str] {
        &[
            "DELETE FROM t USING s WHERE t.id = s.id",
            "UPDATE t SET c = 1",
            "SELECT 1 FROM t",
            "WITH c AS (SELECT a FROM s) INSERT INTO t SELECT a FROM c",
            "WITH c AS (SELECT a FROM s) INSERT INTO t SELECT 1",
            "INSERT INTO t SELECT a FROM s1 JOIN s2 ON s1.id = s2.id",
            "INSERT INTO target SELECT x FROM source WHERE id IN (SELECT id FROM x)",
            "INSERT INTO t SELECT (SELECT max(v) FROM u) FROM s",
            "INSERT INTO t SELECT t.a FROM t",
            "UPDATE t SET c = (SELECT v FROM u WHERE u.id = t.id)",
            "CREATE TABLE dst AS SELECT a FROM s1 UNION SELECT b FROM s2",
        ]
    }

    #[test]
    fn catalog_free_table_match_resolver() {
        for sql in covered_corpus()
            .iter()
            .chain(reads_writes_only_corpus())
            .chain(reads_only_corpus())
            .chain(table_only_corpus())
        {
            assert_table_parity(sql, None);
        }
    }

    #[test]
    fn catalog_aware_table_match_resolver() {
        // `public.x` / `public.y` are registered; `dup` is registered in
        // two schemas (→ Ambiguous); `missing` is unregistered (→ Inferred).
        let catalog: Catalog = [
            CatalogTable::new("public", "x").columns(["id", "a"]),
            CatalogTable::new("public", "y").columns(["id", "b"]),
            CatalogTable::new("public", "dup").columns(["a"]),
            CatalogTable::new("other", "dup").columns(["a"]),
        ]
        .into_iter()
        .collect();
        for sql in [
            "SELECT a FROM x",                       // x Cataloged
            "SELECT a FROM x JOIN y ON x.id = y.id", // both Cataloged
            "SELECT a FROM missing",                 // Inferred (no hit)
            "SELECT a FROM dup",                     // Ambiguous (two hits)
            "INSERT INTO x SELECT a FROM y",         // y Cataloged feeds x
            "SELECT 1 FROM x",                       // no-column read, Cataloged
        ] {
            assert_table_parity(sql, Some(&catalog));
        }
    }
}
