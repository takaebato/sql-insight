//! Scope arena, `Binding` enum, and the resolver-side helpers that
//! create and inspect them.

use indexmap::IndexMap;
use sqlparser::ast::{Ident, ObjectName, Statement};
use sqlparser::tokenizer::Span;

use crate::catalog::ColumnSchema;
use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::reference::TableReference;

use super::{ProjectionGroup, Resolution, Resolver};

/// Internal role a table binding carries within a statement. Surfaced
/// to the operation extractor via [`Resolution::read_tables`]
/// and [`Resolution::write_tables`]; the public API exposes
/// two separate lists instead of this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TableRole {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ScopeId(pub(super) usize);

/// Whether a scope contributes data to its enclosing write target.
///
/// - `Body`: data moves through — query bodies, CTE bodies, derived
///   tables, INSERT/MERGE sources, scalar subqueries in projection or
///   SET. Tables bound here participate in `TableLineageEdge` edges when the
///   statement has a write target.
/// - `Predicate`: scope is referenced only in a constraint — WHERE,
///   HAVING, JOIN ON, EXISTS, IN, QUALIFY. Tables bound under any
///   Predicate ancestor are filtered out of `TableLineageEdge` regardless of
///   their own kind, so `INSERT INTO t SELECT FROM s WHERE id IN
///   (SELECT id FROM x)` emits `s → t` but not `x → t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ScopeKind {
    Body,
    Predicate,
}

/// A normalized identifier key for binding lookup.
///
/// Two identifiers match iff their normalized forms are equal. The
/// rule: fold an unquoted name to lowercase, keep a quoted name exact.
/// So `"id"` and unquoted `id` are the same column, while `"ID"` and
/// `id` are not.
///
/// This is one fixed rule, applied uniformly — it is *not* varied by
/// dialect, nor by table-vs-column. Real dialects do diverge there
/// (e.g. MySQL / BigQuery / SQLite treat quoting as mere escaping and
/// keep quoted names case-insensitive; BigQuery columns are
/// case-insensitive but its tables are case-sensitive; ClickHouse is
/// fully case-sensitive). Modelling each faithfully would need a
/// per-dialect identifier-resolution strategy, which is deferred — the
/// fixed rule here is a deliberate common-denominator approximation:
///
/// - **Unquoted → lowercase** makes unquoted matching case-insensitive,
///   which every supported dialect except ClickHouse does. (ClickHouse
///   is over-matched — sound, just imprecise.) The fold *direction*
///   only affects the quoted/unquoted edge; lowercase follows the
///   popular majority (PG / MySQL / SQLite / BigQuery / Redshift / Spark)
///   over the uppercase minority (ANSI / Oracle / Snowflake).
/// - **Quoted → exact** follows the ANSI / PostgreSQL family, where
///   quoting makes an identifier case-sensitive. The MySQL / BigQuery /
///   SQLite family instead treat quoting as escaping, so this is
///   stricter than they are for quoted names — accepted, since quoted
///   identifiers are rare in practice.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct BindingKey(String);

