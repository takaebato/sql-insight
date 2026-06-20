//! The binder: lowers a `sqlparser` AST into the bound [`LogicalPlan`] tree,
//! resolving every column reference against the bind-time scope. One pass,
//! AST → tree; an unmodelled construct falls to [`LogicalPlan::Empty`] (with an
//! `UnsupportedStatement` diagnostic) rather than hard-erroring.
//!
//! This root module holds the [`Binder`] context, the entry point, and the
//! plan- / AST-construction glue; the bind logic is split by concern across
//! submodules, each adding an `impl Binder` block over the shared types:
//!
//! - [`Binder`] — the bind context (`catalog` / `casing` / the CTE and
//!   correlation stacks / the shared diagnostics sink) and the small core
//!   methods over it (child-binder construction, table-ref canonicalization,
//!   diagnostic recording).
//! - [`scope`] — the bind-time [`Scope`] model (FROM `relations` / `query_outputs`
//!   / USING `merge_columns`) threaded bottom-up (`bind_* -> (LogicalPlan,
//!   Scope)`), never stored on the tree.
//! - [`statement`] — the DML / DDL roots (INSERT / UPDATE / DELETE / MERGE /
//!   CREATE / ALTER / DROP) and write-target resolution.
//! - [`query`] — `WITH` / CTEs, set operations, `VALUES`, SELECT, the FROM
//!   clause (joins / table factors / derived tables / table functions), pipes.
//! - [`expr`] — expression binding (the value/filter split) plus the auxiliary
//!   clause read collectors.
//! - [`resolve`] — column-name resolution: candidate ranking, the catalog-aware
//!   tiebreaker, catalog matching, and case-folded identifier comparison.
//!
//! A table factor is matched against the catalog (right-anchored, dialect-cased)
//! into a canonical identity with its `Cataloged` columns and table-level
//! [`ResolutionKind`], or `Unknown` (catalog-free / miss / ambiguous). Column
//! resolution ranks the in-scope relations: a `Cataloged`-witness over an
//! `Unknown` suspect downgrades to `Inferred`, several owners give `Ambiguous`,
//! none `Unresolved`,
//! and a derived / CTE relation that exposes the column gives `Derived`. A DML
//! target is in scope for resolving SET / WHERE but is the **write target**
//! named on the root, never a read scan.

use sqlparser::ast::{
    AlterTable as SqlAlterTable, AlterTableOperation, AssignmentTarget, CreateTable,
    CreateView as SqlCreateView, Cte as SqlCte, Delete as SqlDelete, Expr as SqlExpr, FromTable,
    Function, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, GroupByWithModifier,
    Ident, Insert as SqlInsert, JoinConstraint, JoinOperator, Merge as SqlMerge, MergeAction,
    MergeInsertKind, ObjectName, ObjectType, OnConflictAction, OnInsert, OrderBy, OrderByExpr,
    OrderByKind, PipeOperator, PivotValueSource, Query, Select, SelectItem, SetExpr, Statement,
    TableAlias, TableFactor, TableObject, TableWithJoins, Update as SqlUpdate, UpdateTableFromKind,
    Values as SqlValues,
};

use std::cell::RefCell;

use sqlparser::ast::{
    AccessExpr, ConnectByKind, Distinct, FunctionArgumentClause, LimitClause, ListAggOnOverflow,
    NamedWindowExpr, SelectItemQualifiedWildcardKind, Subscript, TopQuantity, WindowFrameBound,
    WindowSpec, WindowType,
};
use sqlparser::tokenizer::Span;

use super::logical_plan::{
    Aggregate, AlterTable, Assignment, Binding, BoundColumn, Columns, CreateTableAs, CreateView,
    Cte, CteRef, Delete, Drop, Expr, Filter, Insert, Join, LogicalPlan, Merge, MergeClause,
    NamedExpr, Projection, Scan, SetOp, Sort, SubqueryAlias, TableFunction, Update, Values, With,
};
use crate::casing::{CaseRule, IdentifierCasing};
use crate::catalog::{Catalog, CatalogTable};
use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::reference::{ResolutionKind, TableReference};

// The bind pass is split by concern; each submodule adds an `impl Binder`
// block over the shared types — the `Binder` context and free helpers here,
// the `Scope` relation / output model in `scope`.
mod expr;
mod query;
mod resolve;
mod scope;
mod statement;

use scope::*;

