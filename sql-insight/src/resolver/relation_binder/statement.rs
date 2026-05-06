use super::Binder;
use crate::error::Error;
use crate::relation::TableReference;
use sqlparser::ast::{
    Delete, FromTable, Merge, ObjectName, ObjectType, Statement, TableFactor, TableWithJoins,
    Update, UpdateTableFromKind,
};

impl Binder {
    pub(super) fn bind_statement(&mut self, statement: &Statement) -> Result<(), Error> {
        // Keep this match exhaustive. Unsupported variants are listed explicitly so sqlparser
        // Statement additions become compile errors instead of silent misses.
        match statement {
            Statement::Query(query) => self.bind_query(query),
            Statement::Insert(insert) => self.bind_insert(insert),
            Statement::Update(update) => self.bind_update(update),
            Statement::Delete(delete) => self.bind_delete(delete),
            Statement::Merge(merge) => self.bind_merge(merge),
            Statement::CreateTable(create_table) => {
                self.record_base_table(TableReference::try_from(&create_table.name)?);
                if let Some(query) = &create_table.query {
                    self.bind_query(query)?;
                }
                Ok(())
            }
            Statement::CreateView(create_view) => {
                self.record_base_table(TableReference::try_from(&create_view.name)?);
                self.bind_query(&create_view.query)?;
                if let Some(to) = &create_view.to {
                    self.record_base_table(TableReference::try_from(to)?);
                }
                Ok(())
            }
            Statement::AlterView { name, query, .. } => {
                self.record_base_table(TableReference::try_from(name)?);
                self.bind_query(query)
            }
            Statement::CreateVirtualTable { name, .. } => {
                self.record_base_table(TableReference::try_from(name)?);
                Ok(())
            }
            Statement::AlterTable(alter_table) => {
                self.record_base_table(TableReference::try_from(&alter_table.name)?);
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
                        self.record_base_table(TableReference::try_from(name)?);
                    }
                }
                if let Some(table) = table {
                    self.record_base_table(TableReference::try_from(table)?);
                }
                Ok(())
            }
            Statement::Truncate(truncate) => {
                for table in &truncate.table_names {
                    self.record_base_table(TableReference::try_from(&table.name)?);
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

    fn bind_insert(&mut self, insert: &sqlparser::ast::Insert) -> Result<(), Error> {
        self.record_base_table(TableReference::try_from(insert)?);
        if let Some(source) = &insert.source {
            self.bind_query(source)?;
        }
        for assignment in &insert.assignments {
            self.bind_expr(&assignment.value)?;
        }
        Ok(())
    }

    fn bind_update(&mut self, update: &Update) -> Result<(), Error> {
        self.bind_table_with_joins(&update.table)?;
        if let Some(from) = &update.from {
            let tables = match from {
                UpdateTableFromKind::BeforeSet(tables) | UpdateTableFromKind::AfterSet(tables) => {
                    tables
                }
            };
            for table in tables {
                self.bind_table_with_joins(table)?;
            }
        }
        for assignment in &update.assignments {
            self.bind_expr(&assignment.value)?;
        }
        if let Some(selection) = &update.selection {
            self.bind_expr(selection)?;
        }
        Ok(())
    }

    fn bind_delete(&mut self, delete: &Delete) -> Result<(), Error> {
        let insertion_index = self.references.len();
        let target_names = if !delete.tables.is_empty() {
            delete.tables.clone()
        } else if delete.using.is_some() {
            delete_from_table_names(delete)
        } else {
            Vec::new()
        };

        if delete.using.is_some() {
            if let Some(using) = &delete.using {
                for table in using {
                    self.bind_table_with_joins(table)?;
                }
            }
        } else {
            for table in from_table_items(&delete.from) {
                self.bind_table_with_joins(table)?;
            }
        }

        if let Some(selection) = &delete.selection {
            self.bind_expr(selection)?;
        }

        if !target_names.is_empty() {
            let mut targets = Vec::new();
            for target in &target_names {
                targets.push(self.resolve_delete_target(target)?);
            }
            self.references.insert_many_at(insertion_index, targets);
        }
        Ok(())
    }

    fn bind_merge(&mut self, merge: &Merge) -> Result<(), Error> {
        self.bind_table_factor(&merge.table)?;
        self.bind_table_factor(&merge.source)?;
        self.bind_expr(&merge.on)?;
        for clause in &merge.clauses {
            if let Some(predicate) = &clause.predicate {
                self.bind_expr(predicate)?;
            }
        }
        Ok(())
    }
}

fn delete_from_table_names(delete: &Delete) -> Vec<ObjectName> {
    let from = match &delete.from {
        FromTable::WithFromKeyword(items) => items,
        FromTable::WithoutKeyword(items) => items,
    };
    let mut names = Vec::new();
    for table_with_joins in from {
        collect_table_factor_names(&table_with_joins.relation, &mut names);
        for join in &table_with_joins.joins {
            collect_table_factor_names(&join.relation, &mut names);
        }
    }
    names
}

fn from_table_items(from: &FromTable) -> &[TableWithJoins] {
    match from {
        FromTable::WithFromKeyword(items) | FromTable::WithoutKeyword(items) => items,
    }
}

fn collect_table_factor_names(table_factor: &TableFactor, names: &mut Vec<ObjectName>) {
    match table_factor {
        TableFactor::Table { name, .. } => names.push(name.clone()),
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => {
            collect_table_factor_names(&table_with_joins.relation, names);
            for join in &table_with_joins.joins {
                collect_table_factor_names(&join.relation, names);
            }
        }
        TableFactor::Pivot { table, .. }
        | TableFactor::Unpivot { table, .. }
        | TableFactor::MatchRecognize { table, .. } => {
            collect_table_factor_names(table, names);
        }
        TableFactor::Derived { .. }
        | TableFactor::TableFunction { .. }
        | TableFactor::Function { .. }
        | TableFactor::UNNEST { .. }
        | TableFactor::JsonTable { .. }
        | TableFactor::OpenJsonTable { .. }
        | TableFactor::XmlTable { .. }
        | TableFactor::SemanticView { .. } => {}
    }
}
