//! Walker for `Statement`: top-level dispatch + per-DML walkers
//! (INSERT / UPDATE / DELETE / MERGE) including RETURNING, ON
//! CONFLICT / EXCLUDED binding, and CTAS / CREATE VIEW / ALTER VIEW
//! lineage emission.

use super::{BodyOutput, LineageTargetSpec, OutputColumn, Resolver, TableRole};
use crate::error::Error;
use crate::reference::TableReference;
use sqlparser::ast::{
    Delete, FromTable, Ident, Merge, ObjectType, OnConflictAction, OnInsert, SelectItem, Statement,
    TableWithJoins, Update, UpdateTableFromKind,
};

impl<'a> Resolver<'a> {
    pub(super) fn visit_statement(&mut self, statement: &Statement) -> Result<(), Error> {
        // Keep this match exhaustive. Unsupported variants are listed explicitly so sqlparser
        // Statement additions become compile errors instead of silent misses.
        match statement {
            Statement::Query(query) => self.resolve_query_emitting_query_output(query).map(|_| ()),
            Statement::Insert(insert) => self.visit_insert(insert),
            Statement::Update(update) => self.visit_update(update),
            Statement::Delete(delete) => self.visit_delete(delete),
            Statement::Merge(merge) => self.visit_merge(merge),
            Statement::CreateTable(create_table) => {
                let target = TableReference::try_from(&create_table.name)?;
                self.bind_real_table(target.clone(), None, TableRole::Write);
                if let Some(query) = &create_table.query {
                    // CTAS: source projections pair with the new
                    // table's columns. Explicit column defs (if any)
                    // win over inferred names from the source SELECT.
                    let explicit: Vec<sqlparser::ast::Ident> = create_table
                        .columns
                        .iter()
                        .map(|c| c.name.clone())
                        .collect();
                    let resolved = self.resolve_query(query)?;
                    self.emit_relation_to_created(&target, &explicit, &resolved);
                }
                Ok(())
            }
            Statement::CreateView(create_view) => {
                let target = TableReference::try_from(&create_view.name)?;
                self.bind_real_table(target.clone(), None, TableRole::Write);
                let explicit: Vec<sqlparser::ast::Ident> =
                    create_view.columns.iter().map(|c| c.name.clone()).collect();
                let resolved = self.resolve_query(&create_view.query)?;
                self.emit_relation_to_created(&target, &explicit, &resolved);
                if let Some(to) = &create_view.to {
                    self.bind_real_table(TableReference::try_from(to)?, None, TableRole::Write);
                }
                Ok(())
            }
            Statement::AlterView {
                name,
                query,
                columns,
                ..
            } => {
                let target = TableReference::try_from(name)?;
                self.bind_real_table(target.clone(), None, TableRole::Write);
                let resolved = self.resolve_query(query)?;
                self.emit_relation_to_created(&target, columns, &resolved);
                Ok(())
            }
            Statement::CreateVirtualTable { name, .. } => {
                self.bind_real_table(TableReference::try_from(name)?, None, TableRole::Write);
                Ok(())
            }
            Statement::AlterTable(alter_table) => {
                self.bind_real_table(
                    TableReference::try_from(&alter_table.name)?,
                    None,
                    TableRole::Write,
                );
                Ok(())
            }
            Statement::Drop {
                object_type,
                names,
                table,
                ..
            } => {
                if matches!(
                    object_type,
                    ObjectType::Table | ObjectType::View | ObjectType::MaterializedView
                ) {
                    for name in names {
                        self.bind_real_table(
                            TableReference::try_from(name)?,
                            None,
                            TableRole::Write,
                        );
                    }
                }
                if let Some(table) = table {
                    self.bind_real_table(TableReference::try_from(table)?, None, TableRole::Write);
                }
                Ok(())
            }
            Statement::Truncate(truncate) => {
                for table in &truncate.table_names {
                    self.bind_real_table(
                        TableReference::try_from(&table.name)?,
                        None,
                        TableRole::Write,
                    );
                }
                Ok(())
            }
            Statement::Analyze(_)
            | Statement::Set(_)
            | Statement::Msck(_)
            | Statement::Install { .. }
            | Statement::Load { .. }
            | Statement::Directory { .. }
            | Statement::Case(_)
            | Statement::If(_)
            | Statement::While(_)
            | Statement::Raise(_)
            | Statement::Call(_)
            | Statement::Copy { .. }
            | Statement::CopyIntoSnowflake { .. }
            | Statement::Open(_)
            | Statement::Close { .. }
            | Statement::CreateIndex(_)
            | Statement::CreateRole(_)
            | Statement::CreateSecret { .. }
            | Statement::CreateServer(_)
            | Statement::CreatePolicy(_)
            | Statement::CreateConnector(_)
            | Statement::CreateOperator(_)
            | Statement::CreateOperatorFamily(_)
            | Statement::CreateOperatorClass(_)
            | Statement::AlterSchema(_)
            | Statement::AlterIndex { .. }
            | Statement::AlterType(_)
            | Statement::AlterOperator(_)
            | Statement::AlterOperatorFamily(_)
            | Statement::AlterOperatorClass(_)
            | Statement::AlterRole { .. }
            | Statement::AlterPolicy(_)
            | Statement::AlterConnector { .. }
            | Statement::AlterSession { .. }
            | Statement::AttachDatabase { .. }
            | Statement::AttachDuckDBDatabase { .. }
            | Statement::DetachDuckDBDatabase { .. }
            | Statement::DropFunction(_)
            | Statement::DropDomain(_)
            | Statement::DropProcedure { .. }
            | Statement::DropSecret { .. }
            | Statement::DropPolicy(_)
            | Statement::DropConnector { .. }
            | Statement::Declare { .. }
            | Statement::CreateExtension(_)
            | Statement::DropExtension(_)
            | Statement::DropOperator(_)
            | Statement::DropOperatorFamily(_)
            | Statement::DropOperatorClass(_)
            | Statement::Fetch { .. }
            | Statement::Flush { .. }
            | Statement::Discard { .. }
            | Statement::ShowFunctions { .. }
            | Statement::ShowVariable { .. }
            | Statement::ShowStatus { .. }
            | Statement::ShowVariables { .. }
            | Statement::ShowCreate { .. }
            | Statement::ShowColumns { .. }
            | Statement::ShowDatabases { .. }
            | Statement::ShowSchemas { .. }
            | Statement::ShowCharset(_)
            | Statement::ShowObjects(_)
            | Statement::ShowTables { .. }
            | Statement::ShowViews { .. }
            | Statement::ShowCollation { .. }
            | Statement::Use(_)
            | Statement::StartTransaction { .. }
            | Statement::Comment { .. }
            | Statement::Commit { .. }
            | Statement::Rollback { .. }
            | Statement::CreateSchema { .. }
            | Statement::CreateDatabase { .. }
            | Statement::CreateFunction(_)
            | Statement::CreateTrigger(_)
            | Statement::DropTrigger(_)
            | Statement::CreateProcedure { .. }
            | Statement::CreateMacro { .. }
            | Statement::CreateStage { .. }
            | Statement::Assert { .. }
            | Statement::Grant(_)
            | Statement::Deny(_)
            | Statement::Revoke(_)
            | Statement::Deallocate { .. }
            | Statement::Execute { .. }
            | Statement::Prepare { .. }
            | Statement::Kill { .. }
            | Statement::ExplainTable { .. }
            | Statement::Explain { .. }
            | Statement::Savepoint { .. }
            | Statement::ReleaseSavepoint { .. }
            | Statement::Cache { .. }
            | Statement::UNCache { .. }
            | Statement::CreateSequence { .. }
            | Statement::CreateDomain(_)
            | Statement::CreateType { .. }
            | Statement::Pragma { .. }
            | Statement::LockTables { .. }
            | Statement::UnlockTables
            | Statement::Unload { .. }
            | Statement::OptimizeTable { .. }
            | Statement::LISTEN { .. }
            | Statement::UNLISTEN { .. }
            | Statement::NOTIFY { .. }
            | Statement::LoadData { .. }
            | Statement::RenameTable(_)
            | Statement::List(_)
            | Statement::Remove(_)
            | Statement::RaisError { .. }
            | Statement::Print(_)
            | Statement::Return(_)
            | Statement::ExportData(_)
            | Statement::CreateUser(_)
            | Statement::AlterUser(_)
            | Statement::Vacuum(_)
            | Statement::Reset(_) => {
                self.record_unsupported_statement(statement);
                Ok(())
            }
        }
    }

