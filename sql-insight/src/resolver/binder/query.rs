use super::*;

impl Binder<'_> {
    /// Bind a query, returning the plan node and its output scope. A
    /// leading `WITH` is peeled first: each CTE binds in declaration order
    /// into an environment the later CTEs and the body resolve against. A
    /// `RECURSIVE` CTE also sees itself (via its anchor's columns) while
    /// its recursive branch binds.
    pub(super) fn bind_query(&self, query: &Query) -> (Plan, Scope) {
        let Some(with) = &query.with else {
            return self.bind_query_body(query);
        };
        let mut env = self.ctes.clone();
        // The bodies declared by *this* clause, kept to hang on the `With`
        // node so each is walked exactly once (inherited outer CTEs hang on
        // their own outer `With`, so they aren't re-attached here).
        let mut declared = Vec::new();
        for cte in &with.cte_tables {
            let (relation, plan) = if with.recursive {
                self.bind_recursive_cte(cte, &env)
            } else {
                let (plan, scope) = self.with_ctes(env.clone()).bind_query(&cte.query);
                let mut outputs = scope.outputs;
                apply_column_aliases(&mut outputs, &cte.alias);
                (
                    CteRelation {
                        name: cte.alias.name.clone(),
                        outputs,
                    },
                    plan,
                )
            };
            declared.push(CtePlan {
                name: relation.name.clone(),
                plan,
            });
            env.push(relation);
        }
        let (body, scope) = self.with_ctes(env).bind_query_body(query);
        (
            Plan::With(With {
                ctes: declared,
                body: Box::new(body),
            }),
            scope,
        )
    }

    /// Bind a `RECURSIVE` CTE: bind the anchor (the body set-operation's
    /// non-recursive left branch) to learn the output shape, register the
    /// CTE name with those columns so the recursive branch's
    /// self-reference resolves, then bind the full body. Self-reference
    /// resolution sees the anchor's columns (shallow — matching the
    /// resolver's deferred recursive collapse). A `RECURSIVE` CTE whose
    /// body isn't a set operation degenerates to a plain CTE.
    pub(super) fn bind_recursive_cte(&self, cte: &Cte, env: &[CteRelation]) -> (CteRelation, Plan) {
        let name = cte.alias.name.clone();
        let SetExpr::SetOperation { left, .. } = cte.query.body.as_ref() else {
            // A `RECURSIVE` CTE whose body isn't a set operation has no
            // separate anchor to learn columns from, but its name is still
            // in scope inside its own body — register it (with no known
            // columns) so a self-reference resolves to a `CteRef`, not a
            // phantom real table read.
            let mut provisional = env.to_vec();
            provisional.push(CteRelation {
                name: name.clone(),
                outputs: Vec::new(),
            });
            let (plan, scope) = self.with_ctes(provisional).bind_query(&cte.query);
            let mut outputs = scope.outputs;
            apply_column_aliases(&mut outputs, &cte.alias);
            return (CteRelation { name, outputs }, plan);
        };
        let mut anchor_outputs = self.with_ctes(env.to_vec()).bind_set_expr(left).1.outputs;
        apply_column_aliases(&mut anchor_outputs, &cte.alias);
        // Provisional registration: the recursive branch sees the CTE name
        // resolving to the anchor's columns (resolution consults only the
        // outputs; the self-reference binds to a `CteRef`, never a body).
        let mut provisional = env.to_vec();
        provisional.push(CteRelation {
            name: name.clone(),
            outputs: anchor_outputs,
        });
        let (plan, scope) = self.with_ctes(provisional).bind_query(&cte.query);
        let mut outputs = scope.outputs;
        apply_column_aliases(&mut outputs, &cte.alias);
        (CteRelation { name, outputs }, plan)
    }

    /// Bind a query's body and its trailing ORDER BY / LIMIT (the WITH
    /// clause is already in scope via `self.ctes`).
    pub(super) fn bind_query_body(&self, query: &Query) -> (Plan, Scope) {
        let (body, mut scope) = self.bind_set_expr(&query.body);
        // Pipe operators (`|> WHERE`, `|> SELECT`, …) transform the body in
        // sequence. Fold the chain on top of `node`, evolving the output
        // scope: an output-producing operator (SELECT / EXTEND / AGGREGATE
        // / SET) layers a `Project` so its value expressions feed
        // `QueryOutput` lineage, while a filter operator (WHERE / ORDER BY
        // / LIMIT / JOIN / …) adds reads. Pipe expressions resolve against
        // the body's relations plus the running outputs (relations stay in
        // scope across the chain — loose pipe scoping).
        let mut node = body;
        if !query.pipe_operators.is_empty() {
            let mut pipe_scope = match &node {
                // A set-op body exposes no single relation scope for refs.
                Plan::SetOp(_) => Scope {
                    relations: Vec::new(),
                    outputs: scope.outputs.clone(),
                    merge_columns: Vec::new(),
                },
                _ => scope.clone(),
            };
            for op in &query.pipe_operators {
                node = self.bind_pipe_operator(op, node, &mut pipe_scope);
            }
            scope = pipe_scope;
        }
        // A trailing ORDER BY / LIMIT sits above the (possibly piped) body
        // and sees its output aliases — but only for a single relation.
        // Over a set operation there's no single relation to resolve
        // against, so a reference is unresolved (an empty scope makes that
        // fall out).
        let clause_scope = match &node {
            Plan::SetOp(_) => Scope::empty(),
            _ => scope.clone(),
        };
        let mut reads = Vec::new();
        let mut subqueries = Vec::new();
        if let Some(order_by) = &query.order_by {
            let (r, s) = self.order_by_reads(order_by, &clause_scope);
            reads.extend(r);
            subqueries.extend(s);
        }
        // LIMIT / OFFSET / LIMIT BY are row-count bounds — filter reads.
        if let Some(limit) = &query.limit_clause {
            let (r, s) = self.limit_reads(limit, &clause_scope);
            reads.extend(r);
            subqueries.extend(s);
        }
        // ClickHouse `SETTINGS key = expr`: a value may hold a subquery.
        if let Some(settings) = &query.settings {
            let mut c = ExprCollector::filter();
            for setting in settings {
                self.collect_expr(&setting.value, &clause_scope, &mut c);
            }
            let (r, s) = c.into_filter_parts();
            reads.extend(r);
            subqueries.extend(s);
        }
        (wrap_reads(node, reads, subqueries), scope)
    }

    /// Bind one pipe operator (`|> …`) on top of `input`, returning the new
    /// plan node and updating `scope.outputs` when the operator reshapes the
    /// output. An **output-producing** operator (`SELECT` / `EXTEND` /
    /// `AGGREGATE` / `SET`) layers a [`Project`] whose value expressions feed
    /// `QueryOutput` lineage; a **filter** operator (`WHERE` / `ORDER BY` /
    /// `LIMIT` / `CALL` / `JOIN` / …) wraps a non-feeding read `PassThrough`.
    /// The match is exhaustive so new pipe operators are reviewed here.
    pub(super) fn bind_pipe_operator(
        &self,
        op: &PipeOperator,
        input: Plan,
        scope: &mut Scope,
    ) -> Plan {
        match op {
            // `|> SELECT exprs`: replace the output with these columns.
            PipeOperator::Select { exprs } => {
                let bound = exprs
                    .iter()
                    .filter_map(|i| self.bind_output_column(i, scope));
                let (node, outputs) = self.pipe_project(input, Vec::new(), bound);
                scope.outputs = outputs;
                node
            }
            // `|> EXTEND exprs`: append columns to the running output.
            PipeOperator::Extend { exprs } => {
                let base = scope.outputs.clone();
                let bound = exprs
                    .iter()
                    .filter_map(|i| self.bind_output_column(i, scope));
                let (node, outputs) = self.pipe_project(input, base, bound);
                scope.outputs = outputs;
                node
            }
            // `|> SET col = expr`: each assignment is a value column named by
            // its target, replacing a same-named output (or appended).
            PipeOperator::Set { assignments } => {
                let bound: Vec<BoundValue> = assignments
                    .iter()
                    .flat_map(|a| {
                        assignment_target_columns(&a.target)
                            .into_iter()
                            .map(move |col| (col, &a.value))
                    })
                    .map(|(col, value)| self.bind_value_column(Some(col), value, scope))
                    .collect();
                let (node, outputs) = self.pipe_set_project(input, scope.outputs.clone(), bound);
                scope.outputs = outputs;
                node
            }
            // `|> AGGREGATE aggs GROUP BY keys`: the output is the aggregate
            // expressions plus the grouping keys (both value position).
            PipeOperator::Aggregate {
                full_table_exprs,
                group_by_expr,
            } => {
                let bound = full_table_exprs.iter().chain(group_by_expr).map(|e| {
                    let name = e
                        .expr
                        .alias
                        .clone()
                        .or_else(|| inferred_output_name(&e.expr.expr));
                    self.bind_value_column(name, &e.expr.expr, scope)
                });
                let (node, outputs) = self.pipe_project(input, Vec::new(), bound);
                scope.outputs = outputs;
                node
            }
            // Filter operators below: reads only, output unchanged.
            PipeOperator::Limit { expr, offset } => self.pipe_filter(input, |c| {
                self.collect_expr(expr, scope, c);
                if let Some(offset) = offset {
                    self.collect_expr(offset, scope, c);
                }
            }),
            PipeOperator::Where { expr } => {
                self.pipe_filter(input, |c| self.collect_expr(expr, scope, c))
            }
            PipeOperator::OrderBy { exprs } => self.pipe_filter(input, |c| {
                for order_by in exprs {
                    self.collect_order_by_expr(order_by, scope, c);
                }
            }),
            PipeOperator::Call { function, .. } => {
                self.pipe_filter(input, |c| self.collect_function(function, scope, c))
            }
            PipeOperator::Pivot {
                aggregate_functions,
                value_source,
                ..
            } => self.pipe_filter(input, |c| {
                for expr in aggregate_functions {
                    self.collect_expr(&expr.expr, scope, c);
                }
                self.collect_pivot_value_source(value_source, scope, c);
            }),
            PipeOperator::Union { queries, .. }
            | PipeOperator::Intersect { queries, .. }
            | PipeOperator::Except { queries, .. } => self.pipe_filter(input, |c| {
                for query in queries {
                    self.collect_subquery(query, scope, c);
                }
            }),
            // `|> JOIN t ON …` introduces another relation: bind it as a
            // read scan (a non-feeding sub-plan so its table surfaces) and
            // collect the ON predicate's reads.
            PipeOperator::Join(join) => self.pipe_filter(input, |c| {
                let (plan, _) = self.bind_table_factor(&join.relation, scope);
                c.filter_subplans.push(plan);
                if let Some(JoinConstraint::On(expr)) = join_constraint(join) {
                    self.collect_expr(expr, scope, c);
                }
            }),
            // No inspectable column expressions (or a later refinement):
            // a sampling clause, a rename / drop / unpivot.
            PipeOperator::TableSample { .. }
            | PipeOperator::Drop { .. }
            | PipeOperator::As { .. }
            | PipeOperator::Rename { .. }
            | PipeOperator::Unpivot { .. } => input,
        }
    }

    /// Wrap `input` in a non-feeding read `PassThrough` carrying whatever a
    /// filter pipe operator collected (reads + predicate sub-plans).
    pub(super) fn pipe_filter(
        &self,
        input: Plan,
        collect: impl FnOnce(&mut ExprCollector),
    ) -> Plan {
        let mut c = ExprCollector::filter();
        collect(&mut c);
        let (reads, subplans) = c.into_filter_parts();
        wrap_reads(input, reads, subplans)
    }

    /// Build the [`Project`] for an output-producing pipe operator: append
    /// the `bound` value columns to `base` (empty when the operator replaces
    /// the output, the running outputs when it extends them). Value
    /// sub-plans feed lineage; filter reads / sub-plans ride a non-feeding
    /// `PassThrough` below the projection.
    pub(super) fn pipe_project(
        &self,
        input: Plan,
        mut outputs: Vec<BoundColumn>,
        bound: impl Iterator<Item = BoundValue>,
    ) -> (Plan, Vec<BoundColumn>) {
        let mut value_subqueries = Vec::new();
        let mut filter_reads = Vec::new();
        let mut filter_subqueries = Vec::new();
        for b in bound {
            outputs.push(b.column);
            value_subqueries.extend(b.value_subplans);
            filter_reads.extend(b.filter_reads);
            filter_subqueries.extend(b.filter_subplans);
        }
        let project = Plan::Project(Project {
            input: Box::new(wrap_reads(input, filter_reads, filter_subqueries)),
            outputs: outputs.clone(),
            subqueries: value_subqueries,
        });
        (project, outputs)
    }

    /// Like [`pipe_project`](Self::pipe_project) but for `|> SET`: each bound
    /// column replaces a same-named output in `base` (or is appended), so a
    /// `SET` after a `SELECT` rewrites that column in place rather than
    /// duplicating it.
    pub(super) fn pipe_set_project(
        &self,
        input: Plan,
        mut outputs: Vec<BoundColumn>,
        bound: Vec<BoundValue>,
    ) -> (Plan, Vec<BoundColumn>) {
        let mut value_subqueries = Vec::new();
        let mut filter_reads = Vec::new();
        let mut filter_subqueries = Vec::new();
        for b in bound {
            value_subqueries.extend(b.value_subplans);
            filter_reads.extend(b.filter_reads);
            filter_subqueries.extend(b.filter_subplans);
            let slot = b.column.name.as_ref().and_then(|name| {
                outputs
                    .iter_mut()
                    .find(|o| o.name.as_ref().is_some_and(|n| n.value == name.value))
            });
            match slot {
                Some(existing) => *existing = b.column,
                None => outputs.push(b.column),
            }
        }
        let project = Plan::Project(Project {
            input: Box::new(wrap_reads(input, filter_reads, filter_subqueries)),
            outputs: outputs.clone(),
            subqueries: value_subqueries,
        });
        (project, outputs)
    }

    /// Filter-position reads from a `LIMIT` / `OFFSET` / `LIMIT BY` clause
    /// (row-count bounds — never value sources).
    pub(super) fn limit_reads(
        &self,
        limit: &LimitClause,
        scope: &Scope,
    ) -> (Vec<ColumnRead>, Vec<Plan>) {
        let mut c = ExprCollector::filter();
        match limit {
            LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            } => {
                for expr in limit.iter().chain(limit_by) {
                    self.collect_expr(expr, scope, &mut c);
                }
                if let Some(offset) = offset {
                    self.collect_expr(&offset.value, scope, &mut c);
                }
            }
            LimitClause::OffsetCommaLimit { offset, limit } => {
                self.collect_expr(offset, scope, &mut c);
                self.collect_expr(limit, scope, &mut c);
            }
        }
        c.into_filter_parts()
    }

    /// Bind a query body's set expression: a leaf `SELECT`, a
    /// parenthesized inner query, a set operation (`UNION` / `INTERSECT` /
    /// `EXCEPT`), or a DML body. A set operation fans its operands into a
    /// [`SetOp`] and merges their outputs positionally — each result
    /// column unions the branches' provenance (so a derived / CTE over a
    /// `UNION` traces to every branch's base columns), taking its name
    /// from the left branch. The set-operation kind itself doesn't change
    /// lineage, so it's dropped. A leading statement-level `WITH` parses as
    /// a `Query` whose body is the DML (`WITH … INSERT/UPDATE …`), so those
    /// bodies bind through here to their `Write`-rooted tree.
    pub(super) fn bind_set_expr(&self, set_expr: &SetExpr) -> (Plan, Scope) {
        match set_expr {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(inner) => self.bind_query(inner),
            // `WITH … INSERT/UPDATE/DELETE/MERGE …`: the DML statement is
            // the query body. Bind it to its `Write` tree; it exposes no
            // output scope to an enclosing query.
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => (
                self.bind_statement(statement).unwrap_or(Plan::OpaqueLeaf),
                Scope::empty(),
            ),
            // `VALUES (…), (…)`: a literal row set. Each column position is
            // an output whose provenance unions that position's row
            // expressions, so a `(VALUES …) AS v(x)` exposes resolvable
            // columns (literals collapse to nothing; a row expression
            // referencing an outer relation surfaces it).
            SetExpr::Values(values) => self.bind_values(values),
            SetExpr::SetOperation { left, right, .. } => {
                let (left_plan, left_scope) = self.bind_set_expr(left);
                let (right_plan, right_scope) = self.bind_set_expr(right);
                let outputs = merge_set_outputs(left_scope.outputs, right_scope.outputs);
                (
                    Plan::SetOp(SetOp {
                        operands: vec![left_plan, right_plan],
                    }),
                    Scope {
                        relations: Vec::new(),
                        outputs,
                        merge_columns: Vec::new(),
                    },
                )
            }
            // `TABLE foo` / `TABLE schema.foo`: a whole-table query body
            // (e.g. the source of `CREATE TABLE t AS TABLE foo`). Bind it as
            // a read scan so the table surfaces.
            SetExpr::Table(table) => match table_set_expr_ref(table) {
                Some(written) => self.bind_named_table(&written, None),
                None => (Plan::OpaqueLeaf, Scope::empty()),
            },
        }
    }

    /// Bind a `VALUES (…), (…)` row set into a column-defining `Project`:
    /// one opaque output per column position (no provenance — the produced
    /// row is synthesized, so a reference to it collapses to a synthetic
    /// self-source, never to a row expression). The row expressions
    /// themselves are reads, resolved against the empty current scope and
    /// falling through to the correlation stack — a `(VALUES (t.a)) AS v`
    /// reads the enclosing / sibling `t.a` like a derived subquery's body.
    pub(super) fn bind_values(&self, values: &Values) -> (Plan, Scope) {
        let width = values.rows.iter().map(Vec::len).max().unwrap_or(0);
        let outputs: Vec<BoundColumn> = (0..width)
            .map(|_| BoundColumn {
                name: None,
                provenance: Vec::new(),
            })
            .collect();
        let mut reads = Vec::new();
        let mut subplans = Vec::new();
        for expr in values.rows.iter().flatten() {
            let (r, s) = self.expr_reads(expr, &Scope::empty());
            reads.extend(r);
            subplans.extend(s);
        }
        let plan = Plan::Project(Project {
            input: Box::new(wrap_reads(Plan::OpaqueLeaf, reads, subplans)),
            outputs: outputs.clone(),
            subqueries: Vec::new(),
        });
        (
            plan,
            Scope {
                relations: Vec::new(),
                outputs,
                merge_columns: Vec::new(),
            },
        )
    }

    /// Filter-position reads from a SELECT's auxiliary clauses, resolved
    /// against the FROM scope: `DISTINCT ON` keys, `TOP n`, Hive `LATERAL
    /// VIEW` generators, `PREWHERE`, `QUALIFY`, `CONNECT BY` / `START WITH`,
    /// `CLUSTER BY` / `DISTRIBUTE BY`, and named `WINDOW` specs. None feed
    /// values — they are all reads, never lineage sources.
    pub(super) fn select_clause_reads(
        &self,
        select: &Select,
        scope: &Scope,
    ) -> (Vec<ColumnRead>, Vec<Plan>) {
        let mut c = ExprCollector::filter();
        if let Some(Distinct::On(exprs)) = &select.distinct {
            for expr in exprs {
                self.collect_expr(expr, scope, &mut c);
            }
        }
        if let Some(top) = &select.top {
            if let Some(TopQuantity::Expr(expr)) = &top.quantity {
                self.collect_expr(expr, scope, &mut c);
            }
        }
        for lateral_view in &select.lateral_views {
            self.collect_expr(&lateral_view.lateral_view, scope, &mut c);
        }
        if let Some(expr) = &select.prewhere {
            self.collect_expr(expr, scope, &mut c);
        }
        if let Some(expr) = &select.qualify {
            self.collect_expr(expr, scope, &mut c);
        }
        for connect_by in &select.connect_by {
            match connect_by {
                ConnectByKind::ConnectBy { relationships, .. } => {
                    for expr in relationships {
                        self.collect_expr(expr, scope, &mut c);
                    }
                }
                ConnectByKind::StartWith { condition, .. } => {
                    self.collect_expr(condition, scope, &mut c)
                }
            }
        }
        for expr in select.cluster_by.iter().chain(&select.distribute_by) {
            self.collect_expr(expr, scope, &mut c);
        }
        for window in &select.named_window {
            if let NamedWindowExpr::WindowSpec(spec) = &window.1 {
                self.collect_window_spec(spec, scope, &mut c);
            }
        }
        c.into_filter_parts()
    }

    pub(super) fn bind_select(&self, select: &Select) -> (Plan, Scope) {
        let (from, from_scope) = self.bind_from(&select.from);
        // The WHERE-family clauses wrap the FROM in a filter PassThrough:
        // they resolve against the FROM scope only (Project is above, so no
        // output aliases are visible — the clause-phase rule, structurally).
        let (mut reads, mut subqueries) = select
            .selection
            .as_ref()
            .map(|predicate| self.expr_reads(predicate, &from_scope))
            .unwrap_or_default();
        let (clause_reads, clause_subqueries) = self.select_clause_reads(select, &from_scope);
        reads.extend(clause_reads);
        subqueries.extend(clause_subqueries);
        let input = wrap_reads(from, reads, subqueries);
        // PassThrough is identity, so the projection resolves against the
        // FROM scope either way. A projection scalar subquery's sub-plan is
        // kept on the Project (walked for its tables / reads); its output
        // already folded into the owning column's provenance.
        let mut outputs = Vec::new();
        let mut projection_subqueries = Vec::new();
        // Predicate references / sub-plans inside a projection expression (a
        // `CASE` condition, an `EXISTS` test) are reads, not value sources —
        // carry them on a non-feeding PassThrough below the Project so they
        // surface as reads without feeding lineage.
        let mut projection_filter_reads = Vec::new();
        let mut projection_filter_subqueries = Vec::new();
        for item in &select.projection {
            if let Some(bound) = self.bind_output_column(item, &from_scope) {
                outputs.push(bound.column);
                projection_subqueries.extend(bound.value_subplans);
                projection_filter_reads.extend(bound.filter_reads);
                projection_filter_subqueries.extend(bound.filter_subplans);
            }
        }
        let project = Plan::Project(Project {
            input: Box::new(wrap_reads(
                input,
                projection_filter_reads,
                projection_filter_subqueries,
            )),
            outputs: outputs.clone(),
            subqueries: projection_subqueries,
        });
        // GROUP BY / HAVING / SORT BY see the output aliases (clause
        // phase): resolve against the FROM relations *plus* the outputs,
        // keeping the USING merge columns so they fan in there too.
        let clause_scope = Scope {
            relations: from_scope.relations,
            outputs,
            merge_columns: from_scope.merge_columns,
        };
        let (mut clause_reads, mut clause_subqueries) =
            self.group_by_reads(&select.group_by, &clause_scope);
        if let Some(having) = &select.having {
            let (reads, subqueries) = self.expr_reads(having, &clause_scope);
            clause_reads.extend(reads);
            clause_subqueries.extend(subqueries);
        }
        for sort in &select.sort_by {
            let (reads, subqueries) = self.expr_reads(&sort.expr, &clause_scope);
            clause_reads.extend(reads);
            clause_subqueries.extend(subqueries);
        }
        let body = wrap_reads(project, clause_reads, clause_subqueries);
        // `SELECT … INTO t`: the query also creates / writes table `t`
        // (MsSql / Postgres). Wrap the projection as the write source so `t`
        // surfaces as a write target (its columns feed it positionally).
        let plan = match &select.into {
            Some(into) => match self.table_ref(&into.name) {
                Some(target) => Plan::Write(Write {
                    target: self.canonical_target(target),
                    target_columns: Vec::new(),
                    input: Box::new(body),
                    returning: Vec::new(),
                    conflict_updates: Vec::new(),
                }),
                None => body,
            },
            None => body,
        };
        // A trailing top-level ORDER BY also resolves against this scope,
        // so hand it back to `bind_query`.
        (plan, clause_scope)
    }

    pub(super) fn bind_from(&self, items: &[TableWithJoins]) -> (Plan, Scope) {
        // Bind each comma-separated FROM item in order, accumulating the
        // scope so a later item (a LATERAL table function / derived table)
        // can resolve references to an earlier sibling.
        let mut scope = Scope::empty();
        let mut inputs = Vec::with_capacity(items.len());
        for twj in items {
            let (node, node_scope) = self.bind_table_with_joins(twj, &scope);
            inputs.push(node);
            scope = scope.merge(node_scope);
        }
        match inputs.len() {
            // `SELECT 1` (no FROM) — an empty opaque source.
            0 => (Plan::OpaqueLeaf, Scope::empty()),
            1 => (inputs.pop().unwrap(), scope),
            // Comma join: a PassThrough with no predicate.
            _ => (
                Plan::PassThrough(PassThrough {
                    inputs,
                    reads: Vec::new(),
                    subqueries: Vec::new(),
                }),
                scope,
            ),
        }
    }

    pub(super) fn bind_table_with_joins(
        &self,
        twj: &TableWithJoins,
        siblings: &Scope,
    ) -> (Plan, Scope) {
        let (mut node, mut scope) = self.bind_table_factor(&twj.relation, siblings);
        for join in &twj.joins {
            // A joined relation sees the preceding FROM siblings plus the
            // relations bound so far in this join chain (LATERAL).
            let joined_siblings = siblings.clone().merge(scope.clone());
            let (right, right_scope) = self.bind_table_factor(&join.relation, &joined_siblings);
            // The ON predicate sees both sides; resolve its reads against
            // the combined scope, which is also this PassThrough's output.
            let mut combined = scope.merge(right_scope);
            let (reads, subqueries) = match join_constraint(join) {
                Some(JoinConstraint::On(expr)) => self.expr_reads(expr, &combined),
                // USING (col, …) records merge columns: a later unqualified
                // reference fans in to every side that could own one. The
                // join itself contributes no reads (only references do).
                // NATURAL is not expanded (needs both schemas).
                Some(JoinConstraint::Using(columns)) => {
                    combined
                        .merge_columns
                        .extend(columns.iter().filter_map(object_name_last_ident));
                    (Vec::new(), Vec::new())
                }
                _ => (Vec::new(), Vec::new()),
            };
            node = Plan::PassThrough(PassThrough {
                inputs: vec![node, right],
                reads,
                subqueries,
            });
            scope = combined;
        }
        (node, scope)
    }

    /// Bind a bare named table reference into a read `Scan` plus a
    /// single-relation scope. A unique catalog hit canonicalizes the
    /// identity, supplies the columns, and is `Cataloged`; an ambiguous hit
    /// stays as written and `Ambiguous`; a miss / no-catalog stays as
    /// written, open, and `Inferred`.
    pub(super) fn bind_named_table(
        &self,
        written: &TableReference,
        alias: Option<Ident>,
    ) -> (Plan, Scope) {
        let TableMatch {
            table,
            resolution,
            columns,
        } = self.table_match(written);
        let columns = if columns.is_empty() {
            RelationColumns::Open
        } else {
            RelationColumns::Known(columns)
        };
        let relation = Relation {
            alias,
            source: RelationSource::Table {
                table: table.clone(),
                columns,
            },
        };
        let scan = Plan::Scan(Scan {
            table,
            resolution,
            role: ScanRole::Read,
        });
        (scan, Scope::of(relation))
    }

    pub(super) fn bind_table_factor(
        &self,
        factor: &TableFactor,
        siblings: &Scope,
    ) -> (Plan, Scope) {
        match factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                let Some(written) = self.table_ref(name) else {
                    return (Plan::OpaqueLeaf, Scope::empty());
                };
                // A parameterised table reference `foo(args)` carries
                // argument expressions (read against the surrounding scope).
                let arg_reads = args.as_ref().map(|args| {
                    let mut c = ExprCollector::filter();
                    for arg in &args.args {
                        self.collect_function_arg(arg, siblings, &mut c);
                    }
                    c
                });
                let alias = alias.as_ref().map(|a| a.name.clone());
                // A bare name matching an in-scope CTE resolves to that
                // CTE's synthetic relation — its pre-collapsed outputs,
                // exposed via the scope exactly like a derived table. The
                // plan node is a lightweight `CteRef` (not a clone of the
                // body): the body is walked once at its `With` declaration,
                // so references neither double-count nor lose its reads.
                // Qualified names are never CTEs.
                if written.schema.is_none() && written.catalog.is_none() {
                    if let Some(cte) = self.lookup_cte(&written.name) {
                        let name = cte.name.clone();
                        let relation = Relation {
                            alias: alias.or_else(|| Some(cte.name.clone())),
                            source: RelationSource::Derived {
                                columns: cte.outputs.clone(),
                            },
                        };
                        return (Plan::CteRef(CteRef { name }), Scope::of(relation));
                    }
                }
                let (scan, scope) = self.bind_named_table(&written, alias);
                // Table-function args (rare on a named table) read against
                // the surrounding scope; embed them so they surface.
                let plan = match arg_reads {
                    Some(c) => {
                        let (reads, subplans) = c.into_filter_parts();
                        wrap_reads(scan, reads, subplans)
                    }
                    None => scan,
                };
                (plan, scope)
            }
            // A derived table `(<subquery>) AS d`: bind the subquery and
            // expose its output columns as a synthetic relation. Those
            // outputs already carry collapsed provenance, so an outer
            // reference through `d` surfaces the inner real columns —
            // collapse falls out of construction. The subquery's plan is
            // this factor's plan (an input to the enclosing operators). The
            // preceding FROM siblings are visible to a LATERAL subquery (the
            // `lateral` flag is not enforced, matching the resolver).
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let (plan, sub_scope) = self
                    .with_outer_scope(siblings.relations.clone())
                    .bind_query(subquery);
                let mut columns = sub_scope.outputs;
                if let Some(alias) = alias {
                    apply_column_aliases(&mut columns, alias);
                }
                let relation = Relation {
                    alias: alias.as_ref().map(|a| a.name.clone()),
                    source: RelationSource::Derived { columns },
                };
                (plan, Scope::of(relation))
            }
            // A parenthesized join `(a JOIN b ...)`: the inner tables bind
            // directly into the current scope (so refs to them resolve and
            // their ON reads surface); the wrapper alias exposes nothing.
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => self.bind_table_with_joins(table_with_joins, siblings),
            // PIVOT / UNPIVOT / MATCH_RECOGNIZE wrap an inner table whose
            // columns the clause expressions read; the produced relation is
            // opaque (dynamic columns).
            TableFactor::Pivot {
                table,
                aggregate_functions,
                value_column,
                value_source,
                default_on_null,
                alias,
                ..
            } => {
                let (inner, inner_scope) = self.bind_table_factor(table, siblings);
                let mut c = ExprCollector::filter();
                for agg in aggregate_functions {
                    self.collect_expr(&agg.expr, &inner_scope, &mut c);
                }
                for expr in value_column {
                    self.collect_expr(expr, &inner_scope, &mut c);
                }
                self.collect_pivot_value_source(value_source, &inner_scope, &mut c);
                if let Some(expr) = default_on_null {
                    self.collect_expr(expr, &inner_scope, &mut c);
                }
                self.opaque_relation(inner, alias.as_ref(), c)
            }
            TableFactor::Unpivot {
                table,
                value,
                columns,
                alias,
                ..
            } => {
                let (inner, inner_scope) = self.bind_table_factor(table, siblings);
                let mut c = ExprCollector::filter();
                self.collect_expr(value, &inner_scope, &mut c);
                for col in columns {
                    self.collect_expr(&col.expr, &inner_scope, &mut c);
                }
                self.opaque_relation(inner, alias.as_ref(), c)
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
                let (inner, inner_scope) = self.bind_table_factor(table, siblings);
                let mut c = ExprCollector::filter();
                for expr in partition_by {
                    self.collect_expr(expr, &inner_scope, &mut c);
                }
                for ob in order_by {
                    self.collect_order_by_expr(ob, &inner_scope, &mut c);
                }
                for measure in measures {
                    self.collect_expr(&measure.expr, &inner_scope, &mut c);
                }
                for symbol in symbols {
                    self.collect_expr(&symbol.definition, &inner_scope, &mut c);
                }
                self.opaque_relation(inner, alias.as_ref(), c)
            }
            // Table functions / UNNEST / JSON_TABLE / XML / semantic views:
            // an opaque relation whose argument expressions read against the
            // surrounding (LATERAL-visible) scope.
            TableFactor::TableFunction { expr, alias } => {
                let mut c = ExprCollector::filter();
                self.collect_expr(expr, siblings, &mut c);
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
            TableFactor::Function { args, alias, .. } => {
                let mut c = ExprCollector::filter();
                for arg in args {
                    self.collect_function_arg(arg, siblings, &mut c);
                }
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
            TableFactor::UNNEST {
                array_exprs, alias, ..
            } => {
                let mut c = ExprCollector::filter();
                for expr in array_exprs {
                    self.collect_expr(expr, siblings, &mut c);
                }
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
            TableFactor::JsonTable {
                json_expr, alias, ..
            }
            | TableFactor::OpenJsonTable {
                json_expr, alias, ..
            } => {
                let mut c = ExprCollector::filter();
                self.collect_expr(json_expr, siblings, &mut c);
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
            TableFactor::XmlTable {
                row_expression,
                passing,
                alias,
                ..
            } => {
                let mut c = ExprCollector::filter();
                self.collect_expr(row_expression, siblings, &mut c);
                for argument in &passing.arguments {
                    self.collect_expr(&argument.expr, siblings, &mut c);
                }
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
            TableFactor::SemanticView {
                dimensions,
                metrics,
                facts,
                where_clause,
                alias,
                ..
            } => {
                let mut c = ExprCollector::filter();
                for expr in dimensions.iter().chain(metrics).chain(facts) {
                    self.collect_expr(expr, siblings, &mut c);
                }
                if let Some(expr) = where_clause {
                    self.collect_expr(expr, siblings, &mut c);
                }
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
        }
    }

    /// Wrap an opaque relation's `base` plan with the reads collected from
    /// its argument expressions, exposing the result as a synthetic
    /// [`TableFunction`](RelationSource::TableFunction) relation (its
    /// produced columns are dynamic, so a reference through its alias yields
    /// nothing).
    pub(super) fn opaque_relation(
        &self,
        base: Plan,
        alias: Option<&TableAlias>,
        collector: ExprCollector,
    ) -> (Plan, Scope) {
        let (reads, subplans) = collector.into_filter_parts();
        let plan = wrap_reads(base, reads, subplans);
        let scope = alias.map_or_else(Scope::empty, |alias| {
            Scope::of(Relation {
                alias: Some(alias.name.clone()),
                source: RelationSource::TableFunction,
            })
        });
        (plan, scope)
    }

    pub(super) fn collect_pivot_value_source(
        &self,
        value_source: &sqlparser::ast::PivotValueSource,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        use sqlparser::ast::PivotValueSource;
        match value_source {
            PivotValueSource::List(values) => {
                for value in values {
                    self.collect_expr(&value.expr, scope, c);
                }
            }
            PivotValueSource::Any(order_by) => {
                for ob in order_by {
                    self.collect_order_by_expr(ob, scope, c);
                }
            }
            PivotValueSource::Subquery(query) => self.collect_subquery(query, scope, c),
        }
    }

    /// Find an in-scope CTE by name (innermost `WITH` shadows outer),
    /// matched with table-alias casing.
    pub(super) fn lookup_cte(&self, name: &Ident) -> Option<&CteRelation> {
        self.ctes
            .iter()
            .rev()
            .find(|c| self.ident_eq(&c.name, name))
    }

    /// Bind one projection item into a [`BoundValue`] (its output column plus
    /// the position-split sub-plans / filter reads it contributes), or `None`
    /// for a wildcard, which isn't expanded. A `(expr).*` qualified wildcard
    /// still reads its base expression (projected as one `Transformation`
    /// output) even though the produced columns are suppressed.
    pub(super) fn bind_output_column(
        &self,
        item: &SelectItem,
        scope: &Scope,
    ) -> Option<BoundValue> {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.clone())),
            // A wildcard isn't expanded (the rigor cost is too high for a
            // SQL-text-only library); record it so consumers know this
            // projection's column lineage is incomplete, and skip it.
            SelectItem::Wildcard(options) => {
                self.record_wildcard_suppressed("wildcard `*`", options.wildcard_token.0.span);
                return None;
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
                // `(expr).*` (Snowflake) still projects its base expression
                // as one output — a structural field access, so the value
                // flows as a `Transformation` even though the produced
                // columns aren't enumerated. `alias.*` (an ObjectName) has
                // no inspectable base expression.
                if let SelectItemQualifiedWildcardKind::Expr(expr) = kind {
                    let mut bound = self.bind_value_column(None, expr, scope);
                    for source in &mut bound.column.provenance {
                        source.kind = ColumnLineageKind::Transformation;
                    }
                    return Some(bound);
                }
                return None;
            }
        };
        let name = alias.or_else(|| inferred_output_name(expr));
        Some(self.bind_value_column(name, expr, scope))
    }

    /// Reads (and any subquery sub-plans) contributed by GROUP BY (plain
    /// keys + ROLLUP / CUBE / GROUPING SETS members + a `GROUPING SETS`
    /// modifier), resolved against `scope` (which carries the output
    /// aliases).
    pub(super) fn group_by_reads(
        &self,
        group_by: &GroupByExpr,
        scope: &Scope,
    ) -> (Vec<ColumnRead>, Vec<Plan>) {
        let mut reads = Vec::new();
        let mut subqueries = Vec::new();
        if let GroupByExpr::Expressions(exprs, modifiers) = group_by {
            let members =
                exprs
                    .iter()
                    .chain(modifiers.iter().filter_map(|modifier| match modifier {
                        GroupByWithModifier::GroupingSets(expr) => Some(expr),
                        _ => None,
                    }));
            for expr in members {
                let (r, s) = self.expr_reads(expr, scope);
                reads.extend(r);
                subqueries.extend(s);
            }
        }
        (reads, subqueries)
    }

    /// Reads (and any subquery sub-plans) contributed by an ORDER BY,
    /// resolved against `scope`.
    pub(super) fn order_by_reads(
        &self,
        order_by: &OrderBy,
        scope: &Scope,
    ) -> (Vec<ColumnRead>, Vec<Plan>) {
        let OrderByKind::Expressions(exprs) = &order_by.kind else {
            return (Vec::new(), Vec::new());
        };
        let mut reads = Vec::new();
        let mut subqueries = Vec::new();
        for expr in exprs {
            let (r, s) = self.expr_reads(&expr.expr, scope);
            reads.extend(r);
            subqueries.extend(s);
        }
        (reads, subqueries)
    }
}
