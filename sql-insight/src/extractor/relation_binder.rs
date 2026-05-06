use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;

use crate::error::Error;
use crate::extractor::table_extractor::TableReference;
use sqlparser::ast::{
    Delete, Ident, ObjectName, Query, Statement, TableFactor, TableWithJoins, Visit, Visitor,
};

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
    pub(crate) scopes: Vec<RelationScope>,
}

impl ResolvedStatement {
    pub(crate) fn into_tables(self) -> Vec<TableReference> {
        let Self {
            table_references,
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

#[derive(Clone, Debug)]
struct PendingCte {
    query: *const Query,
    alias: Ident,
}

#[derive(Default, Debug)]
struct QueryFrame {
    cte_alias_after_body: Option<Ident>,
    pending_ctes: VecDeque<PendingCte>,
}

#[derive(Default, Debug)]
struct CteVisibilityTracker {
    frames: Vec<QueryFrame>,
}

impl CteVisibilityTracker {
    fn begin_query(&mut self, query: &Query) {
        let cte_alias_after_body = self.consume_pending_cte_body(query);
        let pending_ctes = query
            .with
            .as_ref()
            .filter(|with| !with.recursive)
            .map(|with| {
                with.cte_tables
                    .iter()
                    .map(|cte| PendingCte {
                        query: cte.query.as_ref() as *const Query,
                        alias: cte.alias.name.clone(),
                    })
                    .collect::<VecDeque<PendingCte>>()
            })
            .unwrap_or_default();
        self.frames.push(QueryFrame {
            cte_alias_after_body,
            pending_ctes,
        });
    }

    fn end_query(&mut self) -> Option<Ident> {
        self.frames
            .pop()
            .and_then(|frame| frame.cte_alias_after_body)
    }

    fn consume_pending_cte_body(&mut self, query: &Query) -> Option<Ident> {
        let frame = self.frames.last_mut()?;
        let query = query as *const Query;
        if frame
            .pending_ctes
            .front()
            .is_some_and(|pending| pending.query == query)
        {
            frame.pending_ctes.pop_front().map(|pending| pending.alias)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RelationBinding {
    BaseTable(Box<TableReference>),
    Cte,
    DerivedTable,
    TableFunction,
}

#[derive(Clone, Debug)]
struct PendingDeleteTargets {
    insertion_index: usize,
    targets: Vec<ObjectName>,
}

#[derive(Default, Debug)]
struct DeleteTargetTracker {
    pending: Vec<PendingDeleteTargets>,
    skipped_relations: Vec<ObjectName>,
}

impl DeleteTargetTracker {
    fn begin_delete(&mut self, delete: &Delete, insertion_index: usize) {
        if !delete.tables.is_empty() {
            self.pending.push(PendingDeleteTargets {
                insertion_index,
                targets: delete.tables.clone(),
            });
        } else if delete.using.is_some() {
            let targets = delete_from_table_names(delete);
            self.skipped_relations.extend(targets.iter().cloned());
            self.pending.push(PendingDeleteTargets {
                insertion_index,
                targets,
            });
        }
    }

    fn finish_delete(&mut self, delete: &Delete) -> Result<Option<PendingDeleteTargets>, Error> {
        if delete.tables.is_empty() && delete.using.is_none() {
            return Ok(None);
        }
        self.pending.pop().map(Some).ok_or_else(|| {
            Error::AnalysisError("Internal error: pending delete targets not found".to_string())
        })
    }

    fn consume_skipped_relation(&mut self, relation: &ObjectName) -> bool {
        let Some(index) = self
            .skipped_relations
            .iter()
            .position(|target| same_object_name(target, relation))
        else {
            return false;
        };
        self.skipped_relations.remove(index);
        true
    }
}

fn same_object_name(left: &ObjectName, right: &ObjectName) -> bool {
    left.0.len() == right.0.len()
        && left.0.iter().zip(&right.0).all(|(left, right)| {
            match (left.as_ident(), right.as_ident()) {
                (Some(left), Some(right)) => {
                    RelationKey::from_ident(left) == RelationKey::from_ident(right)
                }
                _ => left == right,
            }
        })
}

pub(crate) struct RelationBinder;

impl RelationBinder {
    pub(crate) fn bind_statement(statement: &Statement) -> Result<ResolvedStatement, Error> {
        let mut visitor = BinderVisitor::default();
        match statement.visit(&mut visitor) {
            ControlFlow::Break(e) => Err(e),
            ControlFlow::Continue(()) => Ok(visitor.into_resolved_statement()),
        }
    }

    pub(crate) fn bind_table_node(table: &TableWithJoins) -> Result<ResolvedStatement, Error> {
        let mut visitor = BinderVisitor::default();
        match table.visit(&mut visitor) {
            ControlFlow::Break(e) => Err(e),
            ControlFlow::Continue(()) => Ok(visitor.into_resolved_statement()),
        }
    }
}

#[derive(Default, Debug)]
struct BinderVisitor {
    references: TableReferenceCollector,
    relation_of_table: bool,
    scopes: ScopeStack,
    ctes: CteVisibilityTracker,
    delete_targets: DeleteTargetTracker,
}

impl BinderVisitor {
    fn into_resolved_statement(self) -> ResolvedStatement {
        ResolvedStatement {
            table_references: self.references.into_tables(),
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

    fn bind_table_factor_alias(&mut self, table_factor: &TableFactor) {
        match table_factor {
            TableFactor::Derived { alias, .. } | TableFactor::NestedJoin { alias, .. } => {
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::DerivedTable);
                }
            }
            TableFactor::TableFunction { alias, .. }
            | TableFactor::Function { alias, .. }
            | TableFactor::UNNEST { alias, .. }
            | TableFactor::JsonTable { alias, .. }
            | TableFactor::OpenJsonTable { alias, .. }
            | TableFactor::XmlTable { alias, .. }
            | TableFactor::SemanticView { alias, .. } => {
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::TableFunction);
                }
            }
            TableFactor::Pivot { table, alias, .. }
            | TableFactor::Unpivot { table, alias, .. }
            | TableFactor::MatchRecognize { table, alias, .. } => {
                self.bind_table_factor_alias(table);
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::DerivedTable);
                }
            }
            TableFactor::Table { .. } => {}
        }
    }

    fn bind_relation(&mut self, name: Ident, binding: RelationBinding) {
        self.scopes.bind_current(name, binding);
    }

    fn bind_recursive_ctes(&mut self, query: &Query) {
        if let Some(with) = &query.with {
            if with.recursive {
                for cte in &with.cte_tables {
                    self.bind_relation(cte.alias.name.clone(), RelationBinding::Cte);
                }
            }
        }
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

impl Visitor for BinderVisitor {
    type Break = Error;

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.ctes.begin_query(query);
        self.scopes.push_query_scope();
        self.bind_recursive_ctes(query);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.scopes.pop_scope();
        if let Some(alias) = self.ctes.end_query() {
            self.bind_relation(alias, RelationBinding::Cte);
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<Self::Break> {
        if self.relation_of_table {
            self.relation_of_table = false;
            return ControlFlow::Continue(());
        }
        if self.is_cte_reference(relation) {
            return ControlFlow::Continue(());
        }
        match TableReference::try_from(relation) {
            Ok(table) => {
                self.references.push(table);
            }
            Err(e) => return ControlFlow::Break(e),
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, table_factor: &TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { name, alias, .. } = table_factor {
            self.relation_of_table = true;
            if self.delete_targets.consume_skipped_relation(name) {
                return ControlFlow::Continue(());
            }
            if self.is_cte_reference(name) {
                if let Some(alias) = alias {
                    self.bind_relation(alias.name.clone(), RelationBinding::Cte);
                }
                return ControlFlow::Continue(());
            }
            match TableReference::try_from(table_factor) {
                Ok(table) => {
                    self.record_base_table(table);
                }
                Err(e) => return ControlFlow::Break(e),
            }
        } else {
            self.bind_table_factor_alias(table_factor);
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        if let Statement::Delete(delete) = statement {
            self.delete_targets
                .begin_delete(delete, self.references.len());
        }
        ControlFlow::Continue(())
    }

    fn post_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        if let Statement::Delete(delete) = statement {
            match self.delete_targets.finish_delete(delete) {
                Ok(Some(pending)) => {
                    let mut targets = Vec::new();
                    for table in &pending.targets {
                        match self.resolve_delete_target(table) {
                            Ok(table) => targets.push(table),
                            Err(e) => return ControlFlow::Break(e),
                        }
                    }
                    self.references
                        .insert_many_at(pending.insertion_index, targets);
                }
                Ok(None) => {}
                Err(e) => return ControlFlow::Break(e),
            }
        }
        ControlFlow::Continue(())
    }
}

fn delete_from_table_names(delete: &Delete) -> Vec<ObjectName> {
    let from = match &delete.from {
        sqlparser::ast::FromTable::WithFromKeyword(items) => items,
        sqlparser::ast::FromTable::WithoutKeyword(items) => items,
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
