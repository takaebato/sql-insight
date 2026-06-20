//! Expression binding: a `sqlparser` `Expr` into the resolver's value/filter
//! [`Expr`], the SELECT-item / function / window machinery, and the
//! auxiliary clause read collectors (GROUP BY / ORDER BY / LIMIT / window).

use super::*;

impl<'a> Binder<'a> {
    pub(super) fn bind_select_item(&self, item: &SelectItem, scope: &Scope) -> Option<NamedExpr> {
        match item {
            SelectItem::UnnamedExpr(expr) => Some(NamedExpr {
                name: inferred_name(expr),
                expr: self.bind_expr(expr, scope),
            }),
            SelectItem::ExprWithAlias { expr, alias } => Some(NamedExpr {
                name: Some(alias.clone()),
                expr: self.bind_expr(expr, scope),
            }),
            // A wildcard isn't expanded (the rigor cost is too high for a
            // SQL-text-only library); record it so consumers know this
            // projection's column lineage is incomplete, and skip it.
            SelectItem::Wildcard(options) => {
                self.record_wildcard_suppressed("wildcard `*`", options.wildcard_token.0.span);
                None
            }
            SelectItem::QualifiedWildcard(kind, options) => {
                let description = match kind {
                    SelectItemQualifiedWildcardKind::Expr(_) => {
                        "qualified wildcard `(expr).*`".to_string()
                    }
                    SelectItemQualifiedWildcardKind::ObjectName(name) => {
                        format!("qualified wildcard `{name}.*`")
                    }
                };
                self.record_wildcard_suppressed(&description, options.wildcard_token.0.span);
                // `(expr).*` still projects its base expression as one
                // Transformation output (a structural field access); `alias.*`
                // has no inspectable base.
                match kind {
                    SelectItemQualifiedWildcardKind::Expr(expr) => Some(NamedExpr {
                        name: None,
                        expr: Expr::Call {
                            args: vec![self.bind_expr(expr, scope)],
                        },
                    }),
                    SelectItemQualifiedWildcardKind::ObjectName(_) => None,
                }
            }
        }
    }