    fn visit_insert(&mut self, insert: &sqlparser::ast::Insert) -> Result<(), Error> {
        let (table, alias) = TableReference::from_insert_with_alias(insert)?;
        let target_table = table.clone();
        self.bind_real_table(table, alias, TableRole::Write);
        // Explicit column list wins; otherwise fall back to the
        // catalog-provided schema (when present) for positional
        // pairing. Without either, no lineage edges are emitted —
        // we have no target column names to pair against.
        let effective_columns = self.effective_target_columns(&insert.columns, &target_table);
        let source_output = if let Some(source) = &insert.source {
            // Raw resolve_query (not the QueryOutput-emitting wrapper):
            // INSERT pairs each output column positionally with its
            // target column instead, emitting Relation edges. UNION
            // sources surface as multiple branches, so each branch
            // pairs against the same target columns naturally.
            let resolved = self.resolve_query(source)?;
            if let Some(output) = resolved.output_columns.as_ref() {
                self.emit_per_output_column(&output.per_branch, |position, _col| {
                    effective_columns
                        .get(position)
                        .map(|col| LineageTargetSpec::Relation {
                            table: target_table.clone(),
                            column: col.clone(),
                        })
                });
            }
            resolved.output_columns
        } else {
            None
        };
        for assignment in &insert.assignments {
            self.visit_expr(&assignment.value)?;
        }
        // Walk RETURNING before the ON-clause so EXCLUDED isn't yet
        // bound: RETURNING projects from the target table, never from
        // the would-be-inserted pseudo-row, and an in-scope EXCLUDED
        // would ambify unqualified refs that collide with INSERT cols.
        self.visit_returning(insert.returning.as_deref())?;
        if let Some(on) = &insert.on {
            self.visit_insert_on(
                on,
                &target_table,
                &effective_columns,
                source_output.as_ref(),
            )?;
        }
        Ok(())
    }

