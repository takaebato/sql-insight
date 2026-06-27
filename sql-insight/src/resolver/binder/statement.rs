//! DML / DDL statement roots: the `bind_*` methods that turn an INSERT /
//! UPDATE / DELETE / MERGE / CREATE / ALTER / DROP `Statement` into its
//! [`LogicalPlan`] root, plus the write-target resolution helpers (DELETE
//! multi-target identity, RETURNING projection, ON CONFLICT).

use super::*;

impl<'a> Binder<'a> {
    pub(super) fn bind_statement(&mut self, statement: &Statement) -> LogicalPlan {
        match statement {
            Statement::Query(query) => self.bind_query_into(query),
            Statement::Insert(insert) => self.bind_insert(insert),
            Statement::Update(update) => self.bind_update(update),
            Statement::Delete(delete) => self.bind_delete(delete),
            Statement::Merge(merge) => self.bind_merge(merge),
            Statement::CreateTable(create) => self.bind_create_table(create),
            Statement::CreateView(create) => self.bind_create_view(create),
            Statement::AlterView {
                name,
                columns,
                query,
                ..
            } => self.bind_alter_view(name, columns, query),
            Statement::AlterTable(alter) => self.bind_alter_table(alter),
            Statement::Drop {
                object_type,
                names,
                table,
                ..
            } => self.bind_drop(object_type, names, table.as_ref()),
            // `CREATE VIRTUAL TABLE t USING module(…)`: a new table with no
            // inspectable source — a target-only create.
            Statement::CreateVirtualTable { name, .. } => match self.table_ref(name) {
                Some(written) => LogicalPlan::CreateTableAs(CreateTableAs {
                    target: self.table_write(&written),
                    columns: Vec::new(),
                    input: Box::new(LogicalPlan::Empty),
                    schema_source: None,
                    source_wildcard: false,
                }),
                None => LogicalPlan::Empty,
            },
            Statement::Truncate(truncate) => {
                let written: Vec<_> = truncate
                    .table_names
                    .iter()
                    .filter_map(|t| self.table_ref(&t.name))
                    .collect();
                LogicalPlan::Drop(Drop {
                    targets: written.iter().map(|w| self.table_write(w)).collect(),
                })
            }
            _ => LogicalPlan::Empty,
        }
    }

    /// Bind a top-level query, applying a leading `SELECT … INTO t` as a
    /// `CreateTableAs` over the *whole* result (over a UNION it creates one
    /// table from the combined branches). Only the statement root creates a
    /// table — `INTO` nested in a subquery / CTE body isn't valid SQL there, so
    /// it's ignored rather than leaking a mid-tree `CreateTableAs` that the
    /// write walkers (which peel only a leading `WITH`) would miss.
    fn bind_query_into(&mut self, query: &Query) -> LogicalPlan {
        let plan = self.bind_query(query).0;
        let Some(name) = leading_select_into(&query.body) else {
            return plan;
        };
        let Some(written) = self.table_ref(name) else {
            return plan;
        };
        let target = self.table_write(&written);
        // `SELECT … INTO` lowers to a CTAS with no explicit column list, so an
        // unaliased source expression (`SELECT a + 1 INTO t`) is an unnameable
        // column dropped from `writes` / `lineage` — flag it, like the
        // `CREATE TABLE … AS` path. (No explicit list, so no arity check.)
        let source_wildcard = source_has_wildcard(query);
        self.diagnose_created_columns(&target.reference, &[], &plan, source_wildcard);
        LogicalPlan::CreateTableAs(CreateTableAs {
            target,
            columns: Vec::new(),
            input: Box::new(plan),
            schema_source: None,
            source_wildcard,
        })
    }

