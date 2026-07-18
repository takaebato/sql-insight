//! DML / DDL statement roots: the `bind_*` methods that turn an INSERT /
//! UPDATE / DELETE / MERGE / CREATE / ALTER / DROP `Statement` into its
//! [`LogicalPlan`] root, plus the write-target resolution helpers (DELETE
//! multi-target identity, RETURNING projection, ON CONFLICT).

use super::resolve::TableMatch;
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
        let (plan, scope) = self.bind_query(query);
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
        let source_wildcard = !scope.outputs_complete;
        self.diagnose_created_columns(&target.reference, &[], &plan, source_wildcard);
        // A `WITH` on a SELECT INTO is the *statement's* WITH (PostgreSQL
        // requires data-modifying CTEs at the top level), so it must stay the
        // outermost node: wrapping the CreateTableAs *around* it would bury a
        // data-modifying CTE body inside the root's input, where
        // `dml_roots` — which descends declarations, not inputs — never
        // finds it, silently dropping the CTE's own write.
        let (ctes, input) = match plan {
            LogicalPlan::With(w) => (w.ctes, *w.body),
            other => (Vec::new(), other),
        };
        let create = LogicalPlan::CreateTableAs(CreateTableAs {
            target,
            columns: Vec::new(),
            input: Box::new(input),
            schema_source: None,
            source_wildcard,
        });
        if ctes.is_empty() {
            create
        } else {
            LogicalPlan::With(With {
                ctes,
                body: Box::new(create),
            })
        }
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
        // Resolve the target into its catalog match plus, for an Oracle
        // inline-view target, the view's pieces: its projected column names
        // (the explicit target columns), its bound ON / WHERE predicates
        // (filter reads), and — for a join view — the companion relations.
        let (m, view_columns, target_predicate, target_context) = match &insert.table {
            TableObject::TableName(name) => match self.named_insert_target(name) {
                Some(m) => (m, Vec::new(), Vec::new(), LogicalPlan::Empty),
                None => return LogicalPlan::Empty,
            },
            TableObject::TableFunction(function) => {
                match self.named_insert_target(&function.name) {
                    Some(m) => (m, Vec::new(), Vec::new(), LogicalPlan::Empty),
                    None => return LogicalPlan::Empty,
                }
            }
            // Oracle `INSERT INTO (SELECT …) …`: an inline-view target — the
            // row lands in the view's base table. A single-table view is
            // shape-determined; a join view's base table is whichever relation
            // every projected column attributes to. A view the gate rejects
            // (a non-table factor, a set operation, any other clause) — or a
            // join view with no single determined target — is dropped +
            // flagged like a CTE / derived target.
            TableObject::TableQuery(query) => {
                let Some(view) = crate::reference::insert_target_view(query) else {
                    self.record_unsupported_dml_target("INSERT", query.as_ref());
                    return LogicalPlan::Empty;
                };
                match crate::reference::insert_target_base(&view) {
                    Some(name) => {
                        let Some(m) = self.named_insert_target(name) else {
                            return LogicalPlan::Empty;
                        };
                        // The WHERE resolves against the target alone (filter
                        // reads — e.g. the predicate a `WITH CHECK OPTION`
                        // enforces). The base table may be aliased
                        // (`FROM emp e`); the predicate then qualifies through
                        // the alias (`e.dept`), so carry it on the scope.
                        let predicate = match view.selection {
                            Some(predicate) => {
                                let alias = view.factors[0].1.cloned();
                                let scope = self.target_scope_with_alias(&m.table, alias);
                                vec![self.bind_expr(predicate, &scope)]
                            }
                            None => Vec::new(),
                        };
                        (
                            m,
                            view_target_columns(view.projection),
                            predicate,
                            LogicalPlan::Empty,
                        )
                    }
                    None => {
                        let diagnostics_before = self.diagnostics.len();
                        match self.bind_join_view_target(&view) {
                            Some(resolved) => resolved,
                            None => {
                                // A factor that failed to resolve already
                                // flagged itself; flag only when nothing was.
                                if self.diagnostics.len() == diagnostics_before {
                                    self.record_unsupported_dml_target("INSERT", query.as_ref());
                                }
                                return LogicalPlan::Empty;
                            }
                        }
                    }
                }
            }
        };
        let target = m.table;
        // MySQL `INSERT INTO t SET col = expr, …`: the assignment form (no
        // VALUES / SELECT source) — each assignment is a value column named by
        // its target, like a single-row UPDATE.
        if insert.source.is_none() && !insert.assignments.is_empty() {
            return self.bind_insert_set(insert, target, m.resolution, m.columns);
        }
        let (input, scope) = match &insert.source {
            Some(source) => self.bind_query(source),
            None => (LogicalPlan::Empty, Scope::default()),
        };
        // An *unexpanded* wildcard in the source projection (`SELECT *, y`
        // whose `*` couldn't expand) leaves the column count / positions
        // indeterminate, so neither the arity check nor positional
        // relation-lineage can trust the visible outputs. Carried on
        // `source_wildcard`; the arity check and the lineage walker both skip
        // when set, it words the diagnostic below, and a column-list-less
        // INSERT can't fill from the catalog under it (next). An expanded
        // wildcard is just columns — the pairing proceeds.
        let source_wildcard = insert.source.is_some() && !scope.outputs_complete;
        // The written column list: an explicit list wins — an `(a, b)` list, or
        // an inline-view target's plain-column projection — otherwise a
        // column-less INSERT fills from the target's catalog columns (reusing
        // the `table_match` list above), truncated to the source's projected
        // arity. The catalog columns are kept in their canonical form (quoted as
        // `canonical_quote` dictates), not re-wrapped as plain identifiers — so a
        // case-exact column like `"MyCol"` surfaces quoted and matches both the
        // catalog (→ `Cataloged`, not `Inferred`) and a user's own quoted
        // reference. But a source with an *unexpanded* wildcard has
        // indeterminate arity — the suppressed `*` isn't in `query_outputs`,
        // so the visible count is too low — and the catalog columns can't be
        // positionally paired: leave the list empty so the drop + flag guard
        // below fires rather than mis-truncating to the undercounted outputs.
        let explicit: Vec<Ident> = if !insert.columns.is_empty() {
            // `Insert::columns` is `Vec<ObjectName>`; a target column is named
            // by its final identifier part (mirrors the MERGE-insert path).
            insert
                .columns
                .iter()
                .filter_map(|n| n.0.last().and_then(|p| p.as_ident().cloned()))
                .collect()
        } else {
            view_columns
        };
        let has_explicit = !explicit.is_empty();
        let columns = if has_explicit {
            explicit
        } else if source_wildcard {
            Vec::new()
        } else {
            m.columns
                .iter()
                .take(scope.query_outputs.len())
                .cloned()
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
        // The source's determinate value count: a `VALUES` row width, or a
        // SELECT's projected output count. `None` when indeterminate (a
        // wildcard-bearing source, or no inspectable operand).
        let source_count = if source_wildcard {
            None
        } else {
            match &input {
                LogicalPlan::Values(v) => v.rows.first().map(Vec::len),
                other => output_operands(other)
                    .first()
                    .map(|operand| slot_count(operand.outputs))
                    .filter(|n| *n > 0),
            }
        };
        if let Some(source_count) = source_count {
            if has_explicit {
                // An explicit list must match the source exactly — either
                // direction silently zips to the shorter side.
                self.diagnose_insert_arity(&target, true, columns.len(), source_count);
            } else if !m.columns.is_empty() {
                // A column-less target filled from the catalog: a wider source
                // overflows the table, dropping its surplus columns.
                self.diagnose_insert_arity(&target, false, m.columns.len(), source_count);
            }
        }
        // ON CONFLICT DO UPDATE / ON DUPLICATE KEY UPDATE: extra writes + their
        // `value → target.col` lineage, plus the optional `DO UPDATE … WHERE`
        // (filter reads). A PG `INSERT INTO t AS c` alias is how the conflict
        // action names the existing row (`SET n = c.n + 1`), so it rides the
        // target scope — without it those references fell to `Unresolved`.
        let target_alias = insert.table_alias.as_ref().map(|a| a.alias.clone());
        let (on_conflict, conflict_predicate) = match &insert.on {
            Some(on) => self.bind_conflict(on, &target, target_alias.clone(), &columns),
            None => (Vec::new(), Vec::new()),
        };
        // RETURNING resolves against the target alone (the source query's
        // scope is already popped), under the same alias.
        let returning = self.bind_returning(
            &insert.returning,
            &self.target_scope_with_alias(&target, target_alias),
        );
        // Each written column carries its catalog match against the target
        // (`Cataloged` if listed, else `Inferred` — a catalog-filled column is
        // by definition listed).
        let columns = self.column_writes(&target, &m.columns, columns);
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
            target_predicate,
            target_context: Box::new(target_context),
            source_wildcard,
        })
    }

    /// Resolve a plain named INSERT target: `table_ref` + catalog match. The
    /// target is a real table, never a CTE (you can't INSERT into a read-only
    /// CTE): a name that matches a declared CTE and isn't a catalog table is
    /// the CTE — flag and drop rather than fabricate a write.
    fn named_insert_target(&mut self, name: &ObjectName) -> Option<TableMatch> {
        let written = self.table_ref(name)?;
        let m = self.table_match(&written);
        if m.resolution != ResolutionKind::Cataloged && self.is_declared_cte(&written) {
            self.record_unsupported_dml_target("INSERT", &written);
            return None;
        }
        Some(m)
    }

    /// Resolve an Oracle **join-view** INSERT target: bind every FROM factor
    /// as a relation, then attribute each projected column — a qualifier
    /// resolves it text-only (like a multi-table UPDATE's `SET t2.col`), an
    /// unqualified column by the catalog-owner rule
    /// ([`unqualified_write_binding`](Self::unqualified_write_binding)) — and
    /// the row lands in the one relation **every** column agrees on. Returns
    /// that target's match, the projected column names (the explicit target
    /// columns), the bound ON + WHERE predicates (filter reads over the view's
    /// relations), and the companion relations as scanned context
    /// ([`Insert::target_context`]).
    ///
    /// `None` — the caller flags and drops — when no single target is
    /// determined: a non-column projection item (a wildcard / expression), a
    /// column that is ambiguous / unresolved / owned by a different relation
    /// than its siblings, or any factor naming a declared CTE (not a base
    /// table). Real Oracle rejects those shapes too (the INTO columns must
    /// all belong to one key-preserved table), so no side is fabricated. Key-preservedness itself isn't *verified* (that needs
    /// unique-key metadata the catalog doesn't carry) — attribution assumes a
    /// valid statement.
    fn bind_join_view_target(
        &mut self,
        view: &crate::reference::InsertTargetView<'_>,
    ) -> Option<(TableMatch, Vec<Ident>, Vec<Expr>, LogicalPlan)> {
        // The view's relations — the shape gate already extracted every
        // factor's (name, alias). A factor naming a declared CTE — target or
        // companion — makes the view not-a-view-over-base-tables: flag and
        // drop the whole statement rather than surface the CTE name as a
        // phantom base-table read (binding it as a real `CteRef` is possible,
        // but no engine executes a WITH + inline-view-target INSERT, so the
        // machinery isn't worth it until one does).
        let mut matches: Vec<TableMatch> = Vec::new();
        for (name, _) in &view.factors {
            // An unrepresentable name flags itself inside `table_ref`.
            let written = self.table_ref(name)?;
            let m = self.table_match(&written);
            if m.resolution != ResolutionKind::Cataloged && self.is_declared_cte(&written) {
                self.record_unsupported_dml_target("INSERT", &written);
                return None;
            }
            matches.push(m);
        }
        let relations: Vec<Relation> = view
            .factors
            .iter()
            .zip(&matches)
            .map(|((_, alias), m)| Relation::Table {
                alias: alias.cloned(),
                table: m.table.clone(),
                columns: Columns::from_catalog(m.columns.clone()),
            })
            .collect();
        // Each projected column, split as (qualifier, column name) — plain
        // columns only; anything else leaves the target (and the positional
        // pairing) indeterminate.
        let parts_list: Vec<(Vec<Ident>, Ident)> = view
            .projection
            .iter()
            .map(|item| match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    match expr {
                        SqlExpr::Identifier(id) => Some((Vec::new(), id.clone())),
                        SqlExpr::CompoundIdentifier(parts) => parts
                            .split_last()
                            .map(|(column, qualifier)| (qualifier.to_vec(), column.clone())),
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect::<Option<_>>()?;
        // Attribute every column — a qualified one text-only via its
        // qualifier, an unqualified one by the catalog-owner rule — and
        // require all to agree on the first column's relation. (An empty
        // projection can't parse.)
        let mut owners = parts_list.iter().map(|(qualifier, column)| {
            if qualifier.is_empty() {
                match self.unqualified_write_binding(column, &relations)? {
                    Binding::Base { table, .. } => Some(table),
                    _ => None,
                }
            } else {
                relations
                    .iter()
                    .find_map(|rel| self.writable_qualifier_table(rel, qualifier))
            }
        });
        let target = owners.next()??;
        for owner in owners {
            if !self.table_identity_eq(&target, &owner?) {
                return None; // columns straddle two relations
            }
        }
        // Split off the (first) target relation; the companions become
        // scanned context (reads, but no data feed). The attributed target
        // always names one of the view's relations.
        let position = matches
            .iter()
            .position(|m| self.table_identity_eq(&m.table, &target))?;
        let target_match = matches.remove(position);
        let context = matches
            .into_iter()
            .map(|m| {
                LogicalPlan::Scan(Scan {
                    table: m.table,
                    resolution: m.resolution,
                })
            })
            .reduce(|left, right| join(left, right, Vec::new()))
            .unwrap_or(LogicalPlan::Empty);
        // ON + WHERE are filter reads over the full view scope (all relations,
        // aliases included).
        let scope = Scope::from_relations(&relations);
        let predicate = view
            .join_operators
            .iter()
            .filter_map(|op| join_on(op))
            .chain(view.selection)
            .map(|on| self.bind_expr(on, &scope))
            .collect();
        let columns = parts_list
            .iter()
            .map(|(_, column)| column.clone())
            .collect();
        Some((target_match, columns, predicate, context))
    }

    /// Wrap written column names as [`ColumnWrite`]s, each resolved against the
    /// target's catalog column list: `Cataloged` when listed, else `Inferred`
    /// (catalog-free, target columns unknown, or an unlisted column). Shared by
    /// the INSERT / MERGE-insert write paths.
    fn column_writes(
        &self,
        target: &TableReference,
        catalog_columns: &[Ident],
        columns: Vec<Ident>,
    ) -> Vec<ColumnWrite> {
        columns
            .into_iter()
            .map(|name| ColumnWrite {
                resolution: if self.list_has(catalog_columns, &name) {
                    ResolutionKind::Cataloged
                } else {
                    ResolutionKind::Inferred
                },
                reference: crate::reference::ColumnReference {
                    table: Some(target.clone()),
                    name,
                },
            })
            .collect()
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
        catalog_columns: Vec<Ident>,
    ) -> LogicalPlan {
        let scope = self.target_scope(&target);
        let mut columns = Vec::new();
        let mut exprs = Vec::new();
        for assignment in &insert.assignments {
            for column in assignment_target_columns(&assignment.target) {
                exprs.push(NamedExpr {
                    names: OutputNames::Single(Some(column.clone())),
                    expr: self.bind_expr(&assignment.value, &scope),
                });
                columns.push(column);
            }
        }
        let (on_conflict, conflict_predicate) = match &insert.on {
            Some(on) => self.bind_conflict(on, &target, None, &columns),
            None => (Vec::new(), Vec::new()),
        };
        let returning = self.bind_returning(&insert.returning, &scope);
        let columns = self.column_writes(&target, &catalog_columns, columns);
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
            // The MySQL SET form has no inline-view target and no source
            // query, so no target predicate / context / wildcard.
            target_predicate: Vec::new(),
            target_context: Box::new(LogicalPlan::Empty),
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
        target_alias: Option<Ident>,
        columns: &[Ident],
    ) -> (Vec<Assignment>, Vec<Expr>) {
        let (scope, assignments, selection) = match on {
            OnInsert::DuplicateKeyUpdate(assignments) => (
                self.target_scope_with_alias(target, target_alias),
                assignments.as_slice(),
                None,
            ),
            OnInsert::OnConflict(on_conflict) => match &on_conflict.action {
                OnConflictAction::DoUpdate(do_update) => {
                    let mut scope = self.target_scope_with_alias(target, target_alias);
                    scope.relations.push(Relation::Derived {
                        alias: Some(Ident::new("excluded")),
                        // A synthetic pseudo-relation, not a positional
                        // producer — never `complete` (nothing expands over
                        // the conflict scope anyway).
                        columns: Exposed {
                            slots: columns.iter().cloned().map(Some).collect(),
                            complete: false,
                        },
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
        // A conflict-action SET always targets the insert target's own columns
        // (no other writable relations, hence the empty candidate set).
        let mut bound: Vec<Assignment> = assignments
            .iter()
            .flat_map(|a| self.bind_assignment(a, &scope, target, &[]))
            .collect();
        let mut predicate: Vec<Expr> = selection
            .map(|s| self.bind_expr(s, &scope))
            .into_iter()
            .collect();
        // PostgreSQL parity: an *unqualified* reference in the conflict action
        // is contested between the existing target row and `EXCLUDED` — PG 17 / 18
        // reject it (`column reference "b" is ambiguous`; SQLite instead reads
        // the target), so no single attribution is right and it surfaces
        // `Ambiguous`, matching what the catalog-aware bind already produces
        // (target + EXCLUDED are then two confirming witnesses). The generic
        // scope resolution instead lets the `EXCLUDED` exposure win as the sole
        // witness over a catalog-free (`Unknown`) target — demote exactly those
        // bindings: in this scope an unqualified `Derived` can only be
        // `EXCLUDED` (the sole derived relation). Qualified references
        // (`EXCLUDED.b` / `t.b`) keep their binding.
        for a in &mut bound {
            demote_unqualified_excluded(&mut a.value);
        }
        for p in &mut predicate {
            demote_unqualified_excluded(p);
        }
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
        // T-SQL aliases the target *in the FROM clause*:
        // `UPDATE a SET x = 1 FROM t AS a` writes `t`, the relation the alias
        // names. Resolving the bare target name directly would fabricate a
        // phantom table `a` (and leave the real relation as a second
        // candidate, turning its own references ambiguous) — so, mirroring
        // DELETE's USING-first order, bind the FROM relations first and
        // resolve the target through them when the shape matches.
        if joins.is_empty() {
            if let Some(op) = self.bind_update_via_from_alias(update, target_factor) {
                return op;
            }
        }
        // The root's own resolution isn't needed here — each SET assignment
        // carries its write-target table's resolution (`assignment_target`).
        let diagnostics_before = self.diagnostics.len();
        let Some((target_relation, target, _)) = self.target_relation(target_factor) else {
            // A non-writable target (a CTE name, derived table, subquery, table
            // function, or join) can't be a write target — flag it (like
            // `bind_merge`) rather than dropping silently. A plain table that
            // failed to resolve already flagged itself (e.g. `table_ref`'s too-
            // many-qualifiers), so flag only when nothing was reported.
            if self.diagnostics.len() == diagnostics_before {
                self.record_unsupported_dml_target("UPDATE", target_factor);
            }
            return LogicalPlan::Empty;
        };
        let mut scope = Scope::single(target_relation);
        let mut input = LogicalPlan::Empty;
        // Joins on the UPDATE target clause are read relations. They fan in
        // merge columns like a SELECT join (`bind_table_with_joins`): a `USING
        // (col)` names them, a NATURAL join takes the schema-common columns —
        // both computed before the right scope is absorbed — so an unqualified
        // reference resolves to both sides instead of staying `Ambiguous`.
        for j in joins {
            // An ARRAY JOIN operand is an unnested array column, not a joined
            // table (same special case as `bind_table_with_joins`).
            let (node, jscope) = if is_array_join(&j.join_operator) {
                let visible = scope.relations.clone();
                self.bind_array_join(&j.relation, &visible)
            } else {
                self.bind_table_factor(&j.relation, &scope.relations)
            };
            let merge = if join_is_natural(&j.join_operator) {
                self.natural_merge_columns(&scope, &jscope)
            } else {
                join_using(&j.join_operator)
            };
            scope.absorb(jscope);
            scope.add_merge_columns(merge);
            let on = join_on(&j.join_operator)
                .map(|e| self.bind_expr(e, &scope))
                .into_iter()
                .collect();
            input = join(input, node, on);
        }
        // The unqualified-SET attribution candidates: the target plus its
        // clause joins (MySQL's writable set) — snapshotted *before* the FROM
        // relations join the scope, which are readable but never writable
        // (PostgreSQL / T-SQL `UPDATE t SET … FROM u` only ever writes `t`).
        let writable = scope.relations.clone();
        // FROM relations are reads (resolved against the target + joins so
        // far). `absorb` carries their `USING` / NATURAL merge columns too, so
        // an unqualified SET RHS / WHERE reference to a merge column fans in
        // to both sides like it does in a SELECT (previously only the
        // relations were kept and such a reference fell to `Ambiguous`).
        if let Some(from) = &update.from {
            let tables = match from {
                UpdateTableFromKind::BeforeSet(t) | UpdateTableFromKind::AfterSet(t) => t,
            };
            for twj in tables {
                let (node, fscope) = self.bind_table_with_joins(twj, &scope.relations);
                scope.absorb(fscope);
                input = combine(input, node);
            }
        }
        self.bind_update_clauses(update, scope, input, target, &writable)
    }

    /// The clause tail every UPDATE form shares once its scope is assembled:
    /// WHERE / ORDER BY / LIMIT as filter reads, the SET assignments, and
    /// RETURNING.
    fn bind_update_clauses(
        &mut self,
        update: &SqlUpdate,
        scope: Scope,
        mut input: LogicalPlan,
        target: TableReference,
        writable: &[Relation],
    ) -> LogicalPlan {
        // WHERE + the MySQL `ORDER BY` / `LIMIT` tail are filter-position
        // reads (they pick / order / bound which rows update; their reads /
        // subqueries never feed the new value). ORDER BY keys reference the
        // target's columns, so they read it — mirroring DELETE; a constant
        // LIMIT adds no read but is bound for parity, not dropped.
        let mut filter_reads: Vec<Expr> = update
            .selection
            .iter()
            .map(|predicate| self.bind_expr(predicate, &scope))
            .collect();
        filter_reads.extend(self.order_by_expr_keys(&update.order_by, &scope));
        filter_reads.extend(update.limit.iter().map(|e| self.bind_expr(e, &scope)));
        if !filter_reads.is_empty() {
            input = LogicalPlan::Filter(Filter {
                input: Box::new(input),
                predicate: filter_reads,
            });
        }
        // SET assignments resolve against the target + FROM scope; each writes
        // its resolved target table (see `resolve_assignment_column` for the
        // attribution rules). A tuple `SET (a, b) = …` expands to one
        // assignment per target column.
        let assignments = update
            .assignments
            .iter()
            .flat_map(|a| self.bind_assignment(a, &scope, &target, writable))
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

    /// The T-SQL `UPDATE <alias> SET … FROM t AS <alias>` form: the join-less,
    /// unaliased, single-part target names a FROM-clause alias. Binds the FROM
    /// relations first and resolves the target through them
    /// ([`scope_target`](Self::scope_target), like DELETE's USING-alias form) —
    /// `None` when the shape doesn't match (no FROM, a non-bare target, or no
    /// relation answering to the name), falling back to the ordinary
    /// target-first bind.
    fn bind_update_via_from_alias(
        &mut self,
        update: &SqlUpdate,
        target_factor: &TableFactor,
    ) -> Option<LogicalPlan> {
        let tables = match update.from.as_ref()? {
            UpdateTableFromKind::BeforeSet(t) | UpdateTableFromKind::AfterSet(t) => t,
        };
        let written = match target_factor {
            TableFactor::Table {
                name,
                alias: None,
                args: None,
                ..
            } => TableReference::try_from_name(name).ok()?,
            _ => return None,
        };
        if written.schema.is_some() || written.catalog.is_some() {
            return None;
        }
        // Cheap syntactic pre-check before committing to the reordered bind:
        // some FROM factor must alias the target's name.
        let alias_fold = self.style.casing.table_alias;
        let aliases_target = |factor: &TableFactor| {
            matches!(factor, TableFactor::Table { alias: Some(a), .. }
                if alias_fold.normalize(&a.name) == alias_fold.normalize(&written.name))
        };
        if !tables.iter().any(|twj| {
            aliases_target(&twj.relation) || twj.joins.iter().any(|j| aliases_target(&j.relation))
        }) {
            return None;
        }
        let mut scope = Scope::default();
        let mut input = LogicalPlan::Empty;
        for twj in tables {
            let (node, fscope) = self.bind_table_with_joins(twj, &scope.relations);
            scope.absorb(fscope);
            input = combine(input, node);
        }
        let target = self.scope_target(&written, &scope)?.reference;
        // The aliased relation is the sole writable sink (T-SQL writes only
        // the target); the root pin covers unqualified SET targets, and a
        // qualified `<alias>.col` resolves through the relation's alias.
        Some(self.bind_update_clauses(update, scope, input, target, &[]))
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
            // `absorb` keeps the joins' USING / NATURAL merge columns, so an
            // unqualified predicate reference to one fans in like in a SELECT.
            scope.absorb(uscope);
            input = combine(input, node);
        }
        let using_relations = scope.relations.len();
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
                    scope.absorb(fscope);
                }
            } else {
                let (node, fscope) = self.bind_table_with_joins(twj, &scope.relations);
                scope.absorb(fscope);
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
        // The clauses see the target relations *first*: PostgreSQL's
        // `DELETE FROM t USING s RETURNING *` yields t's columns then s's
        // (verified on PostgreSQL 17), and the UPDATE scope is already
        // target-first — only the *binding* above had to run USING-first (so
        // a target alias can resolve). Rotating the freshly-bound target
        // block ahead changes nothing for name resolution (candidates are
        // gathered scope-wide, not first-match); only positional consumers —
        // a `RETURNING *` expansion — see the order.
        if from_is_target {
            scope.relations.rotate_left(using_relations);
        }
        // WHERE + the MySQL `ORDER BY` / `LIMIT` tail are filter-position reads
        // (row selection / positioning / count), none feeding lineage. ORDER BY
        // keys reference the target's columns, so they read it (source/sink); a
        // constant LIMIT adds no read but is bound for parity, not dropped.
        let mut filter_reads: Vec<Expr> = delete
            .selection
            .iter()
            .map(|predicate| self.bind_expr(predicate, &scope))
            .collect();
        filter_reads.extend(self.order_by_expr_keys(&delete.order_by, &scope));
        filter_reads.extend(delete.limit.iter().map(|e| self.bind_expr(e, &scope)));
        if !filter_reads.is_empty() {
            input = LogicalPlan::Filter(Filter {
                input: Box::new(input),
                predicate: filter_reads,
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
            self.record_unsupported_dml_target("MERGE", &merge.table);
            return LogicalPlan::Empty;
        }
        let diagnostics_before = self.diagnostics.len();
        let Some((target_relation, target, target_resolution)) = self.target_relation(&merge.table)
        else {
            // A non-writable MERGE target (a CTE name, derived table, subquery,
            // or table function) can't be a write target — flag it rather than
            // dropping the whole statement silently. A name that flagged itself
            // (e.g. too many qualifiers) isn't double-reported.
            if self.diagnostics.len() == diagnostics_before {
                self.record_unsupported_dml_target("MERGE", &merge.table);
            }
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
                            let catalog_cols = self.catalog_columns(&target);
                            let columns = if explicit.is_empty() {
                                catalog_cols.clone()
                            } else {
                                explicit
                            };
                            // A MERGE INSERT is a single VALUES row. Each row is
                            // now a `Parens<Vec<Expr>>`; flatten through its
                            // inner expressions (via `Deref`).
                            let row: Vec<Expr> = values
                                .rows
                                .iter()
                                .flat_map(|row| row.iter())
                                .map(|e| self.bind_expr(e, &scope))
                                .collect();
                            // Column-list-less and no catalog to fill the target
                            // columns: the values can't be paired (see `bind_insert`).
                            // A MERGE INSERT VALUES has no wildcard, so the cause
                            // is always the missing catalog.
                            if columns.is_empty() && !row.is_empty() {
                                self.record_insert_columns_unresolved(&target, false);
                            }
                            // Arity (mirrors `bind_insert`): an explicit column
                            // list must match the row exactly; a column-less
                            // catalog-filled target is flagged only when the row
                            // overflows it.
                            if !row.is_empty() {
                                if !insert.columns.is_empty() {
                                    self.diagnose_insert_arity(
                                        &target,
                                        true,
                                        columns.len(),
                                        row.len(),
                                    );
                                } else if !catalog_cols.is_empty() {
                                    self.diagnose_insert_arity(
                                        &target,
                                        false,
                                        catalog_cols.len(),
                                        row.len(),
                                    );
                                }
                            }
                            clauses.push(MergeClause::Insert {
                                columns: self.column_writes(&target, &catalog_cols, columns),
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
                    // own columns (a tuple SET expands per target column; the
                    // source is read-only, hence the empty candidate set).
                    let assignments = update
                        .assignments
                        .iter()
                        .flat_map(|a| self.bind_assignment(a, &scope, &target, &[]))
                        .collect();
                    clauses.push(MergeClause::Update { assignments });
                }
                MergeAction::Delete { .. } => {
                    clauses.push(MergeClause::Delete);
                }
            }
        }
        // RETURNING (Snowflake) / OUTPUT (MSSQL) projects the affected rows
        // over the target + source scope, like the other DML roots. (MSSQL
        // `OUTPUT … INTO <table>`'s secondary write is not modelled yet.)
        let output_items = merge.output.as_ref().map(|o| match o {
            OutputClause::Output { select_items, .. }
            | OutputClause::Returning { select_items, .. } => select_items.clone(),
        });
        let returning = self.bind_returning(&output_items, &scope);
        LogicalPlan::Merge(Merge {
            target: TableWrite {
                reference: target,
                resolution: target_resolution,
            },
            source: Box::new(source),
            on,
            clauses,
            returning,
        })
    }

    /// Resolve a DML target table factor into its scope relation (for
    /// resolving SET / WHERE against the target's columns) and its canonical
    /// write-target identity. Returns `None` for a non-table factor.
    pub(super) fn target_relation(
        &mut self,
        factor: &TableFactor,
    ) -> Option<(Relation, TableReference, ResolutionKind)> {
        // A DML target is a real table, never a CTE: a database resolves the
        // target against the catalog, not the `WITH` list (a CTE is read-only —
        // Postgres errors `relation "c" does not exist` for `WITH c … UPDATE c`,
        // and updates the *base* table when one shares the CTE's name). So
        // resolve a plain name CTE-blind via `bind_named_table` (which skips the
        // `CteRef` shortcut `bind_table_factor` takes); a non-name factor
        // (subquery / derived / join / table function) is never a writable
        // table. Either way a non-table target returns `None` and the caller
        // flags it.
        let TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } = factor
        else {
            return None;
        };
        // `table_ref` flags an over-qualified name (and returns `None`) itself.
        let written = self.table_ref(name)?;
        let m = self.table_match(&written);
        // A name that matches a declared CTE and isn't a catalog table is the
        // CTE, not a writable table — not a target.
        if m.resolution != ResolutionKind::Cataloged && self.is_declared_cte(&written) {
            return None;
        }
        let alias_name = alias.as_ref().map(|a| a.name.clone());
        let (scan, scope) = self.bind_named_table(&written, alias_name);
        let relation = scope.relations.into_iter().next()?;
        let Relation::Table { table, .. } = &relation else {
            return None;
        };
        let resolution = match &scan {
            LogicalPlan::Scan(s) => s.resolution,
            _ => ResolutionKind::Inferred,
        };
        let target = table.clone();
        Some((relation, target, resolution))
    }

    /// Whether a bare (single-segment) target name matches a CTE declared in
    /// scope — used to reject a DML target that names a CTE (a CTE is read-only,
    /// never a writable table). Mirrors the CTE lookup `bind_table_factor` does
    /// for a FROM reference, but here it gates flagging, not resolution.
    pub(super) fn is_declared_cte(&self, written: &TableReference) -> bool {
        written.schema.is_none()
            && written.catalog.is_none()
            && self
                .context
                .ctes
                .iter()
                .any(|c| self.eq(self.style.casing.table_alias, &c.name, &written.name))
    }

    /// The plain-table deletion targets of a FROM `TableWithJoins` (its
    /// relation plus any joined relations), catalog-canonicalised.
    pub(super) fn twj_table_targets(&mut self, twj: &TableWithJoins) -> Vec<TableWrite> {
        let writtens: Vec<TableReference> = std::iter::once(&twj.relation)
            .chain(twj.joins.iter().map(|join| &join.relation))
            .filter_map(|factor| TableReference::try_from(factor).ok())
            .collect();
        writtens
            .into_iter()
            .filter_map(|written| self.writable_target("DELETE", &written))
            .collect()
    }

    /// Resolve a DML target *name* to its real-table [`TableWrite`], or `None`
    /// (recording a diagnostic) when the name is a declared CTE rather than a
    /// writable base table. A catalog match wins (a base table sharing the CTE's
    /// name is the real target); an uncatalogued name that is *not* a CTE stays
    /// an `Inferred` table (the usual catalog-free best effort).
    pub(super) fn writable_target(
        &mut self,
        statement: &str,
        written: &TableReference,
    ) -> Option<TableWrite> {
        let m = self.table_match(written);
        if m.resolution != ResolutionKind::Cataloged && self.is_declared_cte(written) {
            self.record_unsupported_dml_target(statement, written);
            return None;
        }
        Some(TableWrite {
            reference: m.table,
            resolution: m.resolution,
        })
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
        if let Some(target) = self.scope_target(&written, scope) {
            return Some(target);
        }
        self.writable_target("DELETE", &written)
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
        self.target_scope_with_alias(target, None)
    }

    /// Like [`target_scope`](Self::target_scope), but exposing the target under
    /// `alias` — an Oracle inline-view target's base table may be aliased
    /// (`FROM emp e`), and its WHERE predicate qualifies through that alias
    /// (`e.dept`), which then shadows the bare table name as usual.
    pub(super) fn target_scope_with_alias(
        &self,
        target: &TableReference,
        alias: Option<Ident>,
    ) -> Scope {
        let m = self.table_match(target);
        let columns = Columns::from_catalog(m.columns);
        Scope::single(Relation::Table {
            alias,
            table: m.table,
            columns,
        })
    }

    /// Bind one SET assignment into the per-column [`Assignment`]s it writes —
    /// one for a single `col = expr`, several for a tuple `(a, b) = …` (one per
    /// target column). See [`bind_tuple_assignment`](Self::bind_tuple_assignment)
    /// for the tuple pairing. `scope` resolves a qualified `t2.col` and the RHS
    /// reads; `root` / `writable` drive the unqualified write attribution — see
    /// [`resolve_assignment_column`](Self::resolve_assignment_column) for the
    /// rules.
    pub(super) fn bind_assignment(
        &mut self,
        assignment: &SqlAssignment,
        scope: &Scope,
        root: &TableReference,
        writable: &[Relation],
    ) -> Vec<Assignment> {
        match &assignment.target {
            AssignmentTarget::ColumnName(name) => {
                let value = self.bind_expr(&assignment.value, scope);
                self.resolve_assignment_column(name, scope, root, writable)
                    .map(|(target, target_resolution)| Assignment {
                        target,
                        target_resolution,
                        value,
                    })
                    .into_iter()
                    .collect()
            }
            AssignmentTarget::Tuple(names) => {
                self.bind_tuple_assignment(names, &assignment.value, scope, root, writable)
            }
        }
    }

    /// Expand a tuple `SET (a, b, …) = rhs` into one [`Assignment`] per target,
    /// pairing each with its positional value: a row value `(e0, e1)`
    /// element-wise; a `(SELECT x, y)` subquery by output column (each target
    /// projects its positional output via [`Expr::Subquery`]'s `output`). The
    /// subquery is bound once (reads count once); each target's value clones
    /// that plan, differing only in which output column it projects — so the
    /// reads walker must see the subquery on exactly one target (the first; the
    /// rest carry `output > 0`, which it skips). Targets past the RHS arity, or
    /// that resolve to no writable table, are dropped.
    fn bind_tuple_assignment(
        &mut self,
        names: &[ObjectName],
        rhs: &SqlExpr,
        scope: &Scope,
        root: &TableReference,
        writable: &[Relation],
    ) -> Vec<Assignment> {
        let values: Vec<Expr> = match rhs {
            // `(a, b) = (e0, e1)` — a row value: each element is one value.
            SqlExpr::Tuple(elems) => elems.iter().map(|e| self.bind_expr(e, scope)).collect(),
            // `(a, b) = (SELECT x, y …)` — each target projects its positional
            // output column of the one subquery.
            SqlExpr::Subquery(query) => {
                let plan = self.bind_subquery(query, scope);
                (0..names.len())
                    .map(|output| Expr::Subquery {
                        plan: Box::new(plan.clone()),
                        output,
                    })
                    .collect()
            }
            // Any other RHS on a tuple target isn't SQL we model row-wise; bind
            // it once so its reads still surface (paired with the first target).
            other => vec![self.bind_expr(other, scope)],
        };
        names
            .iter()
            .zip(values)
            .filter_map(|(name, value)| {
                self.resolve_assignment_column(name, scope, root, writable)
                    .map(|(target, target_resolution)| Assignment {
                        target,
                        target_resolution,
                        value,
                    })
            })
            .collect()
    }

    /// Resolve a SET assignment's target column to the column it writes,
    /// qualified by its **resolved table**. Running example — a multi-table
    /// MySQL UPDATE, whose target clause makes both tables writable:
    ///
    /// `UPDATE t1 JOIN t2 ON t1.id = t2.id SET t2.col = 1, other = 2`
    ///
    /// The **qualified** target (`t2.col`) writes whichever in-scope real
    /// table its qualifier names — `t2` here. A qualifier naming no writable
    /// relation is **not** dropped (that silently erased the assignment, its
    /// RHS reads, and — for a sole assignment — the whole UPDATE from the
    /// write surfaces): with a single writable sink it reads as a composite
    /// subfield path on the root (PostgreSQL `SET address.city = …` updates
    /// column `address` of the target; `SET t.address.city = …` likewise
    /// with the root named first), and among several writable relations
    /// (MySQL multi-table, where no composite syntax exists) it surfaces
    /// unattributed (`table: None`, `Unresolved`) like an unqualified miss.
    /// The **unqualified** target (`other`) is attributed with the
    /// read side's candidate / pick rules over the `writable` relations
    /// (`t1`, `t2`) — but only when there are several, as here (a genuine
    /// inference); with zero or one the sink is named by the statement
    /// itself, so the DML `root` pins unconditionally. `writable` is the
    /// target-clause relations for a multi-table UPDATE (MySQL's writable
    /// set — snapshotted before `FROM` joins the scope, since PostgreSQL /
    /// T-SQL `UPDATE t SET … FROM u` never writes `u`), and **empty** for a
    /// conflict / MERGE SET (always the statement's own target).
    ///
    /// The full attribution matrix, `t2.col` / `other` as in the example
    /// (`✔` = the column-level catalog match: `Cataloged` iff the pinned
    /// table lists the column, else `Inferred`):
    ///
    /// | SET target | writable relations            | written table       | resolution        |
    /// |------------|-------------------------------|---------------------|-------------------|
    /// | `t2.col`   | (any)                         | `t2` (the qualifier's) | ✔              |
    /// | `other`    | none besides root (MERGE / ON CONFLICT) | root      | ✔                 |
    /// | `other`    | one (single-table UPDATE)     | root                | ✔                 |
    /// | `other`    | several — sole candidate      | the owner           | read-mirrored (`Cataloged` verbatim; witness over catalog-free suspects downgrades to `Inferred`) |
    /// | `other`    | several — several candidates  | *none*              | `Ambiguous`       |
    /// | `other`    | several — no candidate        | *none*              | `Unresolved`      |
    ///
    /// The read mirror keeps `SET a = a + 1` coherent: the write pins exactly
    /// the table the RHS read resolves to (or honestly neither, as `Ambiguous`
    /// / `Unresolved` with `table: None` — the write then contributes no
    /// table-level write, like an unattributed read contributes no scan).
    /// Real MySQL agrees with the unattributed rows: an unqualified column
    /// owned by several joined tables is an error (1052), so no write target
    /// ever exists to pin.
    fn resolve_assignment_column(
        &self,
        name: &ObjectName,
        scope: &Scope,
        root: &TableReference,
        writable: &[Relation],
    ) -> Option<(ColumnWrite, ResolutionKind)> {
        let parts: Vec<Ident> = name
            .0
            .iter()
            .filter_map(|p| p.as_ident().cloned())
            .collect();
        let column = parts.last()?.clone();
        let table = if parts.len() == 1 {
            match self.unqualified_write_binding(&column, writable) {
                // A genuine multi-relation inference: mirror the read outcome.
                Some(Binding::Base { table, resolution }) => {
                    let table_resolution = self.table_match(&table).resolution;
                    return Some((
                        ColumnWrite {
                            reference: crate::reference::ColumnReference {
                                table: Some(table),
                                name: column,
                            },
                            resolution,
                        },
                        table_resolution,
                    ));
                }
                Some(kind @ (Binding::Ambiguous | Binding::Unresolved)) => {
                    let resolution = match kind {
                        Binding::Ambiguous => ResolutionKind::Ambiguous,
                        _ => ResolutionKind::Unresolved,
                    };
                    return Some((
                        ColumnWrite {
                            reference: crate::reference::ColumnReference {
                                table: None,
                                name: column,
                            },
                            resolution,
                        },
                        resolution,
                    ));
                }
                // `Derived` / `Local` can't arise (only real tables are
                // candidates); a `writable` of zero / one relation means the
                // statement names the sink — the root.
                Some(Binding::Derived | Binding::Local) | None => root.clone(),
            }
        } else {
            let qualifier = &parts[..parts.len() - 1];
            match scope
                .relations
                .iter()
                .find_map(|rel| self.writable_qualifier_table(rel, qualifier))
            {
                Some(table) => table,
                None => return Some(self.unmatched_qualifier_write(&parts, root, writable)),
            }
        };
        // Re-match the resolved write-target table (`table` is canonical, so
        // this reproduces the root scan's / joined relation's catalog match):
        // its `resolution` is the table-level write resolution; whether the
        // written `column` is in its catalog column list is the column-level
        // one (`Cataloged` if listed, else `Inferred` — mirroring a base read).
        let m = self.table_match(&table);
        let column_resolution = if self.list_has(&m.columns, &column) {
            ResolutionKind::Cataloged
        } else {
            ResolutionKind::Inferred
        };
        Some((
            ColumnWrite {
                reference: crate::reference::ColumnReference {
                    table: Some(table),
                    name: column,
                },
                resolution: column_resolution,
            },
            m.resolution,
        ))
    }

    /// A SET target whose qualifier names no writable relation. In a
    /// struct-capable dialect ([`supports_struct_set_targets`]) with a **single**
    /// writable sink, the dotted path reads as a struct subfield on the root
    /// (PostgreSQL: `SET address.city = …` updates column `address` of the
    /// target — its SET grammar forbids relation qualifiers outright, so the
    /// leading segment is always a column; `SET t.address.city = …` the same
    /// with the root named first). Everywhere else — a table-qualifier-only
    /// dialect (MySQL / MSSQL, where the qualifier can only be a mistyped
    /// table), several writable relations, or a ≥3-segment path with no root
    /// prefix — the write surfaces unattributed (`table: None`,
    /// `Unresolved`). Either way the assignment (and its RHS reads /
    /// lineage) stays alive.
    ///
    /// [`supports_struct_set_targets`]: Binder::supports_struct_set_targets
    fn unmatched_qualifier_write(
        &self,
        parts: &[Ident],
        root: &TableReference,
        writable: &[Relation],
    ) -> (ColumnWrite, ResolutionKind) {
        let fold = self.style.casing.table;
        let root_prefixed = self.eq(fold, &parts[0], &root.name);
        let struct_reading = self.supports_struct_set_targets() && writable.len() < 2;
        let column = if struct_reading && root_prefixed && parts.len() >= 3 {
            Some(parts[1].clone())
        } else if struct_reading && !root_prefixed && parts.len() == 2 {
            Some(parts[0].clone())
        } else {
            None
        };
        match column {
            Some(column) => {
                let m = self.table_match(root);
                let resolution = if self.list_has(&m.columns, &column) {
                    ResolutionKind::Cataloged
                } else {
                    ResolutionKind::Inferred
                };
                (
                    ColumnWrite {
                        reference: crate::reference::ColumnReference {
                            table: Some(m.table),
                            name: column,
                        },
                        resolution,
                    },
                    m.resolution,
                )
            }
            None => (
                ColumnWrite {
                    reference: crate::reference::ColumnReference {
                        table: None,
                        name: parts[parts.len() - 1].clone(),
                    },
                    resolution: ResolutionKind::Unresolved,
                },
                ResolutionKind::Unresolved,
            ),
        }
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
    /// contributes target reads and a `QueryOutput` lineage edge. A
    /// `RETURNING *` expands like a projection wildcard (a cataloged target
    /// yields one item — a target read — per column); an unexpandable one
    /// stays suppressed. Positional completeness isn't consumed here — a
    /// RETURNING list pairs with nothing — so the flag is dropped.
    pub(super) fn bind_returning(
        &mut self,
        returning: &Option<Vec<SelectItem>>,
        scope: &Scope,
    ) -> Vec<NamedExpr> {
        match returning {
            Some(items) => self.bind_output_items(items, scope).0,
            None => Vec::new(),
        }
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
        let (input, scope) = self.bind_query(query);
        let columns: Vec<Ident> = create.columns.iter().map(|c| c.name.clone()).collect();
        let source_wildcard = !scope.outputs_complete;
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
        let (input, scope) = self.bind_query(&create.query);
        let columns: Vec<Ident> = create.columns.iter().map(|c| c.name.clone()).collect();
        let source_wildcard = !scope.outputs_complete;
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
        let (input, scope) = self.bind_query(query);
        let source_wildcard = !scope.outputs_complete;
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

/// The insertable target columns an Oracle inline-view INSERT target's
/// projection names: each item's underlying plain column (`a` / `t.a` /
/// `a AS x` all name base column `a` — an alias renames the view column, the
/// row still lands in the base one). Empty when *any* item is not a plain
/// column (a wildcard / expression): the positional pairing is then
/// indeterminate as a whole, so the caller falls back to the column-less
/// (catalog-fill / diagnostic) path. The `SelectItem` match is exhaustive so a
/// new variant forces a decision here.
fn view_target_columns(projection: &[SelectItem]) -> Vec<Ident> {
    let mut columns = Vec::new();
    for item in projection {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
            SelectItem::ExprWithAliases { .. }
            | SelectItem::QualifiedWildcard(..)
            | SelectItem::Wildcard(_) => return Vec::new(),
        };
        match inferred_name(expr) {
            Some(column) => columns.push(column),
            None => return Vec::new(),
        }
    }
    columns
}

/// Demote an unqualified conflict-scope reference that resolved to `EXCLUDED`
/// (an unqualified `Binding::Derived` — `EXCLUDED` is the scope's sole derived
/// relation) to [`Binding::Ambiguous`] — see the call in
/// [`Binder::bind_conflict`] for the engine-verified rationale. The walk stays
/// on the expression's **own** operands: a nested subquery owns its own scope,
/// so its bindings are left alone. The `Expr` match is exhaustive so a new
/// variant forces a decision here.
fn demote_unqualified_excluded(expr: &mut Expr) {
    match expr {
        Expr::Column(c) => {
            if c.qualifier.is_none() && matches!(c.binding, Binding::Derived) {
                c.binding = Binding::Ambiguous;
            }
        }
        Expr::Call { args } => args.iter_mut().for_each(demote_unqualified_excluded),
        Expr::Case {
            when,
            then,
            else_result,
        } => {
            when.iter_mut()
                .chain(then.iter_mut())
                .for_each(demote_unqualified_excluded);
            if let Some(e) = else_result {
                demote_unqualified_excluded(e);
            }
        }
        Expr::Window {
            arg,
            partition,
            order,
        } => {
            demote_unqualified_excluded(arg);
            partition
                .iter_mut()
                .chain(order.iter_mut())
                .for_each(demote_unqualified_excluded);
        }
        Expr::InSubquery { expr, .. } => demote_unqualified_excluded(expr),
        Expr::Filter(exprs) => exprs.iter_mut().for_each(demote_unqualified_excluded),
        // A merge fan-in can't arise (no USING join in a conflict scope) and
        // neither can an expanded slot (no projection); a subquery's plan owns
        // its own scope.
        Expr::Fanin(_) | Expr::DerivedSlot { .. } | Expr::Subquery { .. } | Expr::Exists(_) => {}
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
