//! Scope arena: `ScopeId`, `ScopeKind`, `BindingKey`, `Scope`, and
//! `ScopeStack`. Owns the "container" side of name resolution; the
//! `Binding` "contents" live in [`super::binding`].

use indexmap::IndexMap;
use sqlparser::ast::{Ident, ObjectName};

use super::binding::BindingKey;
use super::{Binding, Resolver};

/// Arena index for a [`Scope`]. Stable across later pushes since the
/// arena only grows during a resolver run, so a `ScopeId` captured
/// during the walk still resolves correctly in post-passes.
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

impl<'a> Resolver<'a> {
    pub(super) fn scopes(&self) -> &ScopeStack {
        &self.scopes
    }

    pub(super) fn scopes_mut(&mut self) -> &mut ScopeStack {
        &mut self.scopes
    }

    /// Push a fresh scope, run `f`, then pop it. The current
    /// `scope_kind` is propagated onto the pushed scope, so a subquery
    /// in a predicate stays classified as predicate-position for
    /// table-lineage exclusion.
    ///
    /// Use at every "new query boundary":
    /// - the top of `resolve_query` (the query body's own scope),
    /// - each branch of a `SetExpr::SetOperation` so the branches'
    ///   FROM bindings don't shadow each other and unqualified column
    ///   refs in each branch resolve only against its own FROMs —
    ///   matching SQL's per-SELECT name resolution,
    /// - the DML statement inside `WITH … <DML>` so its target
    ///   binding doesn't share the enclosing query's scope with the
    ///   CTEs (CTEs stay reachable via the parent-scope walk-up).
    ///
    /// Popping happens on the closure's return, including the `Err`
    /// path of a `Result`-returning closure, so this is the safe way
    /// to nest a `?`-bailing walk under a scope push.
    pub(crate) fn with_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let kind = self.scope_kind;
        self.scopes_mut().push_scope(kind);
        let r = f(self);
        self.scopes_mut().pop_scope();
        r
    }

    /// Walk a filter-position clause with `scope_kind = Predicate`, so
    /// any subquery pushed inside is classified as a predicate scope
    /// and thus excluded from table-lineage. Used for WHERE, HAVING,
    /// QUALIFY, JOIN ON, AsOf match, MERGE ON, CONNECT BY, pipe
    /// `|> WHERE`, etc. The previous `scope_kind` is restored on return.
    pub(crate) fn with_filter_clause<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = self.scope_kind;
        self.scope_kind = ScopeKind::Predicate;
        let r = f(self);
        self.scope_kind = prev;
        r
    }
}
