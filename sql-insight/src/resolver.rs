//! Walks a `sqlparser` `Statement` once and produces a
//! [`Resolution`] carrying scope bindings, captured refs, and lineage
//! edges. Two post-passes ([`Resolution::collapsed_lineage_edges`]
//! and [`Resolution::real_column_refs`]) refine the raw walk data
//! into the public extraction surfaces.
//!
//! Module layout (all sub-modules are crate-internal):
//!
//! - [`scope`]: `Scope`, `ScopeId`, `ScopeKind`, plus the arena-
//!   management methods on `Resolver` (`push_scope` / `pop_scope` /
//!   `bind_current` / `resolve_unqualified_relation` / etc.) and the
//!   lexical `with_*` helpers that save / restore scope state around
//!   a clause walk.
//! - [`binding`]: `Binding` enum + `BindingKey` + `bind_*` /
//!   lookup / diagnostic-record methods on `Resolver`.
//! - [`column_ref`]: `RawColumnRef` and walk-time resolution of
//!   identifier parts to owning tables.
//! - [`table_ref`]: `RawTableRef` / `TableRefTarget` and the
//!   `record_*_table_ref` constructors. Parallel to `column_ref`.
//! - [`body_output`]: `BodyOutput` / `OutputColumn` and the helpers
//!   that derive each output column's name / kind from a `SelectItem`.
//! - [`lineage`]: `LineageEdge` / `LineageTargetSpec` and the emit
//!   helpers that drive INSERT / CTAS / QueryOutput edge construction.
//! - [`resolution`]: `Resolution` struct + every `impl Resolution`
//!   method — public table queries and the post-walk collapse /
//!   filter passes.
//!
//! This file owns the top-level [`Resolver`] data shape, [`ResolvedQuery`],
//! and every `visit_*` method on `Resolver` (statement / query / expr /
//! table walkers all live here as one consolidated `impl` block).

mod binding;
mod body_output;
mod column_ref;
mod lineage;
mod resolution;
mod scope;
mod table_ref;

pub(crate) use binding::{Binding, TableRole};
pub(crate) use body_output::{BodyOutput, OutputColumn, SetOperand};
pub(crate) use column_ref::RawColumnRef;
pub(crate) use lineage::{LineageEdge, LineageTargetSpec};
pub(crate) use resolution::Resolution;
pub(crate) use scope::{Scope, ScopeId, ScopeKind};
pub(crate) use table_ref::{RawTableRef, TableRefTarget};

use body_output::{output_column_kind, output_column_name};

use sqlparser::ast::{
    AccessExpr, Array, ConnectByKind, Delete, DictionaryField, Distinct, Expr, Fetch, FromTable,
    Function, FunctionArg, FunctionArgExpr, FunctionArgumentClause, FunctionArgumentList,
    FunctionArguments, GroupByExpr, GroupByWithModifier, Ident, Interpolate, Join, JoinConstraint,
    JoinOperator, LimitClause, ListAggOnOverflow, Map, Merge, NamedWindowExpr, ObjectType,
    OnConflictAction, OnInsert, OrderBy, OrderByExpr, OrderByKind, PipeOperator, PivotValueSource,
    Query, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Statement, Subscript,
    Table, TableFactor, TableSample, TableSampleKind, TableWithJoins, TopQuantity, Update,
    UpdateTableFromKind, Values, WildcardAdditionalOptions, WindowFrameBound, WindowSpec,
    WindowType,
};

use crate::catalog::Catalog;
use crate::error::Error;
use crate::reference::TableReference;

/// What `resolve_query` returns: the body's output columns (grouped
/// by set-operation operand) and the body's scope id. Callers decide
/// whether to emit `QueryOutput` edges (default), pair positionally
/// with relation target columns (INSERT / CTAS), or bubble them
/// through `SetExpr::Query`.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedQuery {
    /// Body-walk output of the query — one [`SetOperand`] per
    /// set-operation operand, each carrying [`OutputColumn`]s with
    /// full lineage info. `None` when nothing was captured (e.g.
    /// recursive CTE pre-bind stub, `VALUES`-only query). Callers
    /// (CTE / derived bind, INSERT pairing, etc.) consume this
    /// directly.
    pub(crate) output_columns: Option<BodyOutput>,
    /// Arena id of the scope pushed for this query's body — exposed
    /// so callers binding the query as a synthetic relation (CTE /
    /// derived table) can record it on the binding for table-lineage
    /// collapse. Equals the scope that held the body's FROM
    /// bindings; pop has already happened by the time the caller
    /// sees this id, but the arena entry remains for post-pass
    /// lookups.
    pub(crate) body_scope: ScopeId,
}

/// The walker. Three fields, three roles:
///
/// - [`catalog`](Self::catalog) — input, optional schema provider.
/// - [`resolution`](Self::resolution) — output, the [`Resolution`]
///   under construction; finalized by `into_resolution`.
/// - [`context`](Self::context) — walk-time scratch state ([`Context`]):
///   per-query body buffer, scope cursor, lexical scope kind.
///
/// All `visit_*` methods and the various `bind_*` / `record_*` /
/// `with_*` helpers live as `impl` blocks in this file or in the
/// sub-modules — this is just the data shape and the top-level entry
/// point.
#[derive(Debug)]
pub(crate) struct Resolver<'a> {
    /// `None` means the resolver runs without external schema
    /// enrichment; tables bound here carry `output_columns: None`.
    catalog: Option<&'a dyn Catalog>,
    /// The [`Resolution`] under construction. Walk-time methods push
    /// directly into its `diagnostics` / `column_refs` /
    /// `lineage_edges` / `table_refs` buffers and the `scopes` arena;
    /// `into_resolution` runs the two post-passes on it and hands it
    /// back to the caller.
    resolution: Resolution,
    /// In-flight walk state — body buffer, scope cursor, scope-kind
    /// flag — see [`Context`].
    context: Context,
}

/// Per-walk scratch state for the [`Resolver`]: the things that change
/// as the walk progresses but don't survive into the final
/// [`Resolution`]. Reset implicitly when the walker is dropped.
#[derive(Debug, Default)]
pub(crate) struct Context {
    /// Per-query in-progress [`BodyOutput`], filled by `visit_select`
    /// (one [`SetOperand`] per SELECT body, accumulated across the
    /// operands of a set operation chain). `resolve_query` swaps a
    /// fresh buffer in for the duration of its walk and hands the
    /// collected body back via the returned `ResolvedQuery`'s
    /// `output_columns`, so each query gets exactly its own operands.
    pub(crate) current_body: BodyOutput,
    /// Cursor into [`Resolution::scopes`]: the currently-open scope.
    /// `None` before any push (then `current_scope_id` lazily inserts
    /// a root); set on each `push_scope` and walked back to the
    /// parent on each `pop_scope`.
    pub(crate) current_scope: Option<ScopeId>,
    /// Lexical context stamped onto every scope pushed while it is in
    /// effect: `Body` by default, flipped to `Predicate` by
    /// [`Resolver::with_filter_clause`] so subqueries nested in WHERE /
    /// HAVING / JOIN ON etc. are excluded from table-lineage. Propagates
    /// *through* subquery boundaries (a subquery in a predicate is itself
    /// predicate-position).
    pub(crate) current_scope_kind: ScopeKind,
}

impl<'a> Resolver<'a> {
    /// Internal constructor. Public callers go through
    /// [`Self::resolve_statement`].
    fn new(catalog: Option<&'a dyn Catalog>) -> Self {
        Self {
            catalog,
            resolution: Resolution::default(),
            context: Context::default(),
        }
    }

    /// Walk one [`Statement`] and return the final [`Resolution`].
    /// The one-shot entry point used by all extractors.
    pub(crate) fn resolve_statement(
        catalog: Option<&'a dyn Catalog>,
        statement: &Statement,
    ) -> Result<Resolution, Error> {
        let mut resolver = Self::new(catalog);
        resolver.visit_statement(statement)?;
        Ok(resolver.into_resolution())
    }