    /// `INSERT INTO target (columns) <source>`: the source query's plan is the
    /// read-carrying `input`, whose output columns pair positionally with the
    /// target columns for relation lineage. An explicit column list wins;
    /// otherwise the target's catalog columns fill in, truncated to the
    /// source's arity (so a column-less `INSERT … SELECT a, b` writes the
    /// target's first two columns). A `VALUES` source binds to
    /// [`LogicalPlan::Values`] (the rows are reads, but synthesise no traceable
    /// output, so there is no column lineage). RETURNING / ON CONFLICT / the
    /// MySQL `SET` form are later bricks.
    ///
    /// The Hive `PARTITION (…)` spec (`insert.partitioned`) is intentionally
    /// not extracted: a partition clause is write-side metadata whose value is
    /// normally a constant (`PARTITION (dt = '2020-01-01')`) or a dynamic
    /// column name (`PARTITION (dt)`), contributing no read / lineage
    /// dependency — and a partition value has no FROM scope to resolve a column
    /// reference against, so binding it would surface a bogus target-column
    /// read and an unresolved ref rather than a real edge. A non-trivial value
    /// expression is dropped (not flagged: it isn't an analyzable-info loss the
    /// common, constant case would false-alarm on).
    pub(super) fn bind_insert(&mut self, insert: &SqlInsert) -> LogicalPlan {
        let name = match &insert.table {
            TableObject::TableName(name) => name,
            TableObject::TableFunction(function) => &function.name,
        };
        let Some(written) = self.table_ref(name) else {
            return LogicalPlan::Empty;
        };
        let m = self.table_match(&written);
        let target = m.table;
        // MySQL `INSERT INTO t SET col = expr, …`: the assignment form (no
        // VALUES / SELECT source) — each assignment is a value column named by
        // its target, like a single-row UPDATE.
        if insert.source.is_none() && !insert.assignments.is_empty() {
            return self.bind_insert_set(insert, target, m.resolution);
        }
        let (input, scope) = match &insert.source {
            Some(source) => self.bind_query(source),
            None => (LogicalPlan::Empty, Scope::default()),
        };
        // A wildcard in the source projection (`SELECT *, y`) leaves the column
        // count / positions indeterminate (wildcards aren't expanded), so
        // neither the arity check nor positional relation-lineage can trust the
        // visible outputs. Carried on `source_wildcard`; the arity check and the
        // lineage walker both skip when set, it words the diagnostic below, and
        // a column-list-less INSERT can't fill from the catalog under it (next).
        let source_wildcard = insert
            .source
            .as_ref()
            .is_some_and(|q| source_has_wildcard(q));
        // The written column list: an explicit `(a, b)` wins; otherwise a
        // column-less INSERT fills from the target's catalog columns (reusing
        // the `table_match` list above, plain identifiers), truncated to the
        // source's projected arity. But a wildcard source has indeterminate
        // arity — the `*` isn't in `query_outputs`, so the visible count is too
        // low — and the catalog columns can't be positionally paired: leave the
        // list empty so the drop + flag guard below fires rather than
        // mis-truncating to the undercounted outputs.
        let columns = if !insert.columns.is_empty() {
            insert.columns.clone()
        } else if source_wildcard {
            Vec::new()
        } else {
            m.columns
                .iter()
                .take(scope.query_outputs.len())
                .map(|c| Ident::new(&c.value))
                .collect()
        };
        // A column-list-less INSERT whose target columns can't be determined
        // drops its column writes / lineage — flag it so the empty surfaces
        // read as "couldn't analyze", not "nothing written". The cause is the
        // wildcard when present (a catalog wouldn't help), else a missing
        // catalog.
        if columns.is_empty() && insert.source.is_some() {
            self.record_insert_columns_unresolved(&target, source_wildcard);
        }
        // An *explicit* target column list whose count differs from the source
        // query's projected columns: relation lineage zips to the shorter side,
        // silently dropping the surplus. Flag it. (Only when the source exposes
        // a determinate projection — a `VALUES` / pure-wildcard source yields no
        // operands here, and a wildcard-bearing source is indeterminate.)
        if !insert.columns.is_empty() && !source_wildcard {
            if let Some(operand) = output_operands(&input).first() {
                let outputs = operand.outputs;
                if !outputs.is_empty() && outputs.len() != columns.len() {
                    self.record_insert_columns_arity_mismatch(
                        &target,
                        columns.len(),
                        outputs.len(),
                    );
                }
            }
        }
        // ON CONFLICT DO UPDATE / ON DUPLICATE KEY UPDATE: extra writes + their
        // `value → target.col` lineage, plus the optional `DO UPDATE … WHERE`
        // (filter reads).
        let (on_conflict, conflict_predicate) = match &insert.on {
            Some(on) => self.bind_conflict(on, &target, &columns),
            None => (Vec::new(), Vec::new()),
        };
        // RETURNING resolves against the target alone (the source query's
        // scope is already popped).
        let returning = self.bind_returning(&insert.returning, &self.target_scope(&target));
        LogicalPlan::Insert(Insert {
            target: TableWrite {
                reference: target,
                resolution: m.resolution,
            },
            columns,
            input: Box::new(input),
            returning,
            on_conflict,
            conflict_predicate,
            source_wildcard,
        })
    }

    /// MySQL `INSERT INTO t SET col = expr, …`: bind the assignment form like a
    /// single-row UPDATE — each assignment is a value column named by its target
    /// (resolved against the target's own columns), placed in a `Projection` so the
    /// `value → target.col` lineage reuses the relation-lineage machinery.
    pub(super) fn bind_insert_set(
        &mut self,
        insert: &SqlInsert,
        target: TableReference,
        resolution: ResolutionKind,
    ) -> LogicalPlan {
        let scope = self.target_scope(&target);
        let mut columns = Vec::new();
        let mut exprs = Vec::new();
        for assignment in &insert.assignments {
            for column in assignment_target_columns(&assignment.target) {
                exprs.push(NamedExpr {
                    name: Some(column.clone()),
                    expr: self.bind_expr(&assignment.value, &scope),
                });
                columns.push(column);
            }
        }
        let (on_conflict, conflict_predicate) = match &insert.on {
            Some(on) => self.bind_conflict(on, &target, &columns),
            None => (Vec::new(), Vec::new()),
        };
        let returning = self.bind_returning(&insert.returning, &scope);
        LogicalPlan::Insert(Insert {
            target: TableWrite {
                reference: target,
                resolution,
            },
            columns,
            input: Box::new(LogicalPlan::Projection(Projection {
                input: Box::new(LogicalPlan::Empty),
                exprs,
            })),
            returning,
            on_conflict,
            conflict_predicate,
            // The MySQL SET form has no source query, so no wildcard.
            source_wildcard: false,
        })
    }

