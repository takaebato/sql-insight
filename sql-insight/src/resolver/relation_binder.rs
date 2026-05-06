mod expr;
mod query;
mod statement;
mod table;

use std::collections::HashMap;

use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::error::Error;
use crate::relation::TableReference;
use sqlparser::ast::{Ident, ObjectName, Statement, TableWithJoins};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ScopeId(usize);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RelationKey {
    Unquoted(String),
    Quoted(String),
}

impl RelationKey {
    fn from_ident(ident: &Ident) -> Self {
        if ident.quote_style.is_some() {
            Self::Quoted(ident.value.clone())
        } else {
            Self::Unquoted(ident.value.to_ascii_lowercase())
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct ResolvedStatement {
    pub(crate) table_references: Vec<TableReference>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) scopes: Vec<RelationScope>,
}

impl ResolvedStatement {
    pub(crate) fn into_tables(self) -> Vec<TableReference> {
        let Self {
            table_references,
            diagnostics: _,
            scopes: _,
        } = self;
        table_references
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct RelationScope {
    pub(crate) id: ScopeId,
    pub(crate) parent: Option<ScopeId>,
    bindings: HashMap<RelationKey, RelationBinding>,
}

impl RelationScope {
    fn new(id: ScopeId, parent: Option<ScopeId>) -> Self {
        Self {
            id,
            parent,
            bindings: HashMap::new(),
        }
    }

    fn bind(&mut self, name: &Ident, binding: RelationBinding) {
        self.bindings.insert(RelationKey::from_ident(name), binding);
    }

    fn resolve(&self, name: &Ident) -> Option<&RelationBinding> {
        self.bindings.get(&RelationKey::from_ident(name))
    }
}

#[derive(Default, Debug)]
struct TableReferenceCollector {
    references: Vec<TableReference>,
}

impl TableReferenceCollector {
    fn len(&self) -> usize {
        self.references.len()
    }

    fn push(&mut self, table: TableReference) {
        self.references.push(table);
    }

    fn insert_many_at(&mut self, index: usize, tables: Vec<TableReference>) {
        self.references.splice(index..index, tables);
    }

    fn into_tables(self) -> Vec<TableReference> {
        self.references
    }
}

#[derive(Default, Debug)]
struct ScopeStack {
    scopes: Vec<RelationScope>,
    stack: Vec<ScopeId>,
}

impl ScopeStack {
    fn into_scopes(self) -> Vec<RelationScope> {
        self.scopes
    }

    fn push_query_scope(&mut self) {
        let parent = self.stack.last().copied();
        self.push_scope(parent);
    }

    fn pop_scope(&mut self) {
        self.stack.pop();
    }

    fn bind_current(&mut self, name: Ident, binding: RelationBinding) {
        self.current_scope_mut().bind(&name, binding);
    }

    fn resolve_unqualified_relation(&self, relation: &ObjectName) -> Option<&RelationBinding> {
        if relation.0.len() != 1 {
            return None;
        }
        let name = relation.0[0].as_ident()?;
        self.stack
            .iter()
            .rev()
            .find_map(|scope_id| self.scopes[scope_id.0].resolve(name))
    }

    fn push_scope(&mut self, parent: Option<ScopeId>) -> ScopeId {
        let id = ScopeId(self.scopes.len());
        self.scopes.push(RelationScope::new(id, parent));
        self.stack.push(id);
        id
    }

    fn current_scope_id(&mut self) -> ScopeId {
        if let Some(id) = self.stack.last() {
            *id
        } else {
            self.push_scope(None)
        }
    }

    fn current_scope_mut(&mut self) -> &mut RelationScope {
        let id = self.current_scope_id();
        &mut self.scopes[id.0]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RelationBinding {
    BaseTable(Box<TableReference>),
    Cte,
    DerivedTable,
    TableFunction,
}

pub(crate) struct RelationBinder;

impl RelationBinder {
    pub(crate) fn bind_statement(statement: &Statement) -> Result<ResolvedStatement, Error> {
        let mut binder = Binder::default();
        binder.bind_statement(statement)?;
        Ok(binder.into_resolved_statement())
    }

    pub(crate) fn bind_table_node(table: &TableWithJoins) -> Result<ResolvedStatement, Error> {
        let mut binder = Binder::default();
        binder.bind_table_with_joins(table)?;
        Ok(binder.into_resolved_statement())
    }
}

#[derive(Default, Debug)]
struct Binder {
    references: TableReferenceCollector,
    diagnostics: Vec<Diagnostic>,
    scopes: ScopeStack,
}

impl Binder {
    fn into_resolved_statement(self) -> ResolvedStatement {
        ResolvedStatement {
            table_references: self.references.into_tables(),
            diagnostics: self.diagnostics,
            scopes: self.scopes.into_scopes(),
        }
    }

    fn is_cte_reference(&self, relation: &ObjectName) -> bool {
        matches!(
            self.scopes.resolve_unqualified_relation(relation),
            Some(RelationBinding::Cte)
        )
    }

    fn record_base_table(&mut self, table: TableReference) {
        self.references.push(table.clone());
        self.bind_base_table(table);
    }

    fn bind_base_table(&mut self, table: TableReference) {
        let binding_name = table.alias.clone().unwrap_or_else(|| table.name.clone());
        self.bind_relation(binding_name, RelationBinding::BaseTable(Box::new(table)));
    }

    fn bind_cte(&mut self, name: Ident) {
        self.bind_relation(name, RelationBinding::Cte);
    }

    fn record_diagnostic(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    fn record_unsupported_statement(&mut self, statement: &Statement) {
        self.record_diagnostic(Diagnostic {
            kind: DiagnosticKind::UnsupportedStatement,
            message: format!("Unsupported statement while inspecting SQL: {}", statement),
        });
    }

    fn bind_relation(&mut self, name: Ident, binding: RelationBinding) {
        self.scopes.bind_current(name, binding);
    }

    fn resolve_delete_target(&self, relation: &ObjectName) -> Result<TableReference, Error> {
        if let Some(RelationBinding::BaseTable(table)) =
            self.scopes.resolve_unqualified_relation(relation)
        {
            Ok((**table).clone())
        } else {
            TableReference::try_from(relation)
        }
    }
}
