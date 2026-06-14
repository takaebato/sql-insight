//! Scope arena types — `ScopeId`, `Scope` — plus the arena-management
//! methods on [`Resolver`] (`push_scope` / `pop_scope` / `bind_current`
//! / `resolve_unqualified_relation` / `with_scope`). Owns the
//! "container" side of name resolution; the `Binding` "contents" live
//! in [`super::binding`]. Predicate-position bookkeeping
//! ([`Resolver::suppress_lineage`]) lives in the parent module since
//! it touches walker state, not the scope arena.

use indexmap::IndexMap;
use sqlparser::ast::ObjectName;

use super::binding::{binding_alias_key, BindingKey};
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
/// ([`super::CapturedColumnRef::is_lineage_source`] /
/// [`super::CapturedTableRef::is_lineage_source`]) rather than on
/// the scope. This lets same-scope refs in different lexical
/// positions (e.g. CASE WHEN cond vs THEN value) be classified
/// independently.
#[derive(Debug)]
pub(crate) struct Scope {
    /// Lexically enclosing scope, or `None` for the root. Drives the
    /// walk-up for unqualified name resolution.
    pub(crate) parent: Option<ScopeId>,
    /// Bindings introduced *in this scope* (FROM tables, CTE
    /// definitions, derived tables, table functions), keyed by
    /// merge-identity ([`binding_alias_key`]). The key is used for the
    /// two exact-identity operations — merge-on-bind and
    /// exact-name lookup (CTE detection) — while right-anchored column
    /// resolution scans [`Scope::iter_bindings`] instead. `IndexMap`
    /// preserves definition order for deterministic iteration.
    pub(super) bindings: IndexMap<BindingKey, Binding>,
    /// The named output columns of the SELECT body that runs in this
    /// scope (its column aliases / projected names). Empty until the
    /// body's projection is recorded. Used only by the projection-alias
    /// suppression post-pass: an unqualified column ref in this scope's
    /// GROUP BY / HAVING / ORDER BY whose name matches one of these is a
    /// reference to the output, not a stored column, and is dropped from
    /// the public reads. For a set operation the operands' own scopes
    /// hold their per-operand names; the query's body scope holds the
    /// first operand's names (the result schema).
    pub(super) output_names: Vec<sqlparser::ast::Ident>,
    /// Column names introduced as `JOIN … USING (col)` merge columns in
    /// this scope. A `USING (a)` join folds both sides' `a` into one
    /// logical column, so an unqualified ref to `a` fans in to *every*
    /// joined relation that could own it (not a single owner). Recorded
    /// when the join constraint is walked; consumed by
    /// [`Resolver::capture_column_ref`](super::Resolver) to emit one
    /// captured ref per member instead of an ambiguous single ref.
    pub(super) merge_columns: Vec<sqlparser::ast::Ident>,
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
            output_names: Vec::new(),
            merge_columns: Vec::new(),
        }
    }

    /// Insert `binding` under its precomputed merge-identity `key`.
    /// When the key already exists:
    /// - both `Table` → merge roles (the `DELETE t1 FROM t1` case where
    ///   one name plays Read and Write);
    /// - otherwise → the new binding replaces it (last definition wins).
    pub(super) fn bind(&mut self, key: BindingKey, binding: Binding) {
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

    /// Exact-identity lookup by key — the merge / CTE-detection path.
    /// Right-anchored column resolution uses [`Self::iter_bindings`]
    /// instead.
    pub(super) fn resolve(&self, key: &BindingKey) -> Option<&Binding> {
        self.bindings.get(key)
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
    /// scope on demand if nothing is open yet. The merge-identity key
    /// is folded per binding kind via [`binding_alias_key`] under the
    /// active [`IdentifierCasing`](super::IdentifierCasing).
    pub(super) fn bind_current(&mut self, binding: Binding) {
        let key = binding_alias_key(&binding, self.resolution.casing);
        let id = self.current_scope_id();
        self.resolution.scopes[id.0].bind(key, binding);
    }

    /// Resolve an unqualified single-segment relation name by walking
    /// up the parent chain from `context.current_scope`, returning
    /// the first binding with that exact identity. Multi-segment
    /// qualified names return `None` — those route through
    /// schema/catalog resolution elsewhere.
    ///
    /// Used to detect CTE references (`FROM cte`): an exact single-name
    /// lookup (CTE names are unqualified), folded under the table-alias
    /// class. Callers filter for the `Cte` kind.
    pub(super) fn resolve_unqualified_relation(&self, relation: &ObjectName) -> Option<&Binding> {
        if relation.0.len() != 1 {
            return None;
        }
        let name = relation.0[0].as_ident()?;
        let from = self.context.current_scope?;
        let key = BindingKey::new(name, self.resolution.casing.table_alias);
        parent_chain(&self.resolution.scopes, from)
            .find_map(|id| self.resolution.scopes[id.0].resolve(&key))
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
    /// [`Context::is_lineage_source`](super::Context::is_lineage_source)
    /// stays at its current value across the nested walk, so refs
    /// captured inside the new scope inherit the surrounding
    /// classification without any per-scope kind bookkeeping.
    ///
    /// Popping happens on the closure's return, including the `Err`
    /// path of a `Result`-returning closure, so this is the safe way
    /// to nest a `?`-bailing walk under a scope push.
    pub(super) fn with_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.push_scope();
        let r = f(self);
        self.pop_scope();
        r
    }
}
