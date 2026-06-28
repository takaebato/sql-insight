//! The **write** surfaces over a [`LogicalPlan`]. These back the
//! [`crate::resolver`] facade's `writes` / `table_writes` entry points:
//!
//! - `writes` — every column a DML root writes (INSERT columns, UPDATE SET
//!   targets, CTAS / CREATE VIEW / ALTER columns, MERGE WHEN-clause writes),
//!   qualified by the target relation.
//! - `table_writes` — one entry per DML target, paired with its catalog-match
//!   [`ResolutionKind`] (a [`TableWrite`]).

use sqlparser::ast::Ident;

use super::logical_plan::{dml_roots, LogicalPlan, MergeClause};
use super::origins::output_operands;
use crate::reference::{ColumnReference, ColumnWrite, ResolutionKind, TableReference, TableWrite};

// ===== writes ============================================================

/// Every column the statement writes — each DML root's target columns,
/// qualified by the write target. Order follows source order (the public
/// contract). Collects from the peeled outer root *and* every data-modifying
/// CTE body (`WITH c AS (INSERT …) …`). Backs [`crate::resolver::writes`].
pub(super) fn collect_writes(plan: &LogicalPlan) -> Vec<ColumnWrite> {
    dml_roots(plan)
        .into_iter()
        .flat_map(write_root_columns)
        .collect()
}

/// The columns one DML / DDL write root targets.
fn write_root_columns(root: &LogicalPlan) -> Vec<ColumnWrite> {
    match root {
        // INSERT columns (already catalog-resolved), then any ON CONFLICT DO
        // UPDATE SET targets (extra writes on the same relation).
        LogicalPlan::Insert(i) => {
            let mut w = i.columns.clone();
            w.extend(i.on_conflict.iter().map(|a| a.target.clone()));
            w
        }
        // Each SET assignment writes its (already resolved) target column.
        LogicalPlan::Update(u) => u.assignments.iter().map(|a| a.target.clone()).collect(),
        // CTAS / CREATE VIEW write the new relation's columns; ALTER TABLE
        // writes its column-naming operations' columns. These are freshly
        // created / altered, so they carry `Inferred` (no catalog column yet).
        LogicalPlan::CreateTableAs(c) => {
            created_relation_writes(&c.columns, &c.input, &c.target.reference)
        }
        LogicalPlan::CreateView(c) => {
            created_relation_writes(&c.columns, &c.input, &c.target.reference)
        }
        LogicalPlan::AlterTable(a) => inferred_writes(&a.columns, &a.target.reference),
        // MERGE writes each WHEN action's target columns (UPDATE SET targets;
        // INSERT columns paired with values).
        LogicalPlan::Merge(m) => m.clauses.iter().flat_map(merge_clause_writes).collect(),
        // No column writes: read-only / structural query operators, the bare
        // FROM-less / synthetic relations, and DML / DDL whose target rows go
        // wholesale (DELETE / DROP / TRUNCATE) — none qualify a column with a
        // write target. Listed explicitly so a new `LogicalPlan` variant
        // forces a write-placement decision rather than silently emitting
        // nothing here.
        LogicalPlan::Scan(_)
        | LogicalPlan::Filter(_)
        | LogicalPlan::Join(_)
        | LogicalPlan::Aggregate(_)
        | LogicalPlan::Projection(_)
        | LogicalPlan::Sort(_)
        | LogicalPlan::SetOp(_)
        | LogicalPlan::SubqueryAlias(_)
        | LogicalPlan::TableFunction(_)
        | LogicalPlan::With(_)
        | LogicalPlan::CteRef(_)
        | LogicalPlan::Values(_)
        | LogicalPlan::Empty
        | LogicalPlan::Delete(_)
        | LogicalPlan::Drop(_) => Vec::new(),
    }
}

/// The columns one MERGE WHEN action writes (already catalog-resolved).
fn merge_clause_writes(clause: &MergeClause) -> Vec<ColumnWrite> {
    match clause {
        MergeClause::Update { assignments } => {
            assignments.iter().map(|a| a.target.clone()).collect()
        }
        // Every named target column is a write, regardless of how many values
        // are supplied (an arity mismatch is flagged separately) — matching a
        // plain INSERT. A column-less MERGE INSERT has no names here, so it
        // writes nothing. (Lineage, which *pairs* a column with its value, zips
        // to the shorter side on its own path.)
        MergeClause::Insert { columns, .. } => columns.clone(),
        MergeClause::Delete => Vec::new(),
    }
}

