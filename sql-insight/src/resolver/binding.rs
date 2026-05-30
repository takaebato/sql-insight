//! Scope arena, `Binding` enum, and the resolver-side helpers that
//! create and inspect them.

use indexmap::IndexMap;
use sqlparser::ast::{Ident, ObjectName, Statement};
use sqlparser::tokenizer::Span;

use crate::catalog::ColumnSchema;
use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::reference::TableReference;

use super::{BodyOutput, Resolution, Resolver};

/// Internal role a table binding carries within a statement. Surfaced
/// to the operation extractor via [`Resolution::read_tables`]
/// and [`Resolution::write_tables`]; the public API exposes
/// two separate lists instead of this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TableRole {
    Read,
    Write,
}

/// Arena index for a [`Scope`]. Stable across later pushes since the
/// arena only grows during a resolver run, so a `ScopeId` captured
/// during the walk still resolves correctly in post-passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ScopeId(pub(super) usize);

/// A single `FROM`-position use of a table-like source captured at walk
/// time. Table-lineage collapse iterates these (instead of walking
/// scope bindings), so an unreferenced CTE — whose declaration binds
/// names but whose body is never `FROM`-used — contributes no lineage
/// sources.
#[derive(Clone, Debug)]
pub(crate) struct RawTableRef {
    /// Scope where the use occurs — used for predicate-ancestor
    /// filtering at collapse time.
    pub(crate) scope_id: ScopeId,
    /// What's being used: a real table (emits as a lineage source) or
    /// a synthetic relation (recurses into its body to find real
    /// tables underneath).
    pub(crate) target: TableRefTarget,
}

/// Resolution of a [`RawTableRef`] target.
///
/// **Terminology note**: "Synthetic" is this codebase's chosen
/// umbrella term for `{Binding::Cte, Binding::DerivedTable,
/// Binding::TableFunction}` — relations defined inside the SQL
/// statement (CTE bodies, derived subqueries, table functions)
/// rather than stored in a catalog. **This is our own
/// classification, not borrowed from SQL spec or vendor docs**:
///
/// - ANSI SQL has no umbrella term covering all three; the spec
///   treats "derived table" (narrower, our `DerivedTable` only),
///   CTE, and table function as separate constructs.
/// - Oracle's "inline view" is similarly narrower — FROM-clause
///   subqueries only.
/// - The compiler-flavored sense of "synthetic" ("produced by the
///   processor, not in source") doesn't fit either: the SQL author
///   wrote these definitions explicitly.
///
/// Despite the inexact fit, "synthetic" is chosen for being short,
/// distinct, free of dialect collision, and consistent with the
/// existing [`RawColumnRef::synthetic`](crate::resolver::RawColumnRef)
/// field and [`is_synthetic_binding`] helper.
///
/// Variants represent **what to do during table-lineage collapse**,
/// not raw storage classification. `Binding::TableFunction` is
/// synthetic at the binding level but is omitted here (and from
/// `RawTableRef` emission entirely), since it has no inspectable
/// body to recurse into.
#[derive(Clone, Debug)]
pub(crate) enum TableRefTarget {
    /// A real table — `collapsed_feeding_table_sources` emits this
    /// `TableReference` directly. Terminal.
    Real(TableReference),
    /// A CTE or derived subquery whose body lives at `body_scope`.
    /// Collapse recurses into that scope's subtree, collecting the
    /// real tables underneath. Covers `Binding::Cte` and
    /// `Binding::DerivedTable` (with non-`None` body_scope).
    Synthetic { body_scope: ScopeId },
}

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

