//! Scoped `with_*` helpers that save / restore the resolver's
//! `scope_kind` for the duration of a closure, so lexical
//! predicate-position context is set and unset around a clause walk
//! without the caller having to remember to restore it.

use super::{Resolver, ScopeKind};

impl<'a> Resolver<'a> {
    /// Push a fresh scope, run `f`, then pop it. Use around each
    /// branch of a `SetExpr::SetOperation` so the branches' FROM
    /// bindings don't shadow each other and unqualified column refs
    /// in each branch resolve only against its own FROMs — matching
    /// SQL's per-SELECT name resolution. The current `scope_kind` is
    /// propagated onto the pushed scope.
    pub(crate) fn with_branch_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let kind = self.scope_kind;
        self.scopes_mut().push_query_scope(kind);
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
