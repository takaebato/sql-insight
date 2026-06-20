//! Query binding: `WITH` / CTEs, set operations, `VALUES`, SELECT, the FROM
//! clause (joins, table factors, derived tables, table functions), and pipe
//! operators. Each `bind_*` returns the operator subtree and its output
//! [`Scope`] (relations + introduced outputs).

use super::*;

impl<'a> Binder<'a> {
    /// Bind a query, returning the operator and its output [`Scope`] (the FROM
    /// relations plus the query outputs). A leading `WITH` is peeled first: each
    /// CTE binds in declaration order into an environment the later CTEs and the
    /// body resolve against; the bodies are owned by a `With` node, references
    /// are `CteRef`s.
    pub(super) fn bind_query(&self, query: &Query) -> (LogicalPlan, Scope) {
        let Some(with) = &query.with else {
            return self.bind_query_body(query);
        };
        let mut env = self.ctes.clone();
        let mut declared = Vec::new();
        for cte in &with.cte_tables {
            let (entry, body) = self.bind_cte(cte, &env, with.recursive);
            declared.push(Cte {
                name: entry.name.clone(),
                body,
            });
            env.push(entry);
        }
        let (body, scope) = self.with_ctes(env).bind_query_body(query);
        (
            LogicalPlan::With(With {
                ctes: declared,
                body: Box::new(body),
            }),
            scope,
        )
    }

    /// Bind one declared CTE against the environment of the earlier CTEs,
    /// returning its scope entry (name + exposed columns) and its bound body.
    /// A `RECURSIVE` CTE registers its name provisionally first so the body's
    /// self-reference resolves: a set-operation body learns its column shape
    /// from the anchor (the left branch); any other body registers with no
    /// known columns (so a self-reference is still a `CteRef`, not a phantom).
    pub(super) fn bind_cte(
        &self,
        cte: &SqlCte,
        env: &[CteDecl],
        recursive: bool,
    ) -> (CteDecl, LogicalPlan) {
        let name = cte.alias.name.clone();
        let inner_env = if recursive {
            let provisional = match cte.query.body.as_ref() {
                SetExpr::SetOperation { left, .. } => self
                    .with_ctes(env.to_vec())
                    .bind_set_expr(left)
                    .1
                    .exposed_columns(Some(&cte.alias)),
                _ => Vec::new(),
            };
            let mut e = env.to_vec();
            e.push(CteDecl {
                name: name.clone(),
                columns: provisional,
            });
            e
        } else {
            env.to_vec()
        };
        let (mut plan, scope) = self.with_ctes(inner_env).bind_query(&cte.query);
        let columns = scope.exposed_columns(Some(&cte.alias));
        // An explicit `c (x, y)` column list renames the body's output columns
        // so a reference through the CTE traces to them.
        rename_outputs(&mut plan, &alias_column_names(&cte.alias));
        (CteDecl { name, columns }, plan)
    }

    /// Bind a query's body, its pipe-operator chain, and trailing ORDER BY (the
    /// `WITH` is already in scope via `self.ctes`).
    pub(super) fn bind_query_body(&self, query: &Query) -> (LogicalPlan, Scope) {
        let (mut op, mut scope) = self.bind_set_expr(&query.body);
        // A trailing ORDER BY / LIMIT over a set operation resolves in the
        // outer scope (both branch scopes are popped), so a reference to a
        // UNION output column — not a real table — is unresolved.
        let set_op_body = matches!(op, LogicalPlan::SetOp(_));
        // Pipe operators (`|> WHERE`, `|> SELECT`, …) transform the body in
        // sequence: an output-producing operator layers a `Projection` (evolving
        // the output scope), a filter operator adds reads. They resolve against
        // the body's relations plus the running outputs (relations stay in
        // scope across the chain).
        if !query.pipe_operators.is_empty() {
            let mut pipe_scope = match &op {
                // A set-op body exposes no single relation scope for refs.
                LogicalPlan::SetOp(_) => Scope {
                    relations: Vec::new(),
                    query_outputs: std::mem::take(&mut scope.query_outputs),
                    merge_columns: Vec::new(),
                },
                _ => scope,
            };
            for pipe_op in &query.pipe_operators {
                op = self.bind_pipe(pipe_op, op, &mut pipe_scope);
            }
            scope = pipe_scope;
        }
        // Trailing clauses see the body's output scope — except over a set
        // operation, where there's no single relation to resolve against.
        let empty = Scope::default();
        let tail_scope = if set_op_body { &empty } else { &scope };
        if let Some(order_by) = &query.order_by {
            let keys = self.order_by_keys(order_by, tail_scope);
            if !keys.is_empty() {
                op = sort(op, keys);
            }
        }
        // LIMIT / OFFSET / LIMIT BY (row-count bounds) and ClickHouse
        // `SETTINGS key = expr` are filter reads above the (possibly piped)
        // body.
        let mut tail_reads = Vec::new();
        if let Some(limit) = &query.limit_clause {
            tail_reads.extend(self.limit_reads(limit, tail_scope));
        }
        if let Some(settings) = &query.settings {
            tail_reads.extend(
                settings
                    .iter()
                    .map(|s| self.bind_expr(&s.value, tail_scope)),
            );
        }
        if !tail_reads.is_empty() {
            op = LogicalPlan::Filter(Filter {
                input: Box::new(op),
                predicate: tail_reads,
            });
        }
        (op, scope)
    }