    /// Bind an INSERT's conflict action. PG / SQLite `ON CONFLICT DO UPDATE`
    /// puts the `EXCLUDED` pseudo-table in scope — a synthetic relation
    /// exposing the target columns, so `EXCLUDED.col` resolves to a `Derived`
    /// ref the traversal maps back to the source's like-positioned output.
    /// MySQL `ON DUPLICATE KEY UPDATE` has no EXCLUDED (its `VALUES(col)`
    /// self-references the target). Returns the conflict assignments (extra
    /// writes + lineage) and the optional `DO UPDATE … WHERE` (filter reads).
    pub(super) fn bind_conflict(
        &mut self,
        on: &OnInsert,
        target: &TableReference,
        columns: &[Ident],
    ) -> (Vec<Assignment>, Vec<Expr>) {
        let (scope, assignments, selection) = match on {
            OnInsert::DuplicateKeyUpdate(assignments) => {
                (self.target_scope(target), assignments.as_slice(), None)
            }
            OnInsert::OnConflict(on_conflict) => match &on_conflict.action {
                OnConflictAction::DoUpdate(do_update) => {
                    let mut scope = self.target_scope(target);
                    scope.relations.push(Relation::Derived {
                        alias: Some(Ident::new("excluded")),
                        columns: columns.to_vec(),
                    });
                    (
                        scope,
                        do_update.assignments.as_slice(),
                        do_update.selection.as_ref(),
                    )
                }
                OnConflictAction::DoNothing => return (Vec::new(), Vec::new()),
            },
            // `OnInsert` is non-exhaustive; an unmodelled action is a no-op.
            _ => return (Vec::new(), Vec::new()),
        };
        // A conflict-action SET always targets the insert target's own columns.
        let bound = assignments
            .iter()
            .filter_map(|a| {
                let value = self.bind_expr(&a.value, &scope);
                let (target, target_resolution) =
                    self.assignment_target(&a.target, &scope, target)?;
                Some(Assignment {
                    target,
                    target_resolution,
                    value,
                })
            })
            .collect();
        let predicate = selection
            .map(|s| self.bind_expr(s, &scope))
            .into_iter()
            .collect();
        (bound, predicate)
    }

    /// `UPDATE target SET col = expr [FROM src] WHERE pred`: the target is in
    /// scope for resolving SET / WHERE but is the **write target** (named on
    /// `Update.target`), not a read scan — so `input` carries only the read
    /// relations (the target-clause joins and the `FROM` relations) plus the
    /// WHERE predicate as a `Filter`. The SET assignments are the value path
    /// (each `RHS → target.col` for lineage / writes). RETURNING / the MySQL
    /// multi-table form's exotic shapes are later bricks.
    pub(super) fn bind_update(&mut self, update: &SqlUpdate) -> LogicalPlan {
        // Flatten a parenthesized join target `UPDATE (t1 JOIN t2 …) SET …`:
        // the innermost table is the write target, the joins are read relations
        // — so the parenthesized form behaves like the non-paren MySQL
        // `UPDATE t1 JOIN t2 …` form (whose joins live on `update.table.joins`).
        let (target_factor, joins) = flatten_dml_target(&update.table);
        // The root's own resolution isn't needed here — each SET assignment
        // carries its write-target table's resolution (`assignment_target`).
        let Some((target_relation, target, _)) = self.target_relation(target_factor) else {
            return LogicalPlan::Empty;
        };
        let mut scope = Scope::single(target_relation);
        let mut input = LogicalPlan::Empty;
        // Joins on the UPDATE target clause are read relations.
        for j in joins {
            let (node, jscope) = self.bind_table_factor(&j.relation, &scope.relations);
            scope.relations.extend(jscope.relations);
            let on = join_on(&j.join_operator)
                .map(|e| self.bind_expr(e, &scope))
                .into_iter()
                .collect();
            input = join(input, node, on);
        }
        // FROM relations are reads (resolved against the target + joins so far).
        if let Some(from) = &update.from {
            let tables = match from {
                UpdateTableFromKind::BeforeSet(t) | UpdateTableFromKind::AfterSet(t) => t,
            };
            for twj in tables {
                let (node, fscope) = self.bind_table_with_joins(twj, &scope.relations);
                scope.relations.extend(fscope.relations);
                input = combine(input, node);
            }
        }
        // WHERE: a filter over the read input (picks which rows update; its
        // reads / subqueries do not feed the new value).
        if let Some(predicate) = &update.selection {
            input = LogicalPlan::Filter(Filter {
                input: Box::new(input),
                predicate: vec![self.bind_expr(predicate, &scope)],
            });
        }
        // SET assignments resolve against the target + FROM scope; each writes
        // its resolved target table (the root, or the relation a qualifier names
        // in a multi-table `UPDATE t1 JOIN t2 SET t2.col = …`).
        let assignments = update
            .assignments
            .iter()
            .filter_map(|a| {
                let value = self.bind_expr(&a.value, &scope);
                let (target, target_resolution) =
                    self.assignment_target(&a.target, &scope, &target)?;
                Some(Assignment {
                    target,
                    target_resolution,
                    value,
                })
            })
            .collect();
        // RETURNING resolves against the statement scope (target + FROM).
        let returning = self.bind_returning(&update.returning, &scope);
        LogicalPlan::Update(Update {
            target,
            assignments,
            input: Box::new(input),
            returning,
        })
    }

