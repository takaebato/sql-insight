//! The binder: lowers a `sqlparser` AST into the bound [`LogicalPlan`] tree,
//! resolving every column reference against the bind-time scope. One pass,
//! AST → tree; an unmodelled construct falls to [`LogicalPlan::Empty`] (with an
//! `UnsupportedStatement` diagnostic) rather than hard-erroring.
//!
//! This root module holds the [`Binder`] context, the entry point, and the
//! plan- / AST-construction glue; the bind logic is split by concern across
//! submodules, each adding an `impl Binder` block over the shared types:
//!
//! - [`Binder`] — the bind context: `catalog` / `style` (identifier casing +
//!   surface quote), the appended-to diagnostics sink, and the downward
//!   [`Context`] (in-scope CTEs + the enclosing resolution stack), swapped per
//!   scope by [`Binder::in_scope`]. Plus the small core methods over it
//!   (scoped binding, table-ref canonicalization, diagnostic recording).
//! - [`scope`] — the bind-time [`Scope`] model (FROM `relations` / `query_outputs`
//!   / USING `merge_columns`) threaded bottom-up (`bind_* -> (LogicalPlan,
//!   Scope)`), never stored on the tree.
//! - [`context`] — the `Context` (in-scope CTEs + the enclosing resolution
//!   stack) threaded top-down, swapped per scope by the `in_scope` family.
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
    AlterTable as SqlAlterTable, AlterTableOperation, Assignment as SqlAssignment,
    AssignmentTarget, CreateTable, CreateTableLikeKind, CreateView as SqlCreateView, Cte as SqlCte,
    Delete as SqlDelete, Expr as SqlExpr, FromTable, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, GroupByExpr, GroupByWithModifier, Ident, Insert as SqlInsert,
    JoinConstraint, JoinOperator, JsonPathElem, Merge as SqlMerge, MergeAction, MergeInsertKind,
    ObjectName, ObjectType, OnConflictAction, OnInsert, OrderBy, OrderByExpr, OrderByKind,
    OutputClause, PipeOperator, PivotValueSource, Query, Select, SelectItem, SetExpr, Statement,
    TableAlias, TableFactor, TableObject, TableWithJoins, Update as SqlUpdate, UpdateTableFromKind,
    Value, Values as SqlValues,
};

use sqlparser::ast::{
    AccessExpr, ConnectByKind, Distinct, FunctionArgumentClause, LimitClause, ListAggOnOverflow,
    NamedWindowExpr, SelectItemQualifiedWildcardKind, Subscript, TopQuantity,
    WildcardAdditionalOptions, WindowFrameBound, WindowSpec, WindowType,
};
use sqlparser::tokenizer::Span;

use super::logical_plan::{
    Aggregate, AlterTable, Assignment, Binding, BoundColumn, Columns, CreateTableAs, CreateView,
    Cte, CteRef, Delete, Drop, Expr, Filter, Insert, Join, LogicalPlan, Merge, MergeClause,
    NamedExpr, Projection, Scan, SchemaSource, SetOp, Sort, SubqueryAlias, TableFunction, Update,
    Values, With,
};
use super::origins::output_operands;
use crate::casing::{CaseRule, IdentifierStyle};
use crate::catalog::{Catalog, CatalogTable};
use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::reference::{ColumnWrite, ResolutionKind, TableRead, TableReference, TableWrite};

// The bind pass is split by concern; each submodule adds an `impl Binder`
// block over the shared types — the `Binder` context and free helpers here,
// the `Scope` relation / output model in `scope`.
mod context;
mod expr;
mod query;
mod resolve;
mod scope;
mod statement;

use context::*;
use scope::*;

/// Bind a statement into an [`LogicalPlan`] tree plus the column-level
/// diagnostics it raised (unsupported statement, suppressed wildcard,
/// over-qualified table name). An unmodelled statement yields
/// [`LogicalPlan::Empty`] and an `UnsupportedStatement` diagnostic.
pub(crate) fn build_with_diagnostics(
    statement: &Statement,
    catalog: Option<&Catalog>,
    style: IdentifierStyle,
) -> (LogicalPlan, Vec<ColumnLevelDiagnostic>) {
    let mut binder = Binder {
        catalog,
        style,
        diagnostics: Vec::new(),
        context: Context::default(),
    };
    let op = binder.bind_statement(statement);
    (op, binder.diagnostics)
}

