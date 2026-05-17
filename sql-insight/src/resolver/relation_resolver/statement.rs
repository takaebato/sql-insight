use super::{FlowEdge, FlowTargetSpec, RelationResolver, TableRole};
use crate::error::Error;
use crate::relation::TableReference;
use sqlparser::ast::{
    Delete, FromTable, Merge, ObjectType, Statement, TableWithJoins, Update, UpdateTableFromKind,
};

impl<'a> RelationResolver<'a> {
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
                self.bind_base_table(
                    TableReference::try_from(&create_table.name)?,
                    None,
                    TableRole::Write,
                );
                if let Some(query) = &create_table.query {
                    // CTAS: until column-level CREATE TABLE writes are wired,
                    // the source query's projections surface as QueryOutput
                    // edges (not yet paired with the new table's columns).
                    self.resolve_query_emitting_query_output(query)?;
                }
                Ok(())
            }
            Statement::CreateView(create_view) => {
                self.bind_base_table(
                    TableReference::try_from(&create_view.name)?,
                    None,
                    TableRole::Write,
                );
                self.resolve_query_emitting_query_output(&create_view.query)?;
                if let Some(to) = &create_view.to {
                    self.bind_base_table(TableReference::try_from(to)?, None, TableRole::Write);
                }
                Ok(())
            }
            Statement::AlterView { name, query, .. } => {
                self.bind_base_table(TableReference::try_from(name)?, None, TableRole::Write);
                self.resolve_query_emitting_query_output(query).map(|_| ())
            }
            Statement::CreateVirtualTable { name, .. } => {
                self.bind_base_table(TableReference::try_from(name)?, None, TableRole::Write);
                Ok(())
            }
            Statement::AlterTable(alter_table) => {
                self.bind_base_table(
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
                        self.bind_base_table(
                            TableReference::try_from(name)?,
                            None,
                            TableRole::Write,
                        );
                    }
                }
                if let Some(table) = table {
                    self.bind_base_table(TableReference::try_from(table)?, None, TableRole::Write);
                }
                Ok(())
            }
            Statement::Truncate(truncate) => {
                for table in &truncate.table_names {
                    self.bind_base_table(
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
        self.bind_base_table(table, alias, TableRole::Write);
        if let Some(source) = &insert.source {
            // Raw resolve_query (not the QueryOutput-emitting wrapper):
            // INSERT pairs each projection item positionally with its
            // target column instead, emitting Persisted edges. UNION
            // sources surface as multiple projection groups, so each
            // branch pairs against the same target columns naturally.
            let resolved = self.resolve_query(source)?;
            for group in &resolved.projections {
                for (position, item) in group.items.iter().enumerate() {
                    let Some(target_col) = insert.columns.get(position) else {
                        continue;
                    };
                    let target = FlowTargetSpec::Persisted {
                        table: target_table.clone(),
                        column: target_col.clone(),
                    };
                    for source in &item.source_refs {
                        self.push_flow_edge(FlowEdge {
                            source: source.clone(),
                            target: target.clone(),
                            bare: item.bare,
                        });
                    }
                }
            }
        }
        for assignment in &insert.assignments {
            self.visit_expr(&assignment.value)?;
        }
        Ok(())
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
        let target_table = match &update.table.relation {
            sqlparser::ast::TableFactor::Table { .. } => {
                TableReference::try_from(&update.table.relation).ok()
            }
            _ => None,
        };
        for assignment in &update.assignments {
            let target_parts = assignment_target_parts(&assignment.target);
            let bare = super::query::expr_is_bare(&assignment.value);
            let refs_before = self.column_refs_len();
            self.visit_expr(&assignment.value)?;
            let Some(target_parts) = target_parts else {
                continue;
            };
            let Some(target_table_ref) =
                assignment_target_table(&target_parts, target_table.as_ref())
            else {
                continue;
            };
            let target = FlowTargetSpec::Persisted {
                table: target_table_ref,
                column: target_parts.last().cloned().unwrap(),
            };
            let new_count = self.column_refs_len() - refs_before;
            for offset in 0..new_count {
                let source = self.column_refs_slice(refs_before)[offset].clone();
                self.push_flow_edge(FlowEdge {
                    source,
                    target: target.clone(),
                    bare,
                });
            }
        }
        if let Some(selection) = &update.selection {
            self.with_filter_clause(|r| r.visit_expr(selection))?;
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
            self.bind_base_table(TableReference::try_from_name(name)?, None, TableRole::Write);
        }
        if let Some(selection) = &delete.selection {
            self.with_filter_clause(|r| r.visit_expr(selection))?;
        }
        Ok(())
    }

    fn visit_merge(&mut self, merge: &Merge) -> Result<(), Error> {
        self.visit_table_factor(&merge.table, TableRole::Write)?;
        self.visit_table_factor(&merge.source, TableRole::Read)?;
        self.with_filter_clause(|r| r.visit_expr(&merge.on))?;
        for clause in &merge.clauses {
            if let Some(predicate) = &clause.predicate {
                self.with_filter_clause(|r| r.visit_expr(predicate))?;
            }
        }
        Ok(())
    }
}

fn from_table_items(from: &FromTable) -> &[TableWithJoins] {
    match from {
        FromTable::WithFromKeyword(items) | FromTable::WithoutKeyword(items) => items,
    }
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