    /// Finalize the embedded [`Resolution`] via the two post-passes
    /// (lineage collapse + real-column filter) and hand it back. The
    /// walk state (current_body / current_scope / current_scope_kind) is
    /// dropped at this point — only `resolution` survives.
    fn into_resolution(self) -> Resolution {
        let mut resolution = self.resolution;
        // Both post-passes rely on the scope arena being final:
        // - collapse lineage edges so synthetic-binding (Cte/Derived)
        //   sources are collapsed with their body's source refs;
        // - filter column refs so synthetic-owned ones don't surface
        //   in the public reads list.
        resolution.lineage_edges = resolution.collapsed_lineage_edges();
        resolution.column_refs = resolution.real_column_refs();
        resolution
    }
    /// Walk one [`Query`] and return its [`ResolvedQuery`]. Swaps in
    /// a fresh body buffer so set operands don't leak across queries,
    /// pushes a body scope, walks WITH bindings, the body `SetExpr`,
    /// and the trailing clauses (ORDER BY / LIMIT / FETCH / settings
    /// / pipes), then hands back the collected body output + body
    /// scope id.
    pub(super) fn resolve_query(&mut self, query: &Query) -> Result<ResolvedQuery, Error> {
        // Swap in a fresh body buffer for this query — restored on
        // return — so each ResolvedQuery owns exactly its own set
        // operands without leaking into siblings or ancestors. This is
        // independent of the scope-arena push below: operands
        // accumulate into the resolver's own buffer, not into the scope.
        let prev_body = std::mem::take(&mut self.context.current_body);
        let body_scope = self.with_scope(|r| -> Result<ScopeId, Error> {
            let body_scope = r.current_scope_id();
            if let Some(with) = &query.with {
                if with.recursive {
                    for cte in &with.cte_tables {
                        // Recursive CTEs pre-bind with `None`
                        // output_columns; fixpoint-aware capture is
                        // deferred. `body_scope` here is the enclosing
                        // WITH scope (no real body has been walked
                        // yet) — collapse treats `None` as a terminal
                        // stub.
                        r.bind_cte(cte.alias.name.clone(), None, body_scope);
                    }
                    for cte in &with.cte_tables {
                        // Body output is discarded for recursive CTEs
                        // (no collapse either). Raw resolve_query so
                        // the intermediate QueryOutput edges aren't
                        // emitted.
                        r.resolve_query(&cte.query)?;
                    }
                } else {
                    for cte in &with.cte_tables {
                        // Raw resolve_query: the body's output_columns
                        // and body_scope are stored in the binding for
                        // lineage collapse, and no intermediate
                        // QueryOutput edges are emitted since the CTE
                        // output isn't a query result on its own —
                        // references through the CTE collapse end to
                        // end at lineage-emission time.
                        let resolved = r.resolve_query(&cte.query)?;
                        let renames = &cte.alias.columns;
                        let renamed = resolved.output_columns.map(|o| o.renamed(renames));
                        r.bind_cte(cte.alias.name.clone(), renamed, resolved.body_scope);
                    }
                }
            }
            r.visit_set_expr(&query.body)?;
            if let Some(order_by) = &query.order_by {
                r.visit_order_by(order_by)?;
            }
            if let Some(limit_clause) = &query.limit_clause {
                r.visit_limit_clause(limit_clause)?;
            }
            if let Some(fetch) = &query.fetch {
                r.visit_fetch(fetch)?;
            }
            if let Some(settings) = &query.settings {
                for setting in settings {
                    r.visit_expr(&setting.value)?;
                }
            }
            for pipe_operator in &query.pipe_operators {
                r.visit_pipe_operator(pipe_operator)?;
            }
            Ok(body_scope)
        })?;
        let body = std::mem::replace(&mut self.context.current_body, prev_body);
        let output_columns = if body.set_operands.is_empty() {
            None
        } else {
            Some(body)
        };
        Ok(ResolvedQuery {
            output_columns,
            body_scope,
        })
    }

    /// Dispatch one [`SetExpr`] node: SELECT body, parenthesised
    /// query (bubble its operands up), set operation (each operand
    /// in its own scope), DML wrapped under `WITH`, TABLE, or VALUES.
    fn visit_set_expr(&mut self, set_expr: &SetExpr) -> Result<(), Error> {
        match set_expr {
            SetExpr::Select(select) => self.visit_select(select),
            SetExpr::Query(query) => {
                // Parenthesized continuation of the enclosing query —
                // bubble the inner operands up so an outer INSERT (or
                // any other caller) sees them as if they were inline.
                let resolved = self.resolve_query(query)?;
                if let Some(output) = resolved.output_columns {
                    self.extend_set_operands(output.set_operands);
                }
                Ok(())
            }
            SetExpr::SetOperation { left, right, .. } => {
                // Each operand lives in its own scope so name resolution
                // doesn't see sibling operands' FROM bindings — matching
                // SQL's per-SELECT name resolution. The operands' own
                // visit_select calls each contribute one SetOperand entry
                // of output columns, so UNION INSERT naturally pairs
                // every operand with the same target columns.
                self.with_scope(|r| r.visit_set_expr(left))?;
                self.with_scope(|r| r.visit_set_expr(right))?;
                Ok(())
            }
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => {
                // `WITH cte AS (...) <DML>` — the DML statement runs in
                // its own scope so its target binding doesn't share the
                // enclosing query's scope with the CTEs. Without this,
                // an unqualified predicate ref like `id` in
                // `DELETE FROM t WHERE id IN (SELECT id FROM cte)`
                // would see both `t` and `cte` in one scope and resolve
                // ambiguously to None. CTEs stay reachable via the
                // parent-scope walk-up.
                self.with_scope(|r| r.visit_statement(statement))
            }
            SetExpr::Table(table) => {
                self.visit_table_command(table);
                Ok(())
            }
            SetExpr::Values(values) => self.visit_values(values),
        }
    }

    /// Walk a SELECT body. Pushes one [`SetOperand`] of output
    /// columns built from the projection list, then walks the
    /// filter / clause expressions (DISTINCT / TOP / FROM /
    /// WHERE / GROUP BY / HAVING / NAMED WINDOW / QUALIFY /
    /// CONNECT BY) and any SELECT INTO target.
    fn visit_select(&mut self, select: &Select) -> Result<(), Error> {
        if let Some(Distinct::On(exprs)) = &select.distinct {
            self.visit_exprs(exprs)?;
        }
        if let Some(top) = &select.top {
            if let Some(TopQuantity::Expr(expr)) = &top.quantity {
                self.visit_expr(expr)?;
            }
        }
        for table in &select.from {
            self.visit_table_with_joins(table, TableRole::Read)?;
        }
        let mut operand_columns = Vec::with_capacity(select.projection.len());
        for item in &select.projection {
            operand_columns.push(self.build_output_column(item)?);
        }
        self.push_set_operand(SetOperand {
            columns: operand_columns,
        });
        if let Some(into) = &select.into {
            // SELECT ... INTO new_table acts like CTAS — INTO is the write target.
            self.bind_real_table(
                TableReference::try_from(&into.name)?,
                None,
                TableRole::Write,
            );
        }
        // TODO: Hive/Spark `LATERAL VIEW explode(arr) t AS col` — the
        // generator expression is walked as a plain read here, but
        // the alias `t` and its output columns (`col`) are not bound,
        // so column refs against them currently surface as
        // `UnresolvedColumn`. Binding would need `lateral_view.lateral_view_name`
        // + `lateral_col_alias` as a DerivedTable-like with synthetic columns.
        for lateral_view in &select.lateral_views {
            self.visit_expr(&lateral_view.lateral_view)?;
        }
        for expr in [
            select.prewhere.as_ref(),
            select.selection.as_ref(),
            select.having.as_ref(),
            select.qualify.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            self.with_filter_clause(|r| r.visit_expr(expr))?;
        }
        for connect_by in &select.connect_by {
            // CONNECT BY / START WITH are predicate-style hierarchical
            // join conditions (Oracle / Snowflake) — subqueries nested
            // here do not feed the enclosing write target.
            self.with_filter_clause(|r| match connect_by {
                ConnectByKind::ConnectBy { relationships, .. } => r.visit_exprs(relationships),
                ConnectByKind::StartWith { condition, .. } => r.visit_expr(condition),
            })?;
        }
        self.visit_group_by(&select.group_by)?;
        // CLUSTER BY / DISTRIBUTE BY (Hive / Spark) — partitioning /
        // clustering directives, walked as plain reads.
        self.visit_exprs(&select.cluster_by)?;
        self.visit_exprs(&select.distribute_by)?;
        for order_by in &select.sort_by {
            self.visit_order_by_expr(order_by)?;
        }
        for window in &select.named_window {
            if let NamedWindowExpr::WindowSpec(spec) = &window.1 {
                self.visit_window_spec(spec)?;
            }
        }
        Ok(())
    }

    /// Walk a single projection item's expression and snapshot the
    /// refs it records, packaging name / source_refs / kind into an
    /// [`OutputColumn`].
    pub(super) fn build_output_column(&mut self, item: &SelectItem) -> Result<OutputColumn, Error> {
        let refs_before = self.resolution.column_refs.len();
        self.visit_select_item(item)?;
        let source_refs = self.resolution.column_refs[refs_before..].to_vec();
        Ok(OutputColumn {
            name: output_column_name(item),
            source_refs,
            kind: output_column_kind(item),
        })
    }

