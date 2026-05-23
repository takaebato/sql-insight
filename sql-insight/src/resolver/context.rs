//! Lexical walking context — the set of "what is in effect right
//! now" tags the resolver carries as it visits AST nodes — plus the
//! scoped `with_*` helpers that mutate it for the duration of a
//! closure.

use super::{Resolver, ScopeKind};

/// Walking-context state that varies lexically as the resolver walks
/// expressions and clauses. `Copy`, so it is saved / restored cheaply
/// around closure-scoped helpers ([`Resolver::with_filter_clause`])
/// via [`Resolver::with_context`].
///
/// - `scope_kind` is stamped onto every scope pushed while this is in
///   effect. Default `Body`; flipped to `Predicate` by filter-clause
///   walkers so subqueries nested in WHERE / HAVING / JOIN ON etc.
///   inherit the right kind and are excluded from table-flow.
///   Propagates *through* subquery boundaries (a subquery in a
///   predicate is itself predicate-position).
///
/// `scope_kind` is the only field: it is structural (it gates
/// table-flow exclusion). Column refs carry no syntactic clause tag —
/// `reads` is a plain occurrence list — so nothing else needs to ride
/// along the walk.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VisitContext {
    pub(crate) scope_kind: ScopeKind,
}

impl Default for VisitContext {
    fn default() -> Self {
        Self {
            scope_kind: ScopeKind::Body,
        }
    }
}

impl<'a> Resolver<'a> {
    /// Push a fresh scope, run `f`, then pop it. Use around each
    /// branch of a `SetExpr::SetOperation` so the branches' FROM
    /// bindings don't shadow each other and unqualified column refs
    /// in each branch resolve only against its own FROMs — matching
    /// SQL's per-SELECT name resolution.
    pub(crate) fn with_branch_scope<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let kind = self.ctx.scope_kind;
        self.scopes_mut().push_query_scope(kind);
        let r = f(self);
        self.scopes_mut().pop_scope();
        r
    }

    /// Run `f` with a temporarily-modified [`VisitContext`]. `modify`
    /// applies in-place changes to the current `ctx` before `f` runs;
    /// the previous ctx (a Copy snapshot) is restored on return. The
    /// foundation for [`Resolver::with_filter_clause`] below.
    pub(crate) fn with_context<R>(
        &mut self,
        modify: impl FnOnce(&mut VisitContext),
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let prev = self.ctx;
        modify(&mut self.ctx);
        let r = f(self);
        self.ctx = prev;
        r
    }

    /// Walk a filter-position clause with `scope_kind = Predicate`, so
    /// any subquery pushed inside is classified as a predicate scope
    /// and thus excluded from table-flow. Used for WHERE, HAVING,
    /// QUALIFY, JOIN ON, AsOf match, MERGE ON, CONNECT BY, pipe
    /// `|> WHERE`, etc.
    pub(crate) fn with_filter_clause<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.with_context(|c| c.scope_kind = ScopeKind::Predicate, f)
    }
}
