//! Per-SELECT projection facts captured by the resolver during the
//! walk, plus the classification helpers that derive each projection
//! item's name / kind (`Passthrough` / `Transformation`).

use sqlparser::ast::{Expr, Ident, SelectItem};

use crate::extractor::column_operation_extractor::ColumnLineageKind;

use super::{RawColumnRef, Resolver};

/// One SELECT's projection captured during the walk — one
/// [`ProjectionItem`] per output column, in projection order. Set
/// operations contribute one group per branch (so UNION INSERT pairs
/// each branch's items with the same target columns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectionGroup {
    pub(crate) items: Vec<ProjectionItem>,
}

/// A single projection slot's resolver-collected facts.
///
/// `source_refs` are the raw column refs the projection item's
/// expression read, in walk order. `name` is the inferable output
/// name (explicit alias > bare ident name > `None`). `kind`
/// classifies how the source refs turn into the output value
/// (`Passthrough` for a bare forwarded column, `Transformation` for
/// anything value-changing); collapsed with the outer edge's kind when
/// this item participates in a CTE / derived table collapse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectionItem {
    pub(crate) name: Option<Ident>,
    pub(crate) source_refs: Vec<RawColumnRef>,
    pub(crate) kind: ColumnLineageKind,
}

impl<'a> Resolver<'a> {
    /// Push a fully-built `ProjectionGroup` into the active query's
    /// projection buffer. Called by `visit_select` once per SELECT
    /// body.
    pub(super) fn push_projection_group(&mut self, group: ProjectionGroup) {
        self.current_projections.push(group);
    }

    /// Extend the active query's projection buffer with externally
    /// produced groups — used by `SetExpr::Query` to bubble the inner
    /// query's projections up into the enclosing query (so INSERT
    /// pairing reaches through a parenthesized source).
    pub(super) fn extend_projections(&mut self, groups: Vec<ProjectionGroup>) {
        self.current_projections.extend(groups);
    }
}

/// Inferred output name for a projection item:
/// - explicit alias > bare identifier's name > `None` for computed
///   expressions and wildcards.
pub(super) fn projection_item_output_name(item: &SelectItem) -> Option<Ident> {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => Some(alias.clone()),
        SelectItem::UnnamedExpr(expr) => expr_inferred_name(expr),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => None,
    }
}

/// Classify a projection item for `ColumnLineageKind`. Wildcards don't
/// emit lineage edges currently, so the fallback `Transformation` here is
/// safe; if/when wildcard expansion lands, items will be classified
/// individually instead.
pub(super) fn projection_item_kind(item: &SelectItem) -> ColumnLineageKind {
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