    /// Walk a single [`SelectItem`] for its side effects (recording
    /// column refs / nested subqueries). Output-column construction
    /// goes through [`Self::build_output_column`] instead.
    pub(super) fn visit_select_item(&mut self, item: &SelectItem) -> Result<(), Error> {
        match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                self.visit_expr(expr)
            }
            SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::Expr(expr), options) => {
                self.record_wildcard_suppressed(
                    "qualified wildcard `(expr).*`",
                    options.wildcard_token.0.span,
                );
                self.visit_expr(expr)
            }
            SelectItem::QualifiedWildcard(
                SelectItemQualifiedWildcardKind::ObjectName(name),
                options,
            ) => {
                self.record_wildcard_suppressed(
                    &format!("qualified wildcard `{}.*`", name),
                    options.wildcard_token.0.span,
                );
                self.visit_wildcard_options(options)
            }
            SelectItem::Wildcard(options) => {
                self.record_wildcard_suppressed("wildcard `*`", options.wildcard_token.0.span);
                self.visit_wildcard_options(options)
            }
        }
    }

    fn visit_table_command(&mut self, table: &Table) {
        let Some(name) = &table.table_name else {
            return;
        };
        // `TABLE foo` is sugar for `SELECT * FROM foo` — foo is read.
        self.bind_real_table(
            TableReference {
                catalog: None,
                schema: table
                    .schema_name
                    .as_ref()
                    .map(|schema| schema.as_str().into()),
                name: name.as_str().into(),
            },
            None,
            TableRole::Read,
        );
    }

    fn visit_values(&mut self, values: &Values) -> Result<(), Error> {
        for row in &values.rows {
            self.visit_exprs(row)?;
        }
        Ok(())
    }

    fn visit_group_by(&mut self, group_by: &GroupByExpr) -> Result<(), Error> {
        match group_by {
            GroupByExpr::All(modifiers) => self.visit_group_by_modifiers(modifiers),
            GroupByExpr::Expressions(exprs, modifiers) => {
                self.visit_exprs(exprs)?;
                self.visit_group_by_modifiers(modifiers)
            }
        }
    }

    fn visit_group_by_modifiers(&mut self, modifiers: &[GroupByWithModifier]) -> Result<(), Error> {
        for modifier in modifiers {
            if let GroupByWithModifier::GroupingSets(expr) = modifier {
                self.visit_expr(expr)?;
            }
        }
        Ok(())
    }
    /// Top-level [`Statement`] dispatch. Supported DML / DDL variants
    /// route into a per-verb walker; the remaining variants are
    /// enumerated explicitly and recorded as `UnsupportedStatement`
    /// — the match is exhaustive, so a new sqlparser variant
    /// surfaces as a compile error here.
    pub(super) fn visit_statement(&mut self, statement: &Statement) -> Result<(), Error> {
        // Keep this match exhaustive. Unsupported variants are listed explicitly so sqlparser
        // Statement additions become compile errors instead of silent misses.
        match statement {
            Statement::Query(query) => self.resolve_query_emitting_query_output(query).map(|_| ()),
            Statement::Insert(insert) => self.visit_insert(insert),
            Statement::Update(update) => self.visit_update(update),
            Statement::Delete(delete) => self.visit_delete(delete),
            Statement::Merge(merge) => self.visit_merge(merge),
            Statement::CreateTable(create_table) => {
                let target = TableReference::try_from(&create_table.name)?;
                self.bind_real_table(target.clone(), None, TableRole::Write);
                if let Some(query) = &create_table.query {
                    // CTAS: source projections pair with the new
                    // table's columns. Explicit column defs (if any)
                    // win over inferred names from the source SELECT.
                    let explicit: Vec<sqlparser::ast::Ident> = create_table
                        .columns
                        .iter()
                        .map(|c| c.name.clone())
                        .collect();
                    let resolved = self.resolve_query(query)?;
                    self.emit_relation_to_created(&target, &explicit, &resolved);
                }
                Ok(())
            }
            Statement::CreateView(create_view) => {
                let target = TableReference::try_from(&create_view.name)?;
                self.bind_real_table(target.clone(), None, TableRole::Write);
                let explicit: Vec<sqlparser::ast::Ident> =
                    create_view.columns.iter().map(|c| c.name.clone()).collect();
                let resolved = self.resolve_query(&create_view.query)?;
                self.emit_relation_to_created(&target, &explicit, &resolved);
                if let Some(to) = &create_view.to {
                    self.bind_real_table(TableReference::try_from(to)?, None, TableRole::Write);
                }
                Ok(())
            }
            Statement::AlterView {
                name,
                query,
                columns,
                ..
            } => {
                let target = TableReference::try_from(name)?;
                self.bind_real_table(target.clone(), None, TableRole::Write);
                let resolved = self.resolve_query(query)?;
                self.emit_relation_to_created(&target, columns, &resolved);
                Ok(())
            }
            Statement::CreateVirtualTable { name, .. } => {
                self.bind_real_table(TableReference::try_from(name)?, None, TableRole::Write);
                Ok(())
            }
            Statement::AlterTable(alter_table) => {
                self.bind_real_table(
                    TableReference::try_from(&alter_table.name)?,
                    None,
                    TableRole::Write,
                );
                Ok(())
            }
            Statement::Drop {
                object_type,
                names,
                table,
                ..
            } => {
                if matches!(
                    object_type,
                    ObjectType::Table | ObjectType::View | ObjectType::MaterializedView
                ) {
                    for name in names {
                        self.bind_real_table(
                            TableReference::try_from(name)?,
                            None,
                            TableRole::Write,
                        );
                    }
                }
                if let Some(table) = table {
                    self.bind_real_table(TableReference::try_from(table)?, None, TableRole::Write);
                }
                Ok(())
            }
            Statement::Truncate(truncate) => {
                for table in &truncate.table_names {
                    self.bind_real_table(
                        TableReference::try_from(&table.name)?,
                        None,
                        TableRole::Write,
                    );
                }
                Ok(())
            }
            Statement::Analyze(_)
            | Statement::Set(_)
            | Statement::Msck(_)
            | Statement::Install { .. }
            | Statement::Load { .. }
            | Statement::Directory { .. }
            | Statement::Case(_)
            | Statement::If(_)
            | Statement::While(_)
            | Statement::Raise(_)
            | Statement::Call(_)
            | Statement::Copy { .. }
            | Statement::CopyIntoSnowflake { .. }
            | Statement::Open(_)
            | Statement::Close { .. }
            | Statement::CreateIndex(_)
            | Statement::CreateRole(_)
            | Statement::CreateSecret { .. }
            | Statement::CreateServer(_)
            | Statement::CreatePolicy(_)
            | Statement::CreateConnector(_)
            | Statement::CreateOperator(_)
            | Statement::CreateOperatorFamily(_)
            | Statement::CreateOperatorClass(_)
            | Statement::AlterSchema(_)
            | Statement::AlterIndex { .. }
            | Statement::AlterType(_)
            | Statement::AlterOperator(_)
            | Statement::AlterOperatorFamily(_)
            | Statement::AlterOperatorClass(_)
            | Statement::AlterRole { .. }
            | Statement::AlterPolicy(_)
            | Statement::AlterConnector { .. }
            | Statement::AlterSession { .. }
            | Statement::AttachDatabase { .. }
            | Statement::AttachDuckDBDatabase { .. }
            | Statement::DetachDuckDBDatabase { .. }
            | Statement::DropFunction(_)
            | Statement::DropDomain(_)
            | Statement::DropProcedure { .. }
            | Statement::DropSecret { .. }
            | Statement::DropPolicy(_)
            | Statement::DropConnector { .. }
            | Statement::Declare { .. }
            | Statement::CreateExtension(_)
            | Statement::DropExtension(_)
            | Statement::DropOperator(_)
            | Statement::DropOperatorFamily(_)
            | Statement::DropOperatorClass(_)
            | Statement::Fetch { .. }
            | Statement::Flush { .. }
            | Statement::Discard { .. }
            | Statement::ShowFunctions { .. }
            | Statement::ShowVariable { .. }
            | Statement::ShowStatus { .. }
            | Statement::ShowVariables { .. }
            | Statement::ShowCreate { .. }
            | Statement::ShowColumns { .. }
            | Statement::ShowDatabases { .. }
            | Statement::ShowSchemas { .. }
            | Statement::ShowCharset(_)
            | Statement::ShowObjects(_)
            | Statement::ShowTables { .. }
            | Statement::ShowViews { .. }
            | Statement::ShowCollation { .. }
            | Statement::Use(_)
            | Statement::StartTransaction { .. }
            | Statement::Comment { .. }
            | Statement::Commit { .. }
            | Statement::Rollback { .. }
            | Statement::CreateSchema { .. }
            | Statement::CreateDatabase { .. }
            | Statement::CreateFunction(_)
            | Statement::CreateTrigger(_)
            | Statement::DropTrigger(_)
            | Statement::CreateProcedure { .. }
            | Statement::CreateMacro { .. }
            | Statement::CreateStage { .. }
            | Statement::Assert { .. }
            | Statement::Grant(_)
            | Statement::Deny(_)
            | Statement::Revoke(_)
            | Statement::Deallocate { .. }
            | Statement::Execute { .. }
            | Statement::Prepare { .. }
            | Statement::Kill { .. }
            | Statement::ExplainTable { .. }
            | Statement::Explain { .. }
            | Statement::Savepoint { .. }
            | Statement::ReleaseSavepoint { .. }
            | Statement::Cache { .. }
            | Statement::UNCache { .. }
            | Statement::CreateSequence { .. }
            | Statement::CreateDomain(_)
            | Statement::CreateType { .. }
            | Statement::Pragma { .. }
            | Statement::LockTables { .. }
            | Statement::UnlockTables
            | Statement::Unload { .. }
            | Statement::OptimizeTable { .. }
            | Statement::LISTEN { .. }
            | Statement::UNLISTEN { .. }
            | Statement::NOTIFY { .. }
            | Statement::LoadData { .. }
            | Statement::RenameTable(_)
            | Statement::List(_)
            | Statement::Remove(_)
            | Statement::RaisError { .. }
            | Statement::Print(_)
            | Statement::Return(_)
            | Statement::ExportData(_)
            | Statement::CreateUser(_)
            | Statement::AlterUser(_)
            | Statement::Vacuum(_)
            | Statement::Reset(_) => {
                self.record_unsupported_statement(statement);
                Ok(())
            }
        }
    }

    /// Walk an INSERT: bind the target table (Write role), pair
    /// source projections with the target's columns (explicit list
    /// or catalog-inferred) for `Relation` lineage, walk RETURNING,
    /// and dispatch the ON CONFLICT / ON DUPLICATE KEY clause.
    fn visit_insert(&mut self, insert: &sqlparser::ast::Insert) -> Result<(), Error> {
        let (table, alias) = TableReference::from_insert_with_alias(insert)?;
        let target_table = table.clone();
        self.bind_real_table(table, alias, TableRole::Write);
        // Explicit column list wins; otherwise fall back to the
        // catalog-provided schema (when present) for positional
        // pairing. Without either, no lineage edges are emitted —
        // we have no target column names to pair against.
        let effective_columns = self.effective_target_columns(&insert.columns, &target_table);
        let source_output = if let Some(source) = &insert.source {
            // Raw resolve_query (not the QueryOutput-emitting wrapper):
            // INSERT pairs each output column positionally with its
            // target column instead, emitting Relation edges. UNION
            // sources surface as multiple set operands, so each operand
            // pairs against the same target columns naturally.
            let resolved = self.resolve_query(source)?;
            if let Some(output) = resolved.output_columns.as_ref() {
                self.emit_per_output_column(&output.set_operands, |position, _col| {
                    effective_columns
                        .get(position)
                        .map(|col| LineageTargetSpec::Relation {
                            table: target_table.clone(),
                            column: col.clone(),
                        })
                });
            }
            resolved.output_columns
        } else {
            None
        };
        for assignment in &insert.assignments {
            self.visit_expr(&assignment.value)?;
        }
        // Walk RETURNING before the ON-clause so EXCLUDED isn't yet
        // bound: RETURNING projects from the target table, never from
        // the would-be-inserted pseudo-row, and an in-scope EXCLUDED
        // would ambify unqualified refs that collide with INSERT cols.
        self.visit_returning(insert.returning.as_deref())?;
        if let Some(on) = &insert.on {
            self.visit_insert_on(
                on,
                &target_table,
                &effective_columns,
                source_output.as_ref(),
            )?;
        }
        Ok(())
    }

    /// Walk a `RETURNING <select_items>` clause. Each item is treated
    /// like a top-level SELECT projection: it contributes refs to
    /// `column_refs` and a `QueryOutput` lineage edge per item. The
    /// target table is the only binding in scope (the source SELECT's
    /// inner scope has been popped by the time this runs), so
    /// unqualified refs resolve to it.
    fn visit_returning(&mut self, returning: Option<&[SelectItem]>) -> Result<(), Error> {
        let Some(items) = returning else {
            return Ok(());
        };
        let mut columns = Vec::with_capacity(items.len());
        for item in items {
            columns.push(self.build_output_column(item)?);
        }
        let operands = vec![SetOperand { columns }];
        self.emit_per_output_column(&operands, |position, col| {
            Some(LineageTargetSpec::QueryOutput {
                name: col.name.clone(),
                position,
            })
        });
        Ok(())
    }

    /// Walk the optional ON-clause attached to an `INSERT`:
    /// `ON CONFLICT ... DO UPDATE SET ...` (Postgres / Sqlite) or
    /// `ON DUPLICATE KEY UPDATE ...` (MySQL). Both update-style
    /// actions reuse [`Self::emit_assignment_lineage`] so each
    /// assignment's RHS feeds a Relation-target lineage edge into the
    /// INSERT target's column, identical to a standalone `UPDATE`.
    ///
    /// The `EXCLUDED` pseudo-table (Postgres) is bound as a synthetic
    /// derived-table with the INSERT target's column list as its
    /// schema, so `EXCLUDED.<col>` refs filter out of the public
    /// `reads` surface (matching how CTE / derived refs behave) while
    /// still emitting valid lineage sources for the assignment edges.
    /// MySQL's equivalent (`VALUES(<col>)`) is a function-call form
    /// that visit_expr already walks; no extra binding needed.
    fn visit_insert_on(
        &mut self,
        on: &OnInsert,
        target_table: &TableReference,
        effective_columns: &[Ident],
        source_output: Option<&BodyOutput>,
    ) -> Result<(), Error> {
        match on {
            OnInsert::DuplicateKeyUpdate(assignments) => {
                // MySQL ON DUPLICATE KEY UPDATE doesn't expose the
                // would-be-inserted row as a pseudo-table; `VALUES(col)`
                // is the implicit-row form, parsed as a regular
                // function call. Don't bind EXCLUDED here — doing so
                // would make unqualified column refs inside the SET
                // expressions ambiguous against the INSERT target.
                self.emit_assignment_lineage(assignments, Some(target_table))?;
            }
            OnInsert::OnConflict(on_conflict) => {
                if let OnConflictAction::DoUpdate(do_update) = &on_conflict.action {
                    // EXCLUDED in Postgres / Sqlite exposes the
                    // would-be-inserted row as a row source. Bind it
                    // as a synthetic derived-table whose `output_columns`
                    // is the INSERT source's per-operand columns renamed
                    // positionally to the target column names. That way
                    // `EXCLUDED.<col>` collapses back to whatever
                    // expression feeds that position of the source
                    // (e.g. `EXCLUDED.b` → source's `y` when the INSERT
                    // pairs (a, b) ← (x, y)). Refs against `EXCLUDED`
                    // also filter out of the public `reads` surface
                    // (like CTE / derived).
                    //
                    // `body_scope: None` — EXCLUDED's output is
                    // synthesized from the INSERT source's per-operand
                    // columns, not walked into a fresh scope. The
                    // INSERT source's real tables are already covered
                    // by their own RawTableRefs, so EXCLUDED needs no
                    // collapse path for table lineage.
                    let excluded_output = excluded_body_output(effective_columns, source_output);
                    self.bind_derived_table(Ident::new("EXCLUDED"), excluded_output, None);
                    self.emit_assignment_lineage(&do_update.assignments, Some(target_table))?;
                    if let Some(selection) = &do_update.selection {
                        self.with_filter_clause(|r| r.visit_expr(selection))?;
                    }
                }
            }
            // `OnInsert` is `#[non_exhaustive]` in sqlparser. New
            // variants land silently here — revisit when sqlparser
            // grows another conflict-action shape.
            _ => {}
        }
        Ok(())
    }

    /// Emit Relation lineage edges for a CREATE-AS source: each
    /// projection item pairs with the created relation's column at
    /// the same position. Target column name comes from the explicit
    /// column list when present, otherwise from the projection's
    /// inferred name (alias > bare ident name); items without an
    /// inferable name and no explicit slot are silently skipped.
    /// Used by CTAS, CREATE VIEW, and ALTER VIEW.
    ///
    /// For UNION-bodied sources the result schema follows the LEFT
    /// operand's names (SQL standard), so the inferred-name fallback
    /// reads the first operand's column names rather than the current
    /// operand's — making every operand pair against the same target
    /// column at each position. Mirrors INSERT-SELECT-UNION
    /// positional pairing.
    fn emit_relation_to_created(
        &mut self,
        target: &TableReference,
        explicit_columns: &[sqlparser::ast::Ident],
        resolved: &ResolvedQuery,
    ) {
        let Some(output) = resolved.output_columns.as_ref() else {
            return;
        };
        let inferred_left_names: Vec<Option<Ident>> = output
            .set_operands
            .first()
            .map(|operand| operand.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        self.emit_per_output_column(&output.set_operands, |position, _col| {
            explicit_columns
                .get(position)
                .cloned()
                .or_else(|| inferred_left_names.get(position).cloned().flatten())
                .map(|column| LineageTargetSpec::Relation {
                    table: target.clone(),
                    column,
                })
        });
    }

    /// Walk an UPDATE: bind the target table (Write role) plus any
    /// FROM source relations (Read), then per assignment emit
    /// `Relation` lineage from the RHS expression to the SET target
    /// column. WHERE and RETURNING follow.
    fn visit_update(&mut self, update: &Update) -> Result<(), Error> {
        // The head of update.table is the write target; joined tables
        // (inside visit_table_with_joins) are reads by definition.
        self.visit_table_with_joins(&update.table, TableRole::Write)?;
        if let Some(from) = &update.from {
            let tables = match from {
                UpdateTableFromKind::BeforeSet(tables) | UpdateTableFromKind::AfterSet(tables) => {
                    tables
                }
            };
            for table in tables {
                self.visit_table_with_joins(table, TableRole::Read)?;
            }
        }
        let target_table = try_target_table_from_factor(&update.table.relation);
        self.emit_assignment_lineage(&update.assignments, target_table.as_ref())?;
        if let Some(selection) = &update.selection {
            self.with_filter_clause(|r| r.visit_expr(selection))?;
        }
        self.visit_returning(update.returning.as_deref())?;
        Ok(())
    }

    /// Walk each SET-style assignment's RHS expression and emit
    /// Relation lineage edges from any newly recorded source refs into
    /// the assignment's target column. Shared by `visit_update` and
    /// MERGE's `WHEN MATCHED UPDATE` branch — both have identical
    /// per-assignment semantics. Target column qualifier resolution:
    /// qualified target (`t.col`) wins; bare target falls back to
    /// `default_table` (UPDATE head / MERGE INTO target).
    fn emit_assignment_lineage(
        &mut self,
        assignments: &[sqlparser::ast::Assignment],
        default_table: Option<&TableReference>,
    ) -> Result<(), Error> {
        for assignment in assignments {
            let target_parts = assignment_target_parts(&assignment.target);
            let kind = body_output::expr_kind(&assignment.value);
            let refs_before = self.resolution.column_refs.len();
            self.visit_expr(&assignment.value)?;
            let Some(target_parts) = target_parts else {
                continue;
            };
            let Some(target_table_ref) = assignment_target_table(&target_parts, default_table)
            else {
                continue;
            };
            let target = LineageTargetSpec::Relation {
                table: target_table_ref,
                column: target_parts.last().cloned().unwrap(),
            };
            self.push_edges_from_refs_since(refs_before, target, kind);
        }
        Ok(())
    }

    /// Walk a DELETE: bind the target tables (Write role) and any
    /// USING / FROM source relations (Read), walk WHERE and
    /// RETURNING. DELETE doesn't physically move data, so no
    /// lineage edges are emitted here.
    fn visit_delete(&mut self, delete: &Delete) -> Result<(), Error> {
        // Visit in alias-defining order so that later Write binds merge
        // onto already-resolved `TableReference`s rather than overwriting
        // them with bare names.
        //
        // The FROM clause's role depends on the shape of the DELETE:
        //   bare `DELETE FROM t`               → FROM is write target
        //   `DELETE FROM target USING source`  → FROM is write target, USING is read-and-alias-source
        //   `DELETE target FROM source`        → FROM is read-and-alias-source, tables list is write target
        //
        // In the USING shape the alias-defining clause is USING, so visit
        // USING first. In the explicit-target-list shape the
        // alias-defining clause is FROM, which we also want visited before
        // the tables list is merged on top.
        if let Some(using) = &delete.using {
            for table in using {
                self.visit_table_with_joins(table, TableRole::Read)?;
            }
        }
        let from_role = if delete.tables.is_empty() {
            TableRole::Write
        } else {
            TableRole::Read
        };
        for table in from_table_items(&delete.from) {
            self.visit_table_with_joins(table, from_role)?;
        }
        for name in &delete.tables {
            self.bind_real_table(TableReference::try_from_name(name)?, None, TableRole::Write);
        }
        if let Some(selection) = &delete.selection {
            self.with_filter_clause(|r| r.visit_expr(selection))?;
        }
        self.visit_returning(delete.returning.as_deref())?;
        Ok(())
    }

    /// Walk a MERGE: bind the target (Write) and source (Read)
    /// relations, walk the ON clause as a predicate, then per
    /// WHEN clause emit `Relation` lineage — assignment-style for
    /// UPDATE, positional column-pair for INSERT VALUES; DELETE
    /// actions emit nothing.
    fn visit_merge(&mut self, merge: &Merge) -> Result<(), Error> {
        use sqlparser::ast::{MergeAction, MergeInsertKind};
        self.visit_table_factor(&merge.table, TableRole::Write)?;
        self.visit_table_factor(&merge.source, TableRole::Read)?;
        self.with_filter_clause(|r| r.visit_expr(&merge.on))?;
        let target_table = try_target_table_from_factor(&merge.table);
        for clause in &merge.clauses {
            if let Some(predicate) = &clause.predicate {
                self.with_filter_clause(|r| r.visit_expr(predicate))?;
            }
            match &clause.action {
                MergeAction::Insert(insert_expr) => {
                    if let Some(pred) = &insert_expr.insert_predicate {
                        self.with_filter_clause(|r| r.visit_expr(pred))?;
                    }
                    if let MergeInsertKind::Values(values) = &insert_expr.kind {
                        self.emit_merge_insert_lineage(
                            values,
                            &insert_expr.columns,
                            target_table.as_ref(),
                        )?;
                    }
                    // MergeInsertKind::Row (BigQuery `INSERT ROW`) — the
                    // source row is inserted as-is; per-column pairing
                    // needs catalog knowledge of the target schema.
                }
                MergeAction::Update(update_expr) => {
                    self.emit_assignment_lineage(&update_expr.assignments, target_table.as_ref())?;
                }
                MergeAction::Delete { .. } => {
                    // DELETE has no column-level value lineage.
                }
            }
        }
        Ok(())
    }

    /// Emit per-position Relation lineage edges for MERGE's
    /// `WHEN NOT MATCHED THEN INSERT (cols) VALUES (...)`. Each value
    /// expression's source refs pair with the column at the same
    /// position in `columns`. Walks values with default `Projection`
    /// kind for read classification.
    fn emit_merge_insert_lineage(
        &mut self,
        values: &sqlparser::ast::Values,
        columns: &[sqlparser::ast::ObjectName],
        target_table: Option<&TableReference>,
    ) -> Result<(), Error> {
        // Resolve effective target column idents up-front: when the
        // INSERT clause has an explicit list, take each ObjectName's
        // last segment; otherwise fall back to the catalog-provided
        // schema (returns empty without catalog, matching the
        // no-pairing behavior).
        let explicit_idents: Vec<sqlparser::ast::Ident> = columns
            .iter()
            .filter_map(|c| c.0.last().and_then(|p| p.as_ident().cloned()))
            .collect();
        let effective_idents = match target_table {
            Some(target) => self.effective_target_columns(&explicit_idents, target),
            None => explicit_idents,
        };
        for row in &values.rows {
            for (position, value_expr) in row.iter().enumerate() {
                let kind = body_output::expr_kind(value_expr);
                let refs_before = self.resolution.column_refs.len();
                self.visit_expr(value_expr)?;
                let (Some(target_table), Some(col_ident)) =
                    (target_table, effective_idents.get(position))
                else {
                    continue;
                };
                let target = LineageTargetSpec::Relation {
                    table: target_table.clone(),
                    column: col_ident.clone(),
                };
                self.push_edges_from_refs_since(refs_before, target, kind);
            }
        }
        Ok(())
    }
    /// Exhaustive dispatch over [`Expr`]. Most variants delegate to
    /// `visit_expr` on their sub-expressions; `Identifier` /
    /// `CompoundIdentifier` record a column ref; subquery variants
    /// route through `resolve_query`. New sqlparser `Expr` variants
    /// must be added here as compile errors.
    pub(super) fn visit_expr(&mut self, expr: &Expr) -> Result<(), Error> {
        // Keep this match exhaustive so sqlparser Expr additions are reviewed here.
        match expr {
            // Subqueries in expression position (scalar / EXISTS / IN)
            // resolve with raw `resolve_query`, NOT the
            // QueryOutput-emitting wrapper — their transient projection
            // is an intermediate, not a statement output. A scalar
            // subquery in a projection has its source refs absorbed by
            // the enclosing projection item (which emits the meaningful
            // edge); a predicate subquery produces reads but no lineage.
            // Same disposition as CTE / derived bodies.
            Expr::Subquery(query) => self.resolve_query(query).map(|_| ()),
            Expr::Exists { subquery, .. } => self.resolve_query(subquery).map(|_| ()),
            Expr::InSubquery { expr, subquery, .. } => {
                self.visit_expr(expr)?;
                self.resolve_query(subquery).map(|_| ())
            }
            Expr::BinaryOp { left, right, .. }
            | Expr::IsDistinctFrom(left, right)
            | Expr::IsNotDistinctFrom(left, right)
            | Expr::AnyOp { left, right, .. }
            | Expr::AllOp { left, right, .. } => {
                self.visit_expr(left)?;
                self.visit_expr(right)
            }
            Expr::UnaryOp { expr, .. }
            | Expr::Nested(expr)
            | Expr::OuterJoin(expr)
            | Expr::Prior(expr)
            | Expr::IsFalse(expr)
            | Expr::IsNotFalse(expr)
            | Expr::IsTrue(expr)
            | Expr::IsNotTrue(expr)
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr)
            | Expr::IsUnknown(expr)
            | Expr::IsNotUnknown(expr)
            | Expr::Cast { expr, .. }
            | Expr::IsNormalized { expr, .. }
            | Expr::Extract { expr, .. }
            | Expr::Ceil { expr, .. }
            | Expr::Floor { expr, .. }
            | Expr::Collate { expr, .. }
            | Expr::Prefixed { value: expr, .. }
            | Expr::Named { expr, .. } => self.visit_expr(expr),
            Expr::CompoundFieldAccess { root, access_chain } => {
                self.visit_expr(root)?;
                for access in access_chain {
                    self.visit_access_expr(access)?;
                }
                Ok(())
            }
            Expr::JsonAccess { value, .. } => self.visit_expr(value),
            Expr::InList { expr, list, .. } => {
                self.visit_expr(expr)?;
                for item in list {
                    self.visit_expr(item)?;
                }
                Ok(())
            }
            Expr::InUnnest {
                expr, array_expr, ..
            } => {
                self.visit_expr(expr)?;
                self.visit_expr(array_expr)
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.visit_expr(expr)?;
                self.visit_expr(low)?;
                self.visit_expr(high)
            }
            Expr::Like { expr, pattern, .. }
            | Expr::ILike { expr, pattern, .. }
            | Expr::SimilarTo { expr, pattern, .. }
            | Expr::RLike { expr, pattern, .. } => {
                self.visit_expr(expr)?;
                self.visit_expr(pattern)
            }
            Expr::Convert { expr, styles, .. } => {
                self.visit_expr(expr)?;
                for style in styles {
                    self.visit_expr(style)?;
                }
                Ok(())
            }
            Expr::AtTimeZone {
                timestamp,
                time_zone,
            } => {
                self.visit_expr(timestamp)?;
                self.visit_expr(time_zone)
            }
            Expr::Position { expr, r#in } => {
                self.visit_expr(expr)?;
                self.visit_expr(r#in)
            }
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                self.visit_expr(expr)?;
                if let Some(expr) = substring_from {
                    self.visit_expr(expr)?;
                }
                if let Some(expr) = substring_for {
                    self.visit_expr(expr)?;
                }
                Ok(())
            }
            Expr::Trim {
                expr,
                trim_what,
                trim_characters,
                ..
            } => {
                self.visit_expr(expr)?;
                if let Some(expr) = trim_what {
                    self.visit_expr(expr)?;
                }
                if let Some(exprs) = trim_characters {
                    for expr in exprs {
                        self.visit_expr(expr)?;
                    }
                }
                Ok(())
            }
            Expr::Overlay {
                expr,
                overlay_what,
                overlay_from,
                overlay_for,
            } => {
                self.visit_expr(expr)?;
                self.visit_expr(overlay_what)?;
                self.visit_expr(overlay_from)?;
                if let Some(expr) = overlay_for {
                    self.visit_expr(expr)?;
                }
                Ok(())
            }
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                // All CASE sub-expressions (operand, WHEN conditions,
                // THEN/ELSE results) are walked the same way — refs no
                // longer carry a clause kind, so there is nothing to
                // distinguish the condition position from the result.
                if let Some(expr) = operand {
                    self.visit_expr(expr)?;
                }
                for condition in conditions {
                    self.visit_expr(&condition.condition)?;
                    self.visit_expr(&condition.result)?;
                }
                if let Some(expr) = else_result {
                    self.visit_expr(expr)?;
                }
                Ok(())
            }
            Expr::GroupingSets(exprs) | Expr::Cube(exprs) | Expr::Rollup(exprs) => {
                for group in exprs {
                    for expr in group {
                        self.visit_expr(expr)?;
                    }
                }
                Ok(())
            }
            Expr::Tuple(exprs) => {
                for expr in exprs {
                    self.visit_expr(expr)?;
                }
                Ok(())
            }
            Expr::Struct { values, .. } => {
                for expr in values {
                    self.visit_expr(expr)?;
                }
                Ok(())
            }
            Expr::Function(function) => self.visit_function(function),
            Expr::Dictionary(fields) => {
                for field in fields {
                    self.visit_dictionary_field(field)?;
                }
                Ok(())
            }
            Expr::Map(map) => self.visit_map(map),
            Expr::Array(array) => self.visit_array(array),
            Expr::Interval(interval) => self.visit_expr(&interval.value),
            Expr::Lambda(lambda) => self.visit_expr(&lambda.body),
            Expr::MemberOf(member_of) => {
                self.visit_expr(&member_of.value)?;
                self.visit_expr(&member_of.array)
            }
            Expr::Identifier(ident) => {
                self.record_column_ref(vec![ident.clone()]);
                Ok(())
            }
            Expr::CompoundIdentifier(parts) => {
                self.record_column_ref(parts.clone());
                Ok(())
            }
            Expr::Value(_)
            | Expr::TypedString(_)
            | Expr::MatchAgainst { .. }
            | Expr::Wildcard(_)
            | Expr::QualifiedWildcard(_, _) => Ok(()),
        }
    }

    pub(super) fn visit_exprs(&mut self, exprs: &[Expr]) -> Result<(), Error> {
        for expr in exprs {
            self.visit_expr(expr)?;
        }
        Ok(())
    }

    pub(super) fn visit_order_by(&mut self, order_by: &OrderBy) -> Result<(), Error> {
        if let OrderByKind::Expressions(exprs) = &order_by.kind {
            for expr in exprs {
                self.visit_order_by_expr(expr)?;
            }
        }
        if let Some(interpolate) = &order_by.interpolate {
            self.visit_interpolate(interpolate)?;
        }
        Ok(())
    }

    pub(super) fn visit_order_by_expr(&mut self, order_by: &OrderByExpr) -> Result<(), Error> {
        self.visit_expr(&order_by.expr)?;
        if let Some(with_fill) = &order_by.with_fill {
            for expr in [
                with_fill.from.as_ref(),
                with_fill.to.as_ref(),
                with_fill.step.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                self.visit_expr(expr)?;
            }
        }
        Ok(())
    }

    fn visit_interpolate(&mut self, interpolate: &Interpolate) -> Result<(), Error> {
        if let Some(exprs) = &interpolate.exprs {
            for expr in exprs {
                if let Some(expr) = &expr.expr {
                    self.visit_expr(expr)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn visit_limit_clause(&mut self, limit_clause: &LimitClause) -> Result<(), Error> {
        match limit_clause {
            LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            } => {
                if let Some(expr) = limit {
                    self.visit_expr(expr)?;
                }
                if let Some(offset) = offset {
                    self.visit_expr(&offset.value)?;
                }
                self.visit_exprs(limit_by)
            }
            LimitClause::OffsetCommaLimit { offset, limit } => {
                self.visit_expr(offset)?;
                self.visit_expr(limit)
            }
        }
    }

    pub(super) fn visit_fetch(&mut self, fetch: &Fetch) -> Result<(), Error> {
        if let Some(expr) = &fetch.quantity {
            self.visit_expr(expr)?;
        }
        Ok(())
    }

    /// Dispatch one pipe-syntax operator (`|> WHERE`, `|> SELECT`,
    /// `|> AGGREGATE`, etc.). Filter-position operators wrap their
    /// expression walks with `with_filter_clause`; projection-shaped
    /// operators (`|> SELECT` / `|> EXTEND` / `|> AGGREGATE`) push a
    /// fresh `SetOperand` of output columns.
    pub(super) fn visit_pipe_operator(&mut self, operator: &PipeOperator) -> Result<(), Error> {
        match operator {
            PipeOperator::Limit { expr, offset } => {
                self.visit_expr(expr)?;
                if let Some(expr) = offset {
                    self.visit_expr(expr)?;
                }
                Ok(())
            }
            PipeOperator::Where { expr } => self.with_filter_clause(|r| r.visit_expr(expr)),
            PipeOperator::OrderBy { exprs } => {
                for expr in exprs {
                    self.visit_order_by_expr(expr)?;
                }
                Ok(())
            }
            PipeOperator::Select { exprs } | PipeOperator::Extend { exprs } => {
                for expr in exprs {
                    self.visit_select_item(expr)?;
                }
                Ok(())
            }
            PipeOperator::Set { assignments } => {
                for assignment in assignments {
                    self.visit_expr(&assignment.value)?;
                }
                Ok(())
            }
            PipeOperator::Aggregate {
                full_table_exprs,
                group_by_expr,
            } => {
                for expr in full_table_exprs {
                    self.visit_expr(&expr.expr.expr)?;
                }
                for expr in group_by_expr {
                    self.visit_expr(&expr.expr.expr)?;
                }
                Ok(())
            }
            PipeOperator::TableSample { sample } => self.visit_table_sample(sample),
            PipeOperator::Union { queries, .. }
            | PipeOperator::Intersect { queries, .. }
            | PipeOperator::Except { queries, .. } => {
                for query in queries {
                    self.resolve_query_emitting_query_output(query)?;
                }
                Ok(())
            }
            PipeOperator::Call { function, alias } => {
                self.visit_function(function)?;
                if let Some(alias) = alias {
                    self.bind_table_function(alias.clone());
                }
                Ok(())
            }
            PipeOperator::Pivot {
                aggregate_functions,
                value_source,
                ..
            } => {
                for expr in aggregate_functions {
                    self.visit_expr(&expr.expr)?;
                }
                self.visit_pivot_value_source(value_source)
            }
            PipeOperator::Join(join) => self.visit_join(join),
            PipeOperator::Drop { .. }
            | PipeOperator::As { .. }
            | PipeOperator::Rename { .. }
            | PipeOperator::Unpivot { .. } => Ok(()),
        }
    }

    pub(super) fn visit_wildcard_options(
        &mut self,
        options: &WildcardAdditionalOptions,
    ) -> Result<(), Error> {
        if let Some(replace) = &options.opt_replace {
            for item in &replace.items {
                self.visit_expr(&item.expr)?;
            }
        }
        Ok(())
    }

    fn visit_function(&mut self, function: &Function) -> Result<(), Error> {
        self.visit_function_arguments(&function.parameters)?;
        self.visit_function_arguments(&function.args)?;
        if let Some(expr) = &function.filter {
            self.visit_expr(expr)?;
        }
        for expr in &function.within_group {
            self.visit_order_by_expr(expr)?;
        }
        if let Some(over) = &function.over {
            self.visit_window_type(over)?;
        }
        Ok(())
    }

    fn visit_function_arguments(&mut self, arguments: &FunctionArguments) -> Result<(), Error> {
        match arguments {
            FunctionArguments::None => Ok(()),
            // A subquery as a function argument is an intermediate, not
            // a statement output — raw resolve (no QueryOutput edge).
            FunctionArguments::Subquery(query) => self.resolve_query(query).map(|_| ()),
            FunctionArguments::List(args) => self.visit_function_argument_list(args),
        }
    }

    fn visit_function_argument_list(&mut self, args: &FunctionArgumentList) -> Result<(), Error> {
        for arg in &args.args {
            self.visit_function_arg(arg)?;
        }
        for clause in &args.clauses {
            match clause {
                FunctionArgumentClause::OrderBy(order_by) => {
                    for order_by in order_by {
                        self.visit_order_by_expr(order_by)?;
                    }
                }
                FunctionArgumentClause::Limit(expr) => self.visit_expr(expr)?,
                FunctionArgumentClause::OnOverflow(on_overflow) => {
                    self.visit_list_agg_on_overflow(on_overflow)?
                }
                FunctionArgumentClause::Having(bound) => self.visit_expr(&bound.1)?,
                FunctionArgumentClause::IgnoreOrRespectNulls(_)
                | FunctionArgumentClause::Separator(_)
                | FunctionArgumentClause::JsonNullClause(_)
                | FunctionArgumentClause::JsonReturningClause(_) => {}
            }
        }
        Ok(())
    }

    fn visit_list_agg_on_overflow(&mut self, on_overflow: &ListAggOnOverflow) -> Result<(), Error> {
        match on_overflow {
            ListAggOnOverflow::Error => Ok(()),
            ListAggOnOverflow::Truncate { filler, .. } => {
                if let Some(expr) = filler {
                    self.visit_expr(expr)?;
                }
                Ok(())
            }
        }
    }

    pub(super) fn visit_function_arg(&mut self, arg: &FunctionArg) -> Result<(), Error> {
        match arg {
            FunctionArg::Named { arg, .. } | FunctionArg::Unnamed(arg) => {
                self.visit_function_arg_expr(arg)
            }
            FunctionArg::ExprNamed { name, arg, .. } => {
                self.visit_expr(name)?;
                self.visit_function_arg_expr(arg)
            }
        }
    }

    fn visit_function_arg_expr(&mut self, arg: &FunctionArgExpr) -> Result<(), Error> {
        match arg {
            FunctionArgExpr::Expr(expr) => self.visit_expr(expr),
            FunctionArgExpr::QualifiedWildcard(_) | FunctionArgExpr::Wildcard => Ok(()),
        }
    }

    fn visit_access_expr(&mut self, access: &AccessExpr) -> Result<(), Error> {
        match access {
            AccessExpr::Dot(expr) => self.visit_expr(expr),
            AccessExpr::Subscript(subscript) => self.visit_subscript(subscript),
        }
    }

    fn visit_subscript(&mut self, subscript: &Subscript) -> Result<(), Error> {
        match subscript {
            Subscript::Index { index } => self.visit_expr(index),
            Subscript::Slice {
                lower_bound,
                upper_bound,
                stride,
            } => {
                for expr in [lower_bound.as_ref(), upper_bound.as_ref(), stride.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    self.visit_expr(expr)?;
                }
                Ok(())
            }
        }
    }

    fn visit_dictionary_field(&mut self, field: &DictionaryField) -> Result<(), Error> {
        self.visit_expr(&field.value)
    }

    fn visit_map(&mut self, map: &Map) -> Result<(), Error> {
        for entry in &map.entries {
            self.visit_expr(&entry.key)?;
            self.visit_expr(&entry.value)?;
        }
        Ok(())
    }

    fn visit_array(&mut self, array: &Array) -> Result<(), Error> {
        self.visit_exprs(&array.elem)
    }

    fn visit_window_type(&mut self, window_type: &WindowType) -> Result<(), Error> {
        match window_type {
            WindowType::WindowSpec(spec) => self.visit_window_spec(spec),
            WindowType::NamedWindow(_) => Ok(()),
        }
    }

    pub(super) fn visit_window_spec(&mut self, spec: &WindowSpec) -> Result<(), Error> {
        // OVER (...) — PARTITION BY / ORDER BY / frame-bound refs are
        // all walked as plain reads (no clause kind is recorded).
        self.visit_exprs(&spec.partition_by)?;
        for expr in &spec.order_by {
            self.visit_order_by_expr(expr)?;
        }
        if let Some(frame) = &spec.window_frame {
            self.visit_window_frame_bound(&frame.start_bound)?;
            if let Some(bound) = &frame.end_bound {
                self.visit_window_frame_bound(bound)?;
            }
        }
        Ok(())
    }

    fn visit_window_frame_bound(&mut self, bound: &WindowFrameBound) -> Result<(), Error> {
        match bound {
            WindowFrameBound::CurrentRow => Ok(()),
            WindowFrameBound::Preceding(Some(expr)) | WindowFrameBound::Following(Some(expr)) => {
                self.visit_expr(expr)
            }
            WindowFrameBound::Preceding(None) | WindowFrameBound::Following(None) => Ok(()),
        }
    }
    /// Visit a `TableWithJoins`. `role` applies only to the head relation;
    /// joined tables are always read-position (a write target makes no
    /// sense in a JOIN for any of our statement kinds).
    pub(super) fn visit_table_with_joins(
        &mut self,
        table: &TableWithJoins,
        role: TableRole,
    ) -> Result<(), Error> {
        self.visit_table_factor(&table.relation, role)?;
        for join in &table.joins {
            self.visit_join(join)?;
        }
        Ok(())
    }

    pub(super) fn visit_join(&mut self, join: &Join) -> Result<(), Error> {
        self.visit_table_factor(&join.relation, TableRole::Read)?;
        match &join.join_operator {
            JoinOperator::Join(constraint)
            | JoinOperator::Inner(constraint)
            | JoinOperator::Left(constraint)
            | JoinOperator::LeftOuter(constraint)
            | JoinOperator::Right(constraint)
            | JoinOperator::RightOuter(constraint)
            | JoinOperator::FullOuter(constraint)
            | JoinOperator::CrossJoin(constraint)
            | JoinOperator::Semi(constraint)
            | JoinOperator::LeftSemi(constraint)
            | JoinOperator::RightSemi(constraint)
            | JoinOperator::Anti(constraint)
            | JoinOperator::LeftAnti(constraint)
            | JoinOperator::RightAnti(constraint)
            | JoinOperator::StraightJoin(constraint) => self.visit_join_constraint(constraint),
            JoinOperator::AsOf {
                match_condition,
                constraint,
            } => {
                self.with_filter_clause(|r| r.visit_expr(match_condition))?;
                self.visit_join_constraint(constraint)
            }
            JoinOperator::CrossApply | JoinOperator::OuterApply => Ok(()),
        }
    }

    fn visit_join_constraint(&mut self, constraint: &JoinConstraint) -> Result<(), Error> {
        match constraint {
            JoinConstraint::On(expr) => self.with_filter_clause(|r| r.visit_expr(expr)),
            JoinConstraint::Using(_) | JoinConstraint::Natural | JoinConstraint::None => Ok(()),
        }
    }

    /// Visit a `TableFactor`. `role` is consumed only by the `Table`
    /// variant where it controls how the resulting binding is stamped;
    /// the other variants (Derived, NestedJoin, Pivot, ...) only bind
    /// aliases that are `DerivedTable` / `TableFunction` — they don't
    /// carry a table role.
    pub(super) fn visit_table_factor(
        &mut self,
        table_factor: &TableFactor,
        role: TableRole,
    ) -> Result<(), Error> {
        match table_factor {
            TableFactor::Table {
                name,
                alias,
                args,
                with_hints,
                sample,
                ..
            } => {
                if self.is_cte_reference(name) {
                    // Carry the original CTE's `output_columns` and
                    // `body_scope` to the local binding so:
                    //  1. lineage collapse works through the use site
                    //     (`FROM cte AS c` → `c.col` and `FROM cte` →
                    //     `cte.col` both collapse to the body's source);
                    //  2. catalog-aware strictness still applies — refs
                    //     against known columns that don't list the
                    //     name still surface as unresolved instead of
                    //     getting absorbed by the synthetic binding;
                    //  3. unqualified refs in the current scope have a
                    //     single in-scope candidate — without this
                    //     re-bind, bare refs in `WITH cte AS (...)
                    //     INSERT INTO t ... SELECT x FROM cte` would
                    //     walk up and ambify against the outer-bound
                    //     INSERT target.
                    let output = self.cte_output_columns(name);
                    let body_scope = self.cte_body_scope(name);
                    let bind_name = match alias {
                        Some(a) => a.name.clone(),
                        // `is_cte_reference` already returned true,
                        // so `name` is a single-segment ObjectName
                        // whose head is an Ident.
                        None => name.0[0].as_ident().cloned().unwrap(),
                    };
                    // `body_scope.unwrap()` is safe — `is_cte_reference`
                    // just confirmed the name resolves to a `Cte`, and a
                    // `Cte` binding always carries `body_scope`. The
                    // local re-bind shares the original CTE's body so
                    // table-lineage collapse reaches the same place
                    // from either definition or use site.
                    self.bind_cte(bind_name, output, body_scope.unwrap());
                    self.record_synthetic_table_ref(body_scope.unwrap());
                    return Ok(());
                }
                let (table, alias_ident) =
                    TableReference::from_table_factor_with_alias(table_factor)?;
                self.bind_real_table(table, alias_ident, role);
                if let Some(args) = args {
                    self.visit_table_function_args(&args.args)?;
                    if let Some(settings) = &args.settings {
                        for setting in settings {
                            self.visit_expr(&setting.value)?;
                        }
                    }
                }
                self.visit_exprs(with_hints)?;
                if let Some(sample) = sample {
                    self.visit_table_sample_kind(sample)?;
                }
            }
            TableFactor::Derived {
                subquery,
                alias,
                sample,
                ..
            } => {
                // Raw resolve_query — same rationale as CTE bodies:
                // the derived subquery's projection isn't a query
                // result on its own, and storing its projections on
                // the binding lets lineage collapse collapse
                // through the derived alias.
                //
                // TODO: the `lateral: bool` field is intentionally
                // ignored — the parent-chain walk-up always lets the
                // subquery see preceding FROM siblings, so non-LATERAL
                // refs that SQL would reject are silently resolved.
                // Safe for table-level lineage (the outer sibling is
                // already in `reads`), but at column granularity it
                // can mis-resolve a name that should surface as
                // `UnresolvedColumn`. Fix would mean hiding same-FROM
                // siblings under a non-LATERAL derived's scope while
                // keeping the enclosing SELECT visible — a forward
                // walk-up with sibling masking, not a flat parent
                // chain.
                let resolved = self.resolve_query(subquery)?;
                if let Some(alias) = alias {
                    let renames = &alias.columns;
                    let renamed = resolved.output_columns.map(|o| o.renamed(renames));
                    self.bind_derived_table(alias.name.clone(), renamed, Some(resolved.body_scope));
                    self.record_synthetic_table_ref(resolved.body_scope);
                }
                if let Some(sample) = sample {
                    self.visit_table_sample_kind(sample)?;
                }
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                self.visit_table_with_joins(table_with_joins, TableRole::Read)?;
                if let Some(alias) = alias {
                    // Wrapper alias — the inner tables are bound directly
                    // in the current scope (via visit_table_with_joins
                    // above), so they emit their own Real RawTableRefs.
                    // The alias itself doesn't drive collapse.
                    self.bind_derived_table(alias.name.clone(), None, None);
                }
            }
            TableFactor::Pivot {
                table,
                aggregate_functions,
                value_column,
                value_source,
                default_on_null,
                alias,
                ..
            } => {
                self.visit_table_factor(table, TableRole::Read)?;
                for expr in aggregate_functions {
                    self.visit_expr(&expr.expr)?;
                }
                self.visit_exprs(value_column)?;
                self.visit_pivot_value_source(value_source)?;
                if let Some(expr) = default_on_null {
                    self.visit_expr(expr)?;
                }
                if let Some(alias) = alias {
                    self.bind_derived_table(alias.name.clone(), None, None);
                }
            }
            TableFactor::Unpivot {
                table,
                value,
                columns,
                alias,
                ..
            } => {
                self.visit_table_factor(table, TableRole::Read)?;
                self.visit_expr(value)?;
                for expr in columns {
                    self.visit_expr(&expr.expr)?;
                }
                if let Some(alias) = alias {
                    self.bind_derived_table(alias.name.clone(), None, None);
                }
            }
            TableFactor::MatchRecognize {
                table,
                partition_by,
                order_by,
                measures,
                symbols,
                alias,
                ..
            } => {
                self.visit_table_factor(table, TableRole::Read)?;
                self.visit_exprs(partition_by)?;
                for order_by in order_by {
                    self.visit_order_by_expr(order_by)?;
                }
                for measure in measures {
                    self.visit_expr(&measure.expr)?;
                }
                for symbol in symbols {
                    self.visit_expr(&symbol.definition)?;
                }
                if let Some(alias) = alias {
                    self.bind_derived_table(alias.name.clone(), None, None);
                }
            }
            TableFactor::TableFunction { expr, alias } => {
                self.visit_expr(expr)?;
                if let Some(alias) = alias {
                    self.bind_table_function(alias.name.clone());
                }
            }
            TableFactor::Function { args, alias, .. } => {
                self.visit_table_function_args(args)?;
                if let Some(alias) = alias {
                    self.bind_table_function(alias.name.clone());
                }
            }
            TableFactor::UNNEST {
                alias, array_exprs, ..
            } => {
                self.visit_exprs(array_exprs)?;
                if let Some(alias) = alias {
                    self.bind_table_function(alias.name.clone());
                }
            }
            TableFactor::JsonTable {
                json_expr, alias, ..
            }
            | TableFactor::OpenJsonTable {
                json_expr, alias, ..
            } => {
                self.visit_expr(json_expr)?;
                if let Some(alias) = alias {
                    self.bind_table_function(alias.name.clone());
                }
            }
            TableFactor::XmlTable {
                row_expression,
                passing,
                alias,
                ..
            } => {
                self.visit_expr(row_expression)?;
                for argument in &passing.arguments {
                    self.visit_expr(&argument.expr)?;
                }
                if let Some(alias) = alias {
                    self.bind_table_function(alias.name.clone());
                }
            }
            TableFactor::SemanticView {
                dimensions,
                metrics,
                facts,
                where_clause,
                alias,
                ..
            } => {
                self.visit_exprs(dimensions)?;
                self.visit_exprs(metrics)?;
                self.visit_exprs(facts)?;
                if let Some(expr) = where_clause {
                    self.visit_expr(expr)?;
                }
                if let Some(alias) = alias {
                    self.bind_table_function(alias.name.clone());
                }
            }
        }
        Ok(())
    }

    fn visit_table_function_args(&mut self, args: &[FunctionArg]) -> Result<(), Error> {
        for arg in args {
            self.visit_function_arg(arg)?;
        }
        Ok(())
    }

    fn visit_table_sample_kind(&mut self, sample: &TableSampleKind) -> Result<(), Error> {
        match sample {
            TableSampleKind::BeforeTableAlias(sample)
            | TableSampleKind::AfterTableAlias(sample) => self.visit_table_sample(sample),
        }
    }

    pub(super) fn visit_table_sample(&mut self, sample: &TableSample) -> Result<(), Error> {
        if let Some(quantity) = &sample.quantity {
            self.visit_expr(&quantity.value)?;
        }
        if let Some(expr) = &sample.offset {
            self.visit_expr(expr)?;
        }
        Ok(())
    }

    pub(super) fn visit_pivot_value_source(
        &mut self,
        value_source: &PivotValueSource,
    ) -> Result<(), Error> {
        match value_source {
            PivotValueSource::List(values) => {
                for value in values {
                    self.visit_expr(&value.expr)?;
                }
                Ok(())
            }
            PivotValueSource::Any(order_by) => {
                for expr in order_by {
                    self.visit_order_by_expr(expr)?;
                }
                Ok(())
            }
            // PIVOT value subquery is an intermediate — raw resolve.
            PivotValueSource::Subquery(query) => self.resolve_query(query).map(|_| ()),
        }
    }
}

/// Rename each source operand's columns positionally to the INSERT
/// target's column names — the EXCLUDED pseudo-table exposes the
/// would-be-inserted row, so `EXCLUDED.<target_col>` should collapse
/// back to whatever expression feeds that position of the source.
/// Returns `None` when there are no source output columns (e.g.
/// `INSERT ... VALUES (...) ON CONFLICT ...`) or no effective target
/// columns, in which case `collapse_source` falls back to leaving
/// `EXCLUDED.<col>` as the lineage source.
fn excluded_body_output(
    effective_columns: &[Ident],
    source_output: Option<&BodyOutput>,
) -> Option<BodyOutput> {
    if effective_columns.is_empty() {
        return None;
    }
    let source = source_output?;
    let set_operands: Vec<SetOperand> = source
        .set_operands
        .iter()
        .map(|operand| SetOperand {
            columns: operand
                .columns
                .iter()
                .enumerate()
                .map(|(position, col)| {
                    let mut renamed = col.clone();
                    if let Some(name) = effective_columns.get(position) {
                        renamed.name = Some(name.clone());
                    }
                    renamed
                })
                .collect(),
        })
        .collect();
    Some(BodyOutput { set_operands })
}

fn from_table_items(from: &FromTable) -> &[TableWithJoins] {
    match from {
        FromTable::WithFromKeyword(items) | FromTable::WithoutKeyword(items) => items,
    }
}

/// Best-effort extraction of a write-target `TableReference` from a
/// `TableFactor`. Only the plain `TableFactor::Table` variant has a
/// resolvable identity; derived / pivot / table-function targets are
/// not valid SQL write targets and return `None`, leaving the caller's
/// assignment / pairing logic to fall back to qualifier-only target
/// derivation.
fn try_target_table_from_factor(factor: &sqlparser::ast::TableFactor) -> Option<TableReference> {
    matches!(factor, sqlparser::ast::TableFactor::Table { .. })
        .then(|| TableReference::try_from(factor).ok())
        .flatten()
}

fn assignment_target_parts(
    target: &sqlparser::ast::AssignmentTarget,
) -> Option<Vec<sqlparser::ast::Ident>> {
    match target {
        sqlparser::ast::AssignmentTarget::ColumnName(name) => name
            .0
            .iter()
            .map(|p| p.as_ident().cloned())
            .collect::<Option<Vec<_>>>(),
        sqlparser::ast::AssignmentTarget::Tuple(_) => None,
    }
}

/// Derive the owning `TableReference` for an UPDATE SET target.
/// `parts.len() == 1`: bare column, take the UPDATE head as default.
/// `parts.len() >= 2`: take the leading parts as catalog/schema/table.
fn assignment_target_table(
    parts: &[sqlparser::ast::Ident],
    default_table: Option<&TableReference>,
) -> Option<TableReference> {
    match parts.len() {
        0 => None,
        1 => default_table.cloned(),
        2 => Some(TableReference {
            catalog: None,
            schema: None,
            name: parts[0].clone(),
        }),
        3 => Some(TableReference {
            catalog: None,
            schema: Some(parts[0].clone()),
            name: parts[1].clone(),
        }),
        4 => Some(TableReference {
            catalog: Some(parts[0].clone()),
            schema: Some(parts[1].clone()),
            name: parts[2].clone(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnSchema;
    use crate::reference::TableReference;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;
    use std::collections::HashMap;

    #[derive(Debug, Default)]
    struct TestCatalog {
        tables: HashMap<String, Vec<&'static str>>,
    }

    impl TestCatalog {
        fn with(mut self, name: &str, cols: Vec<&'static str>) -> Self {
            self.tables.insert(name.to_string(), cols);
            self
        }
    }

    impl Catalog for TestCatalog {
        fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>> {
            self.tables.get(table.name.value.as_str()).map(|cols| {
                cols.iter()
                    .map(|c| ColumnSchema {
                        name: c.to_string(),
                    })
                    .collect()
            })
        }
    }

    fn resolve(sql: &str, catalog: Option<&dyn Catalog>) -> Resolution {
        let dialect = GenericDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        Resolver::resolve_statement(catalog, &statements[0]).unwrap()
    }

    fn first_table_columns(resolution: &Resolution) -> Option<&Option<Vec<sqlparser::ast::Ident>>> {
        resolution
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.values())
            .find_map(|binding| match binding {
                Binding::Table { output_columns, .. } => Some(output_columns),
                _ => None,
            })
    }

    #[test]
    fn catalog_hit_populates_table_columns() {
        let catalog = TestCatalog::default().with("users", vec!["id", "email"]);
        let resolution = resolve("SELECT * FROM users", Some(&catalog));
        match first_table_columns(&resolution) {
            Some(Some(cols)) => {
                assert_eq!(cols.len(), 2);
                assert_eq!(cols[0].value, "id");
                assert_eq!(cols[1].value, "email");
            }
            other => panic!("expected Some(Some(...)), got {:?}", other),
        }
    }

    #[test]
    fn catalog_miss_leaves_columns_unknown() {
        let catalog = TestCatalog::default();
        let resolution = resolve("SELECT * FROM users", Some(&catalog));
        assert!(matches!(first_table_columns(&resolution), Some(None)));
    }

    #[test]
    fn no_catalog_leaves_columns_unknown() {
        let resolution = resolve("SELECT * FROM users", None);
        assert!(matches!(first_table_columns(&resolution), Some(None)));
    }

    #[test]
    fn catalog_lookup_ignores_alias() {
        let catalog = TestCatalog::default().with("users", vec!["id"]);
        let resolution = resolve("SELECT * FROM users AS u", Some(&catalog));
        assert!(matches!(first_table_columns(&resolution), Some(Some(_))));
    }
}
