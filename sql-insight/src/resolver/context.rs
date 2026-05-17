//! Lexical walking context — the set of "what is in effect right
//! now" tags the resolver carries as it visits AST nodes — plus the
//! scoped `with_*` helpers that mutate it for the duration of a
//! closure.

use crate::extractor::column_operation_extractor::ReadKind;

use super::{RelationResolver, ScopeKind};

/// Walking-context state that varies lexically as the resolver walks
/// expressions and clauses. All fields are `Copy`, so the whole
/// struct is saved / restored cheaply around closure-scoped helpers
/// ([`RelationResolver::with_read_kind`],
/// [`RelationResolver::with_filter_clause`],
/// [`RelationResolver::with_case_condition`]) via
/// [`RelationResolver::with_context`].
///
/// - `scope_kind` is stamped onto every scope pushed while this is in
///   effect. Default `Body`; flipped to `Predicate` by filter-clause
///   walkers so subqueries nested in WHERE / HAVING / JOIN ON etc.
///   inherit the right kind. Propagates *through* subquery boundaries
///   (a subquery in a predicate is itself predicate-position).
/// - `read_kind` is stamped onto every column ref recorded while this
///   is in effect. Default `Projection`; flipped by clause walkers to
///   `Filter` / `GroupBy` / `Sort` / `Window`. Does *not* propagate
///   through subquery boundaries — a subquery's own projection refs
///   are its own kind, not the enclosing clause's.
/// - `in_case_condition` is an additive modifier: when true, recorded
///   refs also carry `ReadKind::Conditional`. Toggled around
///   `Expr::Case` condition expressions. Does *not* propagate through
///   subquery boundaries (the subquery's refs are syntactically the
///   subquery's own, not the outer CASE condition's).
#[derive(Debug, Clone, Copy)]
pub(crate) struct VisitContext {
    pub(crate) scope_kind: ScopeKind,
    pub(crate) read_kind: ReadKind,
    pub(crate) in_case_condition: bool,
}

impl Default for VisitContext {
    fn default() -> Self {
        Self {
            scope_kind: ScopeKind::Body,
            read_kind: ReadKind::Projection,
            in_case_condition: false,
        }
    }
}

impl<'a> RelationResolver<'a> {
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
    /// foundation for all the scoped clause / kind / modifier helpers
    /// below.
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

    /// Temporarily stamp recorded refs with `kind`, then restore. Use
    /// around any walk where the syntactic clause changes — projection
    /// items (default `Projection`), filter clauses (`Filter`), etc.
    pub(crate) fn with_read_kind<R>(
        &mut self,
        kind: ReadKind,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.with_context(|c| c.read_kind = kind, f)
    }

    /// Temporarily mark recorded refs as appearing in a CASE-WHEN
    /// condition position. Stacks additively on top of the current
    /// `read_kind` — a column in a SELECT projection's CASE condition
    /// ends up with `kinds = [Projection, Conditional]`.
    pub(crate) fn with_case_condition<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.with_context(|c| c.in_case_condition = true, f)
    }

    /// Convenience for walking a filter-position clause: stamps both
    /// `read_kind = Filter` (so column refs land with the `Filter`
    /// kind) AND `scope_kind = Predicate` (so any subquery pushed
    /// inside is classified as a predicate scope and thus excluded
    /// from table-flow). Used for WHERE, HAVING, QUALIFY, JOIN ON,
    /// AsOf match, MERGE ON, CONNECT BY, pipe `|> WHERE`, etc.
    pub(crate) fn with_filter_clause<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        self.with_context(
            |c| {
                c.read_kind = ReadKind::Filter;
                c.scope_kind = ScopeKind::Predicate;
            },
            f,
        )
    }
}