struct Binder<'a> {
    catalog: Option<&'a Catalog>,
    style: IdentifierStyle,
    /// The accumulated diagnostics (a write-only sink, appended as binding
    /// proceeds). The binder is a single `&mut` entity, so this is a plain
    /// `Vec` — no interior mutability needed.
    diagnostics: Vec<ColumnLevelDiagnostic>,
    /// The current downward binding environment (CTEs + enclosing stack),
    /// swapped per scope by [`in_scope`](Self::in_scope).
    context: Context,
}

impl<'a> Binder<'a> {
    /// Bind within a child [`Context`] (a subquery / CTE body / lambda body):
    /// install `child`, run `f`, then restore the previous context. The restore
    /// is structural (it always runs after `f` returns), so an early return
    /// inside `f` can't leak the child context to a sibling — the leak-proofness
    /// the old cloned-child-binder gave, without a per-call parameter.
    fn in_scope<R>(&mut self, child: Context, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = std::mem::replace(&mut self.context, child);
        let r = f(self);
        self.context = saved;
        r
    }

    /// Bind `f` with a replaced CTE environment in scope (a `WITH` body / CTE).
    fn in_ctes<R>(&mut self, ctes: Vec<CteDecl>, f: impl FnOnce(&mut Self) -> R) -> R {
        let child = self.context.with_ctes(ctes);
        self.in_scope(child, f)
    }

    /// Bind `f` with `relations` pushed as an enclosing correlation level (a
    /// subquery in an expression / a LATERAL derived table).
    fn in_outer<R>(&mut self, relations: Vec<Relation>, f: impl FnOnce(&mut Self) -> R) -> R {
        let child = self.context.with_outer(relations);
        self.in_scope(child, f)
    }

    /// Bind `f` (a lambda body) with `relations` as an enclosing level and the
    /// lambda `params` as a `Lambda` level on top.
    fn in_lambda<R>(
        &mut self,
        relations: Vec<Relation>,
        params: impl IntoIterator<Item = Ident>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let child = self.context.with_outer(relations).with_lambda(params);
        self.in_scope(child, f)
    }

    /// Build a table reference from a parsed name, recording a
    /// `TooManyTableQualifiers` diagnostic and returning `None` when it has
    /// more identifiers than `catalog.schema.name` (the only conversion
    /// failure) — so the dropped relation stays observable.
    pub(super) fn table_ref(&mut self, name: &ObjectName) -> Option<TableReference> {
        match TableReference::try_from_name(name) {
            Ok(table) => Some(table),
            Err(_) => {
                self.record_unrepresentable_table(name);
                None
            }
        }
    }