    /// Bind one pipe operator on top of `input`, updating `scope.query_outputs` when
    /// it reshapes the output. An output-producing operator (SELECT / EXTEND /
    /// SET / AGGREGATE) layers a [`Projection`] whose value expressions feed
    /// `QueryOutput` lineage; a filter operator (WHERE / ORDER BY / LIMIT /
    /// CALL / PIVOT / set-op / JOIN) wraps a non-feeding read [`Filter`]; the
    /// rest (sampling / rename / drop / unpivot) pass through. The match is
    /// exhaustive so a new pipe operator is reviewed here.
    pub(super) fn bind_pipe(
        &self,
        op: &PipeOperator,
        input: LogicalPlan,
        scope: &mut Scope,
    ) -> LogicalPlan {
        match op {
            PipeOperator::Select { exprs } => {
                let new = self.bind_output_items(exprs, scope);
                let (node, query_outputs) = self.pipe_project(input, &[], new, &scope.relations);
                scope.query_outputs = query_outputs;
                node
            }
            PipeOperator::Extend { exprs } => {
                // The new columns see the running outputs; then they append.
                let new = self.bind_output_items(exprs, scope);
                let base = std::mem::take(&mut scope.query_outputs);
                let (node, query_outputs) = self.pipe_project(input, &base, new, &scope.relations);
                scope.query_outputs = query_outputs;
                node
            }
            PipeOperator::Set { assignments } => {
                let base = std::mem::take(&mut scope.query_outputs);
                let (node, query_outputs) = self.pipe_set(input, base, assignments, scope);
                scope.query_outputs = query_outputs;
                node
            }
            PipeOperator::Aggregate {
                full_table_exprs,
                group_by_expr,
            } => {
                let new = full_table_exprs
                    .iter()
                    .chain(group_by_expr)
                    .map(|e| NamedExpr {
                        name: e.expr.alias.clone().or_else(|| inferred_name(&e.expr.expr)),
                        expr: self.bind_expr(&e.expr.expr, scope),
                    })
                    .collect();
                let (node, query_outputs) = self.pipe_project(input, &[], new, &scope.relations);
                scope.query_outputs = query_outputs;
                node
            }
            PipeOperator::Where { expr } => {
                self.pipe_filter(input, vec![self.bind_expr(expr, scope)])
            }
            PipeOperator::Limit { expr, offset } => {
                let mut reads = vec![self.bind_expr(expr, scope)];
                reads.extend(offset.iter().map(|o| self.bind_expr(o, scope)));
                self.pipe_filter(input, reads)
            }
            PipeOperator::OrderBy { exprs } => self.pipe_filter(
                input,
                exprs
                    .iter()
                    .map(|o| self.bind_expr(&o.expr, scope))
                    .collect(),
            ),
            PipeOperator::Call { function, .. } => {
                self.pipe_filter(input, self.bind_function_args(function, scope))
            }
            PipeOperator::Pivot {
                aggregate_functions,
                value_source,
                ..
            } => {
                let mut reads: Vec<Expr> = aggregate_functions
                    .iter()
                    .map(|a| self.bind_expr(&a.expr, scope))
                    .collect();
                reads.extend(self.pivot_value_source_exprs(value_source, scope));
                self.pipe_filter(input, reads)
            }
            PipeOperator::Union { queries, .. }
            | PipeOperator::Intersect { queries, .. }
            | PipeOperator::Except { queries, .. } => {
                // Each set-op query's reads surface (non-feeding) — model as a
                // filter-position subquery.
                let reads = queries
                    .iter()
                    .map(|q| Expr::Exists(Box::new(self.bind_subquery(q, scope))))
                    .collect();
                self.pipe_filter(input, reads)
            }
            // `|> JOIN t ON …`: the joined table's scan surfaces (a table read),
            // but — matching the resolver's loose pipe scoping — it is NOT added
            // to the scope, so the ON predicate's references to it are
            // unresolved (only the running relations resolve).
            PipeOperator::Join(j) => {
                let (right, _scope) = self.bind_table_factor(&j.relation, &scope.relations);
                let on = join_on(&j.join_operator)
                    .map(|e| self.bind_expr(e, scope))
                    .into_iter()
                    .collect();
                join(input, right, on)
            }
            // No inspectable column expressions: a sampling clause, rename,
            // drop, or unpivot.
            PipeOperator::TableSample { .. }
            | PipeOperator::Drop { .. }
            | PipeOperator::As { .. }
            | PipeOperator::Rename { .. }
            | PipeOperator::Unpivot { .. } => input,
        }
    }

