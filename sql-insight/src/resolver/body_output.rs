//! Per-SELECT body output captured during the walk: per-branch
//! lists of output columns with their inferred names, source refs,
//! and `Passthrough` / `Transformation` kind. Plus the helpers
//! that derive each output column's name / kind from a `SelectItem`.

use sqlparser::ast::{Expr, Ident, SelectItem, TableAliasColumnDef};

use crate::extractor::ColumnLineageKind;

use super::{RawColumnRef, Resolver};

/// Body-walk output of a SELECT-derived relation (CTE / true
/// derived), with full per-column lineage.
///
/// `per_branch[i][j]` is the `j`-th output column of the `i`-th UNION
/// branch (plain SELECT contributes a single branch). Branches are
/// kept separate so INSERT pairing can match each branch's columns
/// against the same target columns. Real tables don't carry an
/// instance of this — their column info comes from the catalog and
/// is just a name list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BodyOutput {
    pub(crate) per_branch: Vec<Vec<OutputColumn>>,
}

/// One output column slot — name, the input refs that produced its
/// value (`source_refs`), and a `Passthrough` / `Transformation`
/// classification (`kind`).
///
/// `kind` is collapsed with the outer edge's kind when this column
/// participates in a CTE / derived collapse chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputColumn {
    pub(crate) name: Option<Ident>,
    pub(crate) source_refs: Vec<RawColumnRef>,
    pub(crate) kind: ColumnLineageKind,
}

impl BodyOutput {
    /// First branch's column names if all of them have an inferable
    /// name. `None` if any column has no name (wildcards, computed
    /// without alias) — equivalent to "column names unknown" for the
    /// relation as a whole.
    pub(super) fn column_names(&self) -> Option<Vec<Ident>> {
        let first = self.per_branch.first()?;
        first.iter().map(|c| c.name.clone()).collect()
    }

    /// Apply a CTE / derived column-alias rename list (`WITH cte(a, b)
    /// AS (...)` or `(SELECT ...) d(a, b)`). Position N in the rename
    /// list overrides position N's column name; positions beyond the
    /// body's columns are appended as name-only entries (no source
    /// refs). Each branch is renamed independently. An empty rename
    /// list returns the body unchanged.
    pub(crate) fn renamed(mut self, renames: &[TableAliasColumnDef]) -> Self {
        if renames.is_empty() {
            return self;
        }
        for branch in &mut self.per_branch {
            apply_renames(branch, renames);
        }
        self
    }
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

impl<'a> Resolver<'a> {
    /// Push a fully-built branch of output columns into the active
    /// query's per-branch buffer. Called by `visit_select` once per
    /// SELECT body.
    pub(super) fn push_output_branch(&mut self, branch: Vec<OutputColumn>) {
        self.current_branches.push(branch);
    }

    /// Extend the active query's per-branch buffer with externally
    /// produced branches — used by `SetExpr::Query` to bubble the
    /// inner query's branches up into the enclosing query (so INSERT
    /// pairing reaches through a parenthesized source).
    pub(super) fn extend_branches(&mut self, branches: Vec<Vec<OutputColumn>>) {
        self.current_branches.extend(branches);
    }
}

/// Inferred output name for a SELECT-list item:
/// - explicit alias > bare identifier's name > `None` for computed
///   expressions and wildcards.
pub(super) fn output_column_name(item: &SelectItem) -> Option<Ident> {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => Some(alias.clone()),
        SelectItem::UnnamedExpr(expr) => expr_inferred_name(expr),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => None,
    }
}

/// Classify a SELECT-list item for `ColumnLineageKind`. Wildcards
/// don't emit lineage edges currently, so the fallback
/// `Transformation` here is safe; if/when wildcard expansion lands,
/// items will be classified individually instead.
pub(super) fn output_column_kind(item: &SelectItem) -> ColumnLineageKind {
    match item {
        SelectItem::ExprWithAlias { expr, .. } | SelectItem::UnnamedExpr(expr) => expr_kind(expr),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
            ColumnLineageKind::Transformation
        }
    }
}

fn expr_inferred_name(expr: &Expr) -> Option<Ident> {
    match expr {
        Expr::Identifier(ident) => Some(ident.clone()),
        Expr::CompoundIdentifier(parts) => parts.last().cloned(),
        _ => None,
    }
}

pub(super) fn expr_is_bare(expr: &Expr) -> bool {
    matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
}

/// Classify an expression for `ColumnLineageKind` — the one clean
/// distinction:
/// - bare `Identifier` / `CompoundIdentifier` → `Passthrough` (value
///   forwarded unchanged; a rename is still `Passthrough`)
/// - anything else (arithmetic, function calls incl. aggregates and
///   window functions, CASE, casts, …) → `Transformation`
pub(super) fn expr_kind(expr: &Expr) -> ColumnLineageKind {
    if expr_is_bare(expr) {
        ColumnLineageKind::Passthrough
    } else {
        ColumnLineageKind::Transformation
    }
}
