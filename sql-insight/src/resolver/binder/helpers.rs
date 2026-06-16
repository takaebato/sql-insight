//! Free helper functions used across more than one [`Binder`](super::Binder)
//! clause family: building a filter `PassThrough`, pulling a bare column
//! name out of the sqlparser AST, and constructing the [`ColumnRead`] /
//! [`ProvenanceSource`] leaf values. Clause-specific helpers live with their
//! sole caller (`statement` / `query` / `expr` / `resolve`); these are the
//! ones shared by several. All `pub(super)` — resolver-internal.

use sqlparser::ast::{AssignmentTarget, Ident, ObjectName};

use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableReference};
use crate::resolver::ir::{PassThrough, Plan, ProvenanceSource};

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

/// The last (rightmost) identifier of a possibly-qualified name — a
/// write-target column's bare name.
pub(super) fn object_name_last_ident(name: &ObjectName) -> Option<Ident> {
    name.0.last().and_then(|part| part.as_ident().cloned())
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