    /// Walk a `RETURNING <select_items>` clause. Each item is treated
    /// like a top-level SELECT projection: it contributes refs to
    /// `column_refs` and a `QueryOutput` lineage edge per item. The
    /// target table is the only binding in scope (the source SELECT's
    /// inner scope has been popped by the time this runs), so
    /// unqualified refs resolve to it.
    fn visit_returning(&mut self, returning: Option<&[SelectItem]>) -> Result<(), Error> {
        let Some(items) = returning else {
            return Ok(());
        };
        let mut columns = Vec::with_capacity(items.len());
        for item in items {
            columns.push(self.build_output_column(item)?);
        }
        let branches = vec![columns];
        self.emit_per_output_column(&branches, |position, col| {
            Some(LineageTargetSpec::QueryOutput {
                name: col.name.clone(),
                position,
            })
        });
        Ok(())
    }

    /// Walk the optional ON-clause attached to an `INSERT`:
    /// `ON CONFLICT ... DO UPDATE SET ...` (Postgres / Sqlite) or
    /// `ON DUPLICATE KEY UPDATE ...` (MySQL). Both update-style
    /// actions reuse [`Self::emit_assignment_lineage`] so each
    /// assignment's RHS feeds a Relation-target lineage edge into the
    /// INSERT target's column, identical to a standalone `UPDATE`.
    ///
    /// The `EXCLUDED` pseudo-table (Postgres) is bound as a synthetic
    /// derived-table with the INSERT target's column list as its
    /// schema, so `EXCLUDED.<col>` refs filter out of the public
    /// `reads` surface (matching how CTE / derived refs behave) while
    /// still emitting valid lineage sources for the assignment edges.
    /// MySQL's equivalent (`VALUES(<col>)`) is a function-call form
    /// that visit_expr already walks; no extra binding needed.
    fn visit_insert_on(
        &mut self,
        on: &OnInsert,
        target_table: &TableReference,
        effective_columns: &[Ident],
        source_output: Option<&BodyOutput>,
    ) -> Result<(), Error> {
        match on {
            OnInsert::DuplicateKeyUpdate(assignments) => {
                // MySQL ON DUPLICATE KEY UPDATE doesn't expose the
                // would-be-inserted row as a pseudo-table; `VALUES(col)`
                // is the implicit-row form, parsed as a regular
                // function call. Don't bind EXCLUDED here — doing so
                // would make unqualified column refs inside the SET
                // expressions ambiguous against the INSERT target.
                self.emit_assignment_lineage(assignments, Some(target_table))?;
            }
            OnInsert::OnConflict(on_conflict) => {
                if let OnConflictAction::DoUpdate(do_update) = &on_conflict.action {
                    // EXCLUDED in Postgres / Sqlite exposes the
                    // would-be-inserted row as a row source. Bind it
                    // as a synthetic derived-table whose `output_columns`
                    // is the INSERT source's per-branch columns renamed
                    // positionally to the target column names. That way
                    // `EXCLUDED.<col>` collapses back to whatever
                    // expression feeds that position of the source
                    // (e.g. `EXCLUDED.b` → source's `y` when the INSERT
                    // pairs (a, b) ← (x, y)). Refs against `EXCLUDED`
                    // also filter out of the public `reads` surface
                    // (like CTE / derived).
                    //
                    // `body_scope: None` — EXCLUDED's output is
                    // synthesized from the INSERT source's per-branch
                    // columns, not walked into a fresh scope. The
                    // INSERT source's real tables are already covered
                    // by their own RawTableRefs, so EXCLUDED needs no
                    // collapse path for table lineage.
                    let excluded_output = excluded_body_output(effective_columns, source_output);
                    self.bind_derived_table(Ident::new("EXCLUDED"), excluded_output, None);
                    self.emit_assignment_lineage(&do_update.assignments, Some(target_table))?;
                    if let Some(selection) = &do_update.selection {
                        self.with_filter_clause(|r| r.visit_expr(selection))?;
                    }
                }
            }
            // `OnInsert` is `#[non_exhaustive]` in sqlparser. New
            // variants land silently here — revisit when sqlparser
            // grows another conflict-action shape.
            _ => {}
        }
        Ok(())
    }

