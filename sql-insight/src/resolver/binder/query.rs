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
    pub(super) fn bind_query(&mut self, query: &Query) -> (LogicalPlan, Scope) {
        let Some(with) = &query.with else {
            return self.bind_query_body(query);
        };
        let mut env = self.context.ctes.clone();
        let mut declared = Vec::new();
        for cte in &with.cte_tables {
            let (entry, body) = self.bind_cte(cte, &env, with.recursive);
            declared.push(Cte {
                name: entry.name.clone(),
                body,
            });
            env.push(entry);
        }
        let (body, scope) = self.in_ctes(env, |b| b.bind_query_body(query));
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
        &mut self,
        cte: &SqlCte,
        env: &[CteDecl],
        recursive: bool,
    ) -> (CteDecl, LogicalPlan) {
        let name = cte.alias.name.clone();
        let inner_env = if recursive {
            let provisional = match cte.query.body.as_ref() {
                SetExpr::SetOperation { left, .. } => {
                    // This binds the anchor only to learn its column shape; the
                    // real bind below binds it again, so discard any diagnostics
                    // it raises here — otherwise they'd be reported twice.
                    let saved = self.diagnostics.len();
                    let columns = self
                        .in_ctes(env.to_vec(), |b| b.bind_set_expr(left))
                        .1
                        .exposed(Some(&cte.alias));
                    self.diagnostics.truncate(saved);
                    columns
                }
                _ => Exposed::default(),
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
        let (mut plan, scope) = self.in_ctes(inner_env, |b| b.bind_query(&cte.query));
        let columns = scope.exposed(Some(&cte.alias));
        // An explicit `c (x, y)` column list renames the body's output columns
        // so a reference through the CTE traces to them.
        rename_outputs(&mut plan, &alias_column_names(&cte.alias));
        (CteDecl { name, columns }, plan)
    }

    /// Bind a query's body, its pipe-operator chain, and trailing ORDER BY (the
    /// `WITH` is already in scope via `self.context.ctes`).
    pub(super) fn bind_query_body(&mut self, query: &Query) -> (LogicalPlan, Scope) {
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
                    outputs_complete: scope.outputs_complete,
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
    /// CALL / PIVOT / set-op / JOIN) wraps a non-feeding read [`Filter`];
    /// RENAME / DROP / AS reshape the running outputs (a positional
    /// re-projection / an aliasing boundary); the rest (sampling / unpivot)
    /// pass through. The match is exhaustive so a new pipe operator is
    /// reviewed here.
    pub(super) fn bind_pipe(
        &mut self,
        op: &PipeOperator,
        input: LogicalPlan,
        scope: &mut Scope,
    ) -> LogicalPlan {
        match op {
            PipeOperator::Select { exprs } => {
                let (new, complete) = self.bind_output_items(exprs, scope);
                let (node, query_outputs) = self.pipe_project(input, &[], new);
                scope.query_outputs = query_outputs;
                // A pipe SELECT replaces the outputs wholesale.
                scope.outputs_complete = complete;
                node
            }
            PipeOperator::Extend { exprs } => {
                // The new columns see the running outputs; then they append.
                let (new, complete) = self.bind_output_items(exprs, scope);
                let base = std::mem::take(&mut scope.query_outputs);
                let (node, query_outputs) = self.pipe_project(input, &base, new);
                scope.query_outputs = query_outputs;
                // EXTEND appends to the running outputs, so both must be
                // determinate.
                scope.outputs_complete &= complete;
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
                        names: OutputNames::Single(
                            e.expr.alias.clone().or_else(|| inferred_name(&e.expr.expr)),
                        ),
                        expr: self.bind_expr(&e.expr.expr, scope),
                    })
                    .collect();
                let (node, query_outputs) = self.pipe_project(input, &[], new);
                scope.query_outputs = query_outputs;
                // AGGREGATE replaces the outputs with its own (wildcard-free)
                // items.
                scope.outputs_complete = true;
                node
            }
            PipeOperator::Where { expr } => {
                let reads = vec![self.bind_expr(expr, scope)];
                self.pipe_filter(input, reads)
            }
            PipeOperator::Limit { expr, offset } => {
                let mut reads = vec![self.bind_expr(expr, scope)];
                reads.extend(offset.iter().map(|o| self.bind_expr(o, scope)));
                self.pipe_filter(input, reads)
            }
            PipeOperator::OrderBy { exprs } => {
                let reads = exprs
                    .iter()
                    .map(|o| self.bind_expr(&o.expr, scope))
                    .collect();
                self.pipe_filter(input, reads)
            }
            PipeOperator::Call { function, .. } => {
                let reads = self.bind_function_args(function, scope);
                self.pipe_filter(input, reads)
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
            // `|> JOIN t ON … / USING (…)`: the joined relation enters the
            // scope — the ON predicate and every later stage resolve it, and
            // its `USING` / NATURAL merge columns fan in like a FROM-clause
            // join (it used to stay out of scope, leaving the ON's right-side
            // references unresolved). The running outputs gain the right
            // side's columns after the left block (identity: they're base
            // columns a later reference re-reads); a right side whose
            // columns can't be enumerated makes the outputs incomplete.
            PipeOperator::Join(j) => {
                // The running slot count *before* the join — the bind-time
                // truth a positional trace above the join splits at
                // ([`Join::left_width`]). Unknowable when the running
                // outputs are incomplete (a suppressed wildcard may hide
                // slots), so record nothing and let the trace refuse.
                let left_width = scope.outputs_complete.then_some(scope.query_outputs.len());
                let (right, right_scope) = self.bind_table_factor(&j.relation, &scope.relations);
                // The NATURAL branch mirrors `bind_table_with_joins` for
                // future-proofing, but is unreachable today: sqlparser 0.62
                // rejects `|> NATURAL JOIN` at parse time (verified).
                let merge = if join_is_natural(&j.join_operator) {
                    self.natural_merge_columns(scope, &right_scope)
                } else {
                    join_using(&j.join_operator)
                };
                match pipe_join_output_cols(&right_scope.relations) {
                    Some(cols) if merge.is_empty() => scope.query_outputs.extend(cols),
                    // A `USING` / NATURAL join coalesces the merge columns
                    // into join-structure-dependent positions (the standard
                    // puts them first) that a flat left-then-right
                    // concatenation misstates — mark the outputs incomplete
                    // instead, the same refusal `scope_star_slots` makes for
                    // a bare `*` over a merged scope.
                    _ => scope.outputs_complete = false,
                }
                scope.absorb(right_scope);
                scope.add_merge_columns(merge);
                let on = join_on(&j.join_operator)
                    .map(|e| self.bind_expr(e, scope))
                    .into_iter()
                    .collect();
                pipe_join(input, right, on, left_width)
            }
            // `|> RENAME old AS new, …`: re-project the running outputs with
            // the mapped slots renamed. The new name is an introduced alias,
            // not the physical column, so its identity drops — a later
            // reference to it traces through the projection instead of
            // falling to the base relation (which fabricated a phantom
            // read). With incomplete outputs the slot list is unknown, so
            // the operator passes through unchanged (the unexpanded-`*`
            // diagnostic already flags the gap); a mapping naming no known
            // output is ignored, best-effort.
            PipeOperator::Rename { mappings } => {
                if !scope.outputs_complete {
                    return input;
                }
                let mut outputs = std::mem::take(&mut scope.query_outputs);
                for m in mappings {
                    if let Some(o) = outputs.iter_mut().find(|o| {
                        o.name
                            .as_ref()
                            .is_some_and(|n| self.eq(self.style.casing.column, n, &m.ident))
                    }) {
                        *o = OutputCol {
                            name: Some(m.alias.clone()),
                            identity: false,
                        };
                    }
                }
                let node = LogicalPlan::Projection(Projection {
                    input: Box::new(input),
                    exprs: passthrough_exprs(&outputs),
                });
                scope.query_outputs = outputs;
                node
            }
            // `|> DROP col, …`: re-project without the dropped slots. Each
            // kept slot's `DerivedSlot` keeps its *base* position (the slot
            // indices below the projection don't move); only the exposed
            // positions compact. Incomplete outputs pass through, as above.
            PipeOperator::Drop { columns } => {
                if !scope.outputs_complete {
                    return input;
                }
                let kept: Vec<(usize, OutputCol)> = std::mem::take(&mut scope.query_outputs)
                    .into_iter()
                    .enumerate()
                    .filter(|(_, o)| {
                        !columns.iter().any(|c| {
                            o.name
                                .as_ref()
                                .is_some_and(|n| self.eq(self.style.casing.column, n, c))
                        })
                    })
                    .collect();
                let exprs = kept
                    .iter()
                    .map(|(index, o)| NamedExpr {
                        names: OutputNames::Single(o.name.clone()),
                        expr: Expr::DerivedSlot {
                            qualifier: None,
                            index: *index,
                        },
                    })
                    .collect();
                scope.query_outputs = kept.into_iter().map(|(_, o)| o).collect();
                LogicalPlan::Projection(Projection {
                    input: Box::new(input),
                    exprs,
                })
            }
            // `|> AS u`: alias the whole running result, like a derived
            // table's alias — the original relation qualifiers stop
            // resolving (BigQuery drops them) and `u.col` addresses the
            // running outputs through the `SubqueryAlias` boundary.
            PipeOperator::As { alias } => {
                scope.relations = vec![Relation::Derived {
                    alias: Some(alias.clone()),
                    columns: Exposed {
                        slots: scope.query_outputs.iter().map(|o| o.name.clone()).collect(),
                        complete: scope.outputs_complete,
                    },
                }];
                LogicalPlan::SubqueryAlias(SubqueryAlias {
                    alias: alias.clone(),
                    input: Box::new(input),
                })
            }
            // No inspectable column expressions: a sampling clause or
            // unpivot.
            PipeOperator::TableSample { .. } | PipeOperator::Unpivot { .. } => input,
        }
    }

    /// Bind a list of output items (SELECT / RETURNING / pipe projection
    /// items). The second return reports whether the list's slot count is
    /// determinate — `false` when any wildcard stayed unexpanded.
    pub(super) fn bind_output_items(
        &mut self,
        items: &[SelectItem],
        scope: &Scope,
    ) -> (Vec<NamedExpr>, bool) {
        let mut exprs = Vec::new();
        let mut complete = true;
        for item in items {
            let (bound, determinate) = self.bind_select_item(item, scope);
            complete &= determinate;
            exprs.extend(bound);
        }
        (exprs, complete)
    }

    /// [`bind_output_items`](Self::bind_output_items) with wildcard expansion
    /// suppressed — see
    /// [`bind_select_item_inner`](Self::bind_select_item_inner).
    pub(super) fn bind_output_items_unexpanded(
        &mut self,
        items: &[SelectItem],
        scope: &Scope,
    ) -> (Vec<NamedExpr>, bool) {
        let mut exprs = Vec::new();
        let mut complete = true;
        for item in items {
            let (bound, determinate) = self.bind_select_item_inner(item, scope, false);
            complete &= determinate;
            exprs.extend(bound);
        }
        (exprs, complete)
    }

    /// Build an output-producing pipe `Projection`: the positional passthrough
    /// of the `base` outputs plus the `new` value columns. The passthrough
    /// keeps each base `OutputCol` verbatim (name *and* identity — so a later
    /// clause reference to an identity output still re-reads its real
    /// column); only the `new` items mint fresh output metadata.
    pub(super) fn pipe_project(
        &mut self,
        input: LogicalPlan,
        base: &[OutputCol],
        new: Vec<NamedExpr>,
    ) -> (LogicalPlan, Vec<OutputCol>) {
        let mut query_outputs = base.to_vec();
        query_outputs.extend(self.output_cols(&new));
        let mut exprs = passthrough_exprs(base);
        exprs.extend(new);
        (
            LogicalPlan::Projection(Projection {
                input: Box::new(input),
                exprs,
            }),
            query_outputs,
        )
    }

    /// `|> SET col = expr`: each assignment replaces a same-named base output in
    /// place (else appends), so a SET after a SELECT rewrites that column.
    pub(super) fn pipe_set(
        &mut self,
        input: LogicalPlan,
        base: Vec<OutputCol>,
        assignments: &[sqlparser::ast::Assignment],
        scope: &Scope,
    ) -> (LogicalPlan, Vec<OutputCol>) {
        let mut exprs = passthrough_exprs(&base);
        let mut query_outputs = base;
        for a in assignments {
            for column in assignment_target_columns(&a.target) {
                let ne = NamedExpr {
                    names: OutputNames::Single(Some(column.clone())),
                    expr: self.bind_expr(&a.value, scope),
                };
                let replaced = self.output_cols(std::slice::from_ref(&ne)).remove(0);
                // Pipe outputs are always single-named passthroughs.
                match exprs.iter_mut().position(|e| {
                    matches!(&e.names, OutputNames::Single(Some(n))
                        if self.eq(self.style.casing.column, n, &column))
                }) {
                    Some(i) => {
                        exprs[i] = ne;
                        query_outputs[i] = replaced;
                    }
                    None => {
                        exprs.push(ne);
                        query_outputs.push(replaced);
                    }
                }
            }
        }
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

    pub(super) fn bind_set_expr(&mut self, body: &SetExpr) -> (LogicalPlan, Scope) {
        match body {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(query) => self.bind_query(query),
            SetExpr::Values(values) => self.bind_values(values),
            // `TABLE foo`: a whole-table query body (e.g. the source of
            // `CREATE TABLE t AS TABLE foo`). A bare name checks the CTE
            // environment first, like a FROM factor (`WITH c AS (…) TABLE c`
            // reads the CTE, not a phantom table `c`); else a read scan.
            SetExpr::Table(table) => match table_set_expr_ref(table) {
                Some(written) => match self.bind_cte_by_name(&written, None) {
                    Some(bound) => bound,
                    None => self.bind_named_table(&written, None),
                },
                None => (LogicalPlan::Empty, Scope::default()),
            },
            // `WITH … INSERT/UPDATE/DELETE/MERGE …`: the DML statement is the
            // query body (the parser wraps a CTE-prefixed DML this way). Bind
            // it to its DML root. Its RETURNING projection is the output the
            // body exposes — a data-modifying CTE's consumer reads exactly
            // those columns (`WITH c AS (INSERT … RETURNING a) SELECT a FROM
            // c`) — so the scope carries them like a SELECT's outputs; with
            // no RETURNING the body exposes nothing.
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => {
                let plan = self.bind_statement(statement);
                let scope = self.dml_returning_scope(statement, &plan);
                (plan, scope)
            }
            // A set operation: result columns are the left operand's (names
            // from the left, positional merge). The result is positionally
            // determinate only when *every* branch is — a right branch with an
            // unexpanded wildcard shifts its positions, so a positional
            // consumer (wildcard expansion over this as a derived table, DML
            // pairing) must not trust the left count alone.
            SetExpr::SetOperation { left, right, .. } => {
                let (l, mut scope) = self.bind_set_expr(left);
                let (r, right_scope) = self.bind_set_expr(right);
                scope.outputs_complete &= right_scope.outputs_complete;
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
    pub(super) fn bind_values(&mut self, values: &SqlValues) -> (LogicalPlan, Scope) {
        // `rows` is `Vec<Parens<Vec<Expr>>>`; each `Parens` derefs to its inner
        // row `Vec`, so `row.len()` is the column count.
        let width = values.rows.iter().map(|row| row.len()).max().unwrap_or(0);
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
            LogicalPlan::Values(Values {
                rows,
                // Declared column names arrive later, via `rename_outputs`,
                // when an alias list exposes the row set as a relation.
                columns: Vec::new(),
            }),
            Scope {
                relations: Vec::new(),
                query_outputs,
                // A VALUES row set's width is determinate (no wildcards).
                outputs_complete: true,
                merge_columns: Vec::new(),
            },
        )
    }

    /// Bind a SELECT into the canonical operator chain `Scan → WHERE →
    /// Aggregate (GROUP BY) → HAVING → Projection → Sort (ORDER BY)`,
    /// returning the operator and its clause scope (FROM relations + projection
    /// outputs) for a trailing ORDER BY to resolve against. The projection
    /// resolves against the FROM scope (so a grouped column is a base read,
    /// counted, not traced through the `Aggregate`); the grouping / HAVING /
    /// ORDER BY clauses resolve against the FROM relations *plus* the projection
    /// outputs (clause-alias visibility) — a resolution-scope rule, independent
    /// of tree position.
    pub(super) fn bind_select(&mut self, select: &Select) -> (LogicalPlan, Scope) {
        let (mut from, mut scope) = self.bind_from(&select.from);
        // Hive `LATERAL VIEW explode(arr) v [AS c, …]`: each view is a
        // lateral table function joined onto the FROM. Its argument reads
        // against the relations so far (the FROM plus the earlier views),
        // and it joins in as a table-function relation — with a declared
        // column-alias list, a *closed* one (`Derived`), so a reference to
        // a generated column resolves to the view and traces to the
        // function's arguments at function granularity, instead of falling
        // to a base relation as a phantom read. (With several views, a
        // bare generated column still resolves to the one view listing it,
        // but the name-keyed trace reaches every view's arguments — the
        // usual function-granularity coarseness.)
        for lv in &select.lateral_views {
            let args = vec![self.bind_expr(&lv.lateral_view, &scope)];
            let alias = lv
                .lateral_view_name
                .0
                .last()
                .and_then(|p| p.as_ident().cloned());
            let node = LogicalPlan::TableFunction(TableFunction {
                alias: alias.clone(),
                input: Box::new(LogicalPlan::Empty),
                args,
            });
            let relation = if lv.lateral_col_alias.is_empty() {
                Relation::TableFunction { alias }
            } else {
                Relation::Derived {
                    alias,
                    columns: Exposed {
                        slots: lv.lateral_col_alias.iter().cloned().map(Some).collect(),
                        complete: true,
                    },
                }
            };
            scope.relations.push(relation);
            from = combine(from, node);
        }
        // WHERE + the WHERE-family auxiliary clauses (DISTINCT ON / TOP /
        // LATERAL VIEW / PREWHERE / CONNECT BY / CLUSTER BY / named WINDOW)
        // filter rows before grouping — a filter over the FROM (no output
        // aliases visible). QUALIFY is post-projection (handled below).
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
        // The projection resolves against the FROM scope (base reads). An
        // *empty* projection is the FROM-first / projection-less form
        // (DuckDB / ClickHouse `FROM t`, a pipe base) — an implicit
        // `SELECT *`: expand it like a written bare `*`, and when the scope
        // isn't completely known, mark the outputs incomplete and flag
        // (previously the zero-item projection passed as a *complete* empty
        // output list, silently claiming the query projects nothing).
        let (exprs, outputs_complete) = if select.projection.is_empty() {
            match self.expand_implicit_star(&scope) {
                Some(items) => (items, true),
                None => {
                    self.record_wildcard_suppressed(
                        "implicit `SELECT *` (FROM-first select)",
                        Span::empty(),
                    );
                    (Vec::new(), false)
                }
            }
        } else if select.exclude.is_some() {
            // Redshift's select-level `EXCLUDE` filters the projected set
            // *after* the items — expanding a wildcard while ignoring it
            // would read the excluded columns, so expansion is suppressed
            // and flagged (all-or-nothing, exactly like the per-wildcard
            // `EXCLUDE` modifier); written items bind as usual.
            let (exprs, _) = self.bind_output_items_unexpanded(&select.projection, &scope);
            (exprs, false)
        } else {
            self.bind_output_items(&select.projection, &scope)
        };
        let clause_scope = scope.with_query_outputs(self.output_cols(&exprs), outputs_complete);
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
        // QUALIFY filters on window / projection outputs (it runs after the
        // window functions the projection computes), so it sees output aliases
        // — bind it against `clause_scope` like HAVING, so an alias reference
        // (`QUALIFY rn = 1`) binds `Derived` and drops from reads rather than
        // surfacing a phantom base column. Filter-position: reads, no lineage.
        if let Some(qualify) = &select.qualify {
            node = LogicalPlan::Filter(Filter {
                input: Box::new(node),
                predicate: vec![self.bind_expr(qualify, &clause_scope)],
            });
        }
        // `DISTINCT ON (keys)` picks one row per key group over the
        // *projected* rows — PostgreSQL 18 (verified): an output alias is
        // visible to the keys and *shadows* a same-named base column, like
        // ORDER BY — so the keys bind against `clause_scope`: an identity
        // output re-reads its column, an introduced alias binds `Derived`
        // (the dependency is at the projection). Filter-position reads.
        if let Some(Distinct::On(on)) = &select.distinct {
            node = LogicalPlan::Filter(Filter {
                input: Box::new(node),
                predicate: self.bind_exprs(on, &clause_scope),
            });
        }
        // SORT BY (Hive) sees the outputs, like a trailing ORDER BY.
        let sort_keys = self.order_by_expr_keys(&select.sort_by, &clause_scope);
        if !sort_keys.is_empty() {
            node = sort(node, sort_keys);
        }
        // `SELECT … INTO t` is *not* wrapped here: `INTO` rides the leading
        // SELECT but targets the whole query (over a UNION it creates one table
        // from the combined result), and it's only a real create at the
        // statement root. The wrap happens once in `bind_statement` so a nested
        // SELECT can't leak a `CreateTableAs` mid-tree (which the write walkers,
        // peeling only a leading `WITH`, would miss).
        (node, clause_scope)
    }

    pub(super) fn bind_from(&mut self, items: &[TableWithJoins]) -> (LogicalPlan, Scope) {
        let mut iter = items.iter();
        let Some(first) = iter.next() else {
            return (LogicalPlan::Empty, Scope::default());
        };
        let (mut node, mut scope) = self.bind_table_with_joins(first, &[]);
        // Comma-separated FROM items are a cross join; a later item sees the
        // earlier ones only if it is LATERAL. Exception: after an item whose
        // join chain ends in an `ARRAY JOIN`, a comma continues the ARRAY
        // JOIN's *operand list* (ClickHouse grammar — a table can't follow;
        // the construct itself pins the dialect family), but sqlparser
        // parses each further operand as an independent FROM item, which
        // surfaced `arr2` in `ARRAY JOIN arr1 AS a, arr2 AS b` as a phantom
        // table read. A join-carrying or non-table item can't be an operand
        // and binds as usual.
        let mut in_array_join = ends_with_array_join(first);
        for twj in iter {
            if in_array_join
                && twj.joins.is_empty()
                && matches!(twj.relation, TableFactor::Table { .. })
            {
                let (right, right_scope) = self.bind_array_join(&twj.relation, &scope.relations);
                scope.absorb(right_scope);
                node = join(node, right, Vec::new());
                continue;
            }
            in_array_join = ends_with_array_join(twj);
            let (right, right_scope) = self.bind_table_with_joins(twj, &scope.relations);
            scope.absorb(right_scope);
            node = join(node, right, Vec::new());
        }
        (node, scope)
    }

    /// `left` are the FROM siblings to this item's left, visible to a LATERAL
    /// factor (and to a joined factor, after the preceding join inputs).
    pub(super) fn bind_table_with_joins(
        &mut self,
        twj: &TableWithJoins,
        left: &[Relation],
    ) -> (LogicalPlan, Scope) {
        let (mut node, mut scope) = self.bind_table_factor(&twj.relation, left);
        for j in &twj.joins {
            // A joined LATERAL factor sees the left siblings plus the join
            // inputs accumulated so far.
            let visible: Vec<Relation> = left.iter().chain(&scope.relations).cloned().collect();
            // ClickHouse `ARRAY JOIN`'s operand is an array column being
            // unnested, not a scanned table — bind it against the visible
            // relations so it reads a column, not a table.
            let (right, right_scope) = if is_array_join(&j.join_operator) {
                self.bind_array_join(&j.relation, &visible)
            } else if is_apply(&j.join_operator) {
                // A T-SQL `CROSS / OUTER APPLY` factor is lateral by
                // construction, but sqlparser parses it with `lateral: false`
                // — so push the visible relations as an enclosing level for
                // the whole factor (a derived body and a table function's
                // arguments both correlate to the left rows).
                self.in_outer(visible.clone(), scope.merge_columns.clone(), |b| {
                    b.bind_table_factor(&j.relation, &visible)
                })
            } else {
                self.bind_table_factor(&j.relation, &visible)
            };
            // Merge columns — an unqualified reference to one fans in to both
            // sides. An explicit `USING (col)` names them; a NATURAL join takes
            // the two sides' schema-common columns (so it needs a catalog —
            // computed before the right scope is absorbed into the left).
            let merge = if join_is_natural(&j.join_operator) {
                self.natural_merge_columns(&scope, &right_scope)
            } else {
                join_using(&j.join_operator)
            };
            scope.absorb(right_scope);
            scope.add_merge_columns(merge);
            // The ON predicate resolves against both sides' columns.
            let on = join_on(&j.join_operator)
                .map(|e| self.bind_expr(e, &scope))
                .into_iter()
                .collect();
            node = join(node, right, on);
        }
        (node, scope)
    }

    /// Bind a ClickHouse `ARRAY JOIN <operand> [AS alias]` right side. sqlparser
    /// parses the operand as a `TableFactor::Table`, but it is an array
    /// *expression* (typically a column of a visible relation) being unnested —
    /// not a scanned table. Resolve its name as a column read against `visible`
    /// and bind it as an opaque unnest (a [`TableFunction`], so no table read),
    /// exposing the unnested output under its alias / column name.
    pub(super) fn bind_array_join(
        &mut self,
        factor: &TableFactor,
        visible: &[Relation],
    ) -> (LogicalPlan, Scope) {
        let (args, alias_name) = match factor {
            // `ARRAY JOIN f(args) [AS m]`: an array-producing *expression* (e.g.
            // `arrayMap(…)`). Its argument expressions carry the reads — the
            // function name is not a column — and the unnested output is the
            // alias, if any.
            TableFactor::Table {
                alias,
                args: Some(fn_args),
                ..
            } => {
                let bound =
                    self.bind_function_arg_list(&fn_args.args, &Scope::from_relations(visible));
                (bound, alias.as_ref().map(|a| a.name.clone()))
            }
            // `ARRAY JOIN arr [AS x]` / `ARRAY JOIN t.arr`: a plain array
            // *column*. Read its name as a column of the visible relations.
            TableFactor::Table { name, alias, .. } => {
                let parts: Vec<Ident> = name
                    .0
                    .iter()
                    .filter_map(|p| p.as_ident().cloned())
                    .collect();
                // The unnested output takes the explicit alias, else the
                // operand's own (last) name — so a later `arr` reference resolves
                // to the unnested element, not the source array.
                let alias_name = alias
                    .as_ref()
                    .map(|a| a.name.clone())
                    .or_else(|| parts.last().cloned());
                let args = if parts.is_empty() {
                    Vec::new()
                } else {
                    vec![self.resolve_expr(&parts, &Scope::from_relations(visible))]
                };
                (args, alias_name)
            }
            // Any other factor shape (a derived subquery, nested join, …) is not
            // valid ClickHouse ARRAY JOIN syntax, but sqlparser accepts it — bind
            // it through the normal factor path (best-effort: keep its reads and
            // alias) rather than silently dropping it.
            _ => return self.bind_table_factor(factor, visible),
        };
        let node = LogicalPlan::TableFunction(TableFunction {
            alias: alias_name.clone(),
            input: Box::new(LogicalPlan::Empty),
            args,
        });
        // Expose the unnested output as a synthetic (Derived) column so a
        // reference to it (`x` / `arr`) binds to the unnest — not falling through
        // to a real table as a phantom column. It is dropped from reads (the
        // operand read is counted once at the argument) and traces to the
        // operand's origins: the unnested element's lineage reaches the
        // source array column.
        let scope = match alias_name {
            Some(name) => Scope::single(Relation::Derived {
                alias: None,
                // The unnest exposes just the element column; the relation's
                // real shape is dynamic, so the view is not `complete` (a bare
                // `*` over an ARRAY JOIN scope stays unexpanded).
                columns: Exposed {
                    slots: vec![Some(name)],
                    complete: false,
                },
            }),
            None => Scope::default(),
        };
        (node, scope)
    }

    /// A NATURAL join's merge columns: the column names exposed by *both* sides
    /// (the schema intersection, case-folded). Catalog-only — a side with no
    /// known column list (a catalog-free table, an opaque table function)
    /// contributes nothing, so a catalog-free NATURAL join yields no merge
    /// columns (an unqualified reference stays ambiguous rather than fanning in).
    pub(super) fn natural_merge_columns(&self, left: &Scope, right: &Scope) -> Vec<Ident> {
        let right_columns: Vec<&Ident> = right
            .relations
            .iter()
            .flat_map(|r| r.known_columns())
            .collect();
        let mut common: Vec<Ident> = Vec::new();
        for column in left.relations.iter().flat_map(|r| r.known_columns()) {
            let in_right = right_columns
                .iter()
                .any(|r| self.eq(self.style.casing.column, column, r));
            let already = common
                .iter()
                .any(|c| self.eq(self.style.casing.column, c, column));
            if in_right && !already {
                common.push(column.clone());
            }
        }
        common
    }

    /// The output scope a DML query body (a data-modifying CTE) exposes:
    /// its RETURNING projection's columns, empty without one. Completeness
    /// is conservative — a RETURNING *wildcard written in the SQL* marks the
    /// outputs incomplete even when it expanded (the expansion flag isn't
    /// carried on the plan), so a positional consumer (an outer `SELECT *`
    /// over the CTE) suppresses rather than trusts a possibly-partial list.
    fn dml_returning_scope(&self, statement: &Statement, plan: &LogicalPlan) -> Scope {
        let returning = match plan {
            LogicalPlan::Insert(i) => &i.returning,
            LogicalPlan::Update(u) => &u.returning,
            LogicalPlan::Delete(d) => &d.returning,
            LogicalPlan::Merge(m) => &m.returning,
            _ => return Scope::default(),
        };
        if returning.is_empty() {
            return Scope::default();
        }
        let ast_items = match statement {
            Statement::Insert(i) => i.returning.as_deref(),
            Statement::Update(u) => u.returning.as_deref(),
            Statement::Delete(d) => d.returning.as_deref(),
            Statement::Merge(m) => m.output.as_ref().map(|o| match o {
                sqlparser::ast::OutputClause::Output { select_items, .. }
                | sqlparser::ast::OutputClause::Returning { select_items, .. } => {
                    select_items.as_slice()
                }
            }),
            _ => None,
        };
        let has_wildcard = ast_items.into_iter().flatten().any(|item| {
            matches!(
                item,
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..)
            )
        });
        Scope::default().with_query_outputs(self.output_cols(returning), !has_wildcard)
    }

    /// Bind a bare named table into a read `Scan` plus a single-relation scope
    /// (a unique catalog hit canonicalises + supplies columns + `Cataloged`;
    /// else open + `Inferred` / `Ambiguous`). Shared by the table factor and a
    /// `TABLE foo` query body.
    pub(super) fn bind_named_table(
        &mut self,
        written: &TableReference,
        alias: Option<Ident>,
    ) -> (LogicalPlan, Scope) {
        let m = self.table_match(written);
        let columns = Columns::from_catalog(m.columns);
        let scan = LogicalPlan::Scan(Scan {
            table: m.table.clone(),
            resolution: m.resolution,
        });
        let relation = Relation::Table {
            alias,
            table: m.table,
            columns,
        };
        (scan, Scope::single(relation))
    }

    /// A bare name matching an in-scope CTE resolves to a `CteRef` (the body
    /// lives once on the owning `With`) exposing the CTE's output columns as
    /// a synthetic relation — so the CTE name never surfaces as a table read.
    /// `None` when the name is qualified or matches no CTE (a base table).
    /// Shared by the table factor and a `TABLE foo` query body.
    fn bind_cte_by_name(
        &mut self,
        written: &TableReference,
        alias_name: Option<Ident>,
    ) -> Option<(LogicalPlan, Scope)> {
        if written.schema.is_some() || written.catalog.is_some() {
            return None;
        }
        let cte = self
            .context
            .ctes
            .iter()
            .rev()
            .find(|c| self.eq(self.style.casing.table_alias, &c.name, &written.name))?;
        let relation = Relation::Derived {
            alias: alias_name.clone().or_else(|| Some(cte.name.clone())),
            columns: cte.columns.clone(),
        };
        Some((
            LogicalPlan::CteRef(CteRef {
                name: cte.name.clone(),
                alias: alias_name,
            }),
            Scope::single(relation),
        ))
    }

    pub(super) fn bind_table_factor(
        &mut self,
        factor: &TableFactor,
        left: &[Relation],
    ) -> (LogicalPlan, Scope) {
        match factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                // `foo(args)` in FROM is a table-valued function, not a base
                // table — bind it as an opaque table-producing factor (like
                // UNNEST / `TableFactor::Function`): its produced columns are
                // dynamic, so a reference through its alias is a synthetic source
                // and the function name is *not* a real table read. The argument
                // expressions read against the sibling scope.
                if let Some(args) = args {
                    let bound =
                        self.bind_function_arg_list(&args.args, &Scope::from_relations(left));
                    return self.opaque(LogicalPlan::Empty, bound, alias.as_ref());
                }
                let Some(written) = self.table_ref(name) else {
                    return (LogicalPlan::Empty, Scope::default());
                };
                let alias_name = alias.as_ref().map(|a| a.name.clone());
                if let Some(bound) = self.bind_cte_by_name(&written, alias_name.clone()) {
                    return bound;
                }
                self.bind_named_table(&written, alias_name)
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
                    // Only the sibling relations are in hand here (the
                    // enclosing scope's merge columns don't thread through
                    // `bind_table_factor`), so a LATERAL body sees them
                    // without any fan-in — a pre-existing limit.
                    self.in_outer(left.to_vec(), Vec::new(), |b| b.bind_query(subquery))
                } else {
                    self.bind_query(subquery)
                };
                let columns = sub_scope.exposed(alias.as_ref());
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
                columns,
                alias,
                ..
            } => {
                let (inner, inner_scope) = self.bind_table_factor(table, left);
                // `value` (the new value column) and `name` (the new name
                // column) are *generated* output column names, not source
                // columns — only the IN-list `columns` are read. (PIVOT differs:
                // its `value_column` is an existing source column, so that arm
                // binds it.)
                let args = columns
                    .iter()
                    .map(|c| self.bind_expr(&c.expr, &inner_scope))
                    .collect();
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
        &mut self,
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
                vec![Expr::Subquery {
                    plan: Box::new(self.bind_subquery(query, scope)),
                    output: 0,
                }]
            }
        }
    }
}

