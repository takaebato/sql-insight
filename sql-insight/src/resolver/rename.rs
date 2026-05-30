//! Column-list rename for `WITH cte(a, b) AS (...)` and
//! `(SELECT ...) d(a, b)` aliases. Applied to the body's
//! [`BodyOutput`] so lineage collapse's name-match lookup finds the
//! renamed columns.

use sqlparser::ast::{Ident, TableAliasColumnDef};

use crate::extractor::column_operation_extractor::ColumnLineageKind;

use super::{BodyOutput, OutputColumn};

/// Apply a column alias rename list to a body's output columns.
/// Position N in the rename list overrides position N's column name;
/// positions beyond the body's columns are appended as name-only
/// entries (no source refs). Each branch (UNION branch / set-op
/// branch) is renamed independently. An empty rename list returns
/// the body unchanged.
pub(crate) fn rename_body_output(
    mut output: BodyOutput,
    renames: &[TableAliasColumnDef],
) -> BodyOutput {
    if renames.is_empty() {
        return output;
    }
    for branch in &mut output.per_branch {
        apply_renames(branch, renames);
    }
    output
}

fn apply_renames(branch: &mut Vec<OutputColumn>, renames: &[TableAliasColumnDef]) {
    for (position, rename) in renames.iter().enumerate() {
        match branch.get_mut(position) {
            Some(col) => col.name = Some(rename.name.clone()),
            None => branch.push(rename_only_column(rename.name.clone())),
        }
    }
}

fn rename_only_column(name: Ident) -> OutputColumn {
    OutputColumn {
        name: Some(name),
        source_refs: Vec::new(),
        kind: ColumnLineageKind::Transformation,
    }
}