    /// Emit Relation lineage edges for a CREATE-AS source: each
    /// projection item pairs with the created relation's column at
    /// the same position. Target column name comes from the explicit
    /// column list when present, otherwise from the projection's
    /// inferred name (alias > bare ident name); items without an
    /// inferable name and no explicit slot are silently skipped.
    /// Used by CTAS, CREATE VIEW, and ALTER VIEW.
    ///
    /// For UNION-bodied sources the result schema follows the LEFT
    /// branch's names (SQL standard), so the inferred-name fallback
    /// reads the first branch's column names rather than the current
    /// branch's — making every branch pair against the same target
    /// column at each position. Mirrors INSERT-SELECT-UNION
    /// positional pairing.
    fn emit_relation_to_created(
        &mut self,
        target: &TableReference,
        explicit_columns: &[sqlparser::ast::Ident],
        resolved: &super::ResolvedQuery,
    ) {
        let Some(output) = resolved.output_columns.as_ref() else {
            return;
        };
        let inferred_left_names: Vec<Option<Ident>> = output
            .per_branch
            .first()
            .map(|branch| branch.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        self.emit_per_output_column(&output.per_branch, |position, _col| {
            explicit_columns
                .get(position)
                .cloned()
                .or_else(|| inferred_left_names.get(position).cloned().flatten())
                .map(|column| LineageTargetSpec::Relation {
                    table: target.clone(),
                    column,
                })
        });
    }

    fn visit_update(&mut self, update: &Update) -> Result<(), Error> {
        // The head of update.table is the write target; joined tables
        // (inside visit_table_with_joins) are reads by definition.
        self.visit_table_with_joins(&update.table, TableRole::Write)?;
        if let Some(from) = &update.from {
            let tables = match from {
                UpdateTableFromKind::BeforeSet(tables) | UpdateTableFromKind::AfterSet(tables) => {
                    tables
                }
            };
            for table in tables {
                self.visit_table_with_joins(table, TableRole::Read)?;
            }
        }
        let target_table = try_target_table_from_factor(&update.table.relation);
        self.emit_assignment_lineage(&update.assignments, target_table.as_ref())?;
        if let Some(selection) = &update.selection {
            self.with_filter_clause(|r| r.visit_expr(selection))?;
        }
        self.visit_returning(update.returning.as_deref())?;
        Ok(())
    }

    /// Walk each SET-style assignment's RHS expression and emit
    /// Relation lineage edges from any newly recorded source refs into
    /// the assignment's target column. Shared by `visit_update` and
    /// MERGE's `WHEN MATCHED UPDATE` branch — both have identical
    /// per-assignment semantics. Target column qualifier resolution:
    /// qualified target (`t.col`) wins; bare target falls back to
    /// `default_table` (UPDATE head / MERGE INTO target).
    fn emit_assignment_lineage(
        &mut self,
        assignments: &[sqlparser::ast::Assignment],
        default_table: Option<&TableReference>,
    ) -> Result<(), Error> {
        for assignment in assignments {
            let target_parts = assignment_target_parts(&assignment.target);
            let kind = super::body_output::expr_kind(&assignment.value);
            let refs_before = self.column_refs_len();
            self.visit_expr(&assignment.value)?;
            let Some(target_parts) = target_parts else {
                continue;
            };
            let Some(target_table_ref) = assignment_target_table(&target_parts, default_table)
            else {
                continue;
            };
            let target = LineageTargetSpec::Relation {
                table: target_table_ref,
                column: target_parts.last().cloned().unwrap(),
            };
            self.push_edges_from_refs_since(refs_before, target, kind);
        }
        Ok(())
    }

    fn visit_delete(&mut self, delete: &Delete) -> Result<(), Error> {
        // Visit in alias-defining order so that later Write binds merge
        // onto already-resolved `TableReference`s rather than overwriting
        // them with bare names.
        //
        // The FROM clause's role depends on the shape of the DELETE:
        //   bare `DELETE FROM t`               → FROM is write target
        //   `DELETE FROM target USING source`  → FROM is write target, USING is read-and-alias-source
        //   `DELETE target FROM source`        → FROM is read-and-alias-source, tables list is write target
        //
        // In the USING shape the alias-defining clause is USING, so visit
        // USING first. In the explicit-target-list shape the
        // alias-defining clause is FROM, which we also want visited before
        // the tables list is merged on top.
        if let Some(using) = &delete.using {
            for table in using {
                self.visit_table_with_joins(table, TableRole::Read)?;
            }
        }
        let from_role = if delete.tables.is_empty() {
            TableRole::Write
        } else {
            TableRole::Read
        };
        for table in from_table_items(&delete.from) {
            self.visit_table_with_joins(table, from_role)?;
        }
        for name in &delete.tables {
            self.bind_real_table(TableReference::try_from_name(name)?, None, TableRole::Write);
        }
        if let Some(selection) = &delete.selection {
            self.with_filter_clause(|r| r.visit_expr(selection))?;
        }
        self.visit_returning(delete.returning.as_deref())?;
        Ok(())
    }

    fn visit_merge(&mut self, merge: &Merge) -> Result<(), Error> {
        use sqlparser::ast::{MergeAction, MergeInsertKind};
        self.visit_table_factor(&merge.table, TableRole::Write)?;
        self.visit_table_factor(&merge.source, TableRole::Read)?;
        self.with_filter_clause(|r| r.visit_expr(&merge.on))?;
        let target_table = try_target_table_from_factor(&merge.table);
        for clause in &merge.clauses {
            if let Some(predicate) = &clause.predicate {
                self.with_filter_clause(|r| r.visit_expr(predicate))?;
            }
            match &clause.action {
                MergeAction::Insert(insert_expr) => {
                    if let Some(pred) = &insert_expr.insert_predicate {
                        self.with_filter_clause(|r| r.visit_expr(pred))?;
                    }
                    if let MergeInsertKind::Values(values) = &insert_expr.kind {
                        self.emit_merge_insert_lineage(
                            values,
                            &insert_expr.columns,
                            target_table.as_ref(),
                        )?;
                    }
                    // MergeInsertKind::Row (BigQuery `INSERT ROW`) — the
                    // source row is inserted as-is; per-column pairing
                    // needs catalog knowledge of the target schema.
                }
                MergeAction::Update(update_expr) => {
                    self.emit_assignment_lineage(&update_expr.assignments, target_table.as_ref())?;
                }
                MergeAction::Delete { .. } => {
                    // DELETE has no column-level value lineage.
                }
            }
        }
        Ok(())
    }

    /// Emit per-position Relation lineage edges for MERGE's
    /// `WHEN NOT MATCHED THEN INSERT (cols) VALUES (...)`. Each value
    /// expression's source refs pair with the column at the same
    /// position in `columns`. Walks values with default `Projection`
    /// kind for read classification.
    fn emit_merge_insert_lineage(
        &mut self,
        values: &sqlparser::ast::Values,
        columns: &[sqlparser::ast::ObjectName],
        target_table: Option<&TableReference>,
    ) -> Result<(), Error> {
        // Resolve effective target column idents up-front: when the
        // INSERT clause has an explicit list, take each ObjectName's
        // last segment; otherwise fall back to the catalog-provided
        // schema (returns empty without catalog, matching the
        // no-pairing behavior).
        let explicit_idents: Vec<sqlparser::ast::Ident> = columns
            .iter()
            .filter_map(|c| c.0.last().and_then(|p| p.as_ident().cloned()))
            .collect();
        let effective_idents = match target_table {
            Some(target) => self.effective_target_columns(&explicit_idents, target),
            None => explicit_idents,
        };
        for row in &values.rows {
            for (position, value_expr) in row.iter().enumerate() {
                let kind = super::body_output::expr_kind(value_expr);
                let refs_before = self.column_refs_len();
                self.visit_expr(value_expr)?;
                let (Some(target_table), Some(col_ident)) =
                    (target_table, effective_idents.get(position))
                else {
                    continue;
                };
                let target = LineageTargetSpec::Relation {
                    table: target_table.clone(),
                    column: col_ident.clone(),
                };
                self.push_edges_from_refs_since(refs_before, target, kind);
            }
        }
        Ok(())
    }
}

/// Rename each source branch's columns positionally to the INSERT
/// target's column names — the EXCLUDED pseudo-table exposes the
/// would-be-inserted row, so `EXCLUDED.<target_col>` should collapse
/// back to whatever expression feeds that position of the source.
/// Returns `None` when there are no source output columns (e.g.
/// `INSERT ... VALUES (...) ON CONFLICT ...`) or no effective target
/// columns, in which case `collapse_source` falls back to leaving
/// `EXCLUDED.<col>` as the lineage source.
fn excluded_body_output(
    effective_columns: &[Ident],
    source_output: Option<&BodyOutput>,
) -> Option<BodyOutput> {
    if effective_columns.is_empty() {
        return None;
    }
    let source = source_output?;
    let per_branch: Vec<Vec<OutputColumn>> = source
        .per_branch
        .iter()
        .map(|branch| {
            branch
                .iter()
                .enumerate()
                .map(|(position, col)| {
                    let mut renamed = col.clone();
                    if let Some(name) = effective_columns.get(position) {
                        renamed.name = Some(name.clone());
                    }
                    renamed
                })
                .collect()
        })
        .collect();
    Some(BodyOutput { per_branch })
}

fn from_table_items(from: &FromTable) -> &[TableWithJoins] {
    match from {
        FromTable::WithFromKeyword(items) | FromTable::WithoutKeyword(items) => items,
    }
}

/// Best-effort extraction of a write-target `TableReference` from a
/// `TableFactor`. Only the plain `TableFactor::Table` variant has a
/// resolvable identity; derived / pivot / table-function targets are
/// not valid SQL write targets and return `None`, leaving the caller's
/// assignment / pairing logic to fall back to qualifier-only target
/// derivation.
fn try_target_table_from_factor(factor: &sqlparser::ast::TableFactor) -> Option<TableReference> {
    matches!(factor, sqlparser::ast::TableFactor::Table { .. })
        .then(|| TableReference::try_from(factor).ok())
        .flatten()
}

fn assignment_target_parts(
    target: &sqlparser::ast::AssignmentTarget,
) -> Option<Vec<sqlparser::ast::Ident>> {
    match target {
        sqlparser::ast::AssignmentTarget::ColumnName(name) => name
            .0
            .iter()
            .map(|p| p.as_ident().cloned())
            .collect::<Option<Vec<_>>>(),
        sqlparser::ast::AssignmentTarget::Tuple(_) => None,
    }
}

/// Derive the owning `TableReference` for an UPDATE SET target.
/// `parts.len() == 1`: bare column, take the UPDATE head as default.
/// `parts.len() >= 2`: take the leading parts as catalog/schema/table.
fn assignment_target_table(
    parts: &[sqlparser::ast::Ident],
    default_table: Option<&TableReference>,
) -> Option<TableReference> {
    match parts.len() {
        0 => None,
        1 => default_table.cloned(),
        2 => Some(TableReference {
            catalog: None,
            schema: None,
            name: parts[0].clone(),
        }),
        3 => Some(TableReference {
            catalog: None,
            schema: Some(parts[0].clone()),
            name: parts[1].clone(),
        }),
        4 => Some(TableReference {
            catalog: Some(parts[0].clone()),
            schema: Some(parts[1].clone()),
            name: parts[2].clone(),
        }),
        _ => None,
    }
}
