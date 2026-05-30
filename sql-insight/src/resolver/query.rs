use super::projection::{projection_item_kind, projection_item_output_name};
use super::{BodyOutput, OutputColumn, ResolvedQuery, Resolver, ScopeId, TableRole};
use crate::error::Error;
use crate::reference::TableReference;
use sqlparser::ast::{
    ConnectByKind, Distinct, GroupByExpr, GroupByWithModifier, NamedWindowExpr, Query, Select,
    SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Table, TopQuantity, Values,
};

impl<'a> Resolver<'a> {
    pub(super) fn resolve_query(&mut self, query: &Query) -> Result<ResolvedQuery, Error> {
        // Swap in a fresh per-branch buffer for this query — restored
        // on return — so each ResolvedQuery owns exactly its own
        // branches without leaking into siblings or ancestors. This is
        // independent of the scope-arena push below: branches
        // accumulate into the resolver's own buffer, not into the scope.
        let prev_branches = std::mem::take(&mut self.current_branches);
        let body_scope = self.with_scope(|r| -> Result<ScopeId, Error> {
            let body_scope = r.scopes_mut().current_scope_id();
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
                        let renamed = resolved
                            .output_columns
                            .map(|o| super::rename_body_output(o, renames));
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
        let branches = std::mem::replace(&mut self.current_branches, prev_branches);
        let output_columns = if branches.is_empty() {
            None
        } else {
            Some(BodyOutput {
                per_branch: branches,
            })
        };
        Ok(ResolvedQuery {
            output_columns,
            body_scope,
        })
    }

    fn visit_set_expr(&mut self, set_expr: &SetExpr) -> Result<(), Error> {
        match set_expr {
            SetExpr::Select(select) => self.visit_select(select),
            SetExpr::Query(query) => {
                // Parenthesized continuation of the enclosing query —
                // bubble the inner branches up so an outer INSERT (or
                // any other caller) sees them as if they were inline.
                let resolved = self.resolve_query(query)?;
                if let Some(output) = resolved.output_columns {
                    self.extend_branches(output.per_branch);
                }
                Ok(())
            }
            SetExpr::SetOperation { left, right, .. } => {
                // Each branch lives in its own scope so name resolution
                // doesn't see sibling branches' FROM bindings — matching
                // SQL's per-SELECT name resolution. The branches' own
                // visit_select calls each contribute one branch entry
                // of output columns, so UNION INSERT naturally pairs
                // every branch with the same target columns.
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
        let mut branch_columns = Vec::with_capacity(select.projection.len());
        for item in &select.projection {
            branch_columns.push(self.build_output_column(item)?);
        }
        self.push_output_branch(branch_columns);
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
        let refs_before = self.column_refs_len();
        self.visit_select_item(item)?;
        let source_refs = self.column_refs_slice(refs_before).to_vec();
        Ok(OutputColumn {
            name: projection_item_output_name(item),
            source_refs,
            kind: projection_item_kind(item),
        })
    }

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
}