    /// Resolve a `sqlparser` expression into a bound [`Expr`], mirroring the
    /// resolver's `collect_expr`. A bare column reference is the only
    /// [`Passthrough`](crate::extractor::ColumnLineageKind::Passthrough) shape
    /// ([`Expr::Column`]);
    /// every other construct is a transformation over its value operands
    /// ([`Expr::Call`]), with the predicate / row-positioning parts in a filter
    /// position ([`Expr::Filter`] / [`Expr::Case`] / [`Expr::Exists`] /
    /// [`Expr::InSubquery`]) so they read but never originate a value.
    pub(super) fn bind_expr(&self, expr: &SqlExpr, scope: &Scope) -> Expr {
        match expr {
            SqlExpr::Identifier(id) => self.resolve_expr(std::slice::from_ref(id), scope),
            SqlExpr::CompoundIdentifier(parts) => self.resolve_expr(parts, scope),
            // Both operands flow / filter with the surrounding position.
            SqlExpr::BinaryOp { left, right, .. }
            | SqlExpr::IsDistinctFrom(left, right)
            | SqlExpr::IsNotDistinctFrom(left, right) => {
                self.call([left.as_ref(), right.as_ref()], scope)
            }
            // ANY / ALL: the LHS keeps the surrounding position; the RHS is a
            // shape test (suppressed).
            SqlExpr::AnyOp { left, right, .. } | SqlExpr::AllOp { left, right, .. } => Expr::Call {
                args: vec![
                    self.bind_expr(left, scope),
                    self.suppress([right.as_ref()], scope),
                ],
            },
            // Single-operand transformations / unwrapped forms.
            SqlExpr::UnaryOp { expr, .. }
            | SqlExpr::Nested(expr)
            | SqlExpr::OuterJoin(expr)
            | SqlExpr::Prior(expr)
            | SqlExpr::IsFalse(expr)
            | SqlExpr::IsNotFalse(expr)
            | SqlExpr::IsTrue(expr)
            | SqlExpr::IsNotTrue(expr)
            | SqlExpr::IsNull(expr)
            | SqlExpr::IsNotNull(expr)
            | SqlExpr::IsUnknown(expr)
            | SqlExpr::IsNotUnknown(expr)
            | SqlExpr::Cast { expr, .. }
            | SqlExpr::IsNormalized { expr, .. }
            | SqlExpr::Extract { expr, .. }
            | SqlExpr::Ceil { expr, .. }
            | SqlExpr::Floor { expr, .. }
            | SqlExpr::Collate { expr, .. }
            | SqlExpr::Prefixed { value: expr, .. }
            | SqlExpr::Named { expr, .. }
            | SqlExpr::JsonAccess { value: expr, .. } => self.call([expr.as_ref()], scope),
            SqlExpr::CompoundFieldAccess { root, access_chain } => {
                let mut args = vec![self.bind_expr(root, scope)];
                for access in access_chain {
                    args.extend(self.bind_access(access, scope));
                }
                Expr::Call { args }
            }
            SqlExpr::InList { expr, list, .. } => Expr::Call {
                args: std::iter::once(self.bind_expr(expr, scope))
                    .chain(list.iter().map(|e| self.bind_expr(e, scope)))
                    .collect(),
            },
            SqlExpr::InUnnest {
                expr, array_expr, ..
            } => self.call([expr.as_ref(), array_expr.as_ref()], scope),
            SqlExpr::Between {
                expr, low, high, ..
            } => self.call([expr.as_ref(), low.as_ref(), high.as_ref()], scope),
            SqlExpr::Like { expr, pattern, .. }
            | SqlExpr::ILike { expr, pattern, .. }
            | SqlExpr::SimilarTo { expr, pattern, .. }
            | SqlExpr::RLike { expr, pattern, .. } => {
                self.call([expr.as_ref(), pattern.as_ref()], scope)
            }
            SqlExpr::Convert { expr, styles, .. } => Expr::Call {
                args: std::iter::once(self.bind_expr(expr, scope))
                    .chain(styles.iter().map(|e| self.bind_expr(e, scope)))
                    .collect(),
            },
            SqlExpr::AtTimeZone {
                timestamp,
                time_zone,
            } => self.call([timestamp.as_ref(), time_zone.as_ref()], scope),
            SqlExpr::Position { expr, r#in } => self.call([expr.as_ref(), r#in.as_ref()], scope),
            SqlExpr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => Expr::Call {
                args: std::iter::once(self.bind_expr(expr, scope))
                    .chain(
                        [substring_from, substring_for]
                            .into_iter()
                            .flatten()
                            .map(|e| self.bind_expr(e, scope)),
                    )
                    .collect(),
            },
            SqlExpr::Trim {
                expr,
                trim_what,
                trim_characters,
                ..
            } => {
                let mut args = vec![self.bind_expr(expr, scope)];
                args.extend(trim_what.iter().map(|e| self.bind_expr(e, scope)));
                if let Some(chars) = trim_characters {
                    args.extend(chars.iter().map(|e| self.bind_expr(e, scope)));
                }
                Expr::Call { args }
            }
            SqlExpr::Overlay {
                expr,
                overlay_what,
                overlay_from,
                overlay_for,
            } => {
                let mut args = vec![
                    self.bind_expr(expr, scope),
                    self.bind_expr(overlay_what, scope),
                    self.bind_expr(overlay_from, scope),
                ];
                args.extend(overlay_for.iter().map(|e| self.bind_expr(e, scope)));
                Expr::Call { args }
            }
            // CASE: the operand and WHEN conditions are predicates (filter);
            // the THEN / ELSE results are the values that flow.
            SqlExpr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                let mut when: Vec<Expr> =
                    operand.iter().map(|e| self.bind_expr(e, scope)).collect();
                let mut then = Vec::new();
                for condition in conditions {
                    when.push(self.bind_expr(&condition.condition, scope));
                    then.push(self.bind_expr(&condition.result, scope));
                }
                Expr::Case {
                    when,
                    then,
                    else_result: else_result
                        .as_ref()
                        .map(|e| Box::new(self.bind_expr(e, scope))),
                }
            }
            SqlExpr::Rollup(sets) | SqlExpr::Cube(sets) | SqlExpr::GroupingSets(sets) => {
                Expr::Call {
                    args: sets
                        .iter()
                        .flatten()
                        .map(|e| self.bind_expr(e, scope))
                        .collect(),
                }
            }
            SqlExpr::Tuple(exprs) | SqlExpr::Struct { values: exprs, .. } => Expr::Call {
                args: self.bind_exprs(exprs, scope),
            },
            SqlExpr::Function(function) => self.bind_function(function, scope),
            SqlExpr::Dictionary(fields) => Expr::Call {
                args: fields
                    .iter()
                    .map(|f| self.bind_expr(&f.value, scope))
                    .collect(),
            },
            SqlExpr::Map(map) => Expr::Call {
                args: map
                    .entries
                    .iter()
                    .flat_map(|e| {
                        [
                            self.bind_expr(&e.key, scope),
                            self.bind_expr(&e.value, scope),
                        ]
                    })
                    .collect(),
            },
            SqlExpr::Array(array) => Expr::Call {
                args: self.bind_exprs(&array.elem, scope),
            },
            SqlExpr::Interval(interval) => self.call([interval.value.as_ref()], scope),
            SqlExpr::Lambda(lambda) => self.call([lambda.body.as_ref()], scope),
            SqlExpr::MemberOf(member_of) => {
                self.call([member_of.value.as_ref(), member_of.array.as_ref()], scope)
            }
            // A scalar subquery (value position): its output flows in.
            SqlExpr::Subquery(query) => Expr::Subquery(Box::new(self.bind_subquery(query, scope))),
            // Tests (filter position): columns read, never an origin.
            SqlExpr::Exists { subquery, .. } => {
                Expr::Exists(Box::new(self.bind_subquery(subquery, scope)))
            }
            SqlExpr::InSubquery { expr, subquery, .. } => Expr::InSubquery {
                expr: Box::new(self.bind_expr(expr, scope)),
                subquery: Box::new(self.bind_subquery(subquery, scope)),
            },
            // Literals and forms with no column references.
            SqlExpr::Value(_)
            | SqlExpr::TypedString(_)
            | SqlExpr::MatchAgainst { .. }
            | SqlExpr::Wildcard(_)
            | SqlExpr::QualifiedWildcard(_, _) => Expr::Call { args: Vec::new() },
        }
    }

