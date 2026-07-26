//! The downward binding environment: the in-scope CTE declarations and the
//! enclosing column-resolution stack threaded into a (sub)query — the
//! top-down counterpart to the bottom-up [`Scope`]. Held on the
//! [`Binder`] and swapped per scope by its `in_scope` family
//! (a child is built with the `with_*` methods, never mutated in place), so a
//! nested scope can't leak to a sibling.

use super::*;

/// One level of the column-resolution stack — an enclosing scope a bare
/// reference falls through to, innermost last. Unifies what used to be two
/// side-by-side mechanisms (a relation correlation stack + a flat lambda-param
/// list) into a single ordered stack, so precedence is managed in one place:
/// `resolve` walks it innermost-first, and a level sits at exactly its lexical
/// depth (a lambda parameter is outside any subquery in the body but inside the
/// enclosing query). CTEs are *not* here — they are a separate (table) namespace
/// resolved in `bind_table_factor`.
#[derive(Clone)]
pub(super) enum Level {
    /// An enclosing query level's FROM relations (a subquery / the query the
    /// current one is correlated into), with the level's `USING` / NATURAL
    /// merge columns — a correlated reference to one fans in to its owners
    /// exactly as it would in the enclosing query itself.
    Relations {
        relations: Vec<Relation>,
        merge_columns: Vec<Ident>,
    },
    /// A lambda's parameters (`x` in `x -> …`): a bare reference resolves to
    /// [`Binding::Local`] — not a table column.
    Lambda(Vec<Ident>),
}

/// The downward binding environment threaded into a (sub)query: the in-scope
/// CTE declarations (table namespace) and the enclosing resolution stack
/// (column namespace). Held on the [`Binder`] as a field and
/// swapped per scope by its `in_scope` family (a child is built with the
/// `with_*` methods below, never mutated in place), so a nested scope can't
/// leak to a sibling.
#[derive(Clone, Default)]
pub(super) struct Context {
    /// CTEs in scope (declaration order, innermost `WITH` last).
    pub(super) ctes: Vec<CteDecl>,
    /// The enclosing column-resolution stack (outermost first, innermost last)
    /// a bare reference falls through to after the current [`Scope`]:
    /// enclosing query relations (correlation) and lambda parameters, each at
    /// its lexical depth. See [`Level`].
    pub(super) outer: Vec<Level>,
    /// The `EXCLUDED` pseudo-relation's columns while binding a conflict
    /// action (`ON CONFLICT DO UPDATE` / `ON DUPLICATE KEY UPDATE`), else
    /// empty. An unqualified reference to one is contested between the
    /// existing target row and `EXCLUDED` (PostgreSQL rejects it as
    /// ambiguous), so resolution demotes it — a *rule*, not a post-pass, so
    /// it reaches a correlated reference inside a subquery of the action
    /// too. Carried into child contexts like the stack (a subquery stays
    /// inside the conflict action).
    pub(super) excluded_columns: Vec<Ident>,
}

impl Context {
    /// A child context with a replaced CTE environment (the caller builds the
    /// full in-scope list, declaration order); the enclosing stack is kept.
    pub(super) fn with_ctes(&self, ctes: Vec<CteDecl>) -> Context {
        Context {
            ctes,
            ..self.clone()
        }
    }

    /// A child context with one more enclosing relation level on the stack (used
    /// when descending into a subquery in an expression / a LATERAL factor).
    pub(super) fn with_outer(
        &self,
        relations: Vec<Relation>,
        merge_columns: Vec<Ident>,
    ) -> Context {
        self.pushing(Level::Relations {
            relations,
            merge_columns,
        })
    }

    /// A child context with the lambda `params` pushed as a level (used to bind
    /// a lambda body, so its parameter references resolve to [`Binding::Local`]
    /// rather than table columns). Push *after* the enclosing relations
    /// (`with_outer`) so the parameters sit inside the enclosing query but
    /// outside any subquery in the body.
    pub(super) fn with_lambda(&self, params: impl IntoIterator<Item = Ident>) -> Context {
        self.pushing(Level::Lambda(params.into_iter().collect()))
    }

    /// A child context marked as binding a conflict action whose `EXCLUDED`
    /// pseudo-relation exposes `columns`.
    pub(super) fn with_excluded(&self, columns: Vec<Ident>) -> Context {
        let mut child = self.clone();
        child.excluded_columns = columns;
        child
    }

    fn pushing(&self, level: Level) -> Context {
        let mut child = self.clone();
        child.outer.push(level);
        child
    }
}
