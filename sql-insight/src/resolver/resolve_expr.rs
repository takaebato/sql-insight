//! Walker for `Expr`: visit every sqlparser `Expr` variant, recording
//! `RawColumnRef`s at identifier sites and descending into nested
//! sub-expressions / sub-queries / function arguments.

use super::Resolver;
use crate::error::Error;
use sqlparser::ast::{
    AccessExpr, Array, DictionaryField, Expr, Fetch, Function, FunctionArg, FunctionArgExpr,
    FunctionArgumentClause, FunctionArgumentList, FunctionArguments, Interpolate, LimitClause,
    ListAggOnOverflow, Map, OrderBy, OrderByExpr, OrderByKind, PipeOperator, Subscript,
    WildcardAdditionalOptions, WindowFrameBound, WindowSpec, WindowType,
};

impl<'a> Resolver<'a> {
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
}