    /// Resolve a column reference into an `Expr`: an unqualified `JOIN … USING
    /// (col)` merge column with several owners fans in to all of them
    /// (`Expr::Fanin`); everything else is a single `Expr::Column`.
    pub(super) fn resolve_expr(&self, parts: &[Ident], scope: &Scope) -> Expr {
        if parts.len() == 1 {
            if let Some(fanin) = self.merge_fanin(&parts[0], scope) {
                return fanin;
            }
        }
        Expr::Column(Box::new(self.resolve(parts, scope)))
    }

    /// If `name` is a `USING` merge column with two or more owners in scope,
    /// the fan-in (one `Passthrough` ref per owning relation). A single owner
    /// falls through to normal resolution; a catalog narrows the owners to the
    /// relations that declare the column (`Cataloged`), catalog-free reaches
    /// every joined relation (`Inferred`).
    pub(super) fn merge_fanin(&self, name: &Ident, scope: &Scope) -> Option<Expr> {
        if !scope
            .merge_columns
            .iter()
            .any(|m| self.eq(self.casing.column, m, name))
        {
            return None;
        }
        let refs: Vec<BoundColumn> = scope
            .relations
            .iter()
            .filter_map(|rel| self.fanin_owner(rel, name))
            .collect();
        (refs.len() >= 2).then_some(Expr::Fanin(refs))
    }

    /// A relation's contribution to a merge-column fan-in: a real table owns
    /// the column if `Unknown` (catalog-free → `Inferred`) or its `Cataloged` schema
    /// lists it (`Cataloged`); a derived / function relation doesn't.
    pub(super) fn fanin_owner(&self, rel: &Relation, name: &Ident) -> Option<BoundColumn> {
        let (table, resolution) = match rel {
            Relation::Table {
                table,
                columns: Columns::Unknown,
                ..
            } => (table, ResolutionKind::Inferred),
            Relation::Table {
                table,
                columns: Columns::Cataloged(cols),
                ..
            } if self.list_has(cols, name) => (table, ResolutionKind::Cataloged),
            _ => return None,
        };
        Some(BoundColumn {
            qualifier: None,
            name: name.clone(),
            binding: base(table, resolution),
        })
    }