    /// `DELETE`: the deletion targets, plus the consulted read relations and
    /// the predicate as the `input`. The FROM clause's role depends on the
    /// shape (mirroring the resolver):
    ///   `DELETE FROM t`                → FROM is the (write) target
    ///   `DELETE FROM t1, t2 USING src` → FROM are targets, USING are reads
    ///   `DELETE t1, t2 FROM src`       → FROM are reads, the list are targets
    /// A target is in scope for the predicate but never scanned (so it isn't a
    /// read). There are no column writes / lineage — rows go wholesale.
    pub(super) fn bind_delete(&mut self, delete: &SqlDelete) -> LogicalPlan {
        let from_tables = match &delete.from {
            FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
        };
        let from_is_target = delete.tables.is_empty();
        let mut scope = Scope::default();
        let mut input = LogicalPlan::Empty;
        let mut targets = Vec::new();
        // USING relations are always reads. Bind first so an explicit target
        // alias can resolve against them.
        for twj in delete.using.iter().flatten() {
            let (node, uscope) = self.bind_table_with_joins(twj, &scope.relations);
            scope.relations.extend(uscope.relations);
            input = combine(input, node);
        }
        for twj in from_tables {
            if from_is_target {
                // The FROM relations are the deletion targets, in scope for the
                // predicate but not read. A target may be an alias into a USING
                // relation already bound above (`DELETE FROM t_alias USING real
                // AS t_alias`) — resolve it through the scope and don't re-bind;
                // otherwise it's a fresh target table.
                let resolved = TableReference::try_from(&twj.relation)
                    .ok()
                    .and_then(|written| self.scope_target(&written, &scope));
                if let Some(target) = resolved {
                    targets.push(target);
                } else {
                    let (_node, fscope) = self.bind_table_with_joins(twj, &scope.relations);
                    targets.extend(self.twj_table_targets(twj));
                    scope.relations.extend(fscope.relations);
                }
            } else {
                let (node, fscope) = self.bind_table_with_joins(twj, &scope.relations);
                scope.relations.extend(fscope.relations);
                input = combine(input, node);
            }
        }
        // An explicit `DELETE t1, … FROM …` list names the targets (each may be
        // a FROM alias, resolved through the scope).
        for name in &delete.tables {
            if let Some(target) = self.resolve_delete_target(name, &scope) {
                targets.push(target);
            }
        }
        if let Some(predicate) = &delete.selection {
            input = LogicalPlan::Filter(Filter {
                input: Box::new(input),
                predicate: vec![self.bind_expr(predicate, &scope)],
            });
        }
        // RETURNING resolves against the FROM / USING scope (which holds the
        // target).
        let returning = self.bind_returning(&delete.returning, &scope);
        LogicalPlan::Delete(Delete {
            targets,
            input: Box::new(input),
            returning,
        })
    }

