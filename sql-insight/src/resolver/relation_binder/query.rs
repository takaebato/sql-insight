use super::Binder;
use crate::error::Error;
use crate::relation::TableReference;
use sqlparser::ast::{
    ConnectByKind, Distinct, GroupByExpr, GroupByWithModifier, NamedWindowExpr, Query, Select,
    SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Table, TopQuantity, Values,
};

impl Binder {
    pub(super) fn bind_query(&mut self, query: &Query) -> Result<(), Error> {
        self.scopes.push_query_scope();
        if let Some(with) = &query.with {
            if with.recursive {
                for cte in &with.cte_tables {
                    self.bind_cte(cte.alias.name.clone());
                }
                for cte in &with.cte_tables {
                    self.bind_query(&cte.query)?;
                }
            } else {
                for cte in &with.cte_tables {
                    self.bind_query(&cte.query)?;
                    self.bind_cte(cte.alias.name.clone());
                }
            }
        }
        self.bind_set_expr(&query.body)?;
        if let Some(order_by) = &query.order_by {
            self.bind_order_by(order_by)?;
        }
        if let Some(limit_clause) = &query.limit_clause {
            self.bind_limit_clause(limit_clause)?;
        }
        if let Some(fetch) = &query.fetch {
            self.bind_fetch(fetch)?;
        }
        if let Some(settings) = &query.settings {
            for setting in settings {
                self.bind_expr(&setting.value)?;
            }
        }
        for pipe_operator in &query.pipe_operators {
            self.bind_pipe_operator(pipe_operator)?;
        }
        self.scopes.pop_scope();
        Ok(())
    }

    fn bind_set_expr(&mut self, set_expr: &SetExpr) -> Result<(), Error> {
        match set_expr {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(query) => self.bind_query(query),
            SetExpr::SetOperation { left, right, .. } => {
                self.bind_set_expr(left)?;
                self.bind_set_expr(right)
            }
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => self.bind_statement(statement),
            SetExpr::Table(table) => {
                self.bind_table_command(table);
                Ok(())
            }
            SetExpr::Values(values) => self.bind_values(values),
        }
    }

    fn bind_select(&mut self, select: &Select) -> Result<(), Error> {
        if let Some(Distinct::On(exprs)) = &select.distinct {
            self.bind_exprs(exprs)?;
        }
        if let Some(top) = &select.top {
            if let Some(TopQuantity::Expr(expr)) = &top.quantity {
                self.bind_expr(expr)?;
            }
        }
        for table in &select.from {
            self.bind_table_with_joins(table)?;
        }
        for item in &select.projection {
            self.bind_select_item(item)?;
        }
        if let Some(into) = &select.into {
            self.record_base_table(TableReference::try_from(&into.name)?);
        }
        for lateral_view in &select.lateral_views {
            self.bind_expr(&lateral_view.lateral_view)?;
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
            self.bind_expr(expr)?;
        }
        for connect_by in &select.connect_by {
            match connect_by {
                ConnectByKind::ConnectBy { relationships, .. } => {
                    self.bind_exprs(relationships)?;
                }
                ConnectByKind::StartWith { condition, .. } => {
                    self.bind_expr(condition)?;
                }
            }
        }
        self.bind_group_by(&select.group_by)?;
        self.bind_exprs(&select.cluster_by)?;
        self.bind_exprs(&select.distribute_by)?;
        for order_by in &select.sort_by {
            self.bind_order_by_expr(order_by)?;
        }
        for window in &select.named_window {
            if let NamedWindowExpr::WindowSpec(spec) = &window.1 {
                self.bind_window_spec(spec)?;
            }
        }
        Ok(())
    }

    pub(super) fn bind_select_item(&mut self, item: &SelectItem) -> Result<(), Error> {
        match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                self.bind_expr(expr)
            }
            SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::Expr(expr), _) => {
                self.bind_expr(expr)
            }
            SelectItem::QualifiedWildcard(
                SelectItemQualifiedWildcardKind::ObjectName(_),
                options,
            )
            | SelectItem::Wildcard(options) => self.bind_wildcard_options(options),
        }
    }

    fn bind_table_command(&mut self, table: &Table) {
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

    fn bind_values(&mut self, values: &Values) -> Result<(), Error> {
        for row in &values.rows {
            self.bind_exprs(row)?;
        }
        Ok(())
    }

    fn bind_group_by(&mut self, group_by: &GroupByExpr) -> Result<(), Error> {
        match group_by {
            GroupByExpr::All(modifiers) => self.bind_group_by_modifiers(modifiers),
            GroupByExpr::Expressions(exprs, modifiers) => {
                self.bind_exprs(exprs)?;
                self.bind_group_by_modifiers(modifiers)
            }
        }
    }

    fn bind_group_by_modifiers(&mut self, modifiers: &[GroupByWithModifier]) -> Result<(), Error> {
        for modifier in modifiers {
            if let GroupByWithModifier::GroupingSets(expr) = modifier {
                self.bind_expr(expr)?;
            }
        }
        Ok(())
    }
}
