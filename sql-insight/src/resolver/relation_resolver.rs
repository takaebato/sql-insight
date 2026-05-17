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
    /// Raw column references collected during the AST walk. Each entry
    /// records the identifier parts (`["t1", "a"]` for `t1.a`, `["a"]`
    /// for the bare unqualified `a`) and the scope where it appeared.
    /// Semantic interpretation (alias resolution, scope-chain lookup,
    /// `Passthrough` vs `Computed` classification) belongs to consumers.
    pub(crate) column_refs: Vec<RawColumnRef>,
    /// Flow edges emitted directly by the resolver — one entry per
    /// (source column ref, target) pair. The column extractor maps
    /// these 1:1 to `ColumnFlow` without re-walking the AST.
    pub(crate) flow_edges: Vec<FlowEdge>,
}

/// A pre-resolution column flow record. `source` still needs scope-chain
/// resolution (for unqualified parts); `target` is fully spec'd by the
/// resolver; `bare` distinguishes a passthrough source (bare
/// `Identifier` / `CompoundIdentifier`) from a computed expression.
///
/// Created by callers from [`ProjectionGroup`]s (for SELECT-style flows
/// — INSERT pairs with target columns, top-level / nested SELECTs emit
/// `QueryOutput`) or directly by UPDATE / similar walkers that already
/// know their write target.
#[derive(Debug, Clone)]
pub(crate) struct FlowEdge {
    pub(crate) source: RawColumnRef,
    pub(crate) target: FlowTargetSpec,
    pub(crate) bare: bool,
}

/// One SELECT's projection captured during the walk — one
/// `ProjectionItem` per output column, in projection order. Set
/// operations contribute one group per branch (so UNION INSERT pairs
/// each branch's items with the same target columns).
#[derive(Debug, Clone)]
pub(crate) struct ProjectionGroup {
    pub(crate) items: Vec<ProjectionItem>,
}

/// A single projection slot's resolver-collected facts.
///
/// `source_refs` are the raw column refs the projection item's
/// expression read, in walk order. `name` is the inferable output name
/// (explicit alias > bare ident name > `None`). `bare` is true iff the
/// projection item is a bare `Identifier` / `CompoundIdentifier`, used
/// to pick `Passthrough` vs `Computed` at the edge-emitter.
#[derive(Debug, Clone)]
pub(crate) struct ProjectionItem {
    pub(crate) name: Option<Ident>,
    pub(crate) source_refs: Vec<RawColumnRef>,
    pub(crate) bare: bool,
}

/// Target spec for a [`FlowEdge`]. `QueryOutput` is for transient
/// SELECT output columns; `Persisted` is for INSERT / UPDATE / etc.
/// target columns that live in a real relation.
#[derive(Debug, Clone)]
pub(crate) enum FlowTargetSpec {
    QueryOutput {
        name: Option<Ident>,
        position: usize,
    },
    Persisted {
        table: TableReference,
        column: Ident,
    },
}