impl BindingKey {
    pub(super) fn from_ident(ident: &Ident) -> Self {
        Self(if ident.quote_style.is_some() {
            ident.value.clone()
        } else {
            ident.value.to_ascii_lowercase()
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RelationSchema {
    /// Column names of a relation with a known schema (from the
    /// catalog). Just the names — the resolver needs identity, not
    /// types.
    Known(Vec<Ident>),
    Unknown,
}

/// What's bound to a name in a [`Scope`] — a real Table or
/// one of the synthetic intermediates (CTE / derived subquery / table
/// function) that SQL exposes as a named row set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Binding {
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
        /// The CTE body's projection groups, captured so that lineage
        /// composition can substitute references to `cte.col` with the
        /// body's source refs (transitive source → target lineage).
        /// Empty for recursive CTEs where the body is walked under a
        /// pre-bound stub and fixpoint-aware projection capture is
        /// deferred.
        body_projections: Vec<ProjectionGroup>,
    },
    DerivedTable {
        alias: Ident,
        schema: RelationSchema,
        /// Same role as `Cte::body_projections` — captured at the
        /// derived subquery walk and consumed by lineage composition.
        body_projections: Vec<ProjectionGroup>,
    },
    TableFunction {
        alias: Ident,
        schema: RelationSchema,
    },
}

#[derive(Debug)]
pub(crate) struct Scope {
    pub(crate) id: ScopeId,
    pub(crate) parent: Option<ScopeId>,
    pub(crate) kind: ScopeKind,
    pub(super) bindings: IndexMap<BindingKey, Binding>,
}

impl Scope {
    fn new(id: ScopeId, parent: Option<ScopeId>, kind: ScopeKind) -> Self {
        Self {
            id,
            parent,
            kind,
            bindings: IndexMap::new(),
        }
    }

    fn bind(&mut self, name: &Ident, binding: Binding) {
        let key = BindingKey::from_ident(name);
        // Re-binding the same name as a Table merges roles rather than
        // replacing — this captures the `DELETE t1 FROM t1` style case
        // where a single name plays multiple roles in one statement.
        if let (
            Some(Binding::Table {
                roles: existing, ..
            }),
            Binding::Table { roles: new, .. },
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

    fn resolve(&self, name: &Ident) -> Option<&Binding> {
        self.bindings.get(&BindingKey::from_ident(name))
    }

    pub(super) fn iter_bindings(&self) -> impl Iterator<Item = &Binding> {
        self.bindings.values()
    }
}

#[derive(Default, Debug)]
pub(super) struct ScopeStack {
    pub(super) scopes: Vec<Scope>,
    stack: Vec<ScopeId>,
}

impl ScopeStack {
    pub(super) fn scope(&self, id: ScopeId) -> &Scope {
        &self.scopes[id.0]
    }

    pub(super) fn into_scopes(self) -> Vec<Scope> {
        self.scopes
    }

    pub(super) fn push_query_scope(&mut self, kind: ScopeKind) -> ScopeId {
        let parent = self.stack.last().copied();
        self.push_scope(parent, kind)
    }

    pub(super) fn pop_scope(&mut self) {
        self.stack.pop();
    }

    pub(super) fn bind_current(&mut self, name: Ident, binding: Binding) {
        self.current_scope_mut().bind(&name, binding);
    }

    pub(super) fn resolve_unqualified_relation(&self, relation: &ObjectName) -> Option<&Binding> {
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
        self.scopes.push(Scope::new(id, parent, kind));
        self.stack.push(id);
        id
    }

    pub(super) fn current_scope_id(&mut self) -> ScopeId {
        if let Some(id) = self.stack.last() {
            *id
        } else {
            self.push_scope(None, ScopeKind::Body)
        }
    }

    fn current_scope_mut(&mut self) -> &mut Scope {
        let id = self.current_scope_id();
        &mut self.scopes[id.0]
    }
}

pub(super) fn is_synthetic_binding(binding: &Binding) -> bool {
    matches!(
        binding,
        Binding::Cte { .. } | Binding::DerivedTable { .. } | Binding::TableFunction { .. }
    )
}

pub(super) fn binding_alias_key(binding: &Binding) -> BindingKey {
    match binding {
        Binding::Table { table, alias, .. } => {
            BindingKey::from_ident(alias.as_ref().unwrap_or(&table.name))
        }
        Binding::Cte { name, .. } => BindingKey::from_ident(name),
        Binding::DerivedTable { alias, .. } | Binding::TableFunction { alias, .. } => {
            BindingKey::from_ident(alias)
        }
    }
}

pub(super) fn binding_could_contain_column(
    binding: &Binding,
    name: &Ident,
) -> Option<TableReference> {
    match binding {
        Binding::Table { table, schema, .. } => {
            schema_could_contain(schema, name).then(|| (**table).clone())
        }
        Binding::Cte {
            name: cte_name,
            schema,
            ..
        } => schema_could_contain(schema, name).then(|| synthetic_table_ref(cte_name)),
        Binding::DerivedTable { alias, schema, .. } => {
            schema_could_contain(schema, name).then(|| synthetic_table_ref(alias))
        }
        // TableFunction schemas are always Unknown for now, so any
        // unqualified column could plausibly come from one.
        Binding::TableFunction { alias, .. } => Some(synthetic_table_ref(alias)),
    }
}

/// Schema-confirmed membership: `true` iff the binding has a `Known`
/// schema that declares the column. Distinguished from
/// `binding_could_contain_column`, which also returns `Some` for
/// `Unknown` schemas. Used by diagnostic emit to separate "definitely
/// ambiguous" from "uncertain over Unknown schemas".
pub(super) fn binding_confirms_column(binding: &Binding, name: &Ident) -> bool {
    matches!(
        binding_schema(binding),
        RelationSchema::Known(cols)
            if cols.iter().any(|c| BindingKey::from_ident(c) == BindingKey::from_ident(name))
    )
}

/// `true` iff the binding's schema is `Known` (not `Unknown`). Used to
/// gate `UnresolvedColumn` diagnostics — without at least one Known
/// schema in scope, the resolver can't claim a column is missing.
pub(super) fn binding_has_known_schema(binding: &Binding) -> bool {
    matches!(binding_schema(binding), RelationSchema::Known(_))
}

fn binding_schema(binding: &Binding) -> &RelationSchema {
    match binding {
        Binding::Table { schema, .. }
        | Binding::Cte { schema, .. }
        | Binding::DerivedTable { schema, .. }
        | Binding::TableFunction { schema, .. } => schema,
    }
}

fn schema_could_contain(schema: &RelationSchema, name: &Ident) -> bool {
    match schema {
        RelationSchema::Unknown => true,
        RelationSchema::Known(cols) => cols
            .iter()
            .any(|c| BindingKey::from_ident(c) == BindingKey::from_ident(name)),
    }
}

pub(super) fn synthetic_table_ref(name: &Ident) -> TableReference {
    TableReference {
        catalog: None,
        schema: None,
        name: name.clone(),
    }
}

/// Convert a raw sqlparser `Span` to the `Option<Span>` shape stored on
/// `ColumnLevelDiagnostic`: an empty span (sqlparser convention: `line == 0`) is
/// flattened to `None` so consumers can distinguish "no source location"
/// from "location at (0, 0)".
pub(super) fn normalize_span(span: Span) -> Option<Span> {
    (span.start.line != 0).then_some(span)
}

/// Format an `Option<Span>` as ` at L<line>:C<col>` for inclusion in
/// diagnostic messages, or an empty string when no location is known.
pub(super) fn span_suffix(span: Option<Span>) -> String {
    match span {
        Some(s) => format!(" at L{}:C{}", s.start.line, s.start.column),
        None => String::new(),
    }
}

// ───────── Resolver binding-related methods ─────────

impl<'a> Resolver<'a> {
    pub(super) fn scopes(&self) -> &ScopeStack {
        &self.scopes
    }

    pub(super) fn scopes_mut(&mut self) -> &mut ScopeStack {
        &mut self.scopes
    }

    pub(super) fn is_cte_reference(&self, relation: &ObjectName) -> bool {
        matches!(
            self.scopes.resolve_unqualified_relation(relation),
            Some(Binding::Cte { .. })
        )
    }

    pub(super) fn bind_base_table(
        &mut self,
        table: TableReference,
        alias: Option<Ident>,
        role: TableRole,
    ) {
        let binding_name = alias.clone().unwrap_or_else(|| table.name.clone());
        let schema = self.lookup_table_schema(&table);
        self.bind_relation(
            binding_name,
            Binding::Table {
                table: Box::new(table),
                alias,
                schema,
                roles: vec![role],
            },
        );
    }

    /// Query the optional catalog for a table's columns.
    /// `TableReference` is already alias-free, so it is a valid
    /// catalog key as-is.
    fn lookup_table_schema(&self, table: &TableReference) -> RelationSchema {
        let Some(catalog) = self.catalog else {
            return RelationSchema::Unknown;
        };
        let lookup_key = table.clone();
        match catalog.columns(&lookup_key) {
            Some(cols) => {
                RelationSchema::Known(cols.into_iter().map(|ColumnSchema { name }| name).collect())
            }
            None => RelationSchema::Unknown,
        }
    }

    /// Resolve the effective target column list for INSERT-style
    /// positional pairing: explicit list wins when non-empty,
    /// otherwise the catalog-provided schema if known. Returns an
    /// empty `Vec` when neither path yields names — the caller then
    /// emits no Relation edges (matches the no-catalog
    /// column-list-less INSERT behavior).
    pub(super) fn effective_target_columns(
        &self,
        explicit: &[Ident],
        target: &TableReference,
    ) -> Vec<Ident> {
        if !explicit.is_empty() {
            return explicit.to_vec();
        }
        match self.lookup_table_schema(target) {
            RelationSchema::Known(cols) => cols,
            RelationSchema::Unknown => Vec::new(),
        }
    }

    /// Look up an in-scope CTE's body projections, for re-binding
    /// under an alias (`FROM cte AS c`). Returns an empty `Vec` when
    /// the reference is multi-segment, not bound, or not a Cte
    /// binding — the caller (alias-bound Cte construction) treats
    /// that as "no composition through this alias", matching
    /// recursive-CTE behavior.
    pub(super) fn cte_body_projections(&self, cte_name: &ObjectName) -> Vec<ProjectionGroup> {
        match self.scopes.resolve_unqualified_relation(cte_name) {
            Some(Binding::Cte {
                body_projections, ..
            }) => body_projections.clone(),
            _ => Vec::new(),
        }
    }

    /// Look up an in-scope CTE's schema (companion to
    /// [`Self::cte_body_projections`]). Returns `RelationSchema::Unknown`
    /// when the lookup misses — same fallthrough semantics as the
    /// body-projections accessor.
    pub(super) fn cte_schema(&self, cte_name: &ObjectName) -> RelationSchema {
        match self.scopes.resolve_unqualified_relation(cte_name) {
            Some(Binding::Cte { schema, .. }) => schema.clone(),
            _ => RelationSchema::Unknown,
        }
    }

    pub(super) fn bind_cte(
        &mut self,
        name: Ident,
        schema: RelationSchema,
        body_projections: Vec<ProjectionGroup>,
    ) {
        self.bind_relation(
            name.clone(),
            Binding::Cte {
                name,
                schema,
                body_projections,
            },
        );
    }

    pub(super) fn bind_derived_table(
        &mut self,
        alias: Ident,
        schema: RelationSchema,
        body_projections: Vec<ProjectionGroup>,
    ) {
        self.bind_relation(
            alias.clone(),
            Binding::DerivedTable {
                alias,
                schema,
                body_projections,
            },
        );
    }

    pub(super) fn bind_table_function(&mut self, alias: Ident) {
        self.bind_relation(
            alias.clone(),
            Binding::TableFunction {
                alias,
                schema: RelationSchema::Unknown,
            },
        );
    }

    pub(super) fn record_diagnostic(&mut self, diagnostic: ColumnLevelDiagnostic) {
        self.diagnostics.push(diagnostic);
    }

    pub(super) fn record_unsupported_statement(&mut self, statement: &Statement) {
        self.record_diagnostic(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::UnsupportedStatement,
            message: format!("Unsupported statement while inspecting SQL: {}", statement),
            span: None,
        });
    }

    pub(super) fn record_wildcard_suppressed(&mut self, description: &str, span: Span) {
        let span = normalize_span(span);
        self.record_diagnostic(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::WildcardSuppressed,
            message: format!(
                "{}{} left unexpanded — column lineage will be incomplete for this projection",
                description,
                span_suffix(span),
            ),
            span,
        });
    }

    fn bind_relation(&mut self, name: Ident, binding: Binding) {
        self.scopes.bind_current(name, binding);
    }
}

// ───────── Resolution binding-related queries ─────────

impl Resolution {
    /// All tables touched by the statement, in scope-arena order. The
    /// union of [`Self::read_tables`] and [`Self::write_tables`] (with
    /// duplicates when a single table carries both roles).
    pub(crate) fn tables(&self) -> Vec<TableReference> {
        self.scopes
            .iter()
            .flat_map(|scope| scope.iter_bindings())
            .filter_map(|binding| match binding {
                Binding::Table { table, .. } => Some((**table).clone()),
                _ => None,
            })
            .collect()
    }

    /// Every table referenced as a Read source, in scope-arena order.
    /// Includes tables inside predicate subqueries (e.g. `x` in
    /// `WHERE id IN (SELECT id FROM x)`). Use
    /// [`Self::feeding_read_tables`] for the stricter "feeds the
    /// enclosing write target" filter.
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
                Binding::Table { table, roles, .. } if roles.contains(&role) => {
                    Some((**table).clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Read-role tables in a data-feeding position — Read role plus no
    /// `Predicate` ancestor in their scope chain. The basis for
    /// `TableLineageEdge` edge sources.
    pub(crate) fn feeding_read_tables(&self) -> Vec<TableReference> {
        self.scopes
            .iter()
            .filter(|scope| !self.has_predicate_ancestor(scope.id))
            .flat_map(|scope| scope.iter_bindings())
            .filter_map(|binding| match binding {
                Binding::Table { table, roles, .. } if roles.contains(&TableRole::Read) => {
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
