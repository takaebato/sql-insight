//! Scoped `with_*` helpers that save / restore one piece of the
//! resolver's walking context — either the scope arena (`with_scope`)
//! or the `scope_kind` field (`with_filter_clause`) — for the duration
//! of a closure, so the prior state is restored on return without the
//! caller having to remember to pair push/pop or save/restore by hand.

use super::{Resolver, ScopeKind};

impl<'a> Resolver<'a> {
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
