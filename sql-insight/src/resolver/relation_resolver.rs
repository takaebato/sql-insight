mod expr;
mod query;
mod statement;
mod table;

use indexmap::IndexMap;

use crate::catalog::{Catalog, ColumnSchema};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::error::Error;
use crate::relation::TableReference;
use sqlparser::ast::{Ident, ObjectName, Statement};

/// Internal role a table binding carries within a statement. Surfaced to
/// the operation extractor via [`RelationResolution::table_reads`] and
/// [`RelationResolution::table_writes`]; the public API exposes two
/// separate lists instead of this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TableRole {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ScopeId(usize);

/// Whether a scope contributes data to its enclosing write target.
///
/// - `Body`: data flows through — query bodies, CTE bodies, derived
///   tables, INSERT/MERGE sources, scalar subqueries in projection or
///   SET. Tables bound here participate in `TableFlow` edges when the
///   statement has a write target.
/// - `Predicate`: scope is referenced only in a constraint — WHERE,
///   HAVING, JOIN ON, EXISTS, IN, QUALIFY. Tables bound under any
///   Predicate ancestor are filtered out of `TableFlow` regardless of
///   their own kind, so `INSERT INTO t SELECT FROM s WHERE id IN
///   (SELECT id FROM x)` emits `s → t` but not `x → t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub(crate) enum ScopeKind {
    Body,
    Predicate,
}

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
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) scopes: Vec<RelationScope>,
}

impl RelationResolution {
    /// All tables touched by the statement, in scope-arena order. The
    /// union of [`read_tables`] and [`write_tables`] (with duplicates
    /// when a single table carries both roles).
    pub(crate) fn tables(&self) -> Vec<TableReference> {
        self.scopes
            .iter()
            .flat_map(|scope| scope.iter_bindings())
            .filter_map(|binding| match binding {
                RelationBinding::Table { table, .. } => Some((**table).clone()),
                _ => None,
            })
            .collect()
    }

    /// Every table referenced as a Read source, in scope-arena order.
    /// Includes tables inside predicate subqueries (e.g. `x` in `WHERE
    /// id IN (SELECT id FROM x)`). Use [`feeding_read_tables`] for the
    /// stricter "feeds the enclosing write target" filter.
    pub(crate) fn read_tables(&self) -> Vec<TableReference> {
        self.collect_tables_by_role(TableRole::Read)
    }

    /// Every table referenced as a Write target, in scope-arena order.
    pub(crate) fn write_tables(&self) -> Vec<TableReference> {
        self.collect_tables_by_role(TableRole::Write)
    }

