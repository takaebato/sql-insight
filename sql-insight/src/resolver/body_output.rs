//! Per-SELECT body output captured during the walk: one
//! [`SetOperand`] per set-operation operand, each carrying the SELECT's
//! output columns with their inferred names, source refs, and
//! `Passthrough` / `Transformation` kind. Plus the helpers that derive
//! each output column's name / kind from a `SelectItem`.
//!
//! **Terminology — "body":** throughout this module (and the resolver
//! in general) "body" means `sqlparser::ast::Query::body` — the
//! [`SetExpr`](sqlparser::ast::SetExpr) node holding the SELECT /
//! UNION / INTERSECT / EXCEPT / VALUES / TABLE expression. It maps
//! directly to the SQL standard's `<query expression body>` (the
//! part of a query stripped of WITH / ORDER BY / LIMIT / FETCH /
//! settings / pipes). So [`QueryBodyOutput`] is "the projection columns
//! produced by walking that body", `current_query_body` is the in-progress
//! such buffer, and `body_scope` on a synthetic binding is the
//! arena scope of that body's FROM bindings.

use sqlparser::ast::{Expr, Ident, SelectItem, TableAliasColumnDef};

use crate::extractor::ColumnLineageKind;

use super::{RawColumnRef, Resolver};

/// Body-walk output of a SELECT-derived relation (CTE / true
/// derived), with full per-column lineage.
///
/// One [`SetOperand`] per set-operation operand (UNION / INTERSECT /
/// EXCEPT); a plain SELECT contributes a single operand. Operands are
/// kept separate so INSERT pairing can match each operand's columns
/// against the same target columns. Real tables don't carry an
/// instance of this — their column info comes from the catalog and
/// is just a name list.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct QueryBodyOutput {
    pub(crate) set_operands: Vec<SetOperand>,
}

/// One operand of a set operation (UNION / INTERSECT / EXCEPT) — or
/// equivalently, the output columns of one SELECT body. A plain
/// SELECT yields one of these; `A UNION B` yields two; an N-way
/// chain yields N. Kept as a named wrapper so the `Vec<SetOperand>`
/// inside [`QueryBodyOutput`] reads as "the operands" rather than as a
/// generic nested vec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetOperand {
    pub(crate) columns: Vec<OutputColumn>,
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

impl QueryBodyOutput {
    /// First operand's column names if all of them have an inferable
    /// name. `None` if any column has no name (wildcards, computed
    /// without alias) — equivalent to "column names unknown" for the
    /// relation as a whole.
    pub(super) fn column_names(&self) -> Option<Vec<Ident>> {
        let first = self.set_operands.first()?;
        first.columns.iter().map(|c| c.name.clone()).collect()
    }

    /// Apply a CTE / derived column-alias rename list (`WITH cte(a, b)
    /// AS (...)` or `(SELECT ...) d(a, b)`). Position N in the rename
    /// list overrides position N's column name; positions beyond the
    /// body's columns are appended as name-only entries (no source
    /// refs). Each operand is renamed independently. An empty rename
    /// list returns the body unchanged.
    pub(crate) fn renamed(mut self, renames: &[TableAliasColumnDef]) -> Self {
        if renames.is_empty() {
            return self;
        }
        for operand in &mut self.set_operands {
            apply_renames(&mut operand.columns, renames);
        }
        self
    }
}

fn apply_renames(columns: &mut Vec<OutputColumn>, renames: &[TableAliasColumnDef]) {
    for (position, rename) in renames.iter().enumerate() {
        match columns.get_mut(position) {
            Some(col) => col.name = Some(rename.name.clone()),
            None => columns.push(rename_only_column(rename.name.clone())),
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
    /// Push a fully-built set operand into the active query's body
    /// output. Called by `visit_select` once per SELECT body.
    pub(super) fn push_set_operand(&mut self, operand: SetOperand) {
        self.context.current_query_body.set_operands.push(operand);
    }

    /// Extend the active query's body output with externally produced
    /// operands — used by `SetExpr::Query` to bubble the inner
    /// query's operands up into the enclosing query (so INSERT
    /// pairing reaches through a parenthesized source).
    pub(super) fn extend_set_operands(&mut self, operands: Vec<SetOperand>) {
        self.context
            .current_query_body
            .set_operands
            .extend(operands);
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
