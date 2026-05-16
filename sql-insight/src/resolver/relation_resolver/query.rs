use super::{Column, RelationResolver, ResolvedQuery, Schema};
use crate::error::Error;
use crate::relation::TableReference;
use sqlparser::ast::{
    ConnectByKind, Distinct, Expr, GroupByExpr, GroupByWithModifier, NamedWindowExpr, Query,
    Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Table, TopQuantity, Values,
};

impl RelationResolver {
    pub(super) fn resolve_query(&mut self, query: &Query) -> Result<ResolvedQuery, Error> {
        let scope_id = self.scopes.push_query_scope();
        if let Some(with) = &query.with {
            if with.recursive {
                for cte in &with.cte_tables {
                    self.bind_cte(cte.alias.name.clone(), Schema::Unknown);
                }
                for cte in &with.cte_tables {
                    // Body's output_schema is discarded for recursive CTEs;
                    // proper handling needs a fixpoint and is deferred.
                    self.resolve_query(&cte.query)?;
                }
            } else {
                for cte in &with.cte_tables {
                    let resolved = self.resolve_query(&cte.query)?;
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
        Ok(ResolvedQuery {
            scope_id,
            output_schema: body_schema,
        })
    }

    fn visit_set_expr(&mut self, set_expr: &SetExpr) -> Result<Schema, Error> {
        match set_expr {
            SetExpr::Select(select) => self.visit_select(select),
            SetExpr::Query(query) => self.resolve_query(query).map(|r| r.output_schema),
            SetExpr::SetOperation { left, right, .. } => {
                // Set ops require column-compatible operands; the result schema
                // conventionally follows the left side's column names.
                let left_schema = self.visit_set_expr(left)?;
                self.visit_set_expr(right)?;
                Ok(left_schema)
            }
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => {
                self.visit_statement(statement)?;
                Ok(Schema::Unknown)
            }
            SetExpr::Table(table) => {
                self.visit_table_command(table);
                Ok(Schema::Unknown)
            }
            SetExpr::Values(values) => {
                self.visit_values(values)?;
                Ok(Schema::Unknown)
            }
        }
    }

    fn visit_select(&mut self, select: &Select) -> Result<Schema, Error> {
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

/// Derive an output `Schema` from a `SELECT` projection, structurally only.
/// Wildcards and computed expressions fall back to `Schema::Unknown`; that
/// gap is filled in later phases once catalog and in-scope relation schemas
/// can drive expansion.
fn projection_schema(projection: &[SelectItem]) -> Schema {
    let mut columns = Vec::with_capacity(projection.len());
    for item in projection {
        match column_from_select_item(item) {
            Some(column) => columns.push(column),
            None => return Schema::Unknown,
        }
    }
    Schema::Known(columns)
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
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .cloned()
            .map(|name| Column { name }),
        _ => None,
    }
}