/// Bind a statement into an [`LogicalPlan`] tree plus the column-level
/// diagnostics it raised (unsupported statement, suppressed wildcard,
/// over-qualified table name). An unmodelled statement yields
/// [`LogicalPlan::Empty`] and an `UnsupportedStatement` diagnostic.
pub(crate) fn build_with_diagnostics(
    statement: &Statement,
    catalog: Option<&Catalog>,
    casing: IdentifierCasing,
) -> (LogicalPlan, Vec<ColumnLevelDiagnostic>) {
    let diagnostics = RefCell::new(Vec::new());
    let op = Binder {
        catalog,
        casing,
        ctes: Vec::new(),
        outer: Vec::new(),
        diagnostics: &diagnostics,
    }
    .bind_statement(statement);
    (op, diagnostics.into_inner())
}

struct Binder<'a> {
    catalog: Option<&'a Catalog>,
    casing: IdentifierCasing,
    /// CTEs in scope (declaration order, innermost `WITH` last).
    ctes: Vec<CteDecl>,
    /// Enclosing queries' relations (the correlation stack, outermost first)
    /// that an inner subquery's references fall through to.
    outer: Vec<Vec<Relation>>,
    /// Shared diagnostic buffer (child binders for subqueries / CTEs push into
    /// the same one, so a nested suppressed wildcard surfaces).
    diagnostics: &'a RefCell<Vec<ColumnLevelDiagnostic>>,
}

impl<'a> Binder<'a> {
    /// A child binder with a different CTE environment (sharing catalog /
    /// casing / correlation stack / diagnostics).
    pub(super) fn with_ctes(&self, ctes: Vec<CteDecl>) -> Binder<'a> {
        Binder {
            catalog: self.catalog,
            casing: self.casing,
            ctes,
            outer: self.outer.clone(),
            diagnostics: self.diagnostics,
        }
    }

    /// A child binder with one more enclosing scope on the correlation stack
    /// (used when descending into a subquery in an expression / a LATERAL
    /// factor).
    pub(super) fn with_outer(&self, relations: Vec<Relation>) -> Binder<'a> {
        let mut outer = self.outer.clone();
        outer.push(relations);
        Binder {
            catalog: self.catalog,
            casing: self.casing,
            ctes: self.ctes.clone(),
            outer,
            diagnostics: self.diagnostics,
        }
    }

    /// Build a table reference from a parsed name, recording a
    /// `TooManyTableQualifiers` diagnostic and returning `None` when it has
    /// more identifiers than `catalog.schema.name` (the only conversion
    /// failure) — so the dropped relation stays observable.
    pub(super) fn table_ref(&self, name: &ObjectName) -> Option<TableReference> {
        match TableReference::try_from_name(name) {
            Ok(table) => Some(table),
            Err(_) => {
                self.record_too_many_table_qualifiers(name);
                None
            }
        }
    }

    /// Record a `WildcardSuppressed` diagnostic for a projection wildcard
    /// (`*` / `t.*` / `(expr).*`) left unexpanded, carrying the wildcard
    /// token's location (a zero-line span is treated as unknown).
    pub(super) fn record_wildcard_suppressed(&self, description: &str, span: Span) {
        let span = (span.start.line != 0).then_some(span);
        let suffix = match span {
            Some(s) => format!(" at L{}:C{}", s.start.line, s.start.column),
            None => String::new(),
        };
        self.diagnostics.borrow_mut().push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::WildcardSuppressed,
            message: format!(
                "{description}{suffix} left unexpanded — column lineage will be incomplete for this projection"
            ),
            span,
        });
    }

    /// Record a `TooManyTableQualifiers` diagnostic for an over-qualified table
    /// name (more than `catalog.schema.name`), carrying its location.
    pub(super) fn record_too_many_table_qualifiers(&self, name: &ObjectName) {
        let span = name
            .0
            .first()
            .and_then(|part| part.as_ident())
            .map(|ident| ident.span)
            .filter(|s| s.start.line != 0);
        let suffix = match span {
            Some(s) => format!(" at L{}:C{}", s.start.line, s.start.column),
            None => String::new(),
        };
        self.diagnostics.borrow_mut().push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::TooManyTableQualifiers,
            message: format!(
                "table reference `{name}`{suffix} has too many qualifiers (max catalog.schema.name) — dropped"
            ),
            span,
        });
    }

    /// Record an `InsertColumnsUnresolved` diagnostic for a column-list-less
    /// INSERT / MERGE-INSERT whose target columns couldn't be filled from a
    /// catalog, so its column-level `writes` / `lineage` are dropped (the
    /// table still surfaces in `table_writes`).
    pub(super) fn record_insert_columns_unresolved(&self, target: &TableReference) {
        self.diagnostics.borrow_mut().push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::InsertColumnsUnresolved,
            message: format!(
                "column-list-less INSERT into `{target}` can't pair source columns to target columns without a catalog — column writes / lineage dropped"
            ),
            span: None,
        });
    }
}

