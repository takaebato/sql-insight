use super::{RelationResolver, RelationSchema, ScopeKind, TableRole};
use crate::error::Error;
use crate::relation::TableReference;
use sqlparser::ast::{
    FunctionArg, Join, JoinConstraint, JoinOperator, PivotValueSource, TableFactor, TableSample,
    TableSampleKind, TableWithJoins,
};

impl<'a> RelationResolver<'a> {
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
                self.with_scope_kind(ScopeKind::Predicate, |r| r.visit_expr(match_condition))?;
                self.visit_join_constraint(constraint)
            }
            JoinOperator::CrossApply | JoinOperator::OuterApply => Ok(()),
        }
    }

    fn visit_join_constraint(&mut self, constraint: &JoinConstraint) -> Result<(), Error> {
        match constraint {
            JoinConstraint::On(expr) => {
                self.with_scope_kind(ScopeKind::Predicate, |r| r.visit_expr(expr))
            }
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
                    if let Some(alias) = alias {
                        self.bind_cte(alias.name.clone(), RelationSchema::Unknown);
                    }
                    return Ok(());
                }
                let (table, alias_ident) =
                    TableReference::from_table_factor_with_alias(table_factor)?;
                self.bind_base_table(table, alias_ident, role);
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
                let resolved = self.resolve_query(subquery)?;
                if let Some(alias) = alias {
                    self.bind_derived_table(alias.name.clone(), resolved.output_schema);
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
                    self.bind_derived_table(alias.name.clone(), RelationSchema::Unknown);
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
                    self.bind_derived_table(alias.name.clone(), RelationSchema::Unknown);
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
                    self.bind_derived_table(alias.name.clone(), RelationSchema::Unknown);
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
                    self.bind_derived_table(alias.name.clone(), RelationSchema::Unknown);
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
            PivotValueSource::Subquery(query) => self.resolve_query(query).map(|_| ()),
        }
    }
}
