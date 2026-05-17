//! Per-SELECT projection facts captured by the resolver during the
//! walk, plus the classification helpers that derive each projection
//! item's name / kind (`Passthrough` / `Aggregation` / `Computed`).

use sqlparser::ast::{Expr, Function, FunctionArguments, Ident, ObjectName, SelectItem};

use crate::extractor::column_operation_extractor::ColumnFlowKind;

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
/// (`Passthrough` / `Aggregation` / `Computed`); composed with the
/// outer flow's kind when this item participates in a CTE / derived
/// table substitution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectionItem {
    pub(crate) name: Option<Ident>,
    pub(crate) source_refs: Vec<RawColumnRef>,
    pub(crate) kind: ColumnFlowKind,
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

/// Classify a projection item for `ColumnFlowKind`. Wildcards don't
/// emit flow edges currently, so the fallback `Computed` here is
/// safe; if/when wildcard expansion lands, items will be classified
/// individually instead.
pub(super) fn projection_item_kind(item: &SelectItem) -> ColumnFlowKind {
    match item {
        SelectItem::ExprWithAlias { expr, .. } | SelectItem::UnnamedExpr(expr) => expr_kind(expr),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => ColumnFlowKind::Computed,
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

/// Classify an expression for `ColumnFlowKind`:
/// - bare `Identifier` / `CompoundIdentifier` → `Passthrough`
/// - top-level aggregate function call (`SUM(a)`, `COUNT(b)`, etc.)
///   → `Aggregation`
/// - anything else → `Computed`
///
/// Note that the top-level test only fires for a bare aggregate
/// call; `SUM(a) + 1`'s top-level is a `BinaryOp`, which classifies
/// as `Computed`. Sub-expressions are not recursively inspected here.
pub(super) fn expr_kind(expr: &Expr) -> ColumnFlowKind {
    if expr_is_bare(expr) {
        return ColumnFlowKind::Passthrough;
    }
    if let Expr::Function(f) = expr {
        if function_is_aggregate(f) {
            return ColumnFlowKind::Aggregation;
        }
    }
    ColumnFlowKind::Computed
}

/// Decide whether a function call should be classified as an
/// aggregate. Two complementary signals:
///
/// 1. **Structural markers** (SQL spec): `FILTER (WHERE ...)`,
///    `WITHIN GROUP (...)`, and `DISTINCT` inside the arg list are
///    attached only to aggregate calls per the SQL standard. These
///    catch dialect-specific aggregates that aren't in our name list
///    (e.g., `LISTAGG(...) WITHIN GROUP (...)` with no listing of
///    `LISTAGG` as a name).
/// 2. **Name match** against the union of common SQL aggregates
///    across dialects. Covers the bare form `SUM(x)` / `COUNT(*)` /
///    etc. that carries no structural marker.
///
/// False positives are theoretically possible only when a user
/// defines a scalar UDF with an aggregate's name (e.g., a custom
/// `SUM` that doesn't actually aggregate) — vanishingly rare in
/// practice, and the structural markers never misfire (their syntax
/// is aggregate-only by spec).
fn function_is_aggregate(f: &Function) -> bool {
    if function_has_aggregate_marker(f) {
        return true;
    }
    is_aggregate_function_name(&f.name)
}

fn function_has_aggregate_marker(f: &Function) -> bool {
    use sqlparser::ast::DuplicateTreatment;
    if f.filter.is_some() {
        return true;
    }
    if !f.within_group.is_empty() {
        return true;
    }
    if let FunctionArguments::List(list) = &f.args {
        if matches!(list.duplicate_treatment, Some(DuplicateTreatment::Distinct)) {
            return true;
        }
    }
    false
}

fn is_aggregate_function_name(name: &ObjectName) -> bool {
    let Some(last) = name.0.last() else {
        return false;
    };
    let Some(ident) = last.as_ident() else {
        return false;
    };
    is_aggregate_name(&ident.value)
}

/// Union of common SQL aggregate function names across major
/// dialects (ANSI / Postgres / MySQL / BigQuery / Snowflake /
/// Redshift). Matched case-insensitively. Window-only functions
/// (`ROW_NUMBER`, `RANK`, `LAG`, `LEAD`, `NTILE`, `FIRST_VALUE`,
/// `LAST_VALUE`, …) are intentionally excluded; they participate via
/// `OVER (...)` and only meaningfully aggregate within a window.
fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        // SQL-92 core
        "SUM" | "COUNT" | "AVG" | "MIN" | "MAX"
        // SQL:2003+ standard statistical / set
        | "STDDEV" | "STDDEV_POP" | "STDDEV_SAMP"
        | "VARIANCE" | "VAR_POP" | "VAR_SAMP"
        | "PERCENTILE_CONT" | "PERCENTILE_DISC"
        | "CORR" | "COVAR_POP" | "COVAR_SAMP"
        | "EVERY"
        // Common dialect aggregates (Postgres / MySQL / BigQuery /
        // Snowflake / Redshift).
        | "ANY_VALUE" | "GROUP_CONCAT" | "STRING_AGG" | "LISTAGG"
        | "ARRAY_AGG" | "JSON_AGG" | "JSONB_AGG" | "JSON_OBJECT_AGG"
        | "BIT_AND" | "BIT_OR" | "BIT_XOR"
        | "BOOL_AND" | "BOOL_OR"
        | "MEDIAN" | "MODE"
        | "APPROX_COUNT_DISTINCT" | "APPROX_PERCENTILE"
    )
}