    /// `MERGE INTO target USING source ON pred WHEN … THEN …`: the target
    /// (write, in scope but not scanned) and source (read) form the scope. The
    /// ON predicate and every per-clause / INSERT predicate are filter reads
    /// (non-feeding), folded onto `on`. Each WHEN action keeps its structure as
    /// a `MergeClause`: an UPDATE SET's `RHS → target.col` and an INSERT's
    /// `value → target.col` drive writes / lineage. A column-less INSERT fills
    /// from the catalog (empty without one — the values are then reads only).
    pub(super) fn bind_merge(&mut self, merge: &SqlMerge) -> LogicalPlan {
        // A parenthesized *join* target `MERGE INTO (t1 JOIN t2) …` can't be a
        // write target (you merge into one table) — flag it rather than
        // silently picking the first relation. A parenthesized *single* table
        // `(t1)` is fine and resolves below.
        if is_join_factor(&merge.table) {
            self.record_unsupported_merge_target(&merge.table);
            return LogicalPlan::Empty;
        }
        let Some((target_relation, target, target_resolution)) = self.target_relation(&merge.table)
        else {
            // A non-table MERGE target (derived table / subquery / table
            // function) can't be a write target — flag it rather than dropping
            // the whole statement silently (best-effort drop + flag).
            self.record_unsupported_merge_target(&merge.table);
            return LogicalPlan::Empty;
        };
        let mut scope = Scope::single(target_relation);
        let (source, source_scope) = self.bind_table_factor(&merge.source, &scope.relations);
        scope.relations.extend(source_scope.relations);

        let mut on = vec![self.bind_expr(&merge.on, &scope)];
        let mut clauses = Vec::new();
        for clause in &merge.clauses {
            if let Some(predicate) = &clause.predicate {
                on.push(self.bind_expr(predicate, &scope));
            }
            match &clause.action {
                MergeAction::Insert(insert) => {
                    if let Some(predicate) = &insert.insert_predicate {
                        on.push(self.bind_expr(predicate, &scope));
                    }
                    match &insert.kind {
                        MergeInsertKind::Values(values) => {
                            let explicit: Vec<Ident> = insert
                                .columns
                                .iter()
                                .filter_map(|n| n.0.last().and_then(|p| p.as_ident().cloned()))
                                .collect();
                            let columns = if explicit.is_empty() {
                                self.catalog_columns(&target)
                            } else {
                                explicit
                            };
                            // A MERGE INSERT is a single VALUES row.
                            let row: Vec<Expr> = values
                                .rows
                                .iter()
                                .flatten()
                                .map(|e| self.bind_expr(e, &scope))
                                .collect();
                            // Column-list-less and no catalog to fill the target
                            // columns: the values can't be paired (see `bind_insert`).
                            // A MERGE INSERT VALUES has no wildcard, so the cause
                            // is always the missing catalog.
                            if columns.is_empty() && !row.is_empty() {
                                self.record_insert_columns_unresolved(&target, false);
                            }
                            clauses.push(MergeClause::Insert {
                                columns,
                                values: row,
                            });
                        }
                        // BigQuery `INSERT ROW`: insert the full source row, with
                        // no explicit column / value lists. The column pairing
                        // isn't recoverable from SQL text, so push a column-less
                        // Insert — the target still surfaces (CRUD create +
                        // `table_lineage` source → target) while the column-level
                        // writes / lineage are a flagged coverage gap.
                        MergeInsertKind::Row => {
                            self.record_merge_insert_row_unresolved(&target);
                            clauses.push(MergeClause::Insert {
                                columns: Vec::new(),
                                values: Vec::new(),
                            });
                        }
                    }
                }
                MergeAction::Update(update) => {
                    for predicate in [&update.update_predicate, &update.delete_predicate]
                        .into_iter()
                        .flatten()
                    {
                        on.push(self.bind_expr(predicate, &scope));
                    }
                    // A MERGE WHEN UPDATE always targets the merge target's
                    // own columns.
                    let assignments = update
                        .assignments
                        .iter()
                        .filter_map(|a| {
                            let value = self.bind_expr(&a.value, &scope);
                            let (target, target_resolution) =
                                self.assignment_target(&a.target, &scope, &target)?;
                            Some(Assignment {
                                target,
                                target_resolution,
                                value,
                            })
                        })
                        .collect();
                    clauses.push(MergeClause::Update { assignments });
                }
                MergeAction::Delete { .. } => {
                    clauses.push(MergeClause::Delete);
                }
            }
        }
        LogicalPlan::Merge(Merge {
            target: TableWrite {
                reference: target,
                resolution: target_resolution,
            },
            source: Box::new(source),
            on,
            clauses,
        })
    }

    /// Resolve a DML target table factor into its scope relation (for
    /// resolving SET / WHERE against the target's columns) and its canonical
    /// write-target identity. Returns `None` for a non-table factor.
    pub(super) fn target_relation(
        &mut self,
        factor: &TableFactor,
    ) -> Option<(Relation, TableReference, ResolutionKind)> {
        // Reuse the table-factor binder for catalog matching / column
        // knowledge; the target isn't read (it's named on the DML root), but
        // the scan's `resolution` is the catalog match we keep for the write.
        let (scan, scope) = self.bind_table_factor(factor, &[]);
        let relation = scope.relations.into_iter().next()?;
        match &relation {
            Relation::Table { table, .. } => {
                let resolution = match &scan {
                    LogicalPlan::Scan(s) => s.resolution,
                    _ => ResolutionKind::Inferred,
                };
                let target = table.clone();
                Some((relation, target, resolution))
            }
            Relation::Derived { .. } | Relation::TableFunction { .. } => None,
        }
    }

    /// The plain-table deletion targets of a FROM `TableWithJoins` (its
    /// relation plus any joined relations), catalog-canonicalised.
    pub(super) fn twj_table_targets(&self, twj: &TableWithJoins) -> Vec<TableWrite> {
        std::iter::once(&twj.relation)
            .chain(twj.joins.iter().map(|join| &join.relation))
            .filter_map(|factor| TableReference::try_from(factor).ok())
            .map(|written| self.table_write(&written))
            .collect()
    }

    /// Resolve an explicit `DELETE` target name to its real table: a
    /// single-segment name may be a FROM alias (or the bare name of an
    /// in-scope relation), so consult the scope first; otherwise canonicalise
    /// it as written.
    pub(super) fn resolve_delete_target(
        &mut self,
        name: &ObjectName,
        scope: &Scope,
    ) -> Option<TableWrite> {
        let written = self.table_ref(name)?;
        Some(
            self.scope_target(&written, scope)
                .unwrap_or_else(|| self.table_write(&written)),
        )
    }