/// What's bound to a name in a [`Scope`] — a real Table or one of
/// the synthetic relations (CTE / derived subquery / table function)
/// that SQL exposes as a named row set.
///
/// Column-name info lives in the `output_columns` field, shaped per
/// binding kind:
/// - `Table::output_columns: Option<Vec<Ident>>` — naked column-name
///   list from the catalog (`Some`) or unknown (`None`).
/// - `Cte` / `DerivedTable` `output_columns: Option<BodyOutput>` —
///   per-branch list of [`OutputColumn`]s with full lineage info, or
///   `None` for recursive CTE stubs / wrapper aliases / walk-failed
///   bodies.
/// - `TableFunction` carries no column info — always unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Binding {
    // `table` is boxed because the variant otherwise dwarfs the others
    // (TableReference is ~300B) and inflates the entire enum's size.
    Table {
        /// The real underlying table. Alias-free so catalog lookup
        /// and cross-statement comparison behave intuitively (alias
        /// lives next door in `alias`).
        table: Box<TableReference>,
        /// Alias given at this use-site, if any. Kept separately so
        /// `TableReference` stays alias-free for catalog lookup and
        /// cross-statement comparison.
        alias: Option<Ident>,
        /// Catalog-derived column names. `Some` when the catalog has
        /// the table, `None` otherwise (no catalog or catalog miss).
        output_columns: Option<Vec<Ident>>,
        /// How this binding is used in the statement — Read, Write,
        /// or both (e.g. `DELETE t1 FROM t1`). Re-binding the same
        /// name merges roles rather than overwriting (see `Scope::bind`).
        roles: Vec<TableRole>,
    },
    Cte {
        /// The CTE's declared name (the `<name>` in `WITH <name> AS …`).
        /// Lookup keys derive from this via `BindingKey`.
        name: Ident,
        /// Body-walk output of the CTE: per-branch list of
        /// [`OutputColumn`]s with full lineage info (name, source
        /// refs, kind). `None` for recursive CTEs pre-bound under a
        /// stub (fixpoint-aware capture is deferred). Renamed via the
        /// CTE's column-alias list when one is given.
        output_columns: Option<BodyOutput>,
        /// Arena id of the scope that holds the CTE body's bindings.
        /// Table-lineage collapse walks descendant scopes of this id
        /// to collect the real tables underneath the CTE — so a
        /// `FROM cte` use can resolve back to the body's `FROM s`.
        body_scope: ScopeId,
    },
    DerivedTable {
        /// Mandatory alias from `(SELECT …) AS d`. Unlike `Table::alias`,
        /// this is the only handle the outer query has on the derived
        /// relation.
        alias: Ident,
        /// Body-walk output. Same shape as `Cte::output_columns`,
        /// renamed via the alias's column list when one is given.
        /// `None` for wrapper aliases (`NestedJoin`, `Pivot`,
        /// `Unpivot`, `MatchRecognize`) whose body isn't a real
        /// subquery with its own projection.
        output_columns: Option<BodyOutput>,
        /// Arena id of the scope holding the derived subquery body's
        /// bindings (`Some`) — or `None` for wrapper aliases whose
        /// inner tables are bound directly in the current scope and
        /// don't need collapse through this synthetic.
        body_scope: Option<ScopeId>,
    },
    TableFunction {
        /// Mandatory alias from `f(...) AS t`. Refs against the alias
        /// surface as synthetic-owned (filtered out of public reads).
        /// No column-info field — TableFunction column inference is
        /// not modelled (always unknown).
        alias: Ident,
    },
}

/// One lexical scope: a `name → Binding` map plus the links
/// (`parent`, `kind`) used to walk up the scope chain at
/// name-resolution and lineage-emission time. Self-id is implicit —
/// the scope's id equals its index in [`ScopeStack::scopes`].
#[derive(Debug)]
pub(crate) struct Scope {
    /// Lexically enclosing scope, or `None` for the root. Drives the
    /// walk-up for unqualified name resolution.
    pub(crate) parent: Option<ScopeId>,
    /// `Body` vs `Predicate`. A `Predicate` anywhere along the
    /// ancestor chain excludes nested scopes from `TableLineageEdge`
    /// even if they themselves are `Body`.
    pub(crate) kind: ScopeKind,
    /// Bindings introduced *in this scope* (FROM tables, CTE
    /// definitions, derived tables, table functions). Keyed by
    /// `BindingKey` (case-folded); `IndexMap` preserves definition
    /// order for deterministic iteration.
    pub(super) bindings: IndexMap<BindingKey, Binding>,
}

