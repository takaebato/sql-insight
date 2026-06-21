//! DML / DDL statement roots: the `bind_*` methods that turn an INSERT /
//! UPDATE / DELETE / MERGE / CREATE / ALTER / DROP `Statement` into its
//! [`LogicalPlan`] root, plus the write-target resolution helpers (DELETE
//! multi-target identity, RETURNING projection, ON CONFLICT).

use super::*;

impl<'a> Binder<'a> {
    pub(super) fn bind_statement(&self, statement: &Statement) -> LogicalPlan {
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
                    target: self.table_match(&written).table,
                    columns: Vec::new(),
                    input: Box::new(LogicalPlan::Empty),
                    schema_source: None,
                }),
                None => LogicalPlan::Empty,
            },
            Statement::Truncate(truncate) => LogicalPlan::Drop(Drop {
                targets: truncate
                    .table_names
                    .iter()
                    .filter_map(|t| self.table_ref(&t.name))
                    .map(|written| self.table_match(&written).table)
                    .collect(),
            }),
            _ => LogicalPlan::Empty,
        }
    }

    /// Bind a top-level query, applying a leading `SELECT … INTO t` as a
    /// `CreateTableAs` over the *whole* result (over a UNION it creates one
    /// table from the combined branches). Only the statement root creates a
    /// table — `INTO` nested in a subquery / CTE body isn't valid SQL there, so
    /// it's ignored rather than leaking a mid-tree `CreateTableAs` that the
    /// write walkers (which peel only a leading `WITH`) would miss.
    fn bind_query_into(&self, query: &Query) -> LogicalPlan {
        let plan = self.bind_query(query).0;
        let Some(name) = leading_select_into(&query.body) else {
            return plan;
        };
        let Some(written) = self.table_ref(name) else {
            return plan;
        };
        LogicalPlan::CreateTableAs(CreateTableAs {
            target: self.table_match(&written).table,
            columns: Vec::new(),
            input: Box::new(plan),
            schema_source: None,
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
    pub(super) fn bind_insert(&self, insert: &SqlInsert) -> LogicalPlan {
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
            return self.bind_insert_set(insert, target);
        }
        let (input, scope) = match &insert.source {
            Some(source) => self.bind_query(source),
            None => (LogicalPlan::Empty, Scope::default()),
        };
        // A column-less INSERT fills from the target's catalog columns — reuse
        // the list from the `table_match` above rather than re-matching via
        // `catalog_columns`. They are quote-wrapped for folded resolution; the
        // written column list takes the plain identifier.
        let columns = if insert.columns.is_empty() {
            m.columns
                .iter()
                .take(scope.query_outputs.len())
                .map(|c| Ident::new(&c.value))
                .collect()
        } else {
            insert.columns.clone()
        };
        // A column-list-less INSERT whose target columns can't be filled (no
        // catalog) drops its column writes / lineage — flag it so the empty
        // surfaces read as "couldn't analyze", not "nothing written".
        if columns.is_empty() && insert.source.is_some() {
            self.record_insert_columns_unresolved(&target);
        }
        // An *explicit* target column list whose count differs from the source
        // query's projected columns: relation lineage zips to the shorter side,
        // silently dropping the surplus. Flag it. (Only when the source exposes
        // a determinate projection — a `VALUES` / pure-wildcard source yields no
        // operands here and is covered elsewhere.)
        if !insert.columns.is_empty() {
            if let Some((outputs, _)) = output_operands(&input).first() {
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
            target,
            columns,
            input: Box::new(input),
            returning,
            on_conflict,
            conflict_predicate,
        })
    }

    /// MySQL `INSERT INTO t SET col = expr, …`: bind the assignment form like a
    /// single-row UPDATE — each assignment is a value column named by its target
    /// (resolved against the target's own columns), placed in a `Projection` so the
    /// `value → target.col` lineage reuses the relation-lineage machinery.
    pub(super) fn bind_insert_set(
        &self,
        insert: &SqlInsert,
        target: TableReference,
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
            target,
            columns,
            input: Box::new(LogicalPlan::Projection(Projection {
                input: Box::new(LogicalPlan::Empty),
                exprs,
            })),
            returning,
            on_conflict,
            conflict_predicate,
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
        &self,
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
        let bound = assignments
            .iter()
            .flat_map(|a| {
                assignment_target_columns(&a.target)
                    .into_iter()
                    .map(move |column| (column, &a.value))
            })
            .map(|(column, value)| Assignment {
                target: column,
                value: self.bind_expr(value, &scope),
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
    pub(super) fn bind_update(&self, update: &SqlUpdate) -> LogicalPlan {
        let Some((target_relation, target)) = self.target_relation(&update.table.relation) else {
            return LogicalPlan::Empty;
        };
        let mut scope = Scope::single(target_relation);
        let mut input = LogicalPlan::Empty;
        // Joins on the UPDATE target clause are read relations.
        for j in &update.table.joins {
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
        // SET assignments resolve against the target + FROM scope.
        let assignments = update
            .assignments
            .iter()
            .flat_map(|a| {
                assignment_target_columns(&a.target)
                    .into_iter()
                    .map(move |column| (column, &a.value))
            })
            .map(|(column, value)| Assignment {
                target: column,
                value: self.bind_expr(value, &scope),
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
    pub(super) fn bind_delete(&self, delete: &SqlDelete) -> LogicalPlan {
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
    pub(super) fn bind_merge(&self, merge: &SqlMerge) -> LogicalPlan {
        let Some((target_relation, target)) = self.target_relation(&merge.table) else {
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
                    if let MergeInsertKind::Values(values) = &insert.kind {
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
                        if columns.is_empty() && !row.is_empty() {
                            self.record_insert_columns_unresolved(&target);
                        }
                        clauses.push(MergeClause::Insert {
                            columns,
                            values: row,
                        });
                    }
                }
                MergeAction::Update(update) => {
                    for predicate in [&update.update_predicate, &update.delete_predicate]
                        .into_iter()
                        .flatten()
                    {
                        on.push(self.bind_expr(predicate, &scope));
                    }
                    let assignments = update
                        .assignments
                        .iter()
                        .flat_map(|a| {
                            assignment_target_columns(&a.target)
                                .into_iter()
                                .map(move |column| (column, &a.value))
                        })
                        .map(|(column, value)| Assignment {
                            target: column,
                            value: self.bind_expr(value, &scope),
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
            target,
            source: Box::new(source),
            on,
            clauses,
        })
    }

    /// Resolve a DML target table factor into its scope relation (for
    /// resolving SET / WHERE against the target's columns) and its canonical
    /// write-target identity. Returns `None` for a non-table factor.
    pub(super) fn target_relation(
        &self,
        factor: &TableFactor,
    ) -> Option<(Relation, TableReference)> {
        // Reuse the table-factor binder for catalog matching / column
        // knowledge, then discard the scan — the target is named on the DML
        // root, not read.
        let (_scan, scope) = self.bind_table_factor(factor, &[]);
        let relation = scope.relations.into_iter().next()?;
        match &relation {
            Relation::Table { table, .. } => {
                let target = table.clone();
                Some((relation, target))
            }
            Relation::Derived { .. } | Relation::TableFunction { .. } => None,
        }
    }

    /// The plain-table deletion targets of a FROM `TableWithJoins` (its
    /// relation plus any joined relations), catalog-canonicalised.
    pub(super) fn twj_table_targets(&self, twj: &TableWithJoins) -> Vec<TableReference> {
        std::iter::once(&twj.relation)
            .chain(twj.joins.iter().map(|join| &join.relation))
            .filter_map(|factor| TableReference::try_from(factor).ok())
            .map(|written| self.table_match(&written).table)
            .collect()
    }

    /// Resolve an explicit `DELETE` target name to its real table: a
    /// single-segment name may be a FROM alias (or the bare name of an
    /// in-scope relation), so consult the scope first; otherwise canonicalise
    /// it as written.
    pub(super) fn resolve_delete_target(
        &self,
        name: &ObjectName,
        scope: &Scope,
    ) -> Option<TableReference> {
        let written = self.table_ref(name)?;
        Some(
            self.scope_target(&written, scope)
                .unwrap_or_else(|| self.table_match(&written).table),
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
    ) -> Option<TableReference> {
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
                matches.then(|| table.clone())
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

    /// Bind a `RETURNING` clause's projected columns against `scope` — a value
    /// projection over the written relation, like a SELECT list, so each item
    /// contributes target reads and a `QueryOutput` lineage edge. A wildcard is
    /// suppressed (its diagnostic is a later brick).
    pub(super) fn bind_returning(
        &self,
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
    pub(super) fn bind_create_table(&self, create: &CreateTable) -> LogicalPlan {
        let Some(written) = self.table_ref(&create.name) else {
            return LogicalPlan::Empty;
        };
        let target = self.table_match(&written).table;
        let Some(query) = create.query.as_ref() else {
            // No `AS <query>`: a target-only create. `LIKE src` / `CLONE src`
            // copies another table's shape — capture that source so it still
            // surfaces in the flat table list (it is a structural reference,
            // not a row-data read; see `CreateTableAs::schema_source`).
            let schema_source = create
                .like
                .as_ref()
                .map(|k| match k {
                    CreateTableLikeKind::Plain(l) | CreateTableLikeKind::Parenthesized(l) => {
                        &l.name
                    }
                })
                .or(create.clone.as_ref())
                .and_then(|name| self.table_ref(name))
                .map(|src| self.table_match(&src).table);
            return LogicalPlan::CreateTableAs(CreateTableAs {
                target,
                columns: Vec::new(),
                input: Box::new(LogicalPlan::Empty),
                schema_source,
            });
        };
        let (input, _) = self.bind_query(query);
        let columns: Vec<Ident> = create.columns.iter().map(|c| c.name.clone()).collect();
        self.flag_anonymous_relation_columns(&target, &columns, &input);
        LogicalPlan::CreateTableAs(CreateTableAs {
            target,
            columns,
            input: Box::new(input),
            schema_source: None,
        })
    }

    /// `CREATE VIEW v AS <query>`: like CTAS — `columns` is the explicit column
    /// list only (empty when none); the implicit source-output names are
    /// resolved at the write / lineage surface.
    pub(super) fn bind_create_view(&self, create: &SqlCreateView) -> LogicalPlan {
        let Some(written) = self.table_ref(&create.name) else {
            return LogicalPlan::Empty;
        };
        let target = self.table_match(&written).table;
        let (input, _) = self.bind_query(&create.query);
        let columns: Vec<Ident> = create.columns.iter().map(|c| c.name.clone()).collect();
        self.flag_anonymous_relation_columns(&target, &columns, &input);
        LogicalPlan::CreateView(CreateView {
            target,
            columns,
            input: Box::new(input),
        })
    }

    /// `ALTER VIEW v AS <query>`: a view replacement — bound like
    /// [`bind_create_view`](Self::bind_create_view) (`columns` = the explicit
    /// list only).
    pub(super) fn bind_alter_view(
        &self,
        name: &ObjectName,
        columns: &[Ident],
        query: &Query,
    ) -> LogicalPlan {
        let Some(written) = self.table_ref(name) else {
            return LogicalPlan::Empty;
        };
        let target = self.table_match(&written).table;
        let (input, _) = self.bind_query(query);
        self.flag_anonymous_relation_columns(&target, columns, &input);
        LogicalPlan::CreateView(CreateView {
            target,
            columns: columns.to_vec(),
            input: Box::new(input),
        })
    }

    /// `ALTER TABLE t <ops>`: the altered table is a write target; each
    /// column-naming operation contributes its column(s) as writes (RENAME /
    /// CHANGE surface both names). Schema-level ops name no columns. No reads
    /// or lineage — ALTER restructures, it doesn't move row data.
    pub(super) fn bind_alter_table(&self, alter: &SqlAlterTable) -> LogicalPlan {
        let Some(written) = self.table_ref(&alter.name) else {
            return LogicalPlan::Empty;
        };
        let target = self.table_match(&written).table;
        let columns = alter
            .operations
            .iter()
            .flat_map(alter_table_op_target_columns)
            .collect();
        LogicalPlan::AlterTable(AlterTable { target, columns })
    }

    /// `DROP TABLE/VIEW/MATERIALIZED VIEW a, b`: the dropped relations are
    /// write targets. Other object types (index / schema / …) name no
    /// relations — unbound (`LogicalPlan::Empty`).
    pub(super) fn bind_drop(
        &self,
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
        let targets = names
            .iter()
            .chain(table)
            .filter_map(|name| self.table_ref(name))
            .map(|written| self.table_match(&written).table)
            .collect();
        LogicalPlan::Drop(Drop { targets })
    }
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
