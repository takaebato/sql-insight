use super::{Binder, RelationBinding};
use crate::error::Error;
use crate::relation::TableReference;
use sqlparser::ast::{
    FunctionArg, Join, JoinConstraint, JoinOperator, PivotValueSource, TableFactor, TableSample,
    TableSampleKind, TableWithJoins,
};

impl Binder {
    pub(super) fn bind_table_with_joins(&mut self, table: &TableWithJoins) -> Result<(), Error> {
        self.bind_table_factor(&table.relation)?;
        for join in &table.joins {
            self.bind_join(join)?;
        }
        Ok(())
    }

    pub(super) fn bind_join(&mut self, join: &Join) -> Result<(), Error> {
        self.bind_table_factor(&join.relation)?;
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
            | JoinOperator::StraightJoin(constraint) => self.bind_join_constraint(constraint),
            JoinOperator::AsOf {
                match_condition,
                constraint,
            } => {
                self.bind_expr(match_condition)?;
                self.bind_join_constraint(constraint)
            }
            JoinOperator::CrossApply | JoinOperator::OuterApply => Ok(()),
        }
    }

    fn bind_join_constraint(&mut self, constraint: &JoinConstraint) -> Result<(), Error> {
        match constraint {
            JoinConstraint::On(expr) => self.bind_expr(expr),
            JoinConstraint::Using(_) | JoinConstraint::Natural | JoinConstraint::None => Ok(()),
        }
    }

    pub(super) fn bind_table_factor(&mut self, table_factor: &TableFactor) -> Result<(), Error> {
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
                        self.bind_relation(alias.name.clone(), RelationBinding::Cte);
                    }
                    return Ok(());
                }
                let table = TableReference::try_from(table_factor)?;
                self.record_base_table(table);
                if let Some(args) = args {
                    self.bind_table_function_args(&args.args)?;
                    if let Some(settings) = &args.settings {
                        for setting in settings {
                            self.bind_expr(&setting.value)?;
                        }
                    }
                }
                self.bind_exprs(with_hints)?;
                if let Some(sample) = sample {
                    self.bind_table_sample_kind(sample)?;
                }
            }
            TableFactor::Derived {
                subquery,
                alias,
                sample,
                ..
            } => {
                self.bind_query(subquery)?;
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::DerivedTable);
                }
                if let Some(sample) = sample {
                    self.bind_table_sample_kind(sample)?;
                }
            }
            TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                self.bind_table_with_joins(table_with_joins)?;
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::DerivedTable);
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
                self.bind_table_factor(table)?;
                for expr in aggregate_functions {
                    self.bind_expr(&expr.expr)?;
                }
                self.bind_exprs(value_column)?;
                self.bind_pivot_value_source(value_source)?;
                if let Some(expr) = default_on_null {
                    self.bind_expr(expr)?;
                }
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::DerivedTable);
                }
            }
            TableFactor::Unpivot {
                table,
                value,
                columns,
                alias,
                ..
            } => {
                self.bind_table_factor(table)?;
                self.bind_expr(value)?;
                for expr in columns {
                    self.bind_expr(&expr.expr)?;
                }
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::DerivedTable);
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
                self.bind_table_factor(table)?;
                self.bind_exprs(partition_by)?;
                for order_by in order_by {
                    self.bind_order_by_expr(order_by)?;
                }
                for measure in measures {
                    self.bind_expr(&measure.expr)?;
                }
                for symbol in symbols {
                    self.bind_expr(&symbol.definition)?;
                }
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::DerivedTable);
                }
            }
            TableFactor::TableFunction { expr, alias } => {
                self.bind_expr(expr)?;
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::TableFunction);
                }
            }
            TableFactor::Function { args, alias, .. } => {
                self.bind_table_function_args(args)?;
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::TableFunction);
                }
            }
            TableFactor::UNNEST {
                alias, array_exprs, ..
            } => {
                self.bind_exprs(array_exprs)?;
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::TableFunction);
                }
            }
            TableFactor::JsonTable {
                json_expr, alias, ..
            }
            | TableFactor::OpenJsonTable {
                json_expr, alias, ..
            } => {
                self.bind_expr(json_expr)?;
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::TableFunction);
                }
            }
            TableFactor::XmlTable {
                row_expression,
                passing,
                alias,
                ..
            } => {
                self.bind_expr(row_expression)?;
                for argument in &passing.arguments {
                    self.bind_expr(&argument.expr)?;
                }
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::TableFunction);
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
                self.bind_exprs(dimensions)?;
                self.bind_exprs(metrics)?;
                self.bind_exprs(facts)?;
                if let Some(expr) = where_clause {
                    self.bind_expr(expr)?;
                }
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::TableFunction);
                }
            }
        }
        Ok(())
    }

    fn bind_table_function_args(&mut self, args: &[FunctionArg]) -> Result<(), Error> {
        for arg in args {
            self.bind_function_arg(arg)?;
        }
        Ok(())
    }

    fn bind_table_sample_kind(&mut self, sample: &TableSampleKind) -> Result<(), Error> {
        match sample {
            TableSampleKind::BeforeTableAlias(sample)
            | TableSampleKind::AfterTableAlias(sample) => self.bind_table_sample(sample),
        }
    }

    pub(super) fn bind_table_sample(&mut self, sample: &TableSample) -> Result<(), Error> {
        if let Some(quantity) = &sample.quantity {
            self.bind_expr(&quantity.value)?;
        }
        if let Some(expr) = &sample.offset {
            self.bind_expr(expr)?;
        }
        Ok(())
    }

    pub(super) fn bind_pivot_value_source(
        &mut self,
        value_source: &PivotValueSource,
    ) -> Result<(), Error> {
        match value_source {
            PivotValueSource::List(values) => {
                for value in values {
                    self.bind_expr(&value.expr)?;
                }
                Ok(())
            }
            PivotValueSource::Any(order_by) => {
                for expr in order_by {
                    self.bind_order_by_expr(expr)?;
                }
                Ok(())
            }
            PivotValueSource::Subquery(query) => self.bind_query(query),
        }
    }
}
