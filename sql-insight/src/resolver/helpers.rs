//! Free helper functions shared by the [`Binder`](super::binder): small,
//! self-contained utilities for building / tagging plan nodes, pulling
//! structure out of the sqlparser AST, and constructing reads / provenance
//! sources. All `pub(super)` — resolver-internal.

use sqlparser::ast::{
    AlterTableOperation, AssignmentTarget, Expr, Ident, Insert, Join, JoinConstraint, JoinOperator,
    ObjectName, SetExpr, Table, TableAlias,
};

use super::ir::{BoundColumn, PassThrough, Plan, ProvenanceSource, Scan, ScanRole};
use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableReference};

/// The join constraint (`ON …` / `USING …`) of a join operator, if any;
/// `CROSS`/`APPLY` forms carry none.
pub(super) fn join_constraint(join: &Join) -> Option<&JoinConstraint> {
    match &join.join_operator {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::CrossJoin(c)
        | JoinOperator::Semi(c)
        | JoinOperator::LeftSemi(c)
        | JoinOperator::RightSemi(c)
        | JoinOperator::Anti(c)
        | JoinOperator::LeftAnti(c)
        | JoinOperator::RightAnti(c)
        | JoinOperator::StraightJoin(c) => Some(c),
        JoinOperator::AsOf { constraint, .. } => Some(constraint),
        JoinOperator::CrossApply | JoinOperator::OuterApply => None,
    }
}

/// Combine source inputs under an optional filter: a lone input with no
/// reads passes through unwrapped; otherwise a `PassThrough` joins the
/// inputs and carries the filter `reads`. Used to build a DML statement's
/// scanned-and-filtered source below its SET / VALUES `Project`.
pub(super) fn wrap_inputs(
    mut inputs: Vec<Plan>,
    reads: Vec<ColumnRead>,
    subqueries: Vec<Plan>,
) -> Plan {
    if reads.is_empty() && subqueries.is_empty() && inputs.len() == 1 {
        inputs.pop().unwrap()
    } else {
        Plan::PassThrough(PassThrough {
            inputs,
            reads,
            subqueries,
        })
    }
}

/// Whether an `INSERT`'s source query exposes a per-column projection
/// (a `SELECT` / set-operation / nested query) rather than a `VALUES`
/// row set. Drives whether `EXCLUDED.col` collapses to the source: a
/// `VALUES` source has no projection, so `EXCLUDED` stays opaque.
pub(super) fn source_has_projection(insert: &Insert) -> bool {
    insert
        .source
        .as_ref()
        .is_some_and(|query| !matches!(query.body.as_ref(), SetExpr::Values(_)))
}

/// The column names an `ALTER TABLE` operation writes to. Column-naming
/// ops (ADD / DROP / MODIFY / ALTER COLUMN) name one column; RENAME /
/// CHANGE name both the old and new (both ends of the rename are useful
/// to a lineage consumer). Schema-level ops (constraints, partitions,
/// RENAME TABLE, …) name no columns.
pub(super) fn alter_table_op_target_columns(op: &AlterTableOperation) -> Vec<Ident> {
    match op {
        AlterTableOperation::AddColumn { column_def, .. } => vec![column_def.name.clone()],
        AlterTableOperation::DropColumn { column_names, .. } => column_names.clone(),
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => vec![old_column_name.clone(), new_column_name.clone()],
        AlterTableOperation::ChangeColumn {
            old_name, new_name, ..
        } if old_name != new_name => vec![old_name.clone(), new_name.clone()],
        AlterTableOperation::ChangeColumn { old_name, .. } => vec![old_name.clone()],
        AlterTableOperation::ModifyColumn { col_name, .. } => vec![col_name.clone()],
        AlterTableOperation::AlterColumn { column_name, .. } => vec![column_name.clone()],
        _ => Vec::new(),
    }
}

