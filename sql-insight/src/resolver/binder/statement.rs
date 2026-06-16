use super::*;

impl Binder<'_> {
    /// Bind a statement into a [`Plan`], or `None` for kinds not modelled
    /// yet. A query is bound directly; the data-moving statements
    /// (INSERT / UPDATE / DELETE / MERGE / CTAS / CREATE VIEW) produce a
    /// [`Write`]-rooted tree whose `input` carries every read (the source
    /// query plus any SET / predicate / VALUES reads).
    pub(super) fn bind_statement(&self, statement: &Statement) -> Option<Plan> {
        match statement {
            Statement::Query(query) => Some(self.bind_query(query).0),
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
            Statement::Truncate(truncate) => Some(Plan::Drop(
                truncate
                    .table_names
                    .iter()
                    .filter_map(|t| self.table_ref(&t.name))
                    .map(|t| self.canonical_target(t))
                    .collect(),
            )),
            // `CREATE VIRTUAL TABLE t USING module(…)`: the new table is a
            // write target with no inspectable source (classified
            // `CreateTable`, like a plain `CREATE TABLE`).
            Statement::CreateVirtualTable { name, .. } => {
                let target = self.canonical_target(self.table_ref(name)?);
                Some(Plan::Write(Write {
                    target,
                    target_columns: Vec::new(),
                    input: Box::new(Plan::OpaqueLeaf),
                    returning: Vec::new(),
                    conflict_updates: Vec::new(),
                }))
            }
            // Other DDL / session statements aren't data operations — not
            // bound (the wildcard mirrors `build`'s "unsupported → None").
            _ => None,
        }
    }

    /// `INSERT INTO target (cols) <source>`: the source query's plan is
    /// the read-carrying input; the target columns are the write targets.
    /// A `VALUES` source binds to an opaque leaf (no column reads).
    pub(super) fn bind_insert(&self, insert: &Insert) -> Option<Plan> {
        let name = match &insert.table {
            TableObject::TableName(name) => name,
            TableObject::TableFunction(function) => &function.name,
        };
        let target = self.canonical_target(self.table_ref(name)?);
        // MySQL `INSERT INTO t SET col = expr, …`: an UPDATE-style assignment
        // form with no VALUES / SELECT source. Each assignment is a value
        // column named by its target → writes + `RHS → target.col` lineage.
        if insert.source.is_none() && !insert.assignments.is_empty() {
            return Some(self.bind_insert_set(insert, target));
        }
        let (input, source_scope) = match &insert.source {
            Some(source) => self.bind_query(source),
            None => (Plan::OpaqueLeaf, Scope::empty()),
        };
        // An explicit column list wins; otherwise pair the source against
        // the target's catalog schema, up to the source's arity (so a
        // column-less `INSERT INTO t SELECT a, b` writes / traces `t`'s
        // first two columns). No catalog → no inferred columns.
        let target_columns = if insert.columns.is_empty() {
            self.catalog_columns(&target)
                .into_iter()
                .take(source_scope.outputs.len())
                .collect()
        } else {
            insert.columns.clone()
        };
        // ON CONFLICT DO UPDATE / ON DUPLICATE KEY UPDATE: a conflict-time
        // mini-UPDATE on the target. Its reads / sub-plans fold onto the
        // input; its assignments drive extra writes + lineage. EXCLUDED
        // collapses to the source only for a query source — a VALUES source
        // exposes no projection, so EXCLUDED stays opaque (a synthetic
        // self-reference), matching the resolver.
        let excluded_source = if source_has_projection(insert) {
            source_scope.clone()
        } else {
            Scope::empty()
        };
        let (conflict_updates, conflict_reads, conflict_subplans) = match &insert.on {
            Some(on) => self.bind_conflict(on, &target, &target_columns, &excluded_source),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let input = wrap_reads(input, conflict_reads, conflict_subplans);
        // RETURNING references resolve against the target alone (the source
        // query's scope is already popped).
        let returning = self.bind_returning(&insert.returning, &self.target_scope(&target));
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(input),
            returning,
            conflict_updates,
        }))
    }

    /// MySQL `INSERT INTO t SET col = expr, …`: bind the assignment form
    /// like a single-table `UPDATE` — each assignment is a value column
    /// named by its target (resolved against the target's own columns), so
    /// it writes that column and feeds `RHS → target.col` lineage. There is
    /// no FROM / WHERE; an `ON DUPLICATE KEY UPDATE` conflict action folds
    /// in as usual.
    pub(super) fn bind_insert_set(&self, insert: &Insert, target: TableReference) -> Plan {
        let scope = self.target_scope(&target);
        let mut outputs = Vec::new();
        let mut target_columns = Vec::new();
        let mut value_subqueries = Vec::new();
        let mut reads = Vec::new();
        let mut filter_subqueries = Vec::new();
        for assignment in &insert.assignments {
            for column in assignment_target_columns(&assignment.target) {
                let bound = self.bind_value_column(Some(column.clone()), &assignment.value, &scope);
                outputs.push(bound.column);
                value_subqueries.extend(bound.value_subplans);
                reads.extend(bound.filter_reads);
                filter_subqueries.extend(bound.filter_subplans);
                target_columns.push(column);
            }
        }
        let (conflict_updates, conflict_reads, conflict_subplans) = match &insert.on {
            Some(on) => self.bind_conflict(on, &target, &target_columns, &Scope::empty()),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        reads.extend(conflict_reads);
        filter_subqueries.extend(conflict_subplans);
        let returning = self.bind_returning(&insert.returning, &scope);
        Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(Plan::Project(Project {
                input: Box::new(wrap_reads(Plan::OpaqueLeaf, reads, filter_subqueries)),
                outputs,
                subqueries: value_subqueries,
            })),
            returning,
            conflict_updates,
        })
    }

    /// `UPDATE target SET col = expr [FROM src] WHERE pred`: the target
    /// (write) plus any FROM relations (read) form the scope. The SET
    /// assignments become a `Project` whose outputs are named by their
    /// target columns (so the lineage `RHS → target` pairing falls out of
    /// the same output-column machinery as INSERT / CTAS); the WHERE
    /// predicate is a filter `PassThrough` below it.
    pub(super) fn bind_update(&self, update: &Update) -> Option<Plan> {
        let (target_plan, mut scope) = self.bind_table_with_joins(&update.table, &Scope::empty());
        // The UPDATE target is in scope for resolving SET / WHERE, but it's
        // a write (reported via `Write.target`), not a table read.
        let mut inputs = vec![into_write_target(target_plan)];
        if let Some(from) = &update.from {
            let tables = match from {
                UpdateTableFromKind::BeforeSet(tables) | UpdateTableFromKind::AfterSet(tables) => {
                    tables
                }
            };
            for twj in tables {
                let (plan, from_scope) = self.bind_table_with_joins(twj, &scope);
                inputs.push(plan);
                scope = scope.merge(from_scope);
            }
        }
        // The WHERE predicate's reads / sub-plans are filter position (they
        // pick which rows update, they don't feed the new value); only a SET
        // RHS value sub-plan feeds the target.
        let (mut reads, mut filter_subqueries) = update
            .selection
            .as_ref()
            .map(|s| self.expr_reads(s, &scope))
            .unwrap_or_default();
        let mut value_subqueries = Vec::new();
        let mut outputs = Vec::new();
        let mut target_columns = Vec::new();
        for assignment in &update.assignments {
            for column in assignment_target_columns(&assignment.target) {
                let bound = self.bind_value_column(Some(column.clone()), &assignment.value, &scope);
                outputs.push(bound.column);
                value_subqueries.extend(bound.value_subplans);
                reads.extend(bound.filter_reads);
                filter_subqueries.extend(bound.filter_subplans);
                target_columns.push(column);
            }
        }
        let target = self.canonical_target(self.table_factor_ref(&update.table.relation)?);
        // RETURNING resolves against the statement scope (target + FROM).
        let returning = self.bind_returning(&update.returning, &scope);
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(Plan::Project(Project {
                input: Box::new(wrap_inputs(inputs, reads, filter_subqueries)),
                outputs,
                subqueries: value_subqueries,
            })),
            returning,
            conflict_updates: Vec::new(),
        }))
    }

    /// `DELETE`: bind every consulted relation as a scan with the right
    /// role and collect the deletion targets. The FROM clause's role
    /// depends on the statement shape (mirroring the resolver):
    ///   `DELETE FROM t`                → FROM is the (write) target
    ///   `DELETE FROM t1, t2 USING src` → FROM are targets, USING are reads
    ///   `DELETE t1, t2 FROM src`       → FROM are reads, the list are targets
    /// An explicit `DELETE alias FROM …` list resolves each name (possibly
    /// a FROM alias) to its real table through the FROM scope. There are no
    /// column writes / lineage — rows go wholesale; only `RETURNING`
    /// projects the deleted rows.
    pub(super) fn bind_delete(&self, delete: &Delete) -> Option<Plan> {
        let from_tables = match &delete.from {
            FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
        };
        // With no explicit `DELETE t, …` list the FROM relations are the
        // deletion targets (write role: in scope for the predicate /
        // RETURNING but not a read); with a list the FROM relations are all
        // reads and the list names the targets.
        let from_is_target = delete.tables.is_empty();
        let mut inputs = Vec::new();
        let mut scope = Scope::empty();
        let mut targets = Vec::new();
        // USING relations are always reads. Bind them first so an explicit
        // target alias can resolve against them too (alias-defining clause).
        for twj in delete.using.iter().flatten() {
            let (plan, twj_scope) = self.bind_table_with_joins(twj, &scope);
            inputs.push(plan);
            scope = scope.merge(twj_scope);
        }
        for twj in from_tables {
            if from_is_target {
                // The FROM relations are the deletion targets. A target may
                // be an alias into a USING relation already bound above
                // (`DELETE FROM t_alias USING real AS t_alias`) — or just the
                // same name as a USING relation — so resolve it through the
                // scope and don't re-bind. Otherwise it's a fresh target
                // table, bound write-role (in scope for the predicate /
                // RETURNING, but not a read).
                let resolved = TableReference::try_from(&twj.relation)
                    .ok()
                    .and_then(|written| self.scope_target(&written, &scope));
                if let Some(target) = resolved {
                    targets.push(target);
                } else {
                    let (plan, twj_scope) = self.bind_table_with_joins(twj, &scope);
                    targets.extend(self.twj_table_targets(twj));
                    inputs.push(into_write_target(plan));
                    scope = scope.merge(twj_scope);
                }
            } else {
                let (plan, twj_scope) = self.bind_table_with_joins(twj, &scope);
                inputs.push(plan);
                scope = scope.merge(twj_scope);
            }
        }
        for name in &delete.tables {
            if let Some(target) = self.resolve_delete_target(name, &scope) {
                targets.push(target);
            }
        }
        let (reads, subqueries) = delete
            .selection
            .as_ref()
            .map(|s| self.expr_reads(s, &scope))
            .unwrap_or_default();
        // RETURNING resolves against the FROM / USING scope (which holds the
        // target).
        let returning = self.bind_returning(&delete.returning, &scope);
        let input = wrap_inputs(inputs, reads, subqueries);
        Some(Plan::Delete(DeletePlan {
            input: Box::new(input),
            targets,
            returning,
        }))
    }

    /// The plain-table targets of a FROM `TableWithJoins` (its relation plus
    /// any joined relations), catalog-canonicalized. Used when the FROM
    /// clause *is* the deletion target (`DELETE FROM t1, t2 …`). Derived /
    /// table-function factors yield no target.
    pub(super) fn twj_table_targets(&self, twj: &TableWithJoins) -> Vec<TableReference> {
        std::iter::once(&twj.relation)
            .chain(twj.joins.iter().map(|join| &join.relation))
            .filter_map(|factor| TableReference::try_from(factor).ok())
            .map(|target| self.canonical_target(target))
            .collect()
    }

    /// Resolve an explicit `DELETE` target name to its real table: a
    /// single-segment name may be a FROM alias (or the bare name of an
    /// in-scope relation), so consult the scope first; otherwise
    /// canonicalize it as written.
    pub(super) fn resolve_delete_target(
        &self,
        name: &ObjectName,
        scope: &Scope,
    ) -> Option<TableReference> {
        let written = self.table_ref(name)?;
        Some(
            self.scope_target(&written, scope)
                .unwrap_or_else(|| self.canonical_target(written)),
        )
    }

    /// If a `written` DELETE-target name matches an in-scope real-table
    /// relation by **merge identity**, return that relation's real table.
    /// An aliased relation matches a single-segment name against its alias;
    /// a non-aliased relation matches its full `catalog.schema.name` path
    /// exactly — so a bare `t1` merges with FROM `t1` but **not** with FROM
    /// `mydb.t1` (we assume no default schema), matching the resolver.
    pub(super) fn scope_target(
        &self,
        written: &TableReference,
        scope: &Scope,
    ) -> Option<TableReference> {
        let canonical = self.canonical_target(written.clone());
        scope
            .relations
            .iter()
            .find_map(|relation| match &relation.source {
                RelationSource::Table { table, .. } => {
                    let matches = match &relation.alias {
                        Some(alias) => {
                            written.schema.is_none()
                                && written.catalog.is_none()
                                && self.ident_eq(alias, &written.name)
                        }
                        None => self.table_identity_eq(&canonical, table),
                    };
                    matches.then(|| table.clone())
                }
                _ => None,
            })
    }

    /// Exact (not right-anchored) identity match of two table references
    /// under the dialect's table casing — every present segment must agree
    /// and a missing segment matches only a missing one.
    pub(super) fn table_identity_eq(&self, a: &TableReference, b: &TableReference) -> bool {
        let fold = self.casing.table;
        let seg_eq = |x: Option<&Ident>, y: Option<&Ident>| match (x, y) {
            (Some(p), Some(q)) => fold.normalize(p) == fold.normalize(q),
            (None, None) => true,
            _ => false,
        };
        fold.normalize(&a.name) == fold.normalize(&b.name)
            && seg_eq(a.schema.as_ref(), b.schema.as_ref())
            && seg_eq(a.catalog.as_ref(), b.catalog.as_ref())
    }

    /// `MERGE INTO target USING source ON pred WHEN … THEN …`: target
    /// (write) and source (read) form the scope. ON and the per-clause
    /// predicates are filter reads; each WHEN action's value expressions
    /// become `Project` outputs named by their written column (UPDATE SET
    /// target / INSERT column), so the `value → target` lineage pairing
    /// reuses the output-column machinery.
    pub(super) fn bind_merge(&self, merge: &Merge) -> Option<Plan> {
        let (target_plan, target_scope) = self.bind_table_factor(&merge.table, &Scope::empty());
        let (source_plan, source_scope) =
            self.bind_table_factor(&merge.source, &target_scope.clone());
        let scope = target_scope.merge(source_scope);
        // ON / WHEN predicates are filter position (non-feeding); only a WHEN
        // action's value expression feeds the target via a `Project` output.
        let (mut reads, mut filter_subqueries) = self.expr_reads(&merge.on, &scope);
        let mut value_subqueries = Vec::new();
        let mut outputs = Vec::new();
        let mut target_columns = Vec::new();
        // The (canonicalized) target identity, needed up-front to catalog-fill
        // a column-less WHEN NOT MATCHED INSERT's columns.
        let target = self.canonical_target(self.table_factor_ref(&merge.table)?);
        for clause in &merge.clauses {
            if let Some(predicate) = &clause.predicate {
                let (r, s) = self.expr_reads(predicate, &scope);
                reads.extend(r);
                filter_subqueries.extend(s);
            }
            match &clause.action {
                MergeAction::Insert(insert) => {
                    if let Some(predicate) = &insert.insert_predicate {
                        let (r, s) = self.expr_reads(predicate, &scope);
                        reads.extend(r);
                        filter_subqueries.extend(s);
                    }
                    if let MergeInsertKind::Values(values) = &insert.kind {
                        // An explicit column list wins; otherwise pair the
                        // values positionally with the target's catalog
                        // schema (empty without a catalog). `zip` stops at
                        // the shorter side, so a short row / schema truncates.
                        let explicit: Vec<Ident> = insert
                            .columns
                            .iter()
                            .filter_map(object_name_last_ident)
                            .collect();
                        let catalog_cols;
                        let columns: &[Ident] = if explicit.is_empty() {
                            catalog_cols = self.catalog_columns(&target);
                            &catalog_cols
                        } else {
                            &explicit
                        };
                        if columns.is_empty() {
                            // No column list and no catalog: the inserted
                            // values surface as reads, but pair with nothing,
                            // so there's no write / lineage.
                            for expr in values.rows.iter().flatten() {
                                let (r, s) = self.expr_reads(expr, &scope);
                                reads.extend(r);
                                filter_subqueries.extend(s);
                            }
                        } else {
                            for row in &values.rows {
                                for (column, expr) in columns.iter().zip(row) {
                                    let bound =
                                        self.bind_value_column(Some(column.clone()), expr, &scope);
                                    outputs.push(bound.column);
                                    value_subqueries.extend(bound.value_subplans);
                                    reads.extend(bound.filter_reads);
                                    filter_subqueries.extend(bound.filter_subplans);
                                    target_columns.push(column.clone());
                                }
                            }
                        }
                    }
                }
                MergeAction::Update(update) => {
                    for assignment in &update.assignments {
                        for column in assignment_target_columns(&assignment.target) {
                            let bound = self.bind_value_column(
                                Some(column.clone()),
                                &assignment.value,
                                &scope,
                            );
                            outputs.push(bound.column);
                            value_subqueries.extend(bound.value_subplans);
                            reads.extend(bound.filter_reads);
                            filter_subqueries.extend(bound.filter_subplans);
                            target_columns.push(column);
                        }
                    }
                    for predicate in [&update.update_predicate, &update.delete_predicate]
                        .into_iter()
                        .flatten()
                    {
                        let (r, s) = self.expr_reads(predicate, &scope);
                        reads.extend(r);
                        filter_subqueries.extend(s);
                    }
                }
                // DELETE moves no column values.
                MergeAction::Delete { .. } => {}
            }
        }
        // The MERGE target is in scope for ON / WHEN resolution but is a
        // write, not a read; the source relation is a read. Predicate
        // sub-plans ride the non-feeding PassThrough.
        let source = wrap_inputs(
            vec![into_write_target(target_plan), source_plan],
            reads,
            filter_subqueries,
        );
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(Plan::Project(Project {
                input: Box::new(source),
                outputs,
                subqueries: value_subqueries,
            })),
            returning: Vec::new(),
            conflict_updates: Vec::new(),
        }))
    }

    /// `CREATE TABLE dst AS <query>` (CTAS): the source query's reads,
    /// paired with the new table's columns (explicit defs win, else the
    /// source output names). A plain `CREATE TABLE t (cols)` (no query) is a
    /// write target with no columns / reads / lineage — its column
    /// definitions aren't writes — so it binds to a target-only `Write`.
    pub(super) fn bind_create_table(&self, create: &CreateTable) -> Option<Plan> {
        let target = self.canonical_target(self.table_ref(&create.name)?);
        let Some(query) = create.query.as_ref() else {
            return Some(Plan::Write(Write {
                target,
                target_columns: Vec::new(),
                input: Box::new(Plan::OpaqueLeaf),
                returning: Vec::new(),
                conflict_updates: Vec::new(),
            }));
        };
        let (input, scope) = self.bind_query(query);
        let target_columns = if create.columns.is_empty() {
            output_names(&scope.outputs)
        } else {
            create.columns.iter().map(|c| c.name.clone()).collect()
        };
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(input),
            returning: Vec::new(),
            conflict_updates: Vec::new(),
        }))
    }

    /// `CREATE VIEW v AS <query>`: like CTAS — the query's reads paired
    /// with the view's columns (explicit list wins, else source names).
    pub(super) fn bind_create_view(&self, create: &CreateView) -> Option<Plan> {
        let (input, scope) = self.bind_query(&create.query);
        let target = self.canonical_target(self.table_ref(&create.name)?);
        let target_columns = if create.columns.is_empty() {
            output_names(&scope.outputs)
        } else {
            create.columns.iter().map(|c| c.name.clone()).collect()
        };
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(input),
            returning: Vec::new(),
            conflict_updates: Vec::new(),
        }))
    }

    /// `ALTER VIEW v AS <query>`: treated like CREATE VIEW — the
    /// replacement query's reads paired with the view's columns (explicit
    /// list wins, else the source output names).
    pub(super) fn bind_alter_view(
        &self,
        name: &ObjectName,
        columns: &[Ident],
        query: &Query,
    ) -> Option<Plan> {
        let (input, scope) = self.bind_query(query);
        let target = self.canonical_target(TableReference::try_from(name).ok()?);
        let target_columns = if columns.is_empty() {
            output_names(&scope.outputs)
        } else {
            columns.to_vec()
        };
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(input),
            returning: Vec::new(),
            conflict_updates: Vec::new(),
        }))
    }

    /// `ALTER TABLE t <ops>`: the altered table is a write target; each
    /// column-naming operation contributes its column(s) as writes (RENAME
    /// / CHANGE surface both the old and new names). Schema-level ops
    /// (constraints, partitions, RENAME TABLE) name no columns. No reads or
    /// lineage — ALTER restructures, it doesn't move row data.
    pub(super) fn bind_alter_table(&self, alter: &AlterTable) -> Option<Plan> {
        let target = self.canonical_target(self.table_ref(&alter.name)?);
        let target_columns = alter
            .operations
            .iter()
            .flat_map(alter_table_op_target_columns)
            .collect();
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(Plan::OpaqueLeaf),
            returning: Vec::new(),
            conflict_updates: Vec::new(),
        }))
    }

    /// `DROP TABLE/VIEW/MATERIALIZED VIEW a, b`: the dropped relations are
    /// write targets. Other object types (index / schema / …) name no
    /// relations and classify as unsupported, so they don't reach here.
    pub(super) fn bind_drop(
        &self,
        object_type: &ObjectType,
        names: &[ObjectName],
        table: Option<&ObjectName>,
    ) -> Option<Plan> {
        if !matches!(
            object_type,
            ObjectType::Table | ObjectType::View | ObjectType::MaterializedView
        ) {
            return None;
        }
        let targets = names
            .iter()
            .chain(table)
            .filter_map(|name| self.table_ref(name))
            .map(|target| self.canonical_target(target))
            .collect();
        Some(Plan::Drop(targets))
    }

    /// Bind a `RETURNING` clause's projected columns against `scope`. Each
    /// is a value-position projection (like a SELECT list) over the written
    /// relation, so it contributes target reads and a `QueryOutput` lineage
    /// edge. A wildcard records `WildcardSuppressed` and is skipped.
    pub(super) fn bind_returning(
        &self,
        returning: &Option<Vec<SelectItem>>,
        scope: &Scope,
    ) -> Vec<BoundColumn> {
        let Some(items) = returning else {
            return Vec::new();
        };
        items
            .iter()
            // Sub-plans / filter reads of a RETURNING expression (rare —
            // a subquery or CASE in RETURNING) aren't modelled yet.
            .filter_map(|item| {
                self.bind_output_column(item, scope)
                    .map(|bound| bound.column)
            })
            .collect()
    }

    /// Bind an `INSERT`'s conflict action (`ON CONFLICT DO UPDATE SET …`
    /// or MySQL `ON DUPLICATE KEY UPDATE …`) into a mini-UPDATE on the
    /// target: the SET assignments become bound columns (named by their
    /// target column, carrying the RHS provenance) for extra writes +
    /// `Relation` lineage, and the optional `DO UPDATE … WHERE` is a filter
    /// read. Returns the assignments plus the conflict's reads / sub-plans
    /// (to fold onto the write's input). For Postgres / SQLite the
    /// `EXCLUDED` pseudo-table is in scope (its columns are the INSERT
    /// source's outputs renamed to the target columns, so `EXCLUDED.col`
    /// collapses back to the source); MySQL's `VALUES(col)` self-references
    /// the target instead, so no `EXCLUDED` is bound.
    pub(super) fn bind_conflict(
        &self,
        on: &OnInsert,
        target: &TableReference,
        target_columns: &[Ident],
        source_scope: &Scope,
    ) -> (Vec<BoundColumn>, Vec<ColumnRead>, Vec<Plan>) {
        let mut updates = Vec::new();
        let mut reads = Vec::new();
        let mut subplans = Vec::new();
        let (assignments, scope, selection) = match on {
            OnInsert::DuplicateKeyUpdate(assignments) => {
                (assignments.as_slice(), self.target_scope(target), None)
            }
            OnInsert::OnConflict(on_conflict) => match &on_conflict.action {
                OnConflictAction::DoUpdate(do_update) => {
                    let scope = self
                        .target_scope(target)
                        .merge(self.excluded_scope(target_columns, source_scope));
                    (
                        do_update.assignments.as_slice(),
                        scope,
                        do_update.selection.as_ref(),
                    )
                }
                // DO NOTHING moves no data.
                OnConflictAction::DoNothing => return (updates, reads, subplans),
            },
            // `OnInsert` is non-exhaustive; an unmodelled action is a no-op.
            _ => return (updates, reads, subplans),
        };
        for assignment in assignments {
            for column in assignment_target_columns(&assignment.target) {
                let bound = self.bind_value_column(Some(column), &assignment.value, &scope);
                updates.push(bound.column);
                // The conflict action's lineage rides the `conflict_updates`
                // provenance, not these sub-plans, so both value- and
                // filter-position sub-plans land on the non-feeding input.
                subplans.extend(bound.value_subplans);
                subplans.extend(bound.filter_subplans);
                reads.extend(bound.filter_reads);
            }
        }
        if let Some(selection) = selection {
            let (r, s) = self.expr_reads(selection, &scope);
            reads.extend(r);
            subplans.extend(s);
        }
        (updates, reads, subplans)
    }

    /// The `EXCLUDED` pseudo-table's resolution scope: a synthetic relation
    /// whose columns are the INSERT source's output columns renamed
    /// positionally to the target columns (so `EXCLUDED.col` collapses to
    /// whatever feeds that position of the source). A source with no
    /// inspectable outputs (`VALUES`) yields an opaque table-function-like
    /// relation, so `EXCLUDED.col` stays a synthetic self-reference.
    pub(super) fn excluded_scope(&self, target_columns: &[Ident], source_scope: &Scope) -> Scope {
        let columns: Vec<BoundColumn> = source_scope
            .outputs
            .iter()
            .enumerate()
            .map(|(i, column)| BoundColumn {
                name: target_columns
                    .get(i)
                    .cloned()
                    .or_else(|| column.name.clone()),
                provenance: column.provenance.clone(),
            })
            .collect();
        let source = if columns.is_empty() {
            RelationSource::TableFunction
        } else {
            RelationSource::Derived { columns }
        };
        Scope::of(Relation {
            alias: Some(Ident::new("EXCLUDED")),
            source,
        })
    }

    /// The target table's catalog column names (unquoted), for filling in
    /// the target columns of a column-less `INSERT` — `INSERT INTO t SELECT
    /// …` pairs the source positionally with `t`'s schema. Empty without a
    /// unique catalog hit (column-less INSERT then writes / pairs nothing).
    pub(super) fn catalog_columns(&self, target: &TableReference) -> Vec<Ident> {
        self.table_match(target)
            .columns
            .iter()
            .map(|column| Ident::new(&column.value))
            .collect()
    }

    /// A resolution scope holding just the write target (for `INSERT …
    /// RETURNING`, whose references resolve against the target alone — the
    /// source query's scope has already been popped).
    pub(super) fn target_scope(&self, target: &TableReference) -> Scope {
        let TableMatch { table, columns, .. } = self.table_match(target);
        let columns = if columns.is_empty() {
            RelationColumns::Open
        } else {
            RelationColumns::Known(columns)
        };
        Scope::of(Relation {
            alias: None,
            source: RelationSource::Table { table, columns },
        })
    }
}
