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
pub(crate) struct RelationResolution {
    pub(crate) table_references: Vec<TableReference>,
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) scopes: Vec<RelationScope>,
}

impl RelationResolution {
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

    fn push_query_scope(&mut self) -> ScopeId {
        let parent = self.stack.last().copied();
        self.push_scope(parent)
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
#[allow(dead_code)]
pub(crate) enum Schema {
    Known(Vec<Column>),
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct Column {
    pub(crate) name: Ident,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum RelationBinding {
    PhysicalTable { table: TableReference, schema: Schema },
    Cte { name: Ident, schema: Schema },
    DerivedTable { alias: Ident, schema: Schema },
    TableFunction { alias: Ident, schema: Schema },
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct ResolvedQuery {
    pub(crate) scope_id: ScopeId,
    pub(crate) output_schema: Schema,
}

#[derive(Default, Debug)]
pub(crate) struct RelationResolver {
    references: TableReferenceCollector,
    diagnostics: Vec<Diagnostic>,
    scopes: ScopeStack,
}

impl RelationResolver {
    pub(crate) fn resolve_statement(
        statement: &Statement,
    ) -> Result<RelationResolution, Error> {
        let mut resolver = Self::default();
        resolver.visit_statement(statement)?;
        Ok(resolver.into_relation_resolution())
    }

    pub(crate) fn resolve_table_node(
        table: &TableWithJoins,
    ) -> Result<RelationResolution, Error> {
        let mut resolver = Self::default();
        resolver.visit_table_with_joins(table)?;
        Ok(resolver.into_relation_resolution())
    }

    fn into_relation_resolution(self) -> RelationResolution {
        RelationResolution {
            table_references: self.references.into_tables(),
            diagnostics: self.diagnostics,
            scopes: self.scopes.into_scopes(),
        }
    }

    fn is_cte_reference(&self, relation: &ObjectName) -> bool {
        matches!(
            self.scopes.resolve_unqualified_relation(relation),
            Some(RelationBinding::Cte { .. })
        )
    }

    fn record_base_table(&mut self, table: TableReference) {
        self.references.push(table.clone());
        self.bind_base_table(table);
    }

    fn bind_base_table(&mut self, table: TableReference) {
        let binding_name = table.alias.clone().unwrap_or_else(|| table.name.clone());
        self.bind_relation(
            binding_name,
            RelationBinding::PhysicalTable {
                table,
                schema: Schema::Unknown,
            },
        );
    }

    fn bind_cte(&mut self, name: Ident, schema: Schema) {
        self.bind_relation(
            name.clone(),
            RelationBinding::Cte { name, schema },
        );
    }

    fn bind_derived_table(&mut self, alias: Ident, schema: Schema) {
        self.bind_relation(
            alias.clone(),
            RelationBinding::DerivedTable { alias, schema },
        );
    }

    fn bind_table_function(&mut self, alias: Ident) {
        self.bind_relation(
            alias.clone(),
            RelationBinding::TableFunction {
                alias,
                schema: Schema::Unknown,
            },
        );
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
        if let Some(RelationBinding::PhysicalTable { table, .. }) =
            self.scopes.resolve_unqualified_relation(relation)
        {
            Ok(table.clone())
        } else {
            TableReference::try_from(relation)
        }
    }
}
