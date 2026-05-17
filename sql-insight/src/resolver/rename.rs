//! Column-list rename for `WITH cte(a, b) AS (...)` and
//! `(SELECT ...) d(a, b)` aliases. Applied to both the body's
//! `output_schema` and its `projection_groups` so flow composition's
//! name-match lookup finds the renamed columns.

use super::{Column, ProjectionGroup, RelationSchema};

/// Apply a column alias rename list to a body's `output_schema`. The
/// alias at position N overrides the body's inferred column at
/// position N; body columns past the alias list keep their inferred
/// names. An empty rename list returns `schema` unchanged; an
/// `Unknown` body schema is promoted to `Known` containing exactly
/// the declared rename columns (the only columns we can name with
/// certainty after a rename clause).
pub(crate) fn rename_relation_schema(
    schema: RelationSchema,
    renames: &[sqlparser::ast::TableAliasColumnDef],
) -> RelationSchema {
    if renames.is_empty() {
        return schema;
    }
    match schema {
        RelationSchema::Unknown => RelationSchema::Known(
            renames
                .iter()
                .map(|r| Column {
                    name: r.name.clone(),
                })
                .collect(),
        ),
        RelationSchema::Known(mut cols) => {
            for (position, rename) in renames.iter().enumerate() {
                if let Some(col) = cols.get_mut(position) {
                    col.name = rename.name.clone();
                } else {
                    cols.push(Column {
                        name: rename.name.clone(),
                    });
                }
            }
            RelationSchema::Known(cols)
        }
    }
}

/// Apply the same rename to the projection items' inferred names so
/// flow composition's name-match lookup finds the renamed columns.
/// Position N in the rename list overrides position N's item name;
/// positions beyond the list keep their body-inferred names. Each
/// `ProjectionGroup` (set-op branch) is renamed independently.
pub(crate) fn rename_projection_groups(
    mut groups: Vec<ProjectionGroup>,
    renames: &[sqlparser::ast::TableAliasColumnDef],
) -> Vec<ProjectionGroup> {
    if renames.is_empty() {
        return groups;
    }
    for group in &mut groups {
        for (position, item) in group.items.iter_mut().enumerate() {
            if let Some(rename) = renames.get(position) {
                item.name = Some(rename.name.clone());
            }
        }
    }
    groups
}
