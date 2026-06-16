use super::*;

impl Binder<'_> {
    /// Bind a value-producing expression (a projection item or a SET /
    /// VALUES assignment RHS) into a named output column, split by position
    /// (see [`BoundValue`]). The column's provenance is the value's lineage
    /// sources (direct refs + any nested value subquery's *output*); each
    /// source's composed kind folds in this expression's own kind (a bare
    /// column ref is `Passthrough`, anything else `Transformation`). Filter
    /// position inside the expression (a `CASE` condition, an `EXISTS` test)
    /// yields `filter_reads` / `filter_subplans` — reads that don't feed
    /// this value, so the caller routes them to a non-feeding position.
    pub(super) fn bind_value_column(
        &self,
        name: Option<Ident>,
        expr: &Expr,
        scope: &Scope,
    ) -> BoundValue {
        let outer = expr_kind(expr);
        let mut collector = ExprCollector::value();
        self.collect_expr(expr, scope, &mut collector);
        let provenance = collector
            .sources
            .into_iter()
            .map(|source| ProvenanceSource {
                kind: combine_kind(source.kind, outer),
                read: source.read,
                synthetic_origin: source.synthetic_origin,
            })
            .collect();
        BoundValue {
            column: BoundColumn { name, provenance },
            value_subplans: collector.value_subplans,
            filter_reads: collector.filter_reads,
            filter_subplans: collector.filter_subplans,
        }
    }

    /// The plain column reads of an expression (filter position — WHERE /
    /// ON / clause predicates / DML predicates) plus the sub-plans of any
    /// subqueries it contains. The whole expression is a predicate, so it
    /// collects in suppressed mode: every direct reference is a read, never
    /// a lineage source. Synthetic-origin references (through a derived /
    /// CTE relation, an output alias, or a nested subquery's output) are
    /// dropped — their physical reads are counted by walking the sub-plans.
    pub(super) fn expr_reads(&self, expr: &Expr, scope: &Scope) -> (Vec<ColumnRead>, Vec<Plan>) {
        let mut collector = ExprCollector::filter();
        self.collect_expr(expr, scope, &mut collector);
        collector.into_filter_parts()
    }

    /// Walk an expression, routing each column reference to the collector by
    /// position: a **value** reference (one whose value flows to the output)
    /// becomes a lineage `source`; a **filter** reference (a predicate that
    /// only influences *which* rows / values are produced — a `CASE`
    /// condition, an `EXISTS` / `IN` / `ANY` / `ALL` test, a window
    /// PARTITION / ORDER key, an aggregate `FILTER`) becomes a `filter_read`
    /// instead. The split mirrors the resolver's `suppress_lineage`. Nested
    /// subqueries are kept whole as `subplans` (walked for their own tables
    /// / reads); a scalar subquery's *output* additionally folds in as a
    /// synthetic-origin value source (unless it sits in a filter position).
    /// The match is exhaustive so new `Expr` variants are reviewed here.
    pub(super) fn collect_expr(&self, expr: &Expr, scope: &Scope, c: &mut ExprCollector) {
        match expr {
            Expr::Identifier(id) => self.emit_ref(std::slice::from_ref(id), scope, c),
            Expr::CompoundIdentifier(ids) => self.emit_ref(ids, scope, c),
            // Both operands flow / filter with the surrounding position.
            Expr::BinaryOp { left, right, .. }
            | Expr::IsDistinctFrom(left, right)
            | Expr::IsNotDistinctFrom(left, right) => {
                self.collect_expr(left, scope, c);
                self.collect_expr(right, scope, c);
            }
            // ANY / ALL: the LHS keeps the surrounding position; the RHS is
            // a shape test (its rows don't flow as values) → suppressed.
            Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
                self.collect_expr(left, scope, c);
                c.suppressed(|c| self.collect_expr(right, scope, c));
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
            | Expr::Named { expr, .. } => self.collect_expr(expr, scope, c),
            Expr::CompoundFieldAccess { root, access_chain } => {
                self.collect_expr(root, scope, c);
                for access in access_chain {
                    self.collect_access(access, scope, c);
                }
            }
            Expr::JsonAccess { value, .. } => self.collect_expr(value, scope, c),
            Expr::InList { expr, list, .. } => {
                self.collect_expr(expr, scope, c);
                for item in list {
                    self.collect_expr(item, scope, c);
                }
            }
            Expr::InUnnest {
                expr, array_expr, ..
            } => {
                self.collect_expr(expr, scope, c);
                self.collect_expr(array_expr, scope, c);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.collect_expr(expr, scope, c);
                self.collect_expr(low, scope, c);
                self.collect_expr(high, scope, c);
            }
            Expr::Like { expr, pattern, .. }
            | Expr::ILike { expr, pattern, .. }
            | Expr::SimilarTo { expr, pattern, .. }
            | Expr::RLike { expr, pattern, .. } => {
                self.collect_expr(expr, scope, c);
                self.collect_expr(pattern, scope, c);
            }
            Expr::Convert { expr, styles, .. } => {
                self.collect_expr(expr, scope, c);
                for style in styles {
                    self.collect_expr(style, scope, c);
                }
            }
            Expr::AtTimeZone {
                timestamp,
                time_zone,
            } => {
                self.collect_expr(timestamp, scope, c);
                self.collect_expr(time_zone, scope, c);
            }
            Expr::Position { expr, r#in } => {
                self.collect_expr(expr, scope, c);
                self.collect_expr(r#in, scope, c);
            }
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                self.collect_expr(expr, scope, c);
                for sub in [substring_from, substring_for].into_iter().flatten() {
                    self.collect_expr(sub, scope, c);
                }
            }
            Expr::Trim {
                expr,
                trim_what,
                trim_characters,
                ..
            } => {
                self.collect_expr(expr, scope, c);
                if let Some(trim_what) = trim_what {
                    self.collect_expr(trim_what, scope, c);
                }
                if let Some(exprs) = trim_characters {
                    for sub in exprs {
                        self.collect_expr(sub, scope, c);
                    }
                }
            }
            Expr::Overlay {
                expr,
                overlay_what,
                overlay_from,
                overlay_for,
            } => {
                self.collect_expr(expr, scope, c);
                self.collect_expr(overlay_what, scope, c);
                self.collect_expr(overlay_from, scope, c);
                if let Some(overlay_for) = overlay_for {
                    self.collect_expr(overlay_for, scope, c);
                }
            }
            // CASE: the operand and the WHEN conditions are predicates (they
            // select which result is produced) → suppressed; the THEN / ELSE
            // results are the values that flow → keep the surrounding position.
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                if let Some(operand) = operand {
                    c.suppressed(|c| self.collect_expr(operand, scope, c));
                }
                for condition in conditions {
                    c.suppressed(|c| self.collect_expr(&condition.condition, scope, c));
                    self.collect_expr(&condition.result, scope, c);
                }
                if let Some(else_result) = else_result {
                    self.collect_expr(else_result, scope, c);
                }
            }
            Expr::Rollup(sets) | Expr::Cube(sets) | Expr::GroupingSets(sets) => {
                for set in sets {
                    for expr in set {
                        self.collect_expr(expr, scope, c);
                    }
                }
            }
            Expr::Tuple(exprs) | Expr::Struct { values: exprs, .. } => {
                for expr in exprs {
                    self.collect_expr(expr, scope, c);
                }
            }
            Expr::Function(function) => self.collect_function(function, scope, c),
            Expr::Dictionary(fields) => {
                for field in fields {
                    self.collect_dictionary_field(field, scope, c);
                }
            }
            Expr::Map(map) => self.collect_map(map, scope, c),
            Expr::Array(array) => self.collect_array(array, scope, c),
            Expr::Interval(interval) => self.collect_expr(&interval.value, scope, c),
            Expr::Lambda(lambda) => self.collect_expr(&lambda.body, scope, c),
            Expr::MemberOf(member_of) => {
                self.collect_expr(&member_of.value, scope, c);
                self.collect_expr(&member_of.array, scope, c);
            }
            // A scalar subquery's value flows out → keep the surrounding
            // position; EXISTS is a boolean test → suppressed.
            Expr::Subquery(query) => self.collect_subquery(query, scope, c),
            Expr::Exists {
                subquery: query, ..
            } => c.suppressed(|c| self.collect_subquery(query, scope, c)),
            Expr::InSubquery { expr, subquery, .. } => {
                self.collect_expr(expr, scope, c);
                c.suppressed(|c| self.collect_subquery(subquery, scope, c));
            }
            Expr::Value(_)
            | Expr::TypedString(_)
            | Expr::MatchAgainst { .. }
            | Expr::Wildcard(_)
            | Expr::QualifiedWildcard(_, _) => {}
        }
    }

    /// Emit one column reference into the collector: a value position adds
    /// the resolved lineage sources; a filter position keeps only the
    /// non-synthetic reads (synthetic-origin references are counted at the
    /// inner producer by walking its sub-plan).
    pub(super) fn emit_ref(&self, parts: &[Ident], scope: &Scope, c: &mut ExprCollector) {
        let resolved = self.resolve_ref(parts, scope);
        if c.is_suppressed {
            c.filter_reads.extend(
                resolved
                    .into_iter()
                    .filter(|source| !source.synthetic_origin)
                    .map(|source| source.read),
            );
        } else {
            c.sources.extend(resolved);
        }
    }

    pub(super) fn collect_function(
        &self,
        function: &Function,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        self.collect_function_arguments(&function.parameters, scope, c);
        self.collect_function_arguments(&function.args, scope, c);
        // Aggregate `FILTER (WHERE …)` is a row-selection predicate — its
        // refs don't flow as values.
        if let Some(filter) = &function.filter {
            c.suppressed(|c| self.collect_expr(filter, scope, c));
        }
        for order_by in &function.within_group {
            self.collect_order_by_expr(order_by, scope, c);
        }
        if let Some(WindowType::WindowSpec(spec)) = &function.over {
            self.collect_window_spec(spec, scope, c);
        }
    }

    pub(super) fn collect_function_arguments(
        &self,
        arguments: &FunctionArguments,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        match arguments {
            FunctionArguments::None => {}
            FunctionArguments::Subquery(query) => self.collect_subquery(query, scope, c),
            FunctionArguments::List(list) => self.collect_function_argument_list(list, scope, c),
        }
    }

    pub(super) fn collect_function_argument_list(
        &self,
        list: &FunctionArgumentList,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        for arg in &list.args {
            self.collect_function_arg(arg, scope, c);
        }
        for clause in &list.clauses {
            match clause {
                FunctionArgumentClause::OrderBy(order_by) => {
                    for order_by in order_by {
                        self.collect_order_by_expr(order_by, scope, c);
                    }
                }
                // Row-count / predicate bounds inside aggregate args.
                FunctionArgumentClause::Limit(expr) => {
                    c.suppressed(|c| self.collect_expr(expr, scope, c))
                }
                FunctionArgumentClause::Having(bound) => {
                    c.suppressed(|c| self.collect_expr(&bound.1, scope, c))
                }
                FunctionArgumentClause::OnOverflow(ListAggOnOverflow::Truncate {
                    filler: Some(filler),
                    ..
                }) => self.collect_expr(filler, scope, c),
                FunctionArgumentClause::OnOverflow(_)
                | FunctionArgumentClause::IgnoreOrRespectNulls(_)
                | FunctionArgumentClause::Separator(_)
                | FunctionArgumentClause::JsonNullClause(_)
                | FunctionArgumentClause::JsonReturningClause(_) => {}
            }
        }
    }

    pub(super) fn collect_function_arg(
        &self,
        arg: &FunctionArg,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        match arg {
            FunctionArg::Named { arg, .. } | FunctionArg::Unnamed(arg) => {
                if let FunctionArgExpr::Expr(expr) = arg {
                    self.collect_expr(expr, scope, c);
                }
            }
            FunctionArg::ExprNamed { name, arg, .. } => {
                self.collect_expr(name, scope, c);
                if let FunctionArgExpr::Expr(expr) = arg {
                    self.collect_expr(expr, scope, c);
                }
            }
        }
    }

    pub(super) fn collect_access(&self, access: &AccessExpr, scope: &Scope, c: &mut ExprCollector) {
        match access {
            AccessExpr::Dot(expr) => self.collect_expr(expr, scope, c),
            AccessExpr::Subscript(subscript) => match subscript {
                Subscript::Index { index } => self.collect_expr(index, scope, c),
                Subscript::Slice {
                    lower_bound,
                    upper_bound,
                    stride,
                } => {
                    for expr in [lower_bound, upper_bound, stride].into_iter().flatten() {
                        self.collect_expr(expr, scope, c);
                    }
                }
            },
        }
    }

    pub(super) fn collect_dictionary_field(
        &self,
        field: &DictionaryField,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        self.collect_expr(&field.value, scope, c);
    }

    pub(super) fn collect_map(&self, map: &Map, scope: &Scope, c: &mut ExprCollector) {
        for entry in &map.entries {
            self.collect_expr(&entry.key, scope, c);
            self.collect_expr(&entry.value, scope, c);
        }
    }

    pub(super) fn collect_array(&self, array: &Array, scope: &Scope, c: &mut ExprCollector) {
        for expr in &array.elem {
            self.collect_expr(expr, scope, c);
        }
    }

    /// An ORDER BY expression in value-bearing position (window / WITHIN
    /// GROUP / aggregate ORDER BY): sort keys never flow as values, so the
    /// key and any WITH FILL bounds are suppressed.
    pub(super) fn collect_order_by_expr(
        &self,
        order_by: &OrderByExpr,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        c.suppressed(|c| {
            self.collect_expr(&order_by.expr, scope, c);
            if let Some(with_fill) = &order_by.with_fill {
                for expr in [&with_fill.from, &with_fill.to, &with_fill.step]
                    .into_iter()
                    .flatten()
                {
                    self.collect_expr(expr, scope, c);
                }
            }
        });
    }

    /// A window `OVER (…)` spec: PARTITION BY keys, ORDER BY keys, and frame
    /// bounds are all row-positioning, not value sources → suppressed.
    pub(super) fn collect_window_spec(
        &self,
        spec: &WindowSpec,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        c.suppressed(|c| {
            for expr in &spec.partition_by {
                self.collect_expr(expr, scope, c);
            }
        });
        for order_by in &spec.order_by {
            self.collect_order_by_expr(order_by, scope, c);
        }
        if let Some(frame) = &spec.window_frame {
            let mut bounds = vec![&frame.start_bound];
            bounds.extend(frame.end_bound.as_ref());
            for bound in bounds {
                if let WindowFrameBound::Preceding(Some(expr))
                | WindowFrameBound::Following(Some(expr)) = bound
                {
                    c.suppressed(|c| self.collect_expr(expr, scope, c));
                }
            }
        }
    }

    /// Bind a subquery nested in an expression (with the containing
    /// `scope`'s relations pushed onto the correlation stack, so a
    /// correlated reference resolves outward). The bound sub-plan is kept
    /// whole in `subplans` (so its tables / reads surface by walking it). In
    /// value position its output columns fold into `sources` as
    /// synthetic-origin lineage sources of the enclosing value; in a filter
    /// position (`EXISTS`, `IN`, an aggregate `FILTER`) the output is a test
    /// result, not a value, so only the sub-plan is kept.
    pub(super) fn collect_subquery(&self, query: &Query, scope: &Scope, c: &mut ExprCollector) {
        let binder = self.with_outer_scope(scope.relations.clone());
        let (plan, _) = binder.bind_query(query);
        if c.is_suppressed {
            // A predicate subquery (`EXISTS` / `IN` / a `CASE` condition):
            // its output doesn't feed the enclosing value, so it's a
            // non-feeding sub-plan (its tables / reads still surface).
            c.filter_subplans.push(plan);
        } else {
            // A value-position subquery (a scalar `(SELECT …)`): its output
            // folds into the enclosing value as a synthetic-origin source,
            // and the sub-plan feeds lineage.
            c.sources.extend(
                output_sources(&plan)
                    .into_iter()
                    .map(|source| ProvenanceSource {
                        synthetic_origin: true,
                        ..source
                    }),
            );
            c.value_subplans.push(plan);
        }
    }
}
