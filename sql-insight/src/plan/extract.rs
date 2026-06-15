//! Walking the bound [`Plan`] to recover the operation surfaces an
//! extractor exposes: the `reads`, `writes`, and `lineage` (column- and
//! table-level) plus the legacy flat table list.

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
        // A `Drop` reads nothing.
        Plan::Scan(_) | Plan::OpaqueLeaf | Plan::CteRef(_) | Plan::Drop(_) => {}
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
            // provenance); a conflict-action SET reads its RHS columns.
            // Both drop synthetic-origin sources (EXCLUDED / derived).
            for column in write.returning.iter().chain(&write.conflict_updates) {
                out.extend(
                    column
                        .provenance
                        .iter()
                        .filter(|source| !source.synthetic_origin)
                        .map(|source| source.read.clone()),
                );
            }
        }
        Plan::Delete(delete) => {
            collect_reads(&delete.input, out);
            // RETURNING reads the deleted relation's columns.
            for column in &delete.returning {
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
            // ON CONFLICT DO UPDATE SET targets are extra writes on the
            // same relation, after the INSERT columns.
            .chain(
                write
                    .conflict_updates
                    .iter()
                    .filter_map(|c| c.name.as_ref()),
            )
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

/// The flat list of every table the statement references — the legacy
/// "what tables does this SQL touch?" surface, with no read/write split.
/// One entry per relation *binding*: a table that plays both roles (an
/// `UPDATE`/`DELETE`/`MERGE` target that is also a row source) appears
/// once, while the same table reached through two separate FROM uses
/// appears twice. Built by collecting every `Scan` (read or write role)
/// plus the *declared* write targets that aren't themselves scans
/// (`INSERT`/CTAS/CREATE/ALTER targets, DROP/TRUNCATE relations) — an
/// `UPDATE`/`MERGE`/`DELETE` target is already a scan in the tree, so it
/// isn't double-counted. Order is the tree walk's, incidental.
pub(crate) fn extract_flat_tables(plan: &Plan) -> Vec<TableReference> {
    let mut tables = Vec::new();
    collect_flat_tables(plan, &mut tables);
    tables
}

fn collect_flat_tables(plan: &Plan, out: &mut Vec<TableReference>) {
    match plan {
        // Every scan is a binding — read sources and write targets alike.
        Plan::Scan(scan) => out.push(scan.table.clone()),
        Plan::OpaqueLeaf | Plan::CteRef(_) => {}
        // DROP / TRUNCATE relations are bindings with no scan.
        Plan::Drop(targets) => out.extend(targets.iter().cloned()),
        Plan::PassThrough(pt) => {
            for input in &pt.inputs {
                collect_flat_tables(input, out);
            }
            for subquery in &pt.subqueries {
                collect_flat_tables(subquery, out);
            }
        }
        Plan::Project(project) => {
            collect_flat_tables(&project.input, out);
            for subquery in &project.subqueries {
                collect_flat_tables(subquery, out);
            }
        }
        Plan::SetOp(set) => {
            for operand in &set.operands {
                collect_flat_tables(operand, out);
            }
        }
        Plan::Write(write) => {
            // An UPDATE / MERGE target is a write-role scan in the input, so
            // it's already a binding; an INSERT / CTAS / CREATE / VIEW /
            // ALTER target is external (no scan) — count it once, target
            // first to match the resolver's bind order.
            if !has_write_scan(&write.input) {
                out.push(write.target.clone());
            }
            collect_flat_tables(&write.input, out);
        }
        // A DELETE's target usually coincides with a scan in the input
        // (implicit target = write scan; explicit / USING target that
        // resolved to a row-source scan), already counted by the input
        // walk. A target that did *not* merge (a bare `t1` against FROM
        // `mydb.t1` — distinct bindings) is a separate reference, so add it.
        Plan::Delete(delete) => {
            let before = out.len();
            collect_flat_tables(&delete.input, out);
            let from_input: Vec<TableReference> = out[before..].to_vec();
            for target in &delete.targets {
                if !from_input.contains(target) {
                    out.push(target.clone());
                }
            }
        }
        Plan::With(with) => {
            collect_flat_tables(&with.body, out);
            for cte in &with.ctes {
                collect_flat_tables(&cte.plan, out);
            }
        }
    }
}

/// Whether `plan`'s subtree contains a write-role scan (an `UPDATE` /
/// `MERGE` / implicit-`DELETE` target bound in scope), distinguishing
/// those statements — whose target is already a scan — from `INSERT` /
/// CTAS / CREATE / ALTER, whose target is external to the input.
fn has_write_scan(plan: &Plan) -> bool {
    match plan {
        Plan::Scan(scan) => scan.role == ScanRole::Write,
        Plan::PassThrough(pt) => pt.inputs.iter().any(has_write_scan),
        Plan::Project(project) => has_write_scan(&project.input),
        Plan::SetOp(set) => set.operands.iter().any(has_write_scan),
        Plan::With(with) => has_write_scan(&with.body),
        Plan::Write(write) => has_write_scan(&write.input),
        Plan::Delete(delete) => has_write_scan(&delete.input),
        Plan::OpaqueLeaf | Plan::CteRef(_) | Plan::Drop(_) => false,
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
        Plan::OpaqueLeaf | Plan::CteRef(_) | Plan::Drop(_) => {}
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
        // A DELETE's read-role scans (FROM / USING row sources) are in the
        // input; its write-role target scans are skipped there (reported as
        // writes instead).
        Plan::Delete(delete) => collect_table_reads(&delete.input, out),
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
        // A DELETE writes (removes rows from) each of its targets.
        Plan::Delete(delete) => delete.targets.clone(),
        // DROP / TRUNCATE name their relations directly as write targets.
        Plan::Drop(targets) => targets.clone(),
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
    let mut active: Vec<&str> = Vec::new();
    feeding_scans(&write.input, &mut ctes, &mut active, &mut sources);
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
fn feeding_scans<'a>(
    plan: &'a Plan,
    ctes: &mut Vec<&'a CtePlan>,
    active: &mut Vec<&'a str>,
    out: &mut Vec<TableRead>,
) {
    match plan {
        Plan::Scan(scan) => {
            if scan.role == ScanRole::Read {
                out.push(TableRead {
                    reference: scan.table.clone(),
                    resolution: scan.resolution,
                });
            }
        }
        // A `Drop` / `TRUNCATE` / `DELETE` moves no row data, so it feeds no
        // lineage (a DELETE is never reached here — its table lineage is
        // gated off by kind — but the match stays exhaustive).
        Plan::OpaqueLeaf | Plan::Drop(_) | Plan::Delete(_) => {}
        Plan::CteRef(cteref) => {
            // A reference on the feeding path pulls the CTE body's sources.
            // `active` tracks the CTE names currently being expanded down
            // this path: a recursive CTE's body references itself, so we
            // skip a name already in flight — its self-reference adds no new
            // real source (the anchor's reads are collected on the first
            // descent), and skipping it breaks the otherwise-infinite loop.
            let name = cteref.name.value.as_str();
            if active.contains(&name) {
                return;
            }
            if let Some(cte) = ctes.iter().rev().find(|c| c.name == cteref.name).copied() {
                active.push(name);
                feeding_scans(&cte.plan, ctes, active, out);
                active.pop();
            }
        }
        Plan::PassThrough(pt) => {
            // Inputs feed; predicate (filter) subqueries do not.
            for input in &pt.inputs {
                feeding_scans(input, ctes, active, out);
            }
        }
        Plan::Project(project) => {
            feeding_scans(&project.input, ctes, active, out);
            // Value-position subqueries (scalar projection / SET RHS) feed.
            for subquery in &project.subqueries {
                feeding_scans(subquery, ctes, active, out);
            }
        }
        Plan::SetOp(set) => {
            for operand in &set.operands {
                feeding_scans(operand, ctes, active, out);
            }
        }
        Plan::Write(write) => feeding_scans(&write.input, ctes, active, out),
        Plan::With(with) => {
            let added = with.ctes.len();
            ctes.extend(with.ctes.iter());
            feeding_scans(&with.body, ctes, active, out);
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
            // A conflict-action SET assignment feeds a `Relation` edge into
            // its target column, like a standalone UPDATE.
            for column in &write.conflict_updates {
                if let Some(name) = &column.name {
                    let target = ColumnTarget::Relation(ColumnReference {
                        table: Some(write.target.clone()),
                        name: name.clone(),
                    });
                    emit_edges(column, &target, &mut edges);
                }
            }
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
        // A DELETE moves no data, so its only lineage is a RETURNING
        // projection of the deleted rows (one `QueryOutput` edge per source).
        Plan::Delete(delete) => {
            for (position, column) in delete.returning.iter().enumerate() {
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
        Plan::Scan(_)
        | Plan::OpaqueLeaf
        | Plan::Write(_)
        | Plan::Delete(_)
        | Plan::CteRef(_)
        | Plan::Drop(_) => Vec::new(),
    }
}