// ===== catalog matching ==================================================

/// Fill a query reference's missing prefix segments from the catalog's
/// defaults (bare → schema then catalog).
fn fill_query_defaults(written: &TableReference, catalog: &Catalog) -> TableReference {
    let mut filled = written.clone();
    if filled.schema.is_none() {
        if let Some(schema) = catalog.default_schema_segment() {
            filled.schema = Some(Ident::with_quote('"', schema));
        }
    }
    if filled.catalog.is_none() && filled.schema.is_some() {
        if let Some(catalog_segment) = catalog.default_catalog_segment() {
            filled.catalog = Some(Ident::with_quote('"', catalog_segment));
        }
    }
    filled
}

/// The *surfaced* identity for a reference that didn't uniquely match a
/// registered table: written segments are kept verbatim, omitted prefix
/// segments are filled from the catalog defaults as plain (unquoted)
/// idents. Unlike [`fill_query_defaults`] (which quotes filled segments for
/// case-exact *matching*), this produces the identity shown to consumers —
/// so a bare `users` under `default_schema = "public"` surfaces as
/// `public.users` and dedups with an explicit `public.users` elsewhere.
/// With no configured defaults it returns the written ref unchanged.
fn surface_with_defaults(written: &TableReference, catalog: &Catalog) -> TableReference {
    let plain = |value: &str| Ident {
        value: value.to_string(),
        quote_style: None,
        span: Span::empty(),
    };
    let schema = written
        .schema
        .clone()
        .or_else(|| catalog.default_schema_segment().map(plain));
    // Catalog default fills only once a schema is present (matching
    // `fill_query_defaults`' gating).
    let catalog_segment = if written.catalog.is_some() {
        written.catalog.clone()
    } else if schema.is_some() {
        catalog.default_catalog_segment().map(plain)
    } else {
        None
    };
    TableReference {
        catalog: catalog_segment,
        schema,
        name: written.name.clone(),
    }
}

/// Right-anchored, dialect-cased match of a (default-filled) query reference
/// against a registered table.
fn catalog_table_matches(query: &TableReference, table: &CatalogTable, fold: CaseRule) -> bool {
    if fold.normalize(&query.name) != normalize_catalog(table.name_segment(), fold) {
        return false;
    }
    // Both sides present and differing → no match; an omitted schema on
    // either side (a bare query ref, or a schema-less registered table) is
    // a wildcard.
    if let (Some(query_schema), Some(table_schema)) = (&query.schema, table.schema_segment()) {
        if fold.normalize(query_schema) != normalize_catalog(table_schema, fold) {
            return false;
        }
    }
    match (&query.catalog, table.catalog_segment()) {
        (Some(query_catalog), Some(table_catalog)) => {
            fold.normalize(query_catalog) == normalize_catalog(table_catalog, fold)
        }
        _ => true,
    }
}

fn normalize_catalog(segment: &str, fold: CaseRule) -> String {
    fold.normalize(&Ident::with_quote('"', segment))
}

/// The surfaced canonical identity of a matched table: plain (unquoted) idents.
/// The canonical identity of a matched catalog table — its registered
/// `catalog.schema.name` path. Each segment's *value* comes from the
/// registration (so a bare `users` and an explicit `public.users` agree), but
/// its *span* is carried from the matching `written` segment so
/// `reference.name.span` still points at where the reference was written (for
/// source-order sorting). A segment the catalog *filled in* has no source token,
/// so it gets an empty span.
fn canonical_ref(table: &CatalogTable, written: &TableReference) -> TableReference {
    let seg = |value: &str, span: Span| Ident {
        value: value.to_string(),
        quote_style: None,
        span,
    };
    let span_of = |ident: Option<&Ident>| ident.map_or(Span::empty(), |i| i.span);
    TableReference {
        catalog: table
            .catalog_segment()
            .map(|c| seg(c, span_of(written.catalog.as_ref()))),
        schema: table
            .schema_segment()
            .map(|s| seg(s, span_of(written.schema.as_ref()))),
        name: seg(table.name_segment(), written.name.span),
    }
}