    /// Bind a list of output items (SELECT / EXTEND projection items).
    pub(super) fn bind_output_items(&self, items: &[SelectItem], scope: &Scope) -> Vec<NamedExpr> {
        items
            .iter()
            .filter_map(|i| self.bind_select_item(i, scope))
            .collect()
    }

    /// Build an output-producing pipe `Projection`: the passthrough of `base`
    /// outputs (each re-resolved by name against the base — an identity output
    /// re-reads its real column, a computed output is `Derived` and traced)
    /// plus the `new` value columns.
    pub(super) fn pipe_project(
        &self,
        input: LogicalPlan,
        base: &[OutputCol],
        new: Vec<NamedExpr>,
        relations: &[Relation],
    ) -> (LogicalPlan, Vec<OutputCol>) {
        let mut exprs = self.passthrough_exprs(base, relations);
        exprs.extend(new);
        let query_outputs = self.output_cols(&exprs);
        (
            LogicalPlan::Projection(Projection {
                input: Box::new(input),
                exprs,
            }),
            query_outputs,
        )
    }

    /// Re-resolve each named `base` output as a passthrough projection item
    /// (clause-alias resolution against the base, so an identity output
    /// re-reads its real column while a computed output stays `Derived`).
    pub(super) fn passthrough_exprs(
        &self,
        base: &[OutputCol],
        relations: &[Relation],
    ) -> Vec<NamedExpr> {
        let pass_scope = Scope {
            relations: relations.to_vec(),
            query_outputs: base.to_vec(),
            merge_columns: Vec::new(),
        };
        base.iter()
            .filter_map(|o| o.name.clone())
            .map(|name| NamedExpr {
                expr: Expr::Column(Box::new(
                    self.resolve(std::slice::from_ref(&name), &pass_scope),
                )),
                name: Some(name),
            })
            .collect()
    }

    /// `|> SET col = expr`: each assignment replaces a same-named base output in
    /// place (else appends), so a SET after a SELECT rewrites that column.
    pub(super) fn pipe_set(
        &self,
        input: LogicalPlan,
        base: Vec<OutputCol>,
        assignments: &[sqlparser::ast::Assignment],
        scope: &Scope,
    ) -> (LogicalPlan, Vec<OutputCol>) {
        let mut exprs = self.passthrough_exprs(&base, &scope.relations);
        for a in assignments {
            for column in assignment_target_columns(&a.target) {
                let ne = NamedExpr {
                    name: Some(column.clone()),
                    expr: self.bind_expr(&a.value, scope),
                };
                match exprs
                    .iter_mut()
                    .find(|e| e.name.as_ref().is_some_and(|n| n.value == column.value))
                {
                    Some(slot) => *slot = ne,
                    None => exprs.push(ne),
                }
            }
        }
        let query_outputs = self.output_cols(&exprs);
        (
            LogicalPlan::Projection(Projection {
                input: Box::new(input),
                exprs,
            }),
            query_outputs,
        )
    }

