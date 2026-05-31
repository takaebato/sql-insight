//! Scope arena types — `ScopeId`, `Scope` — plus the arena-management
//! methods on [`Resolver`] (`push_scope` / `pop_scope` / `bind_current`
//! / `resolve_unqualified_relation` / etc.) and the lexical `with_*`
//! helpers. Owns the "container" side of name resolution; the
//! `Binding` "contents" live in [`super::binding`].

use indexmap::IndexMap;
use sqlparser::ast::{Ident, ObjectName};

use super::binding::BindingKey;
use super::{Binding, Resolver};

/// Arena index for a [`Scope`]. Stable across later pushes since the
/// arena only grows during a resolver run, so a `ScopeId` captured
/// during the walk still resolves correctly in post-passes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ScopeId(pub(crate) usize);

/// One lexical scope: a `name → Binding` map plus the parent link
/// used to walk up the scope chain at name-resolution time. Self-id
/// is implicit — the scope's id equals its index in
/// [`super::Resolution::scopes`].
///
/// Predicate-vs-value position is tracked **on the captured ref**
/// ([`super::CapturedColumnRef::in_predicate`] /
/// [`super::CapturedTableRef::in_predicate`]) rather than on the
/// scope. This lets same-scope refs in different lexical positions
/// (e.g. CASE WHEN cond vs THEN value) be classified independently.
#[derive(Debug)]
pub(crate) struct Scope {
    /// Lexically enclosing scope, or `None` for the root. Drives the
    /// walk-up for unqualified name resolution.
    pub(crate) parent: Option<ScopeId>,
    /// Bindings introduced *in this scope* (FROM tables, CTE
    /// definitions, derived tables, table functions). Keyed by
    /// `BindingKey` (case-folded); `IndexMap` preserves definition
    /// order for deterministic iteration.
    pub(super) bindings: IndexMap<BindingKey, Binding>,
}

/// Iterate the scope ids on the chain `from → parent → parent → … → root`,
/// inclusive of `from`. Underlies every "walk up the parent links"
/// loop in the resolver / resolution side. `scopes` is the arena to
/// index into; both `Resolver` and `Resolution` hold their own.
pub(super) fn parent_chain(scopes: &[Scope], from: ScopeId) -> impl Iterator<Item = ScopeId> + '_ {
    std::iter::successors(Some(from), move |id| scopes[id.0].parent)
}

impl Scope {
    fn new(parent: Option<ScopeId>) -> Self {
        Self {
            parent,
            bindings: IndexMap::new(),
        }
    }

    pub(super) fn bind(&mut self, name: &Ident, binding: Binding) {
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

    pub(super) fn resolve(&self, name: &Ident) -> Option<&Binding> {
        self.bindings.get(&BindingKey::from_ident(name))
    }

    pub(super) fn iter_bindings(&self) -> impl Iterator<Item = &Binding> {
        self.bindings.values()
    }
}

impl<'a> Resolver<'a> {
    /// Push a fresh scope as a child of `self.context.current_scope`.
    /// Returns the new scope's id and makes it current.
    pub(super) fn push_scope(&mut self) -> ScopeId {
        let id = ScopeId(self.resolution.scopes.len());
        self.resolution
            .scopes
            .push(Scope::new(self.context.current_scope));
        self.context.current_scope = Some(id);
        id
    }

    /// Close the current scope by walking back to its parent. The
    /// popped scope stays in the arena for post-pass lookups.
    pub(super) fn pop_scope(&mut self) {
        self.context.current_scope = self
            .context
            .current_scope
            .and_then(|id| self.resolution.scopes[id.0].parent);
    }

    /// Id of the currently-open scope. Lazily inserts a root scope
    /// on first call so the very first bind has somewhere to land.
    pub(super) fn current_scope_id(&mut self) -> ScopeId {
        match self.context.current_scope {
            Some(id) => id,
            None => self.push_scope(),
        }
    }

    /// Insert a binding into the current scope, creating a root
    /// scope on demand if nothing is open yet.
    pub(super) fn bind_current(&mut self, name: Ident, binding: Binding) {
        let id = self.current_scope_id();
        self.resolution.scopes[id.0].bind(&name, binding);
    }

    /// Resolve an unqualified single-segment relation name by walking
    /// up the parent chain from `context.current_scope`, returning
    /// the first matching binding. Multi-segment qualified names
    /// return `None` — those route through schema/catalog resolution
    /// elsewhere.
    pub(super) fn resolve_unqualified_relation(&self, relation: &ObjectName) -> Option<&Binding> {
        if relation.0.len() != 1 {
            return None;
        }
        let name = relation.0[0].as_ident()?;
        let from = self.context.current_scope?;
        parent_chain(&self.resolution.scopes, from)
            .find_map(|id| self.resolution.scopes[id.0].resolve(name))
    }

    /// Push a fresh scope, run `f`, then pop it.
    ///
    /// Use at every "new query boundary":
    /// - the top of `resolve_query` (the query body's own scope),
    /// - each operand of a `SetExpr::SetOperation` so the operands'
    ///   FROM bindings don't shadow each other and unqualified column
    ///   refs in each operand resolve only against its own FROMs —
    ///   matching SQL's per-SELECT name resolution,
    /// - the DML statement inside `WITH … <DML>` so its target
    ///   binding doesn't share the enclosing query's scope with the
    ///   CTEs (CTEs stay reachable via the parent-scope walk-up).
    ///
    /// Predicate-ness flows through automatically:
    /// [`Context::in_predicate`](super::Context::in_predicate) stays
    /// set across the nested walk, so refs captured inside the new
    /// scope inherit the surrounding predicate context without any
    /// per-scope kind bookkeeping.
    ///
    /// Popping happens on the closure's return, including the `Err`
    /// path of a `Result`-returning closure, so this is the safe way
    /// to nest a `?`-bailing walk under a scope push.
    pub(crate) fn with_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.push_scope();
        let r = f(self);
        self.pop_scope();
        r
    }

    /// Walk a filter-position clause with `context.in_predicate = true`,
    /// so column / table refs captured inside (whether directly in the
    /// expression or transitively through a nested subquery) are tagged
    /// as predicate-position and excluded from lineage. Used for WHERE,
    /// HAVING, QUALIFY, JOIN ON, AsOf match, MERGE ON, CONNECT BY,
    /// pipe `|> WHERE`, etc. The previous `in_predicate` value is
    /// restored on return.
    pub(crate) fn with_filter_clause<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = self.context.in_predicate;
        self.context.in_predicate = true;
        let r = f(self);
        self.context.in_predicate = prev;
        r
    }
}
