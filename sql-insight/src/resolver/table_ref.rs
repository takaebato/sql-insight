//! `CapturedTableRef` — `FROM`-position table-source captures the walker
//! produces during the AST walk — plus the `capture_*_table_ref`
//! constructor methods on `Resolver`. Parallel to [`super::column_ref`]
//! for the table-granularity side of lineage tracking.

use crate::reference::TableReference;

use super::{Resolver, ScopeId};

/// A single `FROM`-position use of a table-like source captured at walk
/// time. Table-lineage collapse iterates these (instead of walking
/// scope bindings), so an unreferenced CTE — whose declaration binds
/// names but whose body is never `FROM`-used — contributes no lineage
/// sources.
#[derive(Clone, Debug)]
pub(crate) struct CapturedTableRef {
    /// Scope where the use occurs — used for predicate-ancestor
    /// filtering at collapse time.
    pub(crate) scope_id: ScopeId,
    /// What's being used: a real table (emits as a lineage source) or
    /// a synthetic relation (recurses into its body to find real
    /// tables underneath).
    pub(crate) target: TableRefTarget,
}

/// Resolution of a [`CapturedTableRef`] target.
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
/// existing [`CapturedColumnRef::synthetic`](super::CapturedColumnRef) field
/// and [`is_synthetic_binding`](super::binding::is_synthetic_binding)
/// helper.
///
/// Variants represent **what to do during table-lineage collapse**,
/// not raw storage classification. `Binding::TableFunction` is
/// synthetic at the binding level but is omitted here (and from
/// `CapturedTableRef` emission entirely), since it has no inspectable
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

impl<'a> Resolver<'a> {
    /// Record a use of a real table at the current scope. Called by
    /// `bind_real_table` on Read-position binds.
    pub(super) fn capture_real_table_ref(&mut self, table: TableReference) {
        let scope_id = self.current_scope_id();
        self.resolution.table_refs.push(CapturedTableRef {
            scope_id,
            target: TableRefTarget::Real(table),
        });
    }

    /// Record a use of a synthetic relation (CTE / true derived) at
    /// the current scope. `body_scope` is the arena id of the
    /// synthetic's body — collapse recurses into its subtree.
    pub(super) fn capture_synthetic_table_ref(&mut self, body_scope: ScopeId) {
        let scope_id = self.current_scope_id();
        self.resolution.table_refs.push(CapturedTableRef {
            scope_id,
            target: TableRefTarget::Synthetic { body_scope },
        });
    }
}