impl Scope {
    fn new(parent: Option<ScopeId>, kind: ScopeKind) -> Self {
        Self {
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

/// Arena + active-stack model of the scope tree. `scopes` retains
/// every scope by id so post-passes can still look them up after they
/// have been popped from the active stack; `stack` tracks what is
/// currently "open" as the walker descends.
#[derive(Default, Debug)]
pub(super) struct ScopeStack {
    /// All scopes ever opened during the walk. Kept after `pop_scope`
    /// so later passes (lineage collapse, column-ref resolution)
    /// can address scopes by `ScopeId`. Index in this Vec equals
    /// `ScopeId.0`.
    pub(super) scopes: Vec<Scope>,
    /// Currently-open scope ids, innermost at the top. Drives parent
    /// derivation in `push_scope` and the walk-up in
    /// `resolve_unqualified_relation`.
    stack: Vec<ScopeId>,
}

impl ScopeStack {
    pub(super) fn scope(&self, id: ScopeId) -> &Scope {
        &self.scopes[id.0]
    }

    pub(super) fn into_scopes(self) -> Vec<Scope> {
        self.scopes
    }

    /// Push a fresh scope as a child of the current stack top, with
    /// the given `kind`. Parent is derived from the stack — this is
    /// the normal "open a nested scope" operation.
    pub(super) fn push_scope(&mut self, kind: ScopeKind) -> ScopeId {
        let parent = self.stack.last().copied();
        self.insert_scope(parent, kind)
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

    /// Low-level: allocate a `ScopeId`, append to the `scopes` arena, and
    /// push onto the active `stack`, with an arbitrary `parent` (including
    /// `None` for a root scope). Maintains the invariant that a newly
    /// inserted scope's `ScopeId.0` equals its index in `scopes`.
    fn insert_scope(&mut self, parent: Option<ScopeId>, kind: ScopeKind) -> ScopeId {
        let id = ScopeId(self.scopes.len());
        self.scopes.push(Scope::new(parent, kind));
        self.stack.push(id);
        id
    }

    pub(super) fn current_scope_id(&mut self) -> ScopeId {
        if let Some(id) = self.stack.last() {
            *id
        } else {
            self.insert_scope(None, ScopeKind::Body)
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
        Binding::Table { table, .. } => {
            names_could_contain(binding_column_names(binding).as_deref(), name)
                .then(|| (**table).clone())
        }
        Binding::Cte { name: cte_name, .. } => {
            names_could_contain(binding_column_names(binding).as_deref(), name)
                .then(|| synthetic_table_ref(cte_name))
        }
        Binding::DerivedTable { alias, .. } => {
            names_could_contain(binding_column_names(binding).as_deref(), name)
                .then(|| synthetic_table_ref(alias))
        }
        // TableFunction columns are unmodeled, so any unqualified
        // column could plausibly come from one.
        Binding::TableFunction { alias } => Some(synthetic_table_ref(alias)),
    }
}

/// Known-membership: `true` iff the binding has a known column list
/// that declares the column. Distinguished from
/// `binding_could_contain_column`, which also returns `Some` for
/// bindings with unknown column lists. Used by diagnostic emit to
/// separate "definitely ambiguous" from "uncertain over unknown
/// columns".
pub(super) fn binding_confirms_column(binding: &Binding, name: &Ident) -> bool {
    match binding_column_names(binding) {
        Some(cols) => cols
            .iter()
            .any(|c| BindingKey::from_ident(c) == BindingKey::from_ident(name)),
        None => false,
    }
}

/// `true` iff the binding has a known column list. Used to gate
/// `UnresolvedColumn` diagnostics — without at least one binding
/// with known columns in scope, the resolver can't claim a column is
/// missing.
pub(super) fn binding_has_known_columns(binding: &Binding) -> bool {
    binding_column_names(binding).is_some()
}

/// Cross-binding column-name lookup: the single entry point that
/// abstracts over `Binding::Table`'s catalog list and
/// `Cte`/`DerivedTable`'s body-derived column names. `TableFunction`
/// always returns `None`.
pub(super) fn binding_column_names(binding: &Binding) -> Option<Vec<Ident>> {
    match binding {
        Binding::Table { output_columns, .. } => output_columns.clone(),
        Binding::Cte { output_columns, .. } | Binding::DerivedTable { output_columns, .. } => {
            output_columns.as_ref()?.column_names()
        }
        Binding::TableFunction { .. } => None,
    }
}

fn names_could_contain(names: Option<&[Ident]>, name: &Ident) -> bool {
    match names {
        None => true, // unknown columns — anything could match
        Some(cols) => cols
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

    pub(super) fn bind_real_table(
        &mut self,
        table: TableReference,
        alias: Option<Ident>,
        role: TableRole,
    ) {
        let binding_name = alias.clone().unwrap_or_else(|| table.name.clone());
        let output_columns = self.lookup_catalog_columns(&table);
        if role == TableRole::Read {
            // Read-position FROM/JOIN — emit a RawTableRef so table-lineage
            // collapse sees this as a real source. Write targets feed
            // `write_tables` separately and don't drive collapse.
            self.record_real_table_ref(table.clone());
        }
        self.bind_relation(
            binding_name,
            Binding::Table {
                table: Box::new(table),
                alias,
                output_columns,
                roles: vec![role],
            },
        );
    }

    /// Record a use of a real table at the current scope. Called by
    /// [`Self::bind_real_table`] on Read-position binds.
    pub(super) fn record_real_table_ref(&mut self, table: TableReference) {
        let scope_id = self.scopes.current_scope_id();
        self.table_refs.push(RawTableRef {
            scope_id,
            target: TableRefTarget::Real(table),
        });
    }

    /// Record a use of a synthetic relation (CTE / true derived) at
    /// the current scope. `body_scope` is the arena id of the
    /// synthetic's body — collapse recurses into its subtree.
    pub(super) fn record_synthetic_table_ref(&mut self, body_scope: ScopeId) {
        let scope_id = self.scopes.current_scope_id();
        self.table_refs.push(RawTableRef {
            scope_id,
            target: TableRefTarget::Synthetic { body_scope },
        });
    }

    /// Query the optional catalog for a table's columns.
    /// `TableReference` is already alias-free, so it is a valid
    /// catalog key as-is. Returns `None` when no catalog is supplied
    /// or the catalog has no entry for the table.
    fn lookup_catalog_columns(&self, table: &TableReference) -> Option<Vec<Ident>> {
        let catalog = self.catalog?;
        let cols = catalog.columns(table)?;
        Some(
            cols.into_iter()
                .map(|ColumnSchema { name }| Ident::new(name))
                .collect(),
        )
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
        self.lookup_catalog_columns(target).unwrap_or_default()
    }

    /// Look up an in-scope CTE's `output_columns`, for re-binding
    /// under an alias (`FROM cte AS c`). Returns `None` when the
    /// reference is multi-segment, not bound, or not a Cte binding —
    /// the caller (alias-bound Cte construction) treats that as "no
    /// collapse through this alias", matching recursive-CTE behavior.
    pub(super) fn cte_output_columns(&self, cte_name: &ObjectName) -> Option<BodyOutput> {
        match self.scopes.resolve_unqualified_relation(cte_name) {
            Some(Binding::Cte { output_columns, .. }) => output_columns.clone(),
            _ => None,
        }
    }

    pub(super) fn bind_cte(
        &mut self,
        name: Ident,
        output_columns: Option<BodyOutput>,
        body_scope: ScopeId,
    ) {
        self.bind_relation(
            name.clone(),
            Binding::Cte {
                name,
                output_columns,
                body_scope,
            },
        );
    }

    pub(super) fn bind_derived_table(
        &mut self,
        alias: Ident,
        output_columns: Option<BodyOutput>,
        body_scope: Option<ScopeId>,
    ) {
        self.bind_relation(
            alias.clone(),
            Binding::DerivedTable {
                alias,
                output_columns,
                body_scope,
            },
        );
    }

    /// Look up the body scope for a CTE name. Returns `None` if the
    /// name does not resolve to a `Cte` binding — same fall-through
    /// semantics as [`Self::cte_output_columns`].
    pub(super) fn cte_body_scope(&self, cte_name: &ObjectName) -> Option<ScopeId> {
        match self.scopes.resolve_unqualified_relation(cte_name) {
            Some(Binding::Cte { body_scope, .. }) => Some(*body_scope),
            _ => None,
        }
    }

    pub(super) fn bind_table_function(&mut self, alias: Ident) {
        self.bind_relation(alias.clone(), Binding::TableFunction { alias });
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
    /// [`Self::collapsed_feeding_table_sources`] for the stricter
    /// "feeds the enclosing write target" filter.
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

    /// Walk parent chain from `scope_id`; return true iff any scope along
    /// the way carries `ScopeKind::Predicate`. Drives the
    /// filter-position exclusion in
    /// [`Self::collapsed_feeding_table_sources`].
    pub(super) fn has_predicate_ancestor(&self, scope_id: ScopeId) -> bool {
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