    /// A transformation over the given value operands (`Expr::Call`).
    pub(super) fn call<'e>(
        &self,
        exprs: impl IntoIterator<Item = &'e SqlExpr>,
        scope: &Scope,
    ) -> Expr {
        Expr::Call {
            args: exprs
                .into_iter()
                .map(|e| self.bind_expr(e, scope))
                .collect(),
        }
    }

    /// A filter-position bucket over the given operands (`Expr::Filter`): read
    /// but never a value origin.
    pub(super) fn suppress<'e>(
        &self,
        exprs: impl IntoIterator<Item = &'e SqlExpr>,
        scope: &Scope,
    ) -> Expr {
        Expr::Filter(
            exprs
                .into_iter()
                .map(|e| self.bind_expr(e, scope))
                .collect(),
        )
    }

    /// Bind a field-access step's value expressions (a `.field` / subscript
    /// index / slice bounds).
    pub(super) fn bind_access(&self, access: &AccessExpr, scope: &Scope) -> Vec<Expr> {
        match access {
            AccessExpr::Dot(expr) => vec![self.bind_expr(expr, scope)],
            AccessExpr::Subscript(Subscript::Index { index }) => vec![self.bind_expr(index, scope)],
            AccessExpr::Subscript(Subscript::Slice {
                lower_bound,
                upper_bound,
                stride,
            }) => [lower_bound, upper_bound, stride]
                .into_iter()
                .flatten()
                .map(|e| self.bind_expr(e, scope))
                .collect(),
        }
    }

    /// Bind a function call: the value arguments (parameters + args) plus the
    /// suppressed parts (an aggregate `FILTER` / `WITHIN GROUP`, a window
    /// `OVER (…)` spec's partition / order / frame keys — all row-positioning,
    /// never value sources) gathered into one [`Expr::Filter`].
    pub(super) fn bind_function(&self, function: &Function, scope: &Scope) -> Expr {
        let mut args = Vec::new();
        if let FunctionArguments::List(list) = &function.parameters {
            args.extend(self.bind_function_arg_list(&list.args, scope));
        }
        let mut suppressed = Vec::new();
        if let FunctionArguments::List(list) = &function.args {
            args.extend(self.bind_function_arg_list(&list.args, scope));
            for clause in &list.clauses {
                match clause {
                    FunctionArgumentClause::OrderBy(order_by) => {
                        suppressed.extend(order_by.iter().map(|o| self.bind_expr(&o.expr, scope)));
                    }
                    FunctionArgumentClause::Limit(expr) => {
                        suppressed.push(self.bind_expr(expr, scope));
                    }
                    FunctionArgumentClause::Having(bound) => {
                        suppressed.push(self.bind_expr(&bound.1, scope));
                    }
                    FunctionArgumentClause::OnOverflow(ListAggOnOverflow::Truncate {
                        filler: Some(filler),
                        ..
                    }) => args.push(self.bind_expr(filler, scope)),
                    FunctionArgumentClause::OnOverflow(_)
                    | FunctionArgumentClause::IgnoreOrRespectNulls(_)
                    | FunctionArgumentClause::Separator(_)
                    | FunctionArgumentClause::JsonNullClause(_)
                    | FunctionArgumentClause::JsonReturningClause(_) => {}
                }
            }
        } else if let FunctionArguments::Subquery(query) = &function.args {
            args.push(Expr::Subquery(Box::new(self.bind_subquery(query, scope))));
        }
        if let Some(filter) = &function.filter {
            suppressed.push(self.bind_expr(filter, scope));
        }
        suppressed.extend(
            function
                .within_group
                .iter()
                .map(|o| self.bind_expr(&o.expr, scope)),
        );
        // A window function `f(args) OVER (…)` is an `Expr::Window`: the value
        // arguments flow (a transformation), the PARTITION BY / ORDER BY keys
        // (+ frame bounds) and any FILTER / WITHIN GROUP are row-positioning
        // (filter — reads, never origins).
        match &function.over {
            Some(WindowType::WindowSpec(spec)) => {
                let partition = spec
                    .partition_by
                    .iter()
                    .map(|e| self.bind_expr(e, scope))
                    .collect();
                let mut order: Vec<Expr> = spec
                    .order_by
                    .iter()
                    .map(|o| self.bind_expr(&o.expr, scope))
                    .collect();
                if let Some(frame) = &spec.window_frame {
                    for bound in [Some(&frame.start_bound), frame.end_bound.as_ref()]
                        .into_iter()
                        .flatten()
                    {
                        if let WindowFrameBound::Preceding(Some(e))
                        | WindowFrameBound::Following(Some(e)) = bound
                        {
                            order.push(self.bind_expr(e, scope));
                        }
                    }
                }
                order.extend(suppressed);
                Expr::Window {
                    arg: Box::new(Expr::Call { args }),
                    partition,
                    order,
                }
            }
            // A named window `OVER w`: the spec is in the SELECT's WINDOW clause
            // (read there); here the arg is the value, no inline keys.
            Some(_) => Expr::Window {
                arg: Box::new(Expr::Call { args }),
                partition: Vec::new(),
                order: suppressed,
            },
            None => {
                if !suppressed.is_empty() {
                    args.push(Expr::Filter(suppressed));
                }
                Expr::Call { args }
            }
        }
    }

    /// Bind a subquery nested in an expression: its references resolve against
    /// its own FROM plus the containing scope's relations (pushed onto the
    /// correlation stack), so a correlated reference reaches outward.
    pub(super) fn bind_subquery(&self, query: &Query, scope: &Scope) -> LogicalPlan {
        self.with_outer(scope.relations.clone()).bind_query(query).0
    }

    pub(super) fn bind_function_args(&self, function: &Function, scope: &Scope) -> Vec<Expr> {
        match &function.args {
            FunctionArguments::List(list) => self.bind_function_arg_list(&list.args, scope),
            _ => Vec::new(),
        }
    }

    /// Bind a function-argument list's value expressions (dropping `*` and
    /// other non-expression args). Shared by scalar functions and table
    /// functions (`FROM f(args)`).
    pub(super) fn bind_function_arg_list(&self, args: &[FunctionArg], scope: &Scope) -> Vec<Expr> {
        args.iter()
            .filter_map(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                }
                | FunctionArg::ExprNamed {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                } => Some(self.bind_expr(e, scope)),
                _ => None,
            })
            .collect()
    }

    /// Bind several expressions against `scope`.
    pub(super) fn bind_exprs(&self, exprs: &[SqlExpr], scope: &Scope) -> Vec<Expr> {
        exprs.iter().map(|e| self.bind_expr(e, scope)).collect()
    }

    /// Filter-position reads from a SELECT's auxiliary clauses (`DISTINCT ON`
    /// keys, `TOP n`, Hive `LATERAL VIEW`, `PREWHERE`, `QUALIFY`, `CONNECT BY`
    /// / `START WITH`, `CLUSTER BY` / `DISTRIBUTE BY`, named `WINDOW` specs),
    /// resolved against the FROM scope. None feed values.
    pub(super) fn select_clause_reads(&self, select: &Select, scope: &Scope) -> Vec<Expr> {
        let mut reads = Vec::new();
        if let Some(Distinct::On(exprs)) = &select.distinct {
            reads.extend(self.bind_exprs(exprs, scope));
        }
        if let Some(top) = &select.top {
            if let Some(TopQuantity::Expr(expr)) = &top.quantity {
                reads.push(self.bind_expr(expr, scope));
            }
        }
        for lateral_view in &select.lateral_views {
            reads.push(self.bind_expr(&lateral_view.lateral_view, scope));
        }
        reads.extend(select.prewhere.iter().map(|e| self.bind_expr(e, scope)));
        reads.extend(select.qualify.iter().map(|e| self.bind_expr(e, scope)));
        for connect_by in &select.connect_by {
            match connect_by {
                ConnectByKind::ConnectBy { relationships, .. } => {
                    reads.extend(self.bind_exprs(relationships, scope));
                }
                ConnectByKind::StartWith { condition, .. } => {
                    reads.push(self.bind_expr(condition, scope));
                }
            }
        }
        for expr in select.cluster_by.iter().chain(&select.distribute_by) {
            reads.push(self.bind_expr(expr, scope));
        }
        for window in &select.named_window {
            if let NamedWindowExpr::WindowSpec(spec) = &window.1 {
                reads.extend(self.window_spec_reads(spec, scope));
            }
        }
        reads
    }

    /// A window `OVER (…)` spec's reads (PARTITION BY / ORDER BY keys + frame
    /// bounds) — all row-positioning, never value sources.
    pub(super) fn window_spec_reads(&self, spec: &WindowSpec, scope: &Scope) -> Vec<Expr> {
        let mut reads = self.bind_exprs(&spec.partition_by, scope);
        reads.extend(spec.order_by.iter().map(|o| self.bind_expr(&o.expr, scope)));
        if let Some(frame) = &spec.window_frame {
            for bound in [Some(&frame.start_bound), frame.end_bound.as_ref()]
                .into_iter()
                .flatten()
            {
                if let WindowFrameBound::Preceding(Some(e)) | WindowFrameBound::Following(Some(e)) =
                    bound
                {
                    reads.push(self.bind_expr(e, scope));
                }
            }
        }
        reads
    }

    /// Filter-position reads from a query's `LIMIT` / `OFFSET` / `LIMIT BY`.
    pub(super) fn limit_reads(&self, limit: &LimitClause, scope: &Scope) -> Vec<Expr> {
        let mut reads = Vec::new();
        match limit {
            LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            } => {
                reads.extend(
                    limit
                        .iter()
                        .chain(limit_by)
                        .map(|e| self.bind_expr(e, scope)),
                );
                reads.extend(offset.iter().map(|o| self.bind_expr(&o.value, scope)));
            }
            LimitClause::OffsetCommaLimit { offset, limit } => {
                reads.push(self.bind_expr(offset, scope));
                reads.push(self.bind_expr(limit, scope));
            }
        }
        reads
    }

    // ===== clauses (GROUP BY / HAVING / ORDER BY) ========================

    /// The GROUP BY key expressions (plain + GROUPING SETS members), resolved
    /// against the clause scope. These are reads, not lineage origins.
    pub(super) fn group_by_keys(&self, group_by: &GroupByExpr, scope: &Scope) -> Vec<Expr> {
        let mut keys = Vec::new();
        if let GroupByExpr::Expressions(exprs, modifiers) = group_by {
            let members = exprs.iter().chain(modifiers.iter().filter_map(|m| match m {
                GroupByWithModifier::GroupingSets(expr) => Some(expr),
                _ => None,
            }));
            for expr in members {
                keys.push(self.bind_expr(expr, scope));
            }
        }
        keys
    }

    /// The ORDER BY key expressions (a trailing `query.order_by`).
    pub(super) fn order_by_keys(&self, order_by: &OrderBy, scope: &Scope) -> Vec<Expr> {
        let OrderByKind::Expressions(exprs) = &order_by.kind else {
            return Vec::new();
        };
        self.order_by_expr_keys(exprs, scope)
    }

    /// Bind a list of order-by expressions (`query.order_by` members or a
    /// `SELECT … SORT BY` list) as clause reads.
    pub(super) fn order_by_expr_keys(&self, exprs: &[OrderByExpr], scope: &Scope) -> Vec<Expr> {
        exprs
            .iter()
            .map(|e| self.bind_expr(&e.expr, scope))
            .collect()
    }

    /// Summarise the projection outputs for clause-alias resolution. An output
    /// is *identity* iff it is a bare column reference whose output name equals
    /// that column's own name — `SELECT a` (and the redundant `SELECT a AS a`),
    /// but **not** a rename (`a AS x`) or a computed expr (`a + b AS s`).
    ///
    /// The test is name-*equality*, not alias *presence*: a redundant self-alias
    /// (`a AS a`) stays identity on purpose, so a clause reference like
    /// `GROUP BY a` falls through to the real `a` read. (Keying on "has an alias"
    /// would misclassify `a AS a` as introduced and resolve `GROUP BY a` to the
    /// output as `Binding::Derived`, silently dropping that read.)
    pub(super) fn output_cols(&self, exprs: &[NamedExpr]) -> Vec<OutputCol> {
        exprs
            .iter()
            .map(|ne| {
                let identity = match &ne.expr {
                    Expr::Column(c) => ne
                        .name
                        .as_ref()
                        .is_some_and(|n| self.eq(self.casing.column, n, &c.name)),
                    _ => false,
                };
                OutputCol {
                    name: ne.name.clone(),
                    identity,
                }
            })
            .collect()
    }
}