// ===== small helpers =====================================================

fn base(table: &TableReference, resolution: ResolutionKind) -> Binding {
    Binding::Base {
        table: table.clone(),
        resolution,
    }
}

/// Downgrade a winning real-table witness to `Inferred` — adopted over
/// `Unknown` suspects without firm evidence.
fn downgrade(binding: Binding) -> Binding {
    match binding {
        Binding::Base { table, .. } => Binding::Base {
            table,
            resolution: ResolutionKind::Inferred,
        },
        other => other,
    }
}

fn join(left: LogicalPlan, right: LogicalPlan, on: Vec<Expr>) -> LogicalPlan {
    LogicalPlan::Join(Join {
        left: Box::new(left),
        right: Box::new(right),
        on,
        lateral: false,
    })
}

/// Cross-join `right` onto `left`, but if `left` is the empty placeholder
/// just take `right` (so a single read relation isn't wrapped in a join with
/// nothing). Used to accumulate a DML statement's read relations.
fn combine(left: LogicalPlan, right: LogicalPlan) -> LogicalPlan {
    if matches!(left, LogicalPlan::Empty) {
        right
    } else {
        join(left, right, Vec::new())
    }
}

/// The column(s) a SET assignment writes: a `col` up to
/// `catalog.schema.table.col` (≤ 4 segments) contributes its last identifier;
/// a tuple target `(a, b) = …` or a deeper qualifier contributes nothing
/// (not column-paired).
fn assignment_target_columns(target: &AssignmentTarget) -> Vec<Ident> {
    match target {
        AssignmentTarget::ColumnName(name) if name.0.len() <= 4 => name
            .0
            .last()
            .and_then(|p| p.as_ident().cloned())
            .into_iter()
            .collect(),
        AssignmentTarget::ColumnName(_) | AssignmentTarget::Tuple(_) => Vec::new(),
    }
}