/// The positional passthrough of the running pipe outputs: one
/// [`Expr::DerivedSlot`] per base output — **not** a read (the physical read
/// is already counted at the producing stage, keeping reads
/// occurrence-based), traced by position (so an anonymous output keeps its
/// slot instead of being dropped and shifting the ones after it). The output
/// name rides on the item; the base `OutputCol` itself is carried forward by
/// the callers.
fn passthrough_exprs(base: &[OutputCol]) -> Vec<NamedExpr> {
    base.iter()
        .enumerate()
        .map(|(index, o)| NamedExpr {
            names: OutputNames::Single(o.name.clone()),
            expr: Expr::DerivedSlot {
                qualifier: None,
                index,
            },
        })
        .collect()
}

/// The output columns a pipe `|> JOIN`'s right side appends to the running
/// outputs — `None` when they can't be enumerated (an `Unknown` catalog-free
/// table, an opaque table function, an incompletely-known derived relation),
/// which makes the running outputs incomplete instead of presenting a
/// partial list as the whole one.
fn pipe_join_output_cols(relations: &[Relation]) -> Option<Vec<OutputCol>> {
    let mut out = Vec::new();
    for rel in relations {
        match rel {
            Relation::Table {
                columns: Columns::Cataloged(cols),
                ..
            } => out.extend(cols.iter().map(|c| OutputCol {
                name: Some(c.clone()),
                identity: true,
            })),
            Relation::Derived {
                columns:
                    Exposed {
                        slots,
                        complete: true,
                    },
                ..
            } => out.extend(slots.iter().map(|n| OutputCol {
                name: n.clone(),
                identity: false,
            })),
            Relation::Table {
                columns: Columns::Unknown,
                ..
            }
            | Relation::Derived { .. }
            | Relation::TableFunction { .. } => return None,
        }
    }
    Some(out)
}