/// Re-tag a write target's leaf `Scan` as [`ScanRole::Write`] so
/// table-level read extraction skips it (it is reported via
/// `Write.target`). The target is still kept in the tree so its columns
/// stay in scope for resolving SET / WHERE / ON. A joined target keeps
/// only its leftmost leaf as the write; partners stay reads.
pub(super) fn into_write_target(plan: Plan) -> Plan {
    match plan {
        Plan::Scan(scan) => Plan::Scan(Scan {
            role: ScanRole::Write,
            ..scan
        }),
        // A joined target (`UPDATE t1 JOIN t2 …`): only the target relation
        // (the leftmost leaf) is the write; the joined partners stay reads.
        Plan::PassThrough(mut pt) => {
            if let Some(first) = pt.inputs.first_mut() {
                let taken = std::mem::replace(first, Plan::OpaqueLeaf);
                *first = into_write_target(taken);
            }
            Plan::PassThrough(pt)
        }
        other => other,
    }
}

/// Wrap `plan` in a filter `PassThrough` carrying `reads` and the
/// `subqueries` of those predicates, or return it unchanged when there's
/// nothing to carry.
pub(super) fn wrap_reads(plan: Plan, reads: Vec<ColumnRead>, subqueries: Vec<Plan>) -> Plan {
    if reads.is_empty() && subqueries.is_empty() {
        plan
    } else {
        Plan::PassThrough(PassThrough {
            inputs: vec![plan],
            reads,
            subqueries,
        })
    }
}

/// Merge two set-operation branches' output columns positionally: each
/// result column unions both branches' (kind-carrying) provenance and
/// takes its name from the left branch. Extra columns on either side
/// (mismatched arity) are dropped — a set operation requires equal arity,
/// and any dropped branch's reads still surface from its own sub-plan.
pub(super) fn merge_set_outputs(
    left: Vec<BoundColumn>,
    right: Vec<BoundColumn>,
) -> Vec<BoundColumn> {
    left.into_iter()
        .zip(right)
        .map(|(left, right)| {
            let mut provenance = left.provenance;
            provenance.extend(right.provenance);
            BoundColumn {
                name: left.name,
                provenance,
            }
        })
        .collect()
}

/// Apply a CTE / derived table's explicit column list (`AS c(x, y)`),
/// renaming the body's output columns positionally. Surplus outputs keep
/// their inferred names; surplus alias names have nothing to bind to.
pub(super) fn apply_column_aliases(outputs: &mut [BoundColumn], alias: &TableAlias) {
    for (output, column) in outputs.iter_mut().zip(&alias.columns) {
        output.name = Some(column.name.clone());
    }
}

/// The named output columns of a bound body — the target column list a
/// CTAS / CREATE VIEW pairs its source against when no explicit columns
/// are given. Anonymous (un-nameable) outputs are dropped.
pub(super) fn output_names(outputs: &[BoundColumn]) -> Vec<Ident> {
    outputs.iter().filter_map(|c| c.name.clone()).collect()
}

/// The lineage sources a bound plan exposes through its output columns —
/// a nested subquery's output provenance, used as the lineage sources of
/// the enclosing value (its internal filter reads are collected
/// separately).
pub(super) fn output_sources(plan: &Plan) -> Vec<ProvenanceSource> {
    super::extract::output_operands(plan)
        .iter()
        .flat_map(|operand| operand.iter())
        .flat_map(|column| column.provenance.iter().cloned())
        .collect()
}

/// The last (rightmost) identifier of a possibly-qualified name — a
/// write-target column's bare name.
pub(super) fn object_name_last_ident(name: &ObjectName) -> Option<Ident> {
    name.0.last().and_then(|part| part.as_ident().cloned())
}

/// The table reference of a `TABLE [schema.]name` set-expression body
/// (`SetExpr::Table`), whose parts are plain strings rather than an
/// `ObjectName`. `None` when no table name is present.
pub(super) fn table_set_expr_ref(table: &Table) -> Option<TableReference> {
    let name = table.table_name.as_ref()?;
    let mut parts = Vec::new();
    if let Some(schema) = &table.schema_name {
        parts.push(Ident::new(schema));
    }
    parts.push(Ident::new(name));
    TableReference::try_from_parts(&parts)
}