    /// Wrap `input` in a non-feeding read [`Filter`] for a filter pipe operator
    /// (empty predicate → unchanged).
    pub(super) fn pipe_filter(&self, input: LogicalPlan, predicate: Vec<Expr>) -> LogicalPlan {
        if predicate.is_empty() {
            input
        } else {
            LogicalPlan::Filter(Filter {
                input: Box::new(input),
                predicate,
            })
        }
    }

    pub(super) fn bind_set_expr(&self, body: &SetExpr) -> (LogicalPlan, Scope) {
        match body {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(query) => self.bind_query(query),
            SetExpr::Values(values) => self.bind_values(values),
            // `TABLE foo`: a whole-table query body (e.g. the source of
            // `CREATE TABLE t AS TABLE foo`). Bind it as a read scan.
            SetExpr::Table(table) => match table_set_expr_ref(table) {
                Some(written) => self.bind_named_table(&written, None),
                None => (LogicalPlan::Empty, Scope::default()),
            },
            // `WITH … INSERT/UPDATE/DELETE/MERGE …`: the DML statement is the
            // query body (the parser wraps a CTE-prefixed DML this way). Bind it
            // to its DML root; it exposes no output scope to an enclosing query.
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => (self.bind_statement(statement), Scope::default()),
            // A set operation: result columns are the left operand's (names
            // from the left, positional merge).
            SetExpr::SetOperation { left, right, .. } => {
                let (l, scope) = self.bind_set_expr(left);
                let (r, _) = self.bind_set_expr(right);
                (
                    LogicalPlan::SetOp(SetOp {
                        left: Box::new(l),
                        right: Box::new(r),
                    }),
                    scope,
                )
            }
        }
    }

