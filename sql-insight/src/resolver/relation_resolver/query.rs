use super::{
    Column, ProjectionGroup, ProjectionItem, RelationResolver, RelationSchema, ResolvedQuery,
    ScopeKind, TableRole,
};
use crate::error::Error;
use crate::relation::TableReference;
use sqlparser::ast::{
    ConnectByKind, Distinct, Expr, GroupByExpr, GroupByWithModifier, NamedWindowExpr, Query,
    Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Table, TopQuantity, Values,
};

impl<'a> RelationResolver<'a> {
    pub(super) fn resolve_query(&mut self, query: &Query) -> Result<ResolvedQuery, Error> {
        let scope_id = self.scopes.push_query_scope(self.pending_scope_kind);
        // Swap in a fresh projection buffer for this query — restored on
        // return — so each ResolvedQuery owns exactly its own groups
        // without leaking into siblings or ancestors.
        let prev_projections = std::mem::take(&mut self.current_projections);
        if let Some(with) = &query.with {
            if with.recursive {
                for cte in &with.cte_tables {
                    self.bind_cte(cte.alias.name.clone(), RelationSchema::Unknown);
                }
                for cte in &with.cte_tables {
                    // Body's output_schema is discarded for recursive CTEs;
                    // proper handling needs a fixpoint and is deferred.
                    self.resolve_query_emitting_query_output(&cte.query)?;
                }
            } else {
                for cte in &with.cte_tables {
                    let resolved = self.resolve_query_emitting_query_output(&cte.query)?;
                    self.bind_cte(cte.alias.name.clone(), resolved.output_schema);
                }
            }
        }
        let body_schema = self.visit_set_expr(&query.body)?;
        if let Some(order_by) = &query.order_by {
            self.visit_order_by(order_by)?;
        }
        if let Some(limit_clause) = &query.limit_clause {
            self.visit_limit_clause(limit_clause)?;
        }
        if let Some(fetch) = &query.fetch {
            self.visit_fetch(fetch)?;
        }
        if let Some(settings) = &query.settings {
            for setting in settings {
                self.visit_expr(&setting.value)?;
            }
        }
        for pipe_operator in &query.pipe_operators {
            self.visit_pipe_operator(pipe_operator)?;
        }
        self.scopes.pop_scope();
        let projections = std::mem::replace(&mut self.current_projections, prev_projections);
        Ok(ResolvedQuery {
            scope_id,
            output_schema: body_schema,
            projections,
        })
    }

    fn visit_set_expr(&mut self, set_expr: &SetExpr) -> Result<RelationSchema, Error> {
        match set_expr {
            SetExpr::Select(select) => self.visit_select(select),
            SetExpr::Query(query) => {
                // Parenthesized continuation of the enclosing query —
                // bubble the inner projections up so an outer INSERT (or
                // any other caller) sees them as if they were inline.
                let resolved = self.resolve_query(query)?;
                let output_schema = resolved.output_schema.clone();
                self.extend_projections(resolved.projections);
                Ok(output_schema)
            }
            SetExpr::SetOperation { left, right, .. } => {
                // Each branch lives in its own scope so name resolution
                // doesn't see sibling branches' FROM bindings — matching
                // SQL's per-SELECT name resolution. The branches' own
                // visit_select calls each contribute a ProjectionGroup,
                // so UNION INSERT naturally pairs every branch with the
                // same target columns. Result schema conventionally
                // follows the left side's column names.
                let left_schema = self.with_branch_scope(|r| r.visit_set_expr(left))?;
                self.with_branch_scope(|r| r.visit_set_expr(right))?;
                Ok(left_schema)
            }
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => {
                self.visit_statement(statement)?;
                Ok(RelationSchema::Unknown)
            }
            SetExpr::Table(table) => {
                self.visit_table_command(table);
                Ok(RelationSchema::Unknown)
            }
            SetExpr::Values(values) => {
                self.visit_values(values)?;
                Ok(RelationSchema::Unknown)
            }
        }
    }

