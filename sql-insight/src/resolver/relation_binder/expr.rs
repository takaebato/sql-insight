use super::{Binder, RelationBinding};
use crate::error::Error;
use sqlparser::ast::{
    AccessExpr, Array, DictionaryField, Expr, Fetch, Function, FunctionArg, FunctionArgExpr,
    FunctionArgumentClause, FunctionArgumentList, FunctionArguments, Interpolate, LimitClause,
    ListAggOnOverflow, Map, OrderBy, OrderByExpr, OrderByKind, PipeOperator, Subscript,
    WildcardAdditionalOptions, WindowFrameBound, WindowSpec, WindowType,
};

impl Binder {
    pub(super) fn bind_expr(&mut self, expr: &Expr) -> Result<(), Error> {
        // Keep this match exhaustive so sqlparser Expr additions are reviewed here.
        match expr {
            Expr::Subquery(query) => self.bind_query(query),
            Expr::Exists { subquery, .. } => self.bind_query(subquery),
            Expr::InSubquery { expr, subquery, .. } => {
                self.bind_expr(expr)?;
                self.bind_query(subquery)
            }
            Expr::BinaryOp { left, right, .. }
            | Expr::IsDistinctFrom(left, right)
            | Expr::IsNotDistinctFrom(left, right)
            | Expr::AnyOp { left, right, .. }
            | Expr::AllOp { left, right, .. } => {
                self.bind_expr(left)?;
                self.bind_expr(right)
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
            | Expr::Named { expr, .. } => self.bind_expr(expr),
            Expr::CompoundFieldAccess { root, access_chain } => {
                self.bind_expr(root)?;
                for access in access_chain {
                    self.bind_access_expr(access)?;
                }
                Ok(())
            }
            Expr::JsonAccess { value, .. } => self.bind_expr(value),
            Expr::InList { expr, list, .. } => {
                self.bind_expr(expr)?;
                for item in list {
                    self.bind_expr(item)?;
                }
                Ok(())
            }
            Expr::InUnnest {
                expr, array_expr, ..
            } => {
                self.bind_expr(expr)?;
                self.bind_expr(array_expr)
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.bind_expr(expr)?;
                self.bind_expr(low)?;
                self.bind_expr(high)
            }
            Expr::Like { expr, pattern, .. }
            | Expr::ILike { expr, pattern, .. }
            | Expr::SimilarTo { expr, pattern, .. }
            | Expr::RLike { expr, pattern, .. } => {
                self.bind_expr(expr)?;
                self.bind_expr(pattern)
            }
            Expr::Convert { expr, styles, .. } => {
                self.bind_expr(expr)?;
                for style in styles {
                    self.bind_expr(style)?;
                }
                Ok(())
            }
            Expr::AtTimeZone {
                timestamp,
                time_zone,
            } => {
                self.bind_expr(timestamp)?;
                self.bind_expr(time_zone)
            }
            Expr::Position { expr, r#in } => {
                self.bind_expr(expr)?;
                self.bind_expr(r#in)
            }
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                self.bind_expr(expr)?;
                if let Some(expr) = substring_from {
                    self.bind_expr(expr)?;
                }
                if let Some(expr) = substring_for {
                    self.bind_expr(expr)?;
                }
                Ok(())
            }
            Expr::Trim {
                expr,
                trim_what,
                trim_characters,
                ..
            } => {
                self.bind_expr(expr)?;
                if let Some(expr) = trim_what {
                    self.bind_expr(expr)?;
                }
                if let Some(exprs) = trim_characters {
                    for expr in exprs {
                        self.bind_expr(expr)?;
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
                self.bind_expr(expr)?;
                self.bind_expr(overlay_what)?;
                self.bind_expr(overlay_from)?;
                if let Some(expr) = overlay_for {
                    self.bind_expr(expr)?;
                }
                Ok(())
            }
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                if let Some(expr) = operand {
                    self.bind_expr(expr)?;
                }
                for condition in conditions {
                    self.bind_expr(&condition.condition)?;
                    self.bind_expr(&condition.result)?;
                }
                if let Some(expr) = else_result {
                    self.bind_expr(expr)?;
                }
                Ok(())
            }
            Expr::GroupingSets(exprs) | Expr::Cube(exprs) | Expr::Rollup(exprs) => {
                for group in exprs {
                    for expr in group {
                        self.bind_expr(expr)?;
                    }
                }
                Ok(())
            }
            Expr::Tuple(exprs) => {
                for expr in exprs {
                    self.bind_expr(expr)?;
                }
                Ok(())
            }
            Expr::Struct { values, .. } => {
                for expr in values {
                    self.bind_expr(expr)?;
                }
                Ok(())
            }
            Expr::Function(function) => self.bind_function(function),
            Expr::Dictionary(fields) => {
                for field in fields {
                    self.bind_dictionary_field(field)?;
                }
                Ok(())
            }
            Expr::Map(map) => self.bind_map(map),
            Expr::Array(array) => self.bind_array(array),
            Expr::Interval(interval) => self.bind_expr(&interval.value),
            Expr::Lambda(lambda) => self.bind_expr(&lambda.body),
            Expr::MemberOf(member_of) => {
                self.bind_expr(&member_of.value)?;
                self.bind_expr(&member_of.array)
            }
            Expr::Identifier(_)
            | Expr::CompoundIdentifier(_)
            | Expr::Value(_)
            | Expr::TypedString(_)
            | Expr::MatchAgainst { .. }
            | Expr::Wildcard(_)
            | Expr::QualifiedWildcard(_, _) => Ok(()),
        }
    }

    pub(super) fn bind_exprs(&mut self, exprs: &[Expr]) -> Result<(), Error> {
        for expr in exprs {
            self.bind_expr(expr)?;
        }
        Ok(())
    }

    pub(super) fn bind_order_by(&mut self, order_by: &OrderBy) -> Result<(), Error> {
        if let OrderByKind::Expressions(exprs) = &order_by.kind {
            for expr in exprs {
                self.bind_order_by_expr(expr)?;
            }
        }
        if let Some(interpolate) = &order_by.interpolate {
            self.bind_interpolate(interpolate)?;
        }
        Ok(())
    }

    pub(super) fn bind_order_by_expr(&mut self, order_by: &OrderByExpr) -> Result<(), Error> {
        self.bind_expr(&order_by.expr)?;
        if let Some(with_fill) = &order_by.with_fill {
            for expr in [
                with_fill.from.as_ref(),
                with_fill.to.as_ref(),
                with_fill.step.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                self.bind_expr(expr)?;
            }
        }
        Ok(())
    }

    fn bind_interpolate(&mut self, interpolate: &Interpolate) -> Result<(), Error> {
        if let Some(exprs) = &interpolate.exprs {
            for expr in exprs {
                if let Some(expr) = &expr.expr {
                    self.bind_expr(expr)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn bind_limit_clause(&mut self, limit_clause: &LimitClause) -> Result<(), Error> {
        match limit_clause {
            LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            } => {
                if let Some(expr) = limit {
                    self.bind_expr(expr)?;
                }
                if let Some(offset) = offset {
                    self.bind_expr(&offset.value)?;
                }
                self.bind_exprs(limit_by)
            }
            LimitClause::OffsetCommaLimit { offset, limit } => {
                self.bind_expr(offset)?;
                self.bind_expr(limit)
            }
        }
    }

    pub(super) fn bind_fetch(&mut self, fetch: &Fetch) -> Result<(), Error> {
        if let Some(expr) = &fetch.quantity {
            self.bind_expr(expr)?;
        }
        Ok(())
    }

    pub(super) fn bind_pipe_operator(&mut self, operator: &PipeOperator) -> Result<(), Error> {
        match operator {
            PipeOperator::Limit { expr, offset } => {
                self.bind_expr(expr)?;
                if let Some(expr) = offset {
                    self.bind_expr(expr)?;
                }
                Ok(())
            }
            PipeOperator::Where { expr } => self.bind_expr(expr),
            PipeOperator::OrderBy { exprs } => {
                for expr in exprs {
                    self.bind_order_by_expr(expr)?;
                }
                Ok(())
            }
            PipeOperator::Select { exprs } | PipeOperator::Extend { exprs } => {
                for expr in exprs {
                    self.bind_select_item(expr)?;
                }
                Ok(())
            }
            PipeOperator::Set { assignments } => {
                for assignment in assignments {
                    self.bind_expr(&assignment.value)?;
                }
                Ok(())
            }
            PipeOperator::Aggregate {
                full_table_exprs,
                group_by_expr,
            } => {
                for expr in full_table_exprs {
                    self.bind_expr(&expr.expr.expr)?;
                }
                for expr in group_by_expr {
                    self.bind_expr(&expr.expr.expr)?;
                }
                Ok(())
            }
            PipeOperator::TableSample { sample } => self.bind_table_sample(sample),
            PipeOperator::Union { queries, .. }
            | PipeOperator::Intersect { queries, .. }
            | PipeOperator::Except { queries, .. } => {
                for query in queries {
                    self.bind_query(query)?;
                }
                Ok(())
            }
            PipeOperator::Call { function, alias } => {
                self.bind_function(function)?;
                if let Some(alias) = alias {
                    self.bind_relation(alias.clone(), RelationBinding::TableFunction);
                }
                Ok(())
            }
            PipeOperator::Pivot {
                aggregate_functions,
                value_source,
                ..
            } => {
                for expr in aggregate_functions {
                    self.bind_expr(&expr.expr)?;
                }
                self.bind_pivot_value_source(value_source)
            }
            PipeOperator::Join(join) => self.bind_join(join),
            PipeOperator::Drop { .. }
            | PipeOperator::As { .. }
            | PipeOperator::Rename { .. }
            | PipeOperator::Unpivot { .. } => Ok(()),
        }
    }

    pub(super) fn bind_wildcard_options(
        &mut self,
        options: &WildcardAdditionalOptions,
    ) -> Result<(), Error> {
        if let Some(replace) = &options.opt_replace {
            for item in &replace.items {
                self.bind_expr(&item.expr)?;
            }
        }
        Ok(())
    }

    fn bind_function(&mut self, function: &Function) -> Result<(), Error> {
        self.bind_function_arguments(&function.parameters)?;
        self.bind_function_arguments(&function.args)?;
        if let Some(expr) = &function.filter {
            self.bind_expr(expr)?;
        }
        for expr in &function.within_group {
            self.bind_order_by_expr(expr)?;
        }
        if let Some(over) = &function.over {
            self.bind_window_type(over)?;
        }
        Ok(())
    }

    fn bind_function_arguments(&mut self, arguments: &FunctionArguments) -> Result<(), Error> {
        match arguments {
            FunctionArguments::None => Ok(()),
            FunctionArguments::Subquery(query) => self.bind_query(query),
            FunctionArguments::List(args) => self.bind_function_argument_list(args),
        }
    }

    fn bind_function_argument_list(&mut self, args: &FunctionArgumentList) -> Result<(), Error> {
        for arg in &args.args {
            self.bind_function_arg(arg)?;
        }
        for clause in &args.clauses {
            match clause {
                FunctionArgumentClause::OrderBy(order_by) => {
                    for order_by in order_by {
                        self.bind_order_by_expr(order_by)?;
                    }
                }
                FunctionArgumentClause::Limit(expr) => self.bind_expr(expr)?,
                FunctionArgumentClause::OnOverflow(on_overflow) => {
                    self.bind_list_agg_on_overflow(on_overflow)?
                }
                FunctionArgumentClause::Having(bound) => self.bind_expr(&bound.1)?,
                FunctionArgumentClause::IgnoreOrRespectNulls(_)
                | FunctionArgumentClause::Separator(_)
                | FunctionArgumentClause::JsonNullClause(_)
                | FunctionArgumentClause::JsonReturningClause(_) => {}
            }
        }
        Ok(())
    }

    fn bind_list_agg_on_overflow(&mut self, on_overflow: &ListAggOnOverflow) -> Result<(), Error> {
        match on_overflow {
            ListAggOnOverflow::Error => Ok(()),
            ListAggOnOverflow::Truncate { filler, .. } => {
                if let Some(expr) = filler {
                    self.bind_expr(expr)?;
                }
                Ok(())
            }
        }
    }

    pub(super) fn bind_function_arg(&mut self, arg: &FunctionArg) -> Result<(), Error> {
        match arg {
            FunctionArg::Named { arg, .. } | FunctionArg::Unnamed(arg) => {
                self.bind_function_arg_expr(arg)
            }
            FunctionArg::ExprNamed { name, arg, .. } => {
                self.bind_expr(name)?;
                self.bind_function_arg_expr(arg)
            }
        }
    }

    fn bind_function_arg_expr(&mut self, arg: &FunctionArgExpr) -> Result<(), Error> {
        match arg {
            FunctionArgExpr::Expr(expr) => self.bind_expr(expr),
            FunctionArgExpr::QualifiedWildcard(_) | FunctionArgExpr::Wildcard => Ok(()),
        }
    }

    fn bind_access_expr(&mut self, access: &AccessExpr) -> Result<(), Error> {
        match access {
            AccessExpr::Dot(expr) => self.bind_expr(expr),
            AccessExpr::Subscript(subscript) => self.bind_subscript(subscript),
        }
    }

    fn bind_subscript(&mut self, subscript: &Subscript) -> Result<(), Error> {
        match subscript {
            Subscript::Index { index } => self.bind_expr(index),
            Subscript::Slice {
                lower_bound,
                upper_bound,
                stride,
            } => {
                for expr in [lower_bound.as_ref(), upper_bound.as_ref(), stride.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    self.bind_expr(expr)?;
                }
                Ok(())
            }
        }
    }

    fn bind_dictionary_field(&mut self, field: &DictionaryField) -> Result<(), Error> {
        self.bind_expr(&field.value)
    }

    fn bind_map(&mut self, map: &Map) -> Result<(), Error> {
        for entry in &map.entries {
            self.bind_expr(&entry.key)?;
            self.bind_expr(&entry.value)?;
        }
        Ok(())
    }

    fn bind_array(&mut self, array: &Array) -> Result<(), Error> {
        self.bind_exprs(&array.elem)
    }

    fn bind_window_type(&mut self, window_type: &WindowType) -> Result<(), Error> {
        match window_type {
            WindowType::WindowSpec(spec) => self.bind_window_spec(spec),
            WindowType::NamedWindow(_) => Ok(()),
        }
    }

    pub(super) fn bind_window_spec(&mut self, spec: &WindowSpec) -> Result<(), Error> {
        self.bind_exprs(&spec.partition_by)?;
        for expr in &spec.order_by {
            self.bind_order_by_expr(expr)?;
        }
        if let Some(frame) = &spec.window_frame {
            self.bind_window_frame_bound(&frame.start_bound)?;
            if let Some(bound) = &frame.end_bound {
                self.bind_window_frame_bound(bound)?;
            }
        }
        Ok(())
    }

    fn bind_window_frame_bound(&mut self, bound: &WindowFrameBound) -> Result<(), Error> {
        match bound {
            WindowFrameBound::CurrentRow => Ok(()),
            WindowFrameBound::Preceding(Some(expr)) | WindowFrameBound::Following(Some(expr)) => {
                self.bind_expr(expr)
            }
            WindowFrameBound::Preceding(None) | WindowFrameBound::Following(None) => Ok(()),
        }
    }
}
