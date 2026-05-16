use super::{RelationResolver, ResolvedQuery, Schema};
use crate::error::Error;
use crate::relation::TableReference;
use sqlparser::ast::{
    ConnectByKind, Distinct, GroupByExpr, GroupByWithModifier, NamedWindowExpr, Query, Select,
    SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Table, TopQuantity, Values,
};

impl RelationResolver {
    pub(super) fn resolve_query(&mut self, query: &Query) -> Result<ResolvedQuery, Error> {
        let scope_id = self.scopes.push_query_scope();
        if let Some(with) = &query.with {
            if with.recursive {
                for cte in &with.cte_tables {
                    self.bind_cte(cte.alias.name.clone());
                }
                for cte in &with.cte_tables {
                    self.resolve_query(&cte.query)?;
                }
            } else {
                for cte in &with.cte_tables {
                    self.resolve_query(&cte.query)?;
                    self.bind_cte(cte.alias.name.clone());
                }
            }
        }
        self.visit_set_expr(&query.body)?;
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
        Ok(ResolvedQuery {
            scope_id,
            output_schema: Schema::Unknown,
        })
    }

    fn visit_set_expr(&mut self, set_expr: &SetExpr) -> Result<(), Error> {
        match set_expr {
            SetExpr::Select(select) => self.visit_select(select),
            SetExpr::Query(query) => self.resolve_query(query).map(|_| ()),
            SetExpr::SetOperation { left, right, .. } => {
                self.visit_set_expr(left)?;
                self.visit_set_expr(right)
            }
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => self.visit_statement(statement),
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
            self.visit_table_with_joins(table)?;
        }
        for item in &select.projection {
            self.visit_select_item(item)?;
        }
        if let Some(into) = &select.into {
            self.record_base_table(TableReference::try_from(&into.name)?);
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
            self.visit_expr(expr)?;
        }
        for connect_by in &select.connect_by {
            match connect_by {
                ConnectByKind::ConnectBy { relationships, .. } => {
                    self.visit_exprs(relationships)?;
                }
                ConnectByKind::StartWith { condition, .. } => {
                    self.visit_expr(condition)?;
                }
            }
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
        Ok(())
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
        self.record_base_table(TableReference {
            catalog: None,
            schema: table
                .schema_name
                .as_ref()
                .map(|schema| schema.as_str().into()),
            name: name.as_str().into(),
            alias: None,
        });
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