    fn visit_select(&mut self, select: &Select) -> Result<RelationSchema, Error> {
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
        let mut projection_items = Vec::with_capacity(select.projection.len());
        for item in &select.projection {
            let refs_before = self.column_refs_len();
            self.visit_select_item(item)?;
            let source_refs = self.column_refs_slice(refs_before).to_vec();
            projection_items.push(ProjectionItem {
                name: projection_item_output_name(item),
                source_refs,
                bare: projection_item_is_bare(item),
            });
        }
        self.push_projection_group(ProjectionGroup {
            items: projection_items,
        });
        if let Some(into) = &select.into {
            // SELECT ... INTO new_table acts like CTAS — INTO is the write target.
            self.bind_base_table(
                TableReference::try_from(&into.name)?,
                None,
                TableRole::Write,
            );
        }
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
            self.with_scope_kind(ScopeKind::Predicate, |r| r.visit_expr(expr))?;
        }
        for connect_by in &select.connect_by {
            // CONNECT BY / START WITH are predicate-style hierarchical
            // join conditions (Oracle / Snowflake) — subqueries nested
            // here do not feed the enclosing write target.
            self.with_scope_kind(ScopeKind::Predicate, |r| match connect_by {
                ConnectByKind::ConnectBy { relationships, .. } => r.visit_exprs(relationships),
                ConnectByKind::StartWith { condition, .. } => r.visit_expr(condition),
            })?;
        }
        self.visit_group_by(&select.group_by)?;
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
        Ok(projection_schema(&select.projection))
    }

    pub(super) fn visit_select_item(&mut self, item: &SelectItem) -> Result<(), Error> {
        match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                self.visit_expr(expr)
            }
            SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::Expr(expr), _) => {
                self.visit_expr(expr)
            }
            SelectItem::QualifiedWildcard(
                SelectItemQualifiedWildcardKind::ObjectName(_),
                options,
            )
            | SelectItem::Wildcard(options) => self.visit_wildcard_options(options),
        }
    }

    fn visit_table_command(&mut self, table: &Table) {
        let Some(name) = &table.table_name else {
            return;
        };
        // `TABLE foo` is sugar for `SELECT * FROM foo` — foo is read.
        self.bind_base_table(
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

/// Derive an output `RelationSchema` from a `SELECT` projection, structurally only.
/// Wildcards and computed expressions fall back to `RelationSchema::Unknown`; that
/// gap is filled in later phases once catalog and in-scope relation schemas
/// can drive expansion.
fn projection_schema(projection: &[SelectItem]) -> RelationSchema {
    let mut columns = Vec::with_capacity(projection.len());
    for item in projection {
        match column_from_select_item(item) {
            Some(column) => columns.push(column),
            None => return RelationSchema::Unknown,
        }
    }
    RelationSchema::Known(columns)
}

fn column_from_select_item(item: &SelectItem) -> Option<Column> {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => Some(Column {
            name: alias.clone(),
        }),
        SelectItem::UnnamedExpr(expr) => column_from_expr(expr),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => None,
    }
}

fn column_from_expr(expr: &Expr) -> Option<Column> {
    match expr {
        Expr::Identifier(ident) => Some(Column {
            name: ident.clone(),
        }),
        Expr::CompoundIdentifier(parts) => parts.last().cloned().map(|name| Column { name }),
        _ => None,
    }
}

fn projection_item_output_name(item: &SelectItem) -> Option<sqlparser::ast::Ident> {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => Some(alias.clone()),
        SelectItem::UnnamedExpr(expr) => expr_inferred_name(expr),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => None,
    }
}

fn projection_item_is_bare(item: &SelectItem) -> bool {
    match item {
        SelectItem::ExprWithAlias { expr, .. } | SelectItem::UnnamedExpr(expr) => {
            expr_is_bare(expr)
        }
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => false,
    }
}

fn expr_inferred_name(expr: &Expr) -> Option<sqlparser::ast::Ident> {
    match expr {
        Expr::Identifier(ident) => Some(ident.clone()),
        Expr::CompoundIdentifier(parts) => parts.last().cloned(),
        _ => None,
    }
}

pub(super) fn expr_is_bare(expr: &Expr) -> bool {
    matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
}