/// The column name(s) an `ALTER TABLE` operation writes to. Column-naming ops
/// (ADD / DROP / RENAME / CHANGE / MODIFY / ALTER COLUMN) name their column(s);
/// RENAME / CHANGE surface both old and new names. Schema-level ops
/// (constraints, partitions, RENAME TABLE) name no columns.
fn alter_table_op_target_columns(op: &AlterTableOperation) -> Vec<Ident> {
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

fn sort(input: LogicalPlan, keys: Vec<Expr>) -> LogicalPlan {
    LogicalPlan::Sort(Sort {
        input: Box::new(input),
        keys,
    })
}

/// The names of a table alias's explicit column list (`AS d(x, y)`), empty if
/// none.
fn alias_column_names(alias: &TableAlias) -> Vec<Ident> {
    alias.columns.iter().map(|c| c.name.clone()).collect()
}

/// Rename a (sub)plan's output columns positionally to `names` (an explicit
/// `AS d(x, y)` column list). A no-op when `names` is empty. Descends through
/// the clause layers / `With` to the producing `Projection`; a `SetOp` renames
/// both operands.
fn rename_outputs(op: &mut LogicalPlan, names: &[Ident]) {
    if names.is_empty() {
        return;
    }
    match op {
        LogicalPlan::Projection(p) => {
            for (ne, n) in p.exprs.iter_mut().zip(names) {
                ne.name = Some(n.clone());
            }
        }
        LogicalPlan::Sort(s) => rename_outputs(&mut s.input, names),
        LogicalPlan::Filter(f) => rename_outputs(&mut f.input, names),
        LogicalPlan::With(w) => rename_outputs(&mut w.body, names),
        LogicalPlan::SetOp(so) => {
            rename_outputs(&mut so.left, names);
            rename_outputs(&mut so.right, names);
        }
        _ => {}
    }
}

/// The name SQL infers for an unaliased projection item: a bare column keeps
/// its own name; anything else is anonymous.
fn inferred_name(expr: &SqlExpr) -> Option<Ident> {
    match expr {
        SqlExpr::Identifier(id) => Some(id.clone()),
        SqlExpr::CompoundIdentifier(parts) => parts.last().cloned(),
        _ => None,
    }
}

/// The `ON` predicate of a join operator, if any.
/// The constraint of any constraint-carrying join operator (everything but
/// `CROSS APPLY` / `OUTER APPLY`).
fn join_constraint(op: &JoinOperator) -> Option<&JoinConstraint> {
    match op {
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

/// The table reference of a `TABLE foo` query body (`SetExpr::Table`), whose
/// parts are plain strings rather than identifiers.
fn table_set_expr_ref(table: &sqlparser::ast::Table) -> Option<TableReference> {
    let name = table.table_name.as_ref()?;
    let mut parts = Vec::new();
    if let Some(schema) = &table.schema_name {
        parts.push(Ident::new(schema));
    }
    parts.push(Ident::new(name));
    TableReference::try_from_parts(&parts)
}

/// The `ON` predicate of a join operator, if any.
fn join_on(op: &JoinOperator) -> Option<&SqlExpr> {
    match join_constraint(op) {
        Some(JoinConstraint::On(expr)) => Some(expr),
        _ => None,
    }
}

/// The `USING (col, …)` merge-column names of a join operator, if any.
fn join_using(op: &JoinOperator) -> Vec<Ident> {
    match join_constraint(op) {
        Some(JoinConstraint::Using(names)) => names
            .iter()
            .filter_map(|n| n.0.last().and_then(|p| p.as_ident().cloned()))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, CatalogTable};
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    fn bind(sql: &str) -> LogicalPlan {
        bind_cat(sql, None)
    }

    fn bind_cat(sql: &str, catalog: Option<&Catalog>) -> LogicalPlan {
        let statements = Parser::parse_sql(&GenericDialect {}, sql).unwrap();
        let casing = IdentifierCasing::for_dialect(&GenericDialect {});
        build_with_diagnostics(&statements[0], catalog, casing).0
    }

    fn only_binding(plan: &LogicalPlan) -> &Binding {
        let LogicalPlan::Projection(p) = plan else {
            panic!("expected Projection, got {plan:?}")
        };
        match &p.exprs[..] {
            [NamedExpr {
                expr: Expr::Column(c),
                ..
            }] => &c.binding,
            other => panic!("expected one column expr, got {other:?}"),
        }
    }

    #[test]
    fn catalog_free_single_table_is_inferred() {
        assert!(matches!(only_binding(&bind("SELECT a FROM t")),
            Binding::Base { table, resolution }
            if table.name.value == "t" && *resolution == ResolutionKind::Inferred));
    }

    #[test]
    fn catalog_known_hit_is_cataloged_and_canonical() {
        let cat =
            Catalog::new().table(CatalogTable::new("public", "users").columns(["id", "name"]));
        let op = bind_cat("SELECT name FROM users", Some(&cat));
        match only_binding(&op) {
            Binding::Base { table, resolution } => {
                assert_eq!(table.name.value, "users");
                assert_eq!(table.schema.as_ref().unwrap().value, "public"); // canonicalized
                assert_eq!(*resolution, ResolutionKind::Cataloged);
            }
            other => panic!("expected Base Cataloged, got {other:?}"),
        }
    }

    #[test]
    fn catalog_known_miss_is_unresolved() {
        let cat =
            Catalog::new().table(CatalogTable::new("public", "users").columns(["id", "name"]));
        assert!(matches!(
            only_binding(&bind_cat("SELECT nonexistent FROM users", Some(&cat))),
            Binding::Unresolved
        ));
    }

    #[test]
    fn cataloged_witness_over_unknown_downgrades_to_inferred() {
        // `known_t` lists `a`; `open_t` is not in the catalog → Unknown suspect.
        let cat = Catalog::new().table(CatalogTable::new("public", "known_t").columns(["a", "b"]));
        let op = bind_cat(
            "SELECT a FROM known_t JOIN open_t ON known_t.b = open_t.k",
            Some(&cat),
        );
        match only_binding(&op) {
            Binding::Base { table, resolution } => {
                assert_eq!(table.name.value, "known_t");
                assert_eq!(*resolution, ResolutionKind::Inferred); // downgraded
            }
            other => panic!("expected Base Inferred (downgraded), got {other:?}"),
        }
    }

    #[test]
    fn two_known_owners_is_ambiguous() {
        let cat = Catalog::new()
            .table(CatalogTable::new("public", "t1").columns(["id"]))
            .table(CatalogTable::new("public", "t2").columns(["id"]));
        assert!(matches!(
            only_binding(&bind_cat(
                "SELECT id FROM t1 JOIN t2 ON t1.id = t2.id",
                Some(&cat)
            )),
            Binding::Ambiguous
        ));
    }
}