/// An unresolved column reference captured by the resolver during the
/// AST walk. `parts` mirrors `sqlparser`'s split — 1 part for bare
/// `a`, 2 for `t1.a`, 3 for `schema.t1.a`, 4 for `catalog.schema.t1.a`.
/// `scope_id` is the scope in which the reference appeared and is the
/// entry point for scope-chain resolution of unqualified names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RawColumnRef {
    pub(crate) parts: Vec<Ident>,
    pub(crate) scope_id: ScopeId,
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

    /// Resolve an unqualified column name against the scope chain
    /// rooted at `scope_id`. Walks innermost-first; the first scope
    /// with any candidate wins (standard SQL inner-shadows-outer).
    /// Returns the owning table when exactly one binding in that
    /// scope could carry the column — a real `Table`, or a
    /// synthesized reference for `Cte` / `DerivedTable` /
    /// `TableFunction`. Returns `None` when 0 or 2+ bindings match.
    ///
    /// **Strictness scales with the catalog.** Without a catalog,
    /// Table bindings have `Unknown` schemas and qualify
    /// unconditionally: `SELECT a FROM t1` resolves `a` to t1 even
    /// though column existence is not verified. This matches the SQL
    /// spec's single-relation rule under the assumption that the SQL
    /// is valid — and matches the implicit promise of `catalog: None`
    /// (best-effort, not strict). With a catalog, Table bindings come
    /// back `Known(cols)`; columns absent from the table are rejected
    /// as candidates, eliminating false positives like a `count` typo
    /// (meant `count(*)`) resolving to `t1.count`.
    pub(crate) fn resolve_unqualified_column(
        &self,
        name: &Ident,
        scope_id: ScopeId,
    ) -> Option<TableReference> {
        let mut current = Some(scope_id);
        while let Some(id) = current {
            let scope = &self.scopes[id.0];
            let candidates: Vec<TableReference> = scope
                .iter_bindings()
                .filter_map(|b| binding_could_contain_column(b, name))
                .collect();
            if !candidates.is_empty() {
                // Inner scope shadows outer: as soon as a scope has any
                // candidate, stop walking. Standard SQL name resolution.
                return (candidates.len() == 1).then(|| candidates.into_iter().next().unwrap());
            }
            current = scope.parent;
        }
        None
    }
}

fn binding_could_contain_column(binding: &RelationBinding, name: &Ident) -> Option<TableReference> {
    match binding {
        RelationBinding::Table { table, schema, .. } => {
            schema_could_contain(schema, name).then(|| (**table).clone())
        }
        RelationBinding::Cte {
            name: cte_name,
            schema,
        } => schema_could_contain(schema, name).then(|| synthetic_table_ref(cte_name)),
        RelationBinding::DerivedTable { alias, schema } => {
            schema_could_contain(schema, name).then(|| synthetic_table_ref(alias))
        }
        // TableFunction schemas are always Unknown for now, so any
        // unqualified column could plausibly come from one.
        RelationBinding::TableFunction { alias, .. } => Some(synthetic_table_ref(alias)),
    }
}

fn schema_could_contain(schema: &RelationSchema, name: &Ident) -> bool {
    match schema {
        RelationSchema::Unknown => true,
        RelationSchema::Known(cols) => cols
            .iter()
            .any(|c| RelationKey::from_ident(&c.name) == RelationKey::from_ident(name)),
    }
}