    /// If a `written` DELETE-target name matches an in-scope real-table
    /// relation by **merge identity**, return that relation's real table. An
    /// aliased relation matches a single-segment name against its alias; a
    /// non-aliased relation matches its full `catalog.schema.name` path exactly
    /// (so a bare `t1` merges with FROM `t1` but not FROM `mydb.t1`).
    pub(super) fn scope_target(
        &self,
        written: &TableReference,
        scope: &Scope,
    ) -> Option<TableWrite> {
        let canonical = self.table_match(written).table;
        scope.relations.iter().find_map(|relation| match relation {
            Relation::Table { table, alias, .. } => {
                let matches = match alias {
                    Some(alias) => {
                        written.schema.is_none()
                            && written.catalog.is_none()
                            && self.eq(self.style.casing.table_alias, alias, &written.name)
                    }
                    None => self.table_identity_eq(&canonical, table),
                };
                // Re-match the resolved real table for its catalog resolution
                // (the scope `Relation` carries identity, not resolution).
                matches.then(|| self.table_write(table))
            }
            Relation::Derived { .. } | Relation::TableFunction { .. } => None,
        })
    }

    /// Exact (not right-anchored) identity match of two table references under
    /// the dialect's table casing — every present segment must agree and a
    /// missing segment matches only a missing one.
    pub(super) fn table_identity_eq(&self, a: &TableReference, b: &TableReference) -> bool {
        let fold = self.style.casing.table;
        let seg_eq = |x: Option<&Ident>, y: Option<&Ident>| match (x, y) {
            (Some(p), Some(q)) => self.eq(fold, p, q),
            (None, None) => true,
            _ => false,
        };
        self.eq(fold, &a.name, &b.name)
            && seg_eq(a.schema.as_ref(), b.schema.as_ref())
            && seg_eq(a.catalog.as_ref(), b.catalog.as_ref())
    }

    /// A resolution scope holding just a write target (for `INSERT … RETURNING`,
    /// whose references resolve against the target alone — the source query's
    /// scope is already popped).
    pub(super) fn target_scope(&self, target: &TableReference) -> Scope {
        let m = self.table_match(target);
        let columns = if m.columns.is_empty() {
            Columns::Unknown
        } else {
            Columns::Cataloged(m.columns)
        };
        Scope::single(Relation::Table {
            alias: None,
            table: m.table,
            columns,
        })
    }

    /// Resolve a SET assignment's target to the column it writes, qualified by
    /// its **resolved table**: an unqualified column writes the DML `root`; a
    /// qualified `t2.col` (a multi-table `UPDATE t1 JOIN t2 SET t2.col = …`)
    /// writes whichever in-scope real table the qualifier names. Returns `None`
    /// — dropped (+ flagged by the caller) — for a tuple target (`SET (a,b)=…`,
    /// not modelled) or a qualifier that names no writable table (a derived
    /// table / CTE / unknown alias can't be a write target).
    fn assignment_target(
        &self,
        target: &AssignmentTarget,
        scope: &Scope,
        root: &TableReference,
    ) -> Option<(crate::reference::ColumnReference, ResolutionKind)> {
        let AssignmentTarget::ColumnName(name) = target else {
            return None; // tuple `SET (a, b) = …` — not modelled
        };
        let parts: Vec<Ident> = name
            .0
            .iter()
            .filter_map(|p| p.as_ident().cloned())
            .collect();
        let column = parts.last()?.clone();
        let table = if parts.len() == 1 {
            root.clone() // unqualified → the DML root target
        } else {
            let qualifier = &parts[..parts.len() - 1];
            scope
                .relations
                .iter()
                .find_map(|rel| self.writable_qualifier_table(rel, qualifier))?
        };
        // Re-match the resolved write-target table for its catalog resolution
        // (so the per-target table write carries it). `table` is canonical, so
        // this reproduces the root scan's / joined relation's resolution.
        let resolution = self.table_write(&table).resolution;
        Some((
            crate::reference::ColumnReference {
                table: Some(table),
                name: column,
            },
            resolution,
        ))
    }

    /// The table a SET qualifier names, iff it's a *writable* relation — a real
    /// table matched the way a column qualifier is (an aliased table by its
    /// alias, a non-aliased one right-anchored). A derived table / CTE / table
    /// function isn't writable, so it yields `None`.
    fn writable_qualifier_table(
        &self,
        rel: &Relation,
        qualifier: &[Ident],
    ) -> Option<TableReference> {
        match rel {
            Relation::Table {
                alias: Some(alias),
                table,
                ..
            } => matches!(qualifier, [q] if self.eq(self.style.casing.table_alias, q, alias))
                .then(|| table.clone()),
            Relation::Table {
                alias: None, table, ..
            } => TableReference::try_from_parts(qualifier)
                .filter(|q| self.qualifier_matches_table(q, table))
                .map(|_| table.clone()),
            Relation::Derived { .. } | Relation::TableFunction { .. } => None,
        }
    }

    /// Bind a `RETURNING` clause's projected columns against `scope` — a value
    /// projection over the written relation, like a SELECT list, so each item
    /// contributes target reads and a `QueryOutput` lineage edge. A wildcard is
    /// suppressed (its diagnostic is a later brick).
    pub(super) fn bind_returning(
        &mut self,
        returning: &Option<Vec<SelectItem>>,
        scope: &Scope,
    ) -> Vec<NamedExpr> {
        returning
            .iter()
            .flatten()
            .filter_map(|item| self.bind_select_item(item, scope))
            .collect()
    }