    fn collect_tables_by_role(&self, role: TableRole) -> Vec<TableReference> {
        self.scopes
            .iter()
            .flat_map(|scope| scope.iter_bindings())
            .filter_map(|binding| match binding {
                RelationBinding::Table { table, roles, .. } if roles.contains(&role) => {
                    Some((**table).clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Read-role tables in a data-feeding position — Read role plus no
    /// `Predicate` ancestor in their scope chain. The basis for
    /// `TableFlow` edge sources.
    pub(crate) fn feeding_read_tables(&self) -> Vec<TableReference> {
        self.scopes
            .iter()
            .filter(|scope| !self.has_predicate_ancestor(scope.id))
            .flat_map(|scope| scope.iter_bindings())
            .filter_map(|binding| match binding {
                RelationBinding::Table { table, roles, .. } if roles.contains(&TableRole::Read) => {
                    Some((**table).clone())
                }
                _ => None,
            })
            .collect()
    }

    fn has_predicate_ancestor(&self, scope_id: ScopeId) -> bool {
        let mut current = Some(scope_id);
        while let Some(id) = current {
            let scope = &self.scopes[id.0];
            if scope.kind == ScopeKind::Predicate {
                return true;
            }
            current = scope.parent;
        }
        false
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct RelationScope {
    pub(crate) id: ScopeId,
    pub(crate) parent: Option<ScopeId>,
    pub(crate) kind: ScopeKind,
    bindings: IndexMap<RelationKey, RelationBinding>,
}

impl RelationScope {
    fn new(id: ScopeId, parent: Option<ScopeId>, kind: ScopeKind) -> Self {
        Self {
            id,
            parent,
            kind,
            bindings: IndexMap::new(),
        }
    }

    fn bind(&mut self, name: &Ident, binding: RelationBinding) {
        let key = RelationKey::from_ident(name);
        // Re-binding the same name as a Table merges roles rather
        // than replacing — this captures the `DELETE t1 FROM t1` style
        // case where a single name plays multiple roles in one statement.
        if let (
            Some(RelationBinding::Table {
                roles: existing, ..
            }),
            RelationBinding::Table { roles: new, .. },
        ) = (self.bindings.get_mut(&key), &binding)
        {
            for role in new {
                if !existing.contains(role) {
                    existing.push(*role);
                }
            }
            return;
        }
        self.bindings.insert(key, binding);
    }

    fn resolve(&self, name: &Ident) -> Option<&RelationBinding> {
        self.bindings.get(&RelationKey::from_ident(name))
    }

    fn iter_bindings(&self) -> impl Iterator<Item = &RelationBinding> {
        self.bindings.values()
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

    fn push_query_scope(&mut self, kind: ScopeKind) -> ScopeId {
        let parent = self.stack.last().copied();
        self.push_scope(parent, kind)
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

    fn push_scope(&mut self, parent: Option<ScopeId>, kind: ScopeKind) -> ScopeId {
        let id = ScopeId(self.scopes.len());
        self.scopes.push(RelationScope::new(id, parent, kind));
        self.stack.push(id);
        id
    }

    fn current_scope_id(&mut self) -> ScopeId {
        if let Some(id) = self.stack.last() {
            *id
        } else {
            self.push_scope(None, ScopeKind::Body)
        }
    }

    fn current_scope_mut(&mut self) -> &mut RelationScope {
        let id = self.current_scope_id();
        &mut self.scopes[id.0]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum RelationSchema {
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
    // `table` is boxed because the variant otherwise dwarfs the others
    // (TableReference is ~300B) and inflates the entire enum's size.
    Table {
        table: Box<TableReference>,
        schema: RelationSchema,
        roles: Vec<TableRole>,
    },
    Cte {
        name: Ident,
        schema: RelationSchema,
    },
    DerivedTable {
        alias: Ident,
        schema: RelationSchema,
    },
    TableFunction {
        alias: Ident,
        schema: RelationSchema,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct ResolvedQuery {
    pub(crate) scope_id: ScopeId,
    pub(crate) output_schema: RelationSchema,
}

#[derive(Debug)]
pub(crate) struct RelationResolver<'a> {
    // `None` means the resolver runs without external schema enrichment;
    // table schemas stay `RelationSchema::Unknown` in that case.
    catalog: Option<&'a dyn Catalog>,
    diagnostics: Vec<Diagnostic>,
    scopes: ScopeStack,
    /// Kind stamped on the next pushed scope. Defaults to `Body`; clause
    /// walkers (WHERE, HAVING, JOIN ON, …) flip it to `Predicate` via
    /// [`with_scope_kind`] for the duration of their child walk so that
    /// subqueries nested inside those clauses inherit the right kind.
    pending_scope_kind: ScopeKind,
}

impl<'a> RelationResolver<'a> {
    fn new(catalog: Option<&'a dyn Catalog>) -> Self {
        Self {
            catalog,
            diagnostics: Vec::new(),
            scopes: ScopeStack::default(),
            pending_scope_kind: ScopeKind::Body,
        }
    }

    /// Temporarily set the kind to stamp on subquery scopes pushed inside
    /// `f`, then restore. Use around walks of predicate-position clauses
    /// (WHERE, HAVING, JOIN ON, etc.) so that nested subqueries are
    /// classified as `Predicate`.
    pub(crate) fn with_scope_kind<R>(
        &mut self,
        kind: ScopeKind,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let prev = std::mem::replace(&mut self.pending_scope_kind, kind);
        let r = f(self);
        self.pending_scope_kind = prev;
        r
    }

    pub(crate) fn resolve_statement(
        catalog: Option<&'a dyn Catalog>,
        statement: &Statement,
    ) -> Result<RelationResolution, Error> {
        let mut resolver = Self::new(catalog);
        resolver.visit_statement(statement)?;
        Ok(resolver.into_relation_resolution())
    }

    fn into_relation_resolution(self) -> RelationResolution {
        RelationResolution {
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

    fn bind_base_table(&mut self, table: TableReference, role: TableRole) {
        let binding_name = table.alias.clone().unwrap_or_else(|| table.name.clone());
        let schema = self.lookup_table_schema(&table);
        self.bind_relation(
            binding_name,
            RelationBinding::Table {
                table: Box::new(table),
                schema,
                roles: vec![role],
            },
        );
    }

    /// Query the optional catalog for a table's columns. The alias is
    /// stripped before the lookup because catalogs key tables by their
    /// catalog/schema/name triplet; the alias is a callsite concern.
    fn lookup_table_schema(&self, table: &TableReference) -> RelationSchema {
        let Some(catalog) = self.catalog else {
            return RelationSchema::Unknown;
        };
        let lookup_key = TableReference {
            alias: None,
            ..table.clone()
        };
        match catalog.columns(&lookup_key) {
            Some(cols) => RelationSchema::Known(
                cols.into_iter()
                    .map(|ColumnSchema { name }| Column { name })
                    .collect(),
            ),
            None => RelationSchema::Unknown,
        }
    }

    fn bind_cte(&mut self, name: Ident, schema: RelationSchema) {
        self.bind_relation(name.clone(), RelationBinding::Cte { name, schema });
    }

    fn bind_derived_table(&mut self, alias: Ident, schema: RelationSchema) {
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
                schema: RelationSchema::Unknown,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;
    use std::collections::HashMap;

    #[derive(Debug, Default)]
    struct TestCatalog {
        tables: HashMap<String, Vec<&'static str>>,
    }

    impl TestCatalog {
        fn with(mut self, name: &str, cols: Vec<&'static str>) -> Self {
            self.tables.insert(name.to_string(), cols);
            self
        }
    }

    impl Catalog for TestCatalog {
        fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>> {
            // Catalogs key by the catalog/schema/name triplet; the resolver
            // is responsible for stripping alias before calling. Verify that.
            assert!(
                table.alias.is_none(),
                "resolver must strip alias before catalog lookup"
            );
            self.tables.get(table.name.value.as_str()).map(|cols| {
                cols.iter()
                    .map(|c| ColumnSchema {
                        name: Ident::new(*c),
                    })
                    .collect()
            })
        }
    }

    fn resolve(sql: &str, catalog: Option<&dyn Catalog>) -> RelationResolution {
        let dialect = GenericDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        RelationResolver::resolve_statement(catalog, &statements[0]).unwrap()
    }

    fn first_table_schema(resolution: &RelationResolution) -> Option<&RelationSchema> {
        resolution
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.values())
            .find_map(|binding| match binding {
                RelationBinding::Table { schema, .. } => Some(schema),
                _ => None,
            })
    }

    #[test]
    fn catalog_hit_populates_table_schema() {
        let catalog = TestCatalog::default().with("users", vec!["id", "email"]);
        let resolution = resolve("SELECT * FROM users", Some(&catalog));
        match first_table_schema(&resolution) {
            Some(RelationSchema::Known(cols)) => {
                assert_eq!(cols.len(), 2);
                assert_eq!(cols[0].name.value, "id");
                assert_eq!(cols[1].name.value, "email");
            }
            other => panic!("expected RelationSchema::Known(...), got {:?}", other),
        }
    }

    #[test]
    fn catalog_miss_keeps_schema_unknown() {
        let catalog = TestCatalog::default();
        let resolution = resolve("SELECT * FROM users", Some(&catalog));
        assert!(matches!(
            first_table_schema(&resolution),
            Some(RelationSchema::Unknown)
        ));
    }

    #[test]
    fn no_catalog_keeps_schema_unknown() {
        let resolution = resolve("SELECT * FROM users", None);
        assert!(matches!(
            first_table_schema(&resolution),
            Some(RelationSchema::Unknown)
        ));
    }

    #[test]
    fn catalog_lookup_ignores_alias() {
        // The assert in TestCatalog::columns enforces that the resolver strips
        // the alias before calling, so this test passes only if that contract
        // holds. The Known schema also confirms the catalog matched on name.
        let catalog = TestCatalog::default().with("users", vec!["id"]);
        let resolution = resolve("SELECT * FROM users AS u", Some(&catalog));
        assert!(matches!(
            first_table_schema(&resolution),
            Some(RelationSchema::Known(_))
        ));
    }
}