    /// Record a `WildcardSuppressed` diagnostic for a projection wildcard
    /// (`*` / `t.*` / `(expr).*`) left unexpanded, carrying the wildcard
    /// token's location (a zero-line span is treated as unknown).
    pub(super) fn record_wildcard_suppressed(&mut self, description: &str, span: Span) {
        let span = (span.start.line != 0).then_some(span);
        let suffix = span_suffix(span);
        self.diagnostics.push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::WildcardSuppressed,
            message: format!(
                "{description}{suffix} left unexpanded — column lineage will be incomplete for this projection"
            ),
            span,
        });
    }

    /// Record a `TooManyTableQualifiers` diagnostic for a table name that can't
    /// be represented as a `catalog.schema.name` [`TableReference`] and was
    /// dropped — either over-qualified (> 3 segments) or carrying a
    /// non-identifier part (a function-computed name like Snowflake's
    /// `IDENTIFIER('t')`). The kind covers both "unrepresentable table ref"
    /// cases; the message states which. Carries the name's location.
    pub(super) fn record_unrepresentable_table(&mut self, name: &ObjectName) {
        let span = name
            .0
            .first()
            .and_then(|part| part.as_ident())
            .map(|ident| ident.span)
            .filter(|s| s.start.line != 0);
        let suffix = span_suffix(span);
        let reason = if name.0.iter().any(|part| part.as_ident().is_none()) {
            "is not a plain catalog.schema.name identifier path"
        } else {
            "has too many qualifiers (max catalog.schema.name)"
        };
        self.diagnostics.push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::TooManyTableQualifiers,
            message: format!("table reference `{name}`{suffix} {reason} — dropped"),
            span,
        });
    }

    /// Record an `InsertColumnsUnresolved` diagnostic for a column-list-less
    /// INSERT / MERGE-INSERT whose target columns couldn't be determined, so
    /// its column-level `writes` / `lineage` are dropped (the table still
    /// surfaces in `table_writes`). The message names the actual cause: a
    /// `SELECT *` source can't be paired even *with* a catalog (its arity is
    /// unknown), so blaming a missing catalog there would mislead — that's
    /// reserved for a determinate source with no catalog to fill the target.
    pub(super) fn record_insert_columns_unresolved(
        &mut self,
        target: &TableReference,
        source_wildcard: bool,
    ) {
        let message = if source_wildcard {
            format!(
                "column-list-less INSERT into `{target}`: the `SELECT *` source isn't expanded, so its columns can't be paired with the target — column writes / lineage dropped"
            )
        } else {
            format!(
                "column-list-less INSERT into `{target}` can't pair source columns to target columns without a catalog — column writes / lineage dropped"
            )
        };
        self.diagnostics.push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::InsertColumnsUnresolved,
            message,
            span: None,
        });
    }

    /// Flag a MERGE whose target is not a plain table — a derived table /
    /// subquery / table function, which can't be a write target. The whole
    /// statement then binds to nothing, so an `UnsupportedStatement` (it
    /// projects to the table level) signals the empty surfaces are a coverage
    /// gap, not "nothing there". `message` shows the offending target.
    pub(super) fn record_unsupported_dml_target(
        &mut self,
        statement: &str,
        target: &dyn std::fmt::Display,
    ) {
        self.diagnostics.push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::UnsupportedStatement,
            message: format!(
                "{statement} target `{target}` is not a writable base table (a CTE name, derived table, subquery, table function, or join) — the statement can't be analyzed and is dropped"
            ),
            span: None,
        });
    }

    /// Record an `InsertColumnsUnresolved` diagnostic for a BigQuery
    /// `MERGE … WHEN NOT MATCHED THEN INSERT ROW`: it inserts the full source
    /// row, whose column pairing isn't recoverable from SQL text (and a catalog
    /// wouldn't help — the source columns aren't expanded), so its column-level
    /// `writes` / `lineage` are dropped. The target still surfaces in
    /// `table_writes` and feeds `table_lineage`.
    pub(super) fn record_merge_insert_row_unresolved(&mut self, target: &TableReference) {
        self.diagnostics.push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::InsertColumnsUnresolved,
            message: format!(
                "MERGE INSERT ROW into `{target}` inserts the full source row — its column writes / lineage can't be recovered from SQL text and are dropped"
            ),
            span: None,
        });
    }

    /// Record an `InsertColumnsArityMismatch` for an `INSERT INTO t (cols)
    /// <source>` whose explicit target column count differs from the source's
    /// projected count: the positional pairing zips to the shorter side, so
    /// surplus columns get no lineage edge.
    pub(super) fn record_insert_columns_arity_mismatch(
        &mut self,
        target: &TableReference,
        target_columns: usize,
        source_columns: usize,
    ) {
        self.diagnostics.push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::InsertColumnsArityMismatch,
            message: format!(
                "INSERT into `{target}` lists {target_columns} target column(s) but the source projects {source_columns} — lineage pairs only the first {min} and drops the rest",
                min = target_columns.min(source_columns)
            ),
            span: None,
        });
    }

    /// Record an INSERT / MERGE-INSERT arity mismatch between the target columns
    /// and the source values *if* they disagree (a no-op otherwise) — the caller
    /// passes the two determinate counts (no wildcard). An **explicit** column
    /// list must match exactly: either direction silently zips to the shorter
    /// side. A **column-less** target filled from the catalog is flagged only
    /// when the source is *wider* (the surplus is dropped); a narrower source
    /// may rely on column defaults, so it isn't.
    pub(super) fn diagnose_insert_arity(
        &mut self,
        target: &TableReference,
        explicit: bool,
        target_columns: usize,
        source_columns: usize,
    ) {
        let mismatch = if explicit {
            source_columns != target_columns
        } else {
            source_columns > target_columns
        };
        if mismatch {
            self.record_insert_columns_arity_mismatch(target, target_columns, source_columns);
        }
    }

    /// Flag a CTAS / CREATE VIEW (without an explicit column list) whose source
    /// projects unaliased expressions: those columns have no name recoverable
    /// from the SQL text, so they're dropped from column `writes` / `lineage`.
    /// A no-op when an explicit column list names every column or when every
    /// output is nameable. The set-op result schema follows the left branch, so
    /// only the first operand is inspected (mirroring `created_relation_*`).
    pub(super) fn flag_anonymous_relation_columns(
        &mut self,
        target: &TableReference,
        explicit: &[Ident],
        input: &LogicalPlan,
    ) {
        if !explicit.is_empty() {
            return;
        }
        let operands = output_operands(input);
        let Some(operand) = operands.first() else {
            return;
        };
        let anonymous = operand
            .outputs
            .iter()
            .filter(|ne| ne.name.is_none())
            .count();
        if anonymous == 0 {
            return;
        }
        self.diagnostics.push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::AnonymousColumnsSuppressed,
            message: format!(
                "`{target}` has {anonymous} unaliased expression column(s) with no name recoverable from the SQL text — dropped from column writes / lineage; alias them to surface"
            ),
            span: None,
        });
    }

    /// Diagnose a created relation's column list (CTAS / CREATE VIEW / ALTER
    /// VIEW) against its source projection: an *implicit* list flags an
    /// unaliased source output (`flag_anonymous_relation_columns`); an
    /// *explicit* list whose count differs from the source projection flags an
    /// arity mismatch — mirroring [`Self::bind_insert`]'s check. The arity check
    /// is skipped for a wildcard-bearing source (the count is then
    /// indeterminate; the wildcard is flagged separately and the lineage walker
    /// drops the unreliable pairing).
    pub(super) fn diagnose_created_columns(
        &mut self,
        target: &TableReference,
        explicit: &[Ident],
        input: &LogicalPlan,
        source_wildcard: bool,
    ) {
        if explicit.is_empty() {
            self.flag_anonymous_relation_columns(target, explicit, input);
            return;
        }
        if source_wildcard {
            return;
        }
        if let Some(operand) = output_operands(input).first() {
            let outputs = operand.outputs;
            if !outputs.is_empty() && outputs.len() != explicit.len() {
                self.record_created_columns_arity_mismatch(target, explicit.len(), outputs.len());
            }
        }
    }

    /// Record an `InsertColumnsArityMismatch` for a created relation (CTAS /
    /// CREATE VIEW / ALTER VIEW) whose explicit column count differs from the
    /// source projection — the positional pairing zips to the shorter side, so
    /// the surplus columns get no lineage edge (their `writes` still surface).
    /// Shares the kind with the INSERT form; the message names the relation.
    pub(super) fn record_created_columns_arity_mismatch(
        &mut self,
        target: &TableReference,
        target_columns: usize,
        source_columns: usize,
    ) {
        self.diagnostics.push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::InsertColumnsArityMismatch,
            message: format!(
                "created relation `{target}` lists {target_columns} column(s) but the source projects {source_columns} — lineage pairs only the first {min} and drops the rest",
                min = target_columns.min(source_columns)
            ),
            span: None,
        });
    }
}