    /// `CREATE TABLE dst AS <query>` (CTAS): the source query's reads, paired
    /// with the new table's columns. `columns` carries only the *explicit*
    /// column list (`CREATE TABLE t (a, b) AS …`), empty when none is written;
    /// the implicit case (column names inherited from the source outputs, with
    /// anonymous outputs dropped) is resolved positionally at the write /
    /// lineage surface, so writes and lineage stay aligned with the source's
    /// outputs. A plain `CREATE TABLE t (cols)` (no query) is a target-only
    /// create — its column definitions aren't writes — so it binds with no
    /// columns / input.
    /// The `LIKE` / `CLONE` shape source of a target-only `CREATE TABLE`,
    /// catalog-matched as a [`TableRead`] (it's read either way). `copies_data`
    /// is `true` for `CLONE` (data copied → feeds lineage), `false` for `LIKE`
    /// (schema only). `None` if the source name isn't representable.
    fn schema_source(&mut self, name: &ObjectName, copies_data: bool) -> Option<SchemaSource> {
        let written = self.table_ref(name)?;
        let m = self.table_match(&written);
        Some(SchemaSource {
            source: TableRead {
                reference: m.table,
                resolution: m.resolution,
            },
            copies_data,
        })
    }

    pub(super) fn bind_create_table(&mut self, create: &CreateTable) -> LogicalPlan {
        let Some(written) = self.table_ref(&create.name) else {
            return LogicalPlan::Empty;
        };
        let m = self.table_match(&written);
        let target = m.table;
        let resolution = m.resolution;
        let Some(query) = create.query.as_ref() else {
            // No `AS <query>`: a target-only create. `LIKE src` copies only the
            // column definitions (schema, no rows); `CLONE src` copies the data
            // too. Both read `src`; only CLONE feeds `src → target` lineage
            // (see `SchemaSource`).
            let like_name = create.like.as_ref().map(|k| match k {
                CreateTableLikeKind::Plain(l) | CreateTableLikeKind::Parenthesized(l) => &l.name,
            });
            let schema_source = match (like_name, create.clone.as_ref()) {
                (Some(name), _) => self.schema_source(name, false),
                (None, Some(name)) => self.schema_source(name, true),
                (None, None) => None,
            };
            return LogicalPlan::CreateTableAs(CreateTableAs {
                target: TableWrite {
                    reference: target,
                    resolution,
                },
                columns: Vec::new(),
                input: Box::new(LogicalPlan::Empty),
                schema_source,
                source_wildcard: false,
            });
        };
        let (input, _) = self.bind_query(query);
        let columns: Vec<Ident> = create.columns.iter().map(|c| c.name.clone()).collect();
        let source_wildcard = source_has_wildcard(query);
        self.diagnose_created_columns(&target, &columns, &input, source_wildcard);
        LogicalPlan::CreateTableAs(CreateTableAs {
            target: TableWrite {
                reference: target,
                resolution,
            },
            columns,
            input: Box::new(input),
            schema_source: None,
            source_wildcard,
        })
    }

    /// `CREATE VIEW v AS <query>`: like CTAS — `columns` is the explicit column
    /// list only (empty when none); the implicit source-output names are
    /// resolved at the write / lineage surface.
    pub(super) fn bind_create_view(&mut self, create: &SqlCreateView) -> LogicalPlan {
        let Some(written) = self.table_ref(&create.name) else {
            return LogicalPlan::Empty;
        };
        let m = self.table_match(&written);
        let target = m.table;
        let (input, _) = self.bind_query(&create.query);
        let columns: Vec<Ident> = create.columns.iter().map(|c| c.name.clone()).collect();
        let source_wildcard = source_has_wildcard(&create.query);
        self.diagnose_created_columns(&target, &columns, &input, source_wildcard);
        LogicalPlan::CreateView(CreateView {
            target: TableWrite {
                reference: target,
                resolution: m.resolution,
            },
            columns,
            input: Box::new(input),
            source_wildcard,
        })
    }

    /// `ALTER VIEW v AS <query>`: a view replacement — bound like
    /// [`bind_create_view`](Self::bind_create_view) (`columns` = the explicit
    /// list only).
    pub(super) fn bind_alter_view(
        &mut self,
        name: &ObjectName,
        columns: &[Ident],
        query: &Query,
    ) -> LogicalPlan {
        let Some(written) = self.table_ref(name) else {
            return LogicalPlan::Empty;
        };
        let m = self.table_match(&written);
        let target = m.table;
        let (input, _) = self.bind_query(query);
        let source_wildcard = source_has_wildcard(query);
        self.diagnose_created_columns(&target, columns, &input, source_wildcard);
        LogicalPlan::CreateView(CreateView {
            target: TableWrite {
                reference: target,
                resolution: m.resolution,
            },
            columns: columns.to_vec(),
            input: Box::new(input),
            source_wildcard,
        })
    }