fn synthetic_table_ref(name: &Ident) -> TableReference {
    TableReference {
        catalog: None,
        schema: None,
        name: name.clone(),
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
        /// Alias given at this use-site, if any. Kept separately so
        /// `TableReference` stays alias-free for catalog lookup and
        /// cross-statement comparison.
        alias: Option<Ident>,
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

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct ResolvedQuery {
    pub(crate) scope_id: ScopeId,
    pub(crate) output_schema: RelationSchema,
    /// One entry per top-level SELECT producing output rows for this
    /// query. A bare `SELECT ...` query yields exactly one group; a
    /// `SELECT ... UNION SELECT ...` yields one per branch. Callers
    /// decide what to do with them — emit `QueryOutput` edges (default)
    /// or pair with target columns (INSERT).
    pub(crate) projections: Vec<ProjectionGroup>,
}

#[derive(Debug)]
pub(crate) struct RelationResolver<'a> {
    // `None` means the resolver runs without external schema enrichment;
    // table schemas stay `RelationSchema::Unknown` in that case.
    catalog: Option<&'a dyn Catalog>,
    diagnostics: Vec<Diagnostic>,
    scopes: ScopeStack,
    column_refs: Vec<RawColumnRef>,
    flow_edges: Vec<FlowEdge>,
    /// Per-query buffer of projection groups collected by `visit_select`.
    /// `resolve_query` swaps a fresh buffer in for the duration of its
    /// walk and packs the collected groups into the returned
    /// `ResolvedQuery`, so each query gets exactly its own projections.
    current_projections: Vec<ProjectionGroup>,
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
            column_refs: Vec::new(),
            flow_edges: Vec::new(),
            current_projections: Vec::new(),
            pending_scope_kind: ScopeKind::Body,
        }
    }

    pub(super) fn column_refs_len(&self) -> usize {
        self.column_refs.len()
    }

    pub(super) fn column_refs_slice(&self, since: usize) -> &[RawColumnRef] {
        &self.column_refs[since..]
    }

    pub(super) fn push_flow_edge(&mut self, edge: FlowEdge) {
        self.flow_edges.push(edge);
    }

    /// Push a fully-built `ProjectionGroup` into the active query's
    /// projection buffer. Called by `visit_select` once per SELECT body.
    pub(super) fn push_projection_group(&mut self, group: ProjectionGroup) {
        self.current_projections.push(group);
    }

    /// Extend the active query's projection buffer with externally
    /// produced groups — used by `SetExpr::Query` to bubble the inner
    /// query's projections up into the enclosing query (so INSERT
    /// pairing reaches through a parenthesized source).
    pub(super) fn extend_projections(&mut self, groups: Vec<ProjectionGroup>) {
        self.current_projections.extend(groups);
    }

    /// Emit `QueryOutput` flow edges for every projection item in
    /// `resolved`. The default disposition for queries whose output is
    /// not bound to a persisted target (top-level SELECT, scalar
    /// subqueries, derived tables, CTE bodies, predicate subqueries).
    pub(super) fn emit_query_output_edges(&mut self, resolved: &ResolvedQuery) {
        for group in &resolved.projections {
            for (position, item) in group.items.iter().enumerate() {
                let target = FlowTargetSpec::QueryOutput {
                    name: item.name.clone(),
                    position,
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

    /// Convenience wrapper: resolve `query` and emit `QueryOutput` edges
    /// for its projections in one shot. Use this from any caller that
    /// doesn't have a special target — INSERT calls the raw
    /// [`resolve_query`] instead so it can pair projections with its
    /// target columns.
    pub(super) fn resolve_query_emitting_query_output(
        &mut self,
        query: &sqlparser::ast::Query,
    ) -> Result<ResolvedQuery, Error> {
        let resolved = self.resolve_query(query)?;
        self.emit_query_output_edges(&resolved);
        Ok(resolved)
    }

    /// Record a raw column reference observed in the current scope.
    /// Called from `visit_expr` for every `Expr::Identifier` and
    /// `Expr::CompoundIdentifier` — resolution and classification are
    /// the consumer's concern.
    pub(super) fn record_column_ref(&mut self, parts: Vec<Ident>) {
        let scope_id = self.scopes.current_scope_id();
        self.column_refs.push(RawColumnRef { parts, scope_id });
    }

    /// Push a fresh scope, run `f`, then pop it. Use around each
    /// branch of a `SetExpr::SetOperation` so the branches' FROM
    /// bindings don't shadow each other and unqualified column refs
    /// in each branch resolve only against its own FROMs — matching
    /// SQL's per-SELECT name resolution.
    pub(crate) fn with_branch_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.scopes.push_query_scope(self.pending_scope_kind);
        let r = f(self);
        self.scopes.pop_scope();
        r
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
            column_refs: self.column_refs,
            flow_edges: self.flow_edges,
        }
    }

    fn is_cte_reference(&self, relation: &ObjectName) -> bool {
        matches!(
            self.scopes.resolve_unqualified_relation(relation),
            Some(RelationBinding::Cte { .. })
        )
    }

    fn bind_base_table(&mut self, table: TableReference, alias: Option<Ident>, role: TableRole) {
        let binding_name = alias.clone().unwrap_or_else(|| table.name.clone());
        let schema = self.lookup_table_schema(&table);
        self.bind_relation(
            binding_name,
            RelationBinding::Table {
                table: Box::new(table),
                alias,
                schema,
                roles: vec![role],
            },
        );
    }

    /// Query the optional catalog for a table's columns. `TableReference`
    /// is already alias-free, so it is a valid catalog key as-is.
    fn lookup_table_schema(&self, table: &TableReference) -> RelationSchema {
        let Some(catalog) = self.catalog else {
            return RelationSchema::Unknown;
        };
        let lookup_key = table.clone();
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
            // TableReference is alias-free by construction now; this
            // catalog just keys by table.name for the test.
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