/// Format a known span as the trailing " at L{line}:C{col}" suffix that
/// embedded into a diagnostic message; empty when the span is unknown
/// (the recorders elsewhere `filter` out zero-line spans before calling).
fn span_suffix(span: Option<Span>) -> String {
    match span {
        Some(s) => format!(" at L{}:C{}", s.start.line, s.start.column),
        None => String::new(),
    }
}

// ===== catalog matching ==================================================

/// Fill a query reference's missing prefix segments from the catalog's
/// defaults (bare → schema then catalog) for **matching**. A configured
/// default is a catalog-side schema / catalog *name* — the same kind of
/// case-exact stored identifier [`CatalogTable`] registers — so it is filled
/// **quoted**, matched case-exactly against the registration (a
/// `default_schema("public")` must match a registered `public`; a mismatched
/// case is a caller-side inconsistency, surfaced as a non-match, not folded
/// away). This is the query side's *matching* form; the surfaced identity uses
/// [`surface_with_defaults`] (plain).
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

/// The canonical identity of a matched catalog table — its registered
/// `catalog.schema.name` path. Each segment's *value* comes from the
/// registration (so a bare `users` and an explicit `public.users` agree), and
/// is surfaced **quoted** with the dialect's [`canonical_quote`](crate::casing::canonical_quote): catalog
/// identifiers are case-exact (matched as if quoted, via `normalize_catalog`),
/// so the surfaced identity must be quoted too, or a later fold (e.g. a column
/// qualifier under an upper-folding dialect) would re-case it and fail to match
/// its own relation. Each segment's *span* is carried from the matching
/// `written` segment so `reference.name.span` still points at where the
/// reference was written (for source-order sorting); a segment the catalog
/// *filled in* has no source token, so it gets an empty span.
fn canonical_ref(table: &CatalogTable, written: &TableReference, quote: char) -> TableReference {
    let seg = |value: &str, span: Span| Ident {
        value: value.to_string(),
        quote_style: Some(quote),
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
        // Schema-level ops that name no column (constraints, indexes,
        // partitions, projections, RLS / rule / trigger toggles, table rename /
        // owner / cluster, engine knobs, …). Enumerated rather than `_`-matched
        // so a future *column*-naming `AlterTableOperation` variant is a compile
        // error here, not a silent gap in the column-write surface.
        AlterTableOperation::AddConstraint { .. }
        | AlterTableOperation::AddProjection { .. }
        | AlterTableOperation::DropProjection { .. }
        | AlterTableOperation::MaterializeProjection { .. }
        | AlterTableOperation::ClearProjection { .. }
        | AlterTableOperation::DisableRowLevelSecurity
        | AlterTableOperation::DisableRule { .. }
        | AlterTableOperation::DisableTrigger { .. }
        | AlterTableOperation::DropConstraint { .. }
        | AlterTableOperation::AttachPartition { .. }
        | AlterTableOperation::DetachPartition { .. }
        | AlterTableOperation::FreezePartition { .. }
        | AlterTableOperation::UnfreezePartition { .. }
        | AlterTableOperation::DropPrimaryKey { .. }
        | AlterTableOperation::DropForeignKey { .. }
        | AlterTableOperation::DropIndex { .. }
        | AlterTableOperation::EnableAlwaysRule { .. }
        | AlterTableOperation::EnableAlwaysTrigger { .. }
        | AlterTableOperation::EnableReplicaRule { .. }
        | AlterTableOperation::EnableReplicaTrigger { .. }
        | AlterTableOperation::EnableRowLevelSecurity
        | AlterTableOperation::ForceRowLevelSecurity
        | AlterTableOperation::NoForceRowLevelSecurity
        | AlterTableOperation::EnableRule { .. }
        | AlterTableOperation::EnableTrigger { .. }
        | AlterTableOperation::RenamePartitions { .. }
        | AlterTableOperation::ReplicaIdentity { .. }
        | AlterTableOperation::AddPartitions { .. }
        | AlterTableOperation::DropPartitions { .. }
        | AlterTableOperation::RenameTable { .. }
        | AlterTableOperation::RenameConstraint { .. }
        | AlterTableOperation::SwapWith { .. }
        | AlterTableOperation::SetTblProperties { .. }
        | AlterTableOperation::OwnerTo { .. }
        | AlterTableOperation::ClusterBy { .. }
        | AlterTableOperation::DropClusteringKey
        | AlterTableOperation::SuspendRecluster
        | AlterTableOperation::ResumeRecluster
        | AlterTableOperation::Refresh { .. }
        | AlterTableOperation::Suspend
        | AlterTableOperation::Resume
        | AlterTableOperation::Algorithm { .. }
        | AlterTableOperation::Lock { .. }
        | AlterTableOperation::AutoIncrement { .. }
        | AlterTableOperation::ValidateConstraint { .. }
        | AlterTableOperation::SetOptionsParens { .. } => Vec::new(),
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

/// The name of the query output a positional ordinal key (`GROUP BY 1` /
/// `ORDER BY 1`) refers to — the 1-based n-th [`Scope::query_outputs`] entry,
/// if it has one. `None` for a non-integer / zero / out-of-range position, or
/// an anonymous output: the caller then binds the literal as written.
fn ordinal_output_name(expr: &SqlExpr, scope: &Scope) -> Option<Ident> {
    let SqlExpr::Value(v) = expr else {
        return None;
    };
    let Value::Number(digits, _) = &v.value else {
        return None;
    };
    let n: usize = digits.parse().ok()?;
    scope.query_outputs.get(n.checked_sub(1)?)?.name.clone()
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

/// Whether a join is `NATURAL` — its merge columns are the schema-common ones
/// (computed from both sides' known columns), not an explicit `USING` list.
fn join_is_natural(op: &JoinOperator) -> bool {
    matches!(join_constraint(op), Some(JoinConstraint::Natural))
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
        let style = IdentifierStyle {
            casing: crate::casing::IdentifierCasing::for_dialect(&GenericDialect {}),
            quote: crate::casing::canonical_quote(&GenericDialect {}),
        };
        build_with_diagnostics(&statements[0], catalog, style).0
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