    /// `ALTER TABLE t <ops>`: the altered table is a write target; each
    /// column-naming operation contributes its column(s) as writes (RENAME /
    /// CHANGE surface both names). Schema-level ops name no columns. No reads
    /// or lineage — ALTER restructures, it doesn't move row data.
    pub(super) fn bind_alter_table(&mut self, alter: &SqlAlterTable) -> LogicalPlan {
        let Some(written) = self.table_ref(&alter.name) else {
            return LogicalPlan::Empty;
        };
        let m = self.table_match(&written);
        let columns = alter
            .operations
            .iter()
            .flat_map(alter_table_op_target_columns)
            .collect();
        LogicalPlan::AlterTable(AlterTable {
            target: TableWrite {
                reference: m.table,
                resolution: m.resolution,
            },
            columns,
        })
    }

    /// `DROP TABLE/VIEW/MATERIALIZED VIEW a, b`: the dropped relations are
    /// write targets. Other object types (index / schema / …) name no
    /// relations — unbound (`LogicalPlan::Empty`).
    pub(super) fn bind_drop(
        &mut self,
        object_type: &ObjectType,
        names: &[ObjectName],
        table: Option<&ObjectName>,
    ) -> LogicalPlan {
        if !matches!(
            object_type,
            ObjectType::Table | ObjectType::View | ObjectType::MaterializedView
        ) {
            return LogicalPlan::Empty;
        }
        let written: Vec<_> = names
            .iter()
            .chain(table)
            .filter_map(|name| self.table_ref(name))
            .collect();
        let targets = written.iter().map(|w| self.table_write(w)).collect();
        LogicalPlan::Drop(Drop { targets })
    }
}

/// Flatten a DML target's `TableWithJoins` through any parenthesised
/// (`NestedJoin`) wrapper: the innermost table factor is the write target, and
/// every join (inner-parens joins first, then the outer joins) is a read
/// relation — so a parenthesised `(t1 JOIN t2 …)` target behaves like the
/// non-paren `t1 JOIN t2 …` form.
fn flatten_dml_target(twj: &TableWithJoins) -> (&TableFactor, Vec<&sqlparser::ast::Join>) {
    let mut relation = &twj.relation;
    let mut joins: Vec<&sqlparser::ast::Join> = twj.joins.iter().collect();
    while let TableFactor::NestedJoin {
        table_with_joins, ..
    } = relation
    {
        joins = table_with_joins.joins.iter().chain(joins).collect();
        relation = &table_with_joins.relation;
    }
    (relation, joins)
}

/// Whether a table factor is a parenthesised *join* (two or more relations) —
/// a `(t1 JOIN t2)`, not a parenthesised single table `(t1)`. Used to reject a
/// join as a MERGE target.
fn is_join_factor(factor: &TableFactor) -> bool {
    match factor {
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => !table_with_joins.joins.is_empty() || is_join_factor(&table_with_joins.relation),
        _ => false,
    }
}

/// Whether a query's output projection contains an (unexpanded) wildcard
/// (`*` / `t.*`), anywhere a set operation's branches or a parenthesised
/// subquery reach. An INSERT source with one has an indeterminate column count
/// / positions, so positional pairing with the target columns can't be trusted
/// (see [`Insert::source_wildcard`](super::super::logical_plan::Insert)). The
/// `SetExpr` / `SelectItem` matches are exhaustive so a new variant forces a
/// decision here.
fn source_has_wildcard(query: &Query) -> bool {
    fn body_has_wildcard(body: &SetExpr) -> bool {
        match body {
            SetExpr::Select(select) => select.projection.iter().any(|item| match item {
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => true,
                SelectItem::UnnamedExpr(_) | SelectItem::ExprWithAlias { .. } => false,
            }),
            SetExpr::Query(q) => body_has_wildcard(&q.body),
            SetExpr::SetOperation { left, right, .. } => {
                body_has_wildcard(left) || body_has_wildcard(right)
            }
            SetExpr::Values(_)
            | SetExpr::Insert(_)
            | SetExpr::Update(_)
            | SetExpr::Delete(_)
            | SetExpr::Merge(_)
            | SetExpr::Table(_) => false,
        }
    }
    body_has_wildcard(&query.body)
}

/// The target table of a query's leading `SELECT … INTO t`, if any. `INTO`
/// rides the first SELECT — including the left branch of a top-level set
/// operation, where it targets the combined result — so this follows the left
/// spine. A non-leading SELECT (a right branch, a subquery) can't carry a
/// statement-level `INTO`, so those arms yield `None`.
fn leading_select_into(body: &SetExpr) -> Option<&ObjectName> {
    match body {
        SetExpr::Select(select) => select.into.as_ref().map(|into| &into.name),
        SetExpr::Query(query) => leading_select_into(&query.body),
        SetExpr::SetOperation { left, .. } => leading_select_into(left),
        SetExpr::Values(_)
        | SetExpr::Insert(_)
        | SetExpr::Update(_)
        | SetExpr::Delete(_)
        | SetExpr::Merge(_)
        | SetExpr::Table(_) => None,
    }
}