/// Every table the statement writes to — one per DML target, across the peeled
/// outer root and every data-modifying CTE body. Backs
/// [`crate::resolver::table_writes`].
pub(super) fn collect_table_writes(plan: &LogicalPlan) -> Vec<TableWrite> {
    dml_roots(plan)
        .into_iter()
        .flat_map(write_root_tables)
        .collect()
}

/// The table(s) one DML / DDL write root targets.
pub(super) fn write_root_tables(root: &LogicalPlan) -> Vec<TableWrite> {
    match root {
        LogicalPlan::Insert(i) => vec![i.target.clone()],
        // The distinct tables the SET assignments write — the root for a
        // single-table UPDATE, but each qualified relation for a multi-table
        // `UPDATE t1 JOIN t2 SET t1.a = …, t2.b = …` (a table set in several
        // columns counts once; assignment order preserved). Each assignment
        // carries its target table's resolution (`Assignment::target_resolution`).
        LogicalPlan::Update(u) => {
            let mut out: Vec<TableWrite> = Vec::new();
            for a in &u.assignments {
                if let Some(table) = &a.target.reference.table {
                    if out.iter().any(|w| &w.reference == table) {
                        continue;
                    }
                    out.push(TableWrite {
                        reference: table.clone(),
                        resolution: a.target_resolution,
                    });
                }
            }
            out
        }
        // A DELETE removes rows from each of its targets.
        LogicalPlan::Delete(d) => d.targets.clone(),
        LogicalPlan::CreateTableAs(c) => vec![c.target.clone()],
        LogicalPlan::CreateView(c) => vec![c.target.clone()],
        LogicalPlan::AlterTable(a) => vec![a.target.clone()],
        LogicalPlan::Merge(m) => vec![m.target.clone()],
        // DROP / TRUNCATE name their relations directly as write targets.
        LogicalPlan::Drop(d) => d.targets.clone(),
        // No write target: read-only / structural query operators and the
        // FROM-less / synthetic relations. Listed explicitly so a new
        // `LogicalPlan` variant forces a target decision here.
        LogicalPlan::Scan(_)
        | LogicalPlan::Filter(_)
        | LogicalPlan::Join(_)
        | LogicalPlan::Aggregate(_)
        | LogicalPlan::Projection(_)
        | LogicalPlan::Sort(_)
        | LogicalPlan::SetOp(_)
        | LogicalPlan::SubqueryAlias(_)
        | LogicalPlan::TableFunction(_)
        | LogicalPlan::With(_)
        | LogicalPlan::CteRef(_)
        | LogicalPlan::Values(_)
        | LogicalPlan::Empty => Vec::new(),
    }
}

/// Qualify bare written column names with the write target as `Inferred`
/// [`ColumnWrite`]s — for freshly created / altered relations, whose columns
/// aren't in any catalog yet.
fn inferred_writes(columns: &[Ident], target: &TableReference) -> Vec<ColumnWrite> {
    columns
        .iter()
        .map(|name| ColumnWrite {
            reference: ColumnReference {
                table: Some(target.clone()),
                name: name.clone(),
            },
            resolution: ResolutionKind::Inferred,
        })
        .collect()
}

/// The columns a CTAS / CREATE VIEW writes. An *explicit* column list
/// (`CREATE TABLE t (a, b) AS …`) is authoritative; the implicit form takes
/// each source output's inferred name (an anonymous output is unnameable, so
/// dropped — never positionally shifting later columns). A set-op source takes
/// its result schema from the left branch. Kept in step with
/// [`super::lineage`]'s `created_relation_lineage` so writes and lineage agree.
fn created_relation_writes(
    explicit: &[Ident],
    input: &LogicalPlan,
    target: &TableReference,
) -> Vec<ColumnWrite> {
    if !explicit.is_empty() {
        return inferred_writes(explicit, target);
    }
    let names: Vec<Ident> = match output_operands(input).first() {
        Some(operand) => operand
            .outputs
            .iter()
            .filter_map(|ne| ne.name.clone())
            .collect(),
        None => Vec::new(),
    };
    inferred_writes(&names, target)
}