/// The column(s) an assignment writes: a single `col = …` or a tuple
/// `(a, b) = …`, each reduced to its bare name.
pub(super) fn assignment_target_columns(target: &AssignmentTarget) -> Vec<Ident> {
    match target {
        // A SET target is `col` up to `catalog.schema.table.col` (≤ 4
        // segments); a deeper qualifier overshoots and is skipped.
        AssignmentTarget::ColumnName(name) if name.0.len() <= 4 => {
            object_name_last_ident(name).into_iter().collect()
        }
        // Tuple targets `(a, b) = …` aren't column-paired (skipped, like
        // the resolver), and a too-deep `ColumnName` overshoots.
        AssignmentTarget::ColumnName(_) | AssignmentTarget::Tuple(_) => Vec::new(),
    }
}

/// The output name SQL infers for an unaliased projection item: a bare
/// column keeps its own name; anything else is anonymous.
pub(super) fn inferred_output_name(expr: &Expr) -> Option<Ident> {
    match expr {
        Expr::Identifier(id) => Some(id.clone()),
        Expr::CompoundIdentifier(ids) => ids.last().cloned(),
        _ => None,
    }
}

/// The lineage kind an expression contributes to its direct sources: a
/// bare column reference forwards its value (`Passthrough`); anything
/// else derives a new value (`Transformation`).
pub(super) fn expr_kind(expr: &Expr) -> ColumnLineageKind {
    if matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
        ColumnLineageKind::Passthrough
    } else {
        ColumnLineageKind::Transformation
    }
}

/// Compose two lineage kinds along a chain: `Transformation` wins if
/// either step transforms (so a passthrough of a transformed value is a
/// transformation), else `Passthrough`.
pub(super) fn combine_kind(
    inner: ColumnLineageKind,
    outer: ColumnLineageKind,
) -> ColumnLineageKind {
    if inner == ColumnLineageKind::Transformation || outer == ColumnLineageKind::Transformation {
        ColumnLineageKind::Transformation
    } else {
        ColumnLineageKind::Passthrough
    }
}

/// Wrap a read as a `Passthrough` provenance source — a base column or an
/// unresolved / ambiguous placeholder forwards its value by default; the
/// containing expression's kind folds in later. A direct physical
/// reference, so not synthetic.
pub(super) fn passthrough(read: ColumnRead) -> ProvenanceSource {
    ProvenanceSource {
        read,
        kind: ColumnLineageKind::Passthrough,
        synthetic_origin: false,
    }
}

/// A resolved real-column read of `table.column` with the given kind.
pub(super) fn read(
    table: &TableReference,
    column: &Ident,
    resolution: ResolutionKind,
) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: Some(table.clone()),
            name: column.clone(),
        },
        resolution,
    }
}

/// Downgrade a real-table witness's provenance to `Inferred` — the
/// Known-witness-over-Open tiebreaker adopts it without firm evidence.
/// (Synthetic witnesses skip this: their inner refs keep their own
/// resolution since the synthetic name never surfaces.)
pub(super) fn downgrade_to_inferred(provenance: Vec<ProvenanceSource>) -> Vec<ProvenanceSource> {
    provenance
        .into_iter()
        .map(|mut source| {
            source.read.resolution = ResolutionKind::Inferred;
            source
        })
        .collect()
}

/// An ambiguous column read (several candidate owners) — no resolved table.
pub(super) fn ambiguous(column: &Ident) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: None,
            name: column.clone(),
        },
        resolution: ResolutionKind::Ambiguous,
    }
}

/// An unresolved column read (no candidate owner) — no resolved table.
pub(super) fn unresolved(column: &Ident) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: None,
            name: column.clone(),
        },
        resolution: ResolutionKind::Unresolved,
    }
}