    /// Bind a `VALUES (…), (…)` row set into [`LogicalPlan::Values`]: one
    /// anonymous output per column position (synthesised rows have no base
    /// columns, so a reference to a `(VALUES …) AS v(x)` column is `Derived`
    /// and traces to nothing — there is no column lineage). The row
    /// expressions are reads, resolved against the empty current scope and
    /// falling through to the correlation stack (a `(VALUES (t.a)) AS v` reads
    /// the enclosing / sibling `t.a` like a derived subquery's body).
    pub(super) fn bind_values(&self, values: &SqlValues) -> (LogicalPlan, Scope) {
        let width = values.rows.iter().map(Vec::len).max().unwrap_or(0);
        let rows: Vec<Vec<Expr>> = values
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|expr| self.bind_expr(expr, &Scope::default()))
                    .collect()
            })
            .collect();
        let query_outputs = (0..width)
            .map(|_| OutputCol {
                name: None,
                identity: false,
            })
            .collect();
        (
            LogicalPlan::Values(Values { rows }),
            Scope {
                relations: Vec::new(),
                query_outputs,
                merge_columns: Vec::new(),
            },
        )
    }

    /// Bind a SELECT into the canonical operator chain `Scan → WHERE →
    /// Aggregate (GROUP BY) → HAVING → Projection → SORT BY`, returning the
    /// operator and its clause scope (FROM relations + projection outputs) for
    /// a trailing ORDER BY to resolve against. The projection resolves against
    /// the FROM scope (so a grouped column is a base read, counted, not traced
    /// through the `Aggregate`); the grouping / HAVING / SORT clauses resolve
    /// against the FROM relations *plus* the projection outputs (clause-alias
    /// visibility) — a resolution-scope rule, independent of tree position.
    pub(super) fn bind_select(&self, select: &Select) -> (LogicalPlan, Scope) {
        let (from, scope) = self.bind_from(&select.from);
        // WHERE + the WHERE-family auxiliary clauses (DISTINCT ON / TOP /
        // LATERAL VIEW / PREWHERE / QUALIFY / CONNECT BY / CLUSTER BY / named
        // WINDOW) filter rows before grouping — a filter over the FROM (no
        // output aliases visible).
        let mut where_reads: Vec<Expr> = select
            .selection
            .iter()
            .map(|predicate| self.bind_expr(predicate, &scope))
            .collect();
        where_reads.extend(self.select_clause_reads(select, &scope));
        let mut node = if where_reads.is_empty() {
            from
        } else {
            LogicalPlan::Filter(Filter {
                input: Box::new(from),
                predicate: where_reads,
            })
        };
        // The projection resolves against the FROM scope (base reads).
        let exprs: Vec<NamedExpr> = select
            .projection
            .iter()
            .filter_map(|item| self.bind_select_item(item, &scope))
            .collect();
        let clause_scope = scope.with_query_outputs(self.output_cols(&exprs));
        // GROUP BY → an `Aggregate` over the filtered rows; its keys are reads.
        let group_by = self.group_by_keys(&select.group_by, &clause_scope);
        if !group_by.is_empty() {
            node = LogicalPlan::Aggregate(Aggregate {
                input: Box::new(node),
                group_by,
            });
        }
        // HAVING → a filter on the grouped rows, between Aggregate and Projection.
        if let Some(having) = &select.having {
            node = LogicalPlan::Filter(Filter {
                input: Box::new(node),
                predicate: vec![self.bind_expr(having, &clause_scope)],
            });
        }
        // SELECT: the column-defining projection, on top.
        node = LogicalPlan::Projection(Projection {
            input: Box::new(node),
            exprs,
        });
        // SORT BY (Hive) sees the outputs, like a trailing ORDER BY.
        let sort_keys = self.order_by_expr_keys(&select.sort_by, &clause_scope);
        if !sort_keys.is_empty() {
            node = sort(node, sort_keys);
        }
        // `SELECT … INTO t` (MsSql / Postgres): the query also creates table `t`
        // — wrap the projection as the create source so `t` is a write target.
        if let Some(into) = &select.into {
            if let Some(written) = self.table_ref(&into.name) {
                node = LogicalPlan::CreateTableAs(CreateTableAs {
                    target: self.table_match(&written).table,
                    columns: Vec::new(),
                    input: Box::new(node),
                });
            }
        }
        (node, clause_scope)
    }

    pub(super) fn bind_from(&self, items: &[TableWithJoins]) -> (LogicalPlan, Scope) {
        let mut iter = items.iter();
        let Some(first) = iter.next() else {
            return (LogicalPlan::Empty, Scope::default());
        };
        let (mut node, mut scope) = self.bind_table_with_joins(first, &[]);
        // Comma-separated FROM items are a cross join; a later item sees the
        // earlier ones only if it is LATERAL.
        for twj in iter {
            let (right, right_scope) = self.bind_table_with_joins(twj, &scope.relations);
            scope.absorb(right_scope);
            node = join(node, right, Vec::new());
        }
        (node, scope)
    }

    /// `left` are the FROM siblings to this item's left, visible to a LATERAL
    /// factor (and to a joined factor, after the preceding join inputs).
    pub(super) fn bind_table_with_joins(
        &self,
        twj: &TableWithJoins,
        left: &[Relation],
    ) -> (LogicalPlan, Scope) {
        let (mut node, mut scope) = self.bind_table_factor(&twj.relation, left);
        for j in &twj.joins {
            // A joined LATERAL factor sees the left siblings plus the join
            // inputs accumulated so far.
            let visible: Vec<Relation> = left.iter().chain(&scope.relations).cloned().collect();
            let (right, right_scope) = self.bind_table_factor(&j.relation, &visible);
            scope.absorb(right_scope);
            // A `USING (col)` join records its merge columns: an unqualified
            // reference to one fans in to both sides.
            scope.add_merge_columns(join_using(&j.join_operator));
            // The ON predicate resolves against both sides' columns.
            let on = join_on(&j.join_operator)
                .map(|e| self.bind_expr(e, &scope))
                .into_iter()
                .collect();
            node = join(node, right, on);
        }
        (node, scope)
    }

    /// Bind a bare named table into a read `Scan` plus a single-relation scope
    /// (a unique catalog hit canonicalises + supplies columns + `Cataloged`;
    /// else open + `Inferred` / `Ambiguous`). Shared by the table factor and a
    /// `TABLE foo` query body.
    pub(super) fn bind_named_table(
        &self,
        written: &TableReference,
        alias: Option<Ident>,
    ) -> (LogicalPlan, Scope) {
        let m = self.table_match(written);
        let columns = if m.columns.is_empty() {
            Columns::Unknown
        } else {
            Columns::Cataloged(m.columns)
        };
        let scan = LogicalPlan::Scan(Scan {
            table: m.table.clone(),
            columns: columns.clone(),
            resolution: m.resolution,
        });
        let relation = Relation::Table {
            alias,
            table: m.table,
            columns,
        };
        (scan, Scope::single(relation))
    }

    pub(super) fn bind_table_factor(
        &self,
        factor: &TableFactor,
        left: &[Relation],
    ) -> (LogicalPlan, Scope) {
        match factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                let Some(written) = self.table_ref(name) else {
                    return (LogicalPlan::Empty, Scope::default());
                };
                let alias_name = alias.as_ref().map(|a| a.name.clone());
                // A bare name matching an in-scope CTE resolves to a `CteRef`
                // (the body lives once on the owning `With`) exposing the CTE's
                // output columns as a synthetic relation.
                if written.schema.is_none() && written.catalog.is_none() {
                    if let Some(cte) =
                        self.ctes.iter().rev().find(|c| {
                            self.eq(self.style.casing.table_alias, &c.name, &written.name)
                        })
                    {
                        let relation = Relation::Derived {
                            alias: alias_name.or_else(|| Some(cte.name.clone())),
                            columns: cte.columns.clone(),
                        };
                        return (
                            LogicalPlan::CteRef(CteRef {
                                name: cte.name.clone(),
                            }),
                            Scope::single(relation),
                        );
                    }
                }
                let (scan, scope) = self.bind_named_table(&written, alias_name);
                // A parameterised table reference `foo(args)`: the argument
                // expressions read against the surrounding (sibling) scope —
                // attach them as a non-feeding filter over the scan.
                let node = match args {
                    Some(args) => LogicalPlan::Filter(Filter {
                        input: Box::new(scan),
                        predicate: self
                            .bind_function_arg_list(&args.args, &Scope::from_relations(left)),
                    }),
                    None => scan,
                };
                (node, scope)
            }
            // A derived table `(<subquery>) AS d`: bind the subquery, expose
            // its output columns as a synthetic relation under the alias. A
            // LATERAL derived table sees the left siblings (pushed onto the
            // correlation stack); a non-lateral one does not.
            TableFactor::Derived {
                lateral,
                subquery,
                alias,
                ..
            } => {
                let (mut op, sub_scope) = if *lateral {
                    self.with_outer(left.to_vec()).bind_query(subquery)
                } else {
                    self.bind_query(subquery)
                };
                let columns = sub_scope.exposed_columns(alias.as_ref());
                let relation = Relation::Derived {
                    alias: alias.as_ref().map(|a| a.name.clone()),
                    columns,
                };
                let node = match alias {
                    Some(a) => {
                        rename_outputs(&mut op, &alias_column_names(a));
                        LogicalPlan::SubqueryAlias(SubqueryAlias {
                            alias: a.name.clone(),
                            input: Box::new(op),
                        })
                    }
                    None => op,
                };
                (node, Scope::single(relation))
            }
            // A parenthesized join `(a JOIN b …)`: the inner tables bind
            // directly into the current scope (their refs resolve, their ON
            // reads surface); the wrapper exposes nothing of its own.
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => self.bind_table_with_joins(table_with_joins, left),
            // --- opaque table-producing factors (dynamic columns) ---
            // Bare table functions / UNNEST / JSON_TABLE / XML / semantic views:
            // the argument expressions read against the surrounding
            // (LATERAL-visible) `left` scope; no inner table feeds.
            TableFactor::TableFunction { expr, alias } => {
                let args = vec![self.bind_expr(expr, &Scope::from_relations(left))];
                self.opaque(LogicalPlan::Empty, args, alias.as_ref())
            }
            TableFactor::Function { args, alias, .. } => {
                let bound = self.bind_function_arg_list(args, &Scope::from_relations(left));
                self.opaque(LogicalPlan::Empty, bound, alias.as_ref())
            }
            TableFactor::UNNEST {
                array_exprs, alias, ..
            } => {
                let args = self.bind_exprs(array_exprs, &Scope::from_relations(left));
                self.opaque(LogicalPlan::Empty, args, alias.as_ref())
            }
            TableFactor::JsonTable {
                json_expr, alias, ..
            }
            | TableFactor::OpenJsonTable {
                json_expr, alias, ..
            } => {
                let args = vec![self.bind_expr(json_expr, &Scope::from_relations(left))];
                self.opaque(LogicalPlan::Empty, args, alias.as_ref())
            }
            TableFactor::XmlTable {
                row_expression,
                passing,
                alias,
                ..
            } => {
                let scope = Scope::from_relations(left);
                let mut args = vec![self.bind_expr(row_expression, &scope)];
                args.extend(
                    passing
                        .arguments
                        .iter()
                        .map(|a| self.bind_expr(&a.expr, &scope)),
                );
                self.opaque(LogicalPlan::Empty, args, alias.as_ref())
            }
            TableFactor::SemanticView {
                dimensions,
                metrics,
                facts,
                where_clause,
                alias,
                ..
            } => {
                let scope = Scope::from_relations(left);
                let mut args = self.bind_exprs(dimensions, &scope);
                args.extend(self.bind_exprs(metrics, &scope));
                args.extend(self.bind_exprs(facts, &scope));
                args.extend(where_clause.iter().map(|e| self.bind_expr(e, &scope)));
                self.opaque(LogicalPlan::Empty, args, alias.as_ref())
            }
            // PIVOT / UNPIVOT / MATCH_RECOGNIZE wrap an inner table whose
            // columns the clause expressions read; the produced relation is
            // opaque. The inner table feeds (it's a real source).
            TableFactor::Pivot {
                table,
                aggregate_functions,
                value_column,
                value_source,
                default_on_null,
                alias,
                ..
            } => {
                let (inner, inner_scope) = self.bind_table_factor(table, left);
                let mut args = aggregate_functions
                    .iter()
                    .map(|a| self.bind_expr(&a.expr, &inner_scope))
                    .collect::<Vec<_>>();
                args.extend(self.bind_exprs(value_column, &inner_scope));
                args.extend(self.pivot_value_source_exprs(value_source, &inner_scope));
                args.extend(
                    default_on_null
                        .iter()
                        .map(|e| self.bind_expr(e, &inner_scope)),
                );
                self.opaque(inner, args, alias.as_ref())
            }
            TableFactor::Unpivot {
                table,
                value,
                columns,
                alias,
                ..
            } => {
                let (inner, inner_scope) = self.bind_table_factor(table, left);
                let mut args = vec![self.bind_expr(value, &inner_scope)];
                args.extend(
                    columns
                        .iter()
                        .map(|c| self.bind_expr(&c.expr, &inner_scope)),
                );
                self.opaque(inner, args, alias.as_ref())
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
                let (inner, inner_scope) = self.bind_table_factor(table, left);
                let mut args = self.bind_exprs(partition_by, &inner_scope);
                args.extend(
                    order_by
                        .iter()
                        .map(|o| self.bind_expr(&o.expr, &inner_scope)),
                );
                args.extend(
                    measures
                        .iter()
                        .map(|m| self.bind_expr(&m.expr, &inner_scope)),
                );
                args.extend(
                    symbols
                        .iter()
                        .map(|s| self.bind_expr(&s.definition, &inner_scope)),
                );
                self.opaque(inner, args, alias.as_ref())
            }
        }
    }

    /// Assemble an opaque table-producing factor: an [`LogicalPlan::TableFunction`]
    /// node carrying the (already-bound) argument reads over `input` (the wrapped
    /// inner table, or [`LogicalPlan::Empty`] for a bare function), exposed as a
    /// synthetic [`Relation::TableFunction`] relation under the alias.
    pub(super) fn opaque(
        &self,
        input: LogicalPlan,
        args: Vec<Expr>,
        alias: Option<&TableAlias>,
    ) -> (LogicalPlan, Scope) {
        let alias_name = alias.map(|a| a.name.clone());
        let node = LogicalPlan::TableFunction(TableFunction {
            alias: alias_name.clone(),
            input: Box::new(input),
            args,
        });
        let scope = match alias_name {
            Some(name) => Scope::single(Relation::TableFunction { alias: Some(name) }),
            None => Scope::default(),
        };
        (node, scope)
    }

    /// The value expressions of a PIVOT value source (`IN (list)` / `ANY ORDER
    /// BY …` / a subquery). The subquery's reads come from binding it.
    pub(super) fn pivot_value_source_exprs(
        &self,
        source: &PivotValueSource,
        scope: &Scope,
    ) -> Vec<Expr> {
        match source {
            PivotValueSource::List(values) => values
                .iter()
                .map(|v| self.bind_expr(&v.expr, scope))
                .collect(),
            PivotValueSource::Any(order_by) => order_by
                .iter()
                .map(|o| self.bind_expr(&o.expr, scope))
                .collect(),
            PivotValueSource::Subquery(query) => {
                vec![Expr::Subquery(Box::new(self.bind_subquery(query, scope)))]
            }
        }
    }
}
