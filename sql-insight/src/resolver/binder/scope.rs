//! The bind-time **scope**: what's visible at a point in the bind, as scratch
//! threaded bottom-up (`bind_* -> (LogicalPlan, Scope)`) and never stored on
//! the tree.
//!
//! A [`Scope`] groups three kinds of in-scope state:
//! - `relations` — the FROM relations a column reference resolves its owner
//!   against.
//! - `query_outputs` — this (sub)query's SELECT-list output columns, visible by
//!   name to its own GROUP BY / HAVING / ORDER BY and harvested by a parent as a
//!   derived table / CTE's exposed columns.
//! - `merge_columns` — `JOIN … USING (col)` names that fan in to every owning
//!   side.
//!
//! Enclosing scopes (correlation) and the CTE registry are *not* here — they
//! live on the [`Binder`](super::Binder)'s downward `Context` (`outer` /
//! `ctes`), swapped per scope by `in_scope`, because an inner subquery reaches
//! enclosing *relations* (and CTE declarations), not their outputs or merge
//! columns.

use sqlparser::ast::{Ident, TableAlias};

use super::super::logical_plan::Columns;
use crate::reference::TableReference;

/// A declared CTE in scope: its name and the output slots it exposes (so a
/// `FROM cte` reference resolves through them). The body lives once on the
/// owning `With` node; a reference is a lightweight `CteRef`.
#[derive(Clone)]
pub(super) struct CteDecl {
    pub(super) name: Ident,
    pub(super) columns: Exposed,
}

/// The output-slot view a derived relation (a derived table / CTE) exposes:
/// one entry per output position (`None` = an anonymous output — an unaliased
/// expression, unnameable but still occupying its position).
///
/// `complete` says whether `slots` is the relation's *whole* output list. It
/// is `false` when the producing body's own outputs are indeterminate (it
/// holds an unexpanded wildcard, or it is an opaque body like a `TABLE foo` /
/// unnest whose columns aren't enumerated): named references still resolve
/// against the known slots, but anything positional — wildcard expansion,
/// (`Complete`-gated) slot counting — must not trust an incomplete view.
#[derive(Clone, Default)]
pub(super) struct Exposed {
    pub(super) slots: Vec<Option<Ident>>,
    pub(super) complete: bool,
}

impl Exposed {
    /// The named slots (resolution / NATURAL-merge candidates) — sparse when
    /// anonymous outputs sit between them.
    pub(super) fn names(&self) -> impl Iterator<Item = &Ident> {
        self.slots.iter().flatten()
    }
}

/// The relations and outputs visible at a point in the bind. Scratch — never
/// stored on the [`LogicalPlan`](super::LogicalPlan) tree.
#[derive(Default)]
pub(super) struct Scope {
    pub(super) relations: Vec<Relation>,
    /// This (sub)query's output columns — the SELECT list's results. Visible by
    /// name to its own GROUP BY / HAVING / ORDER BY (clause-alias visibility);
    /// harvested by a parent as a derived table / CTE's exposed columns; and at
    /// the top level these become the [`ColumnTarget::QueryOutput`] lineage
    /// targets — hence the name. Empty at FROM-level resolution (WHERE /
    /// projection), populated once the projection is bound (see
    /// [`with_query_outputs`](Scope::with_query_outputs)).
    ///
    /// [`ColumnTarget::QueryOutput`]: crate::extractor::ColumnTarget::QueryOutput
    pub(super) query_outputs: Vec<OutputCol>,
    /// Whether `query_outputs` covers *every* output position — `false` when
    /// the projection still holds an unexpanded wildcard (its slot count is
    /// then indeterminate), or when the body's outputs aren't enumerated at
    /// all (`TABLE foo`, a DML body). Feeds [`Exposed::complete`] and the DML
    /// roots' `source_wildcard`. Deliberately `false` under `Default` so a
    /// scope is only trusted positionally where a bind path vouches for it.
    pub(super) outputs_complete: bool,
    /// `JOIN … USING (col)` merge-column names: an unqualified reference to one
    /// fans in to every joined relation that could own it.
    pub(super) merge_columns: Vec<Ident>,
}

/// A projection output column, for clause-alias resolution. `identity` marks a
/// bare passthrough (`SELECT a`, or the redundant `SELECT a AS a` — the test is
/// name-equality, not alias presence): a clause reference to it falls through to
/// the real column (a read); a non-identity (introduced) alias — a rename
/// (`a AS x`) or a computed expr (`a + b AS s`) — resolves to the output itself
/// (`Binding::Derived`, dropped from reads). See [`Binder::output_cols`](super::Binder::output_cols).
#[derive(Clone)]
pub(super) struct OutputCol {
    pub(super) name: Option<Ident>,
    pub(super) identity: bool,
}

/// A relation in scope: where its columns come from, plus its use-site `alias`
/// (if any). Cloned onto the correlation stack so an inner subquery can resolve
/// against enclosing relations.
#[derive(Clone)]
pub(super) enum Relation {
    /// A real table: its canonical identity and catalog column knowledge.
    /// (The table-level resolution kind lives on the `Scan`; here the `columns`
    /// — `Cataloged` vs `Unknown` — drive a column reference's resolution.)
    Table {
        alias: Option<Ident>,
        table: TableReference,
        columns: Columns,
    },
    /// A derived table / CTE reference: a synthetic relation exposing the
    /// output slots of an inner query. A reference through it is
    /// `Binding::Derived` — the origin traversal traces into the producing
    /// sub-plan (`SubqueryAlias` / `CteRef`).
    Derived {
        alias: Option<Ident>,
        columns: Exposed,
    },
    /// An opaque table function / PIVOT / … relation with dynamic columns. A
    /// bare name is **not** claimed by it (so it stays resolvable against real
    /// tables); a qualified ref through its alias is `Binding::Derived` — the
    /// origin traversal reaches the [`LogicalPlan::TableFunction`](super::LogicalPlan::TableFunction)
    /// node and traces to the function arguments' origins (dropped from
    /// reads either way).
    TableFunction { alias: Option<Ident> },
}

impl Scope {
    /// A single-relation scope (a bare table / derived factor's own scope).
    pub(super) fn single(relation: Relation) -> Scope {
        Scope {
            relations: vec![relation],
            ..Scope::default()
        }
    }

    /// A resolution scope over the FROM siblings to a factor's left (the
    /// LATERAL-visible relations a table function's arguments read against).
    pub(super) fn from_relations(relations: &[Relation]) -> Scope {
        Scope {
            relations: relations.to_vec(),
            ..Scope::default()
        }
    }

    /// Absorb another scope's relations and merge columns into this one — the
    /// FROM-level combine of a comma / cross / qualified join. The query outputs
    /// are per-SELECT, so they are *not* combined here.
    pub(super) fn absorb(&mut self, other: Scope) {
        self.relations.extend(other.relations);
        self.merge_columns.extend(other.merge_columns);
    }

    /// Record `JOIN … USING (col)` merge-column names.
    pub(super) fn add_merge_columns(&mut self, columns: impl IntoIterator<Item = Ident>) {
        self.merge_columns.extend(columns);
    }

    /// Mint the clause scope: attach the bound projection's output columns to
    /// the FROM scope, so GROUP BY / HAVING / ORDER BY resolve against the FROM
    /// relations *plus* these outputs (clause-alias visibility), while WHERE and
    /// the projection itself saw only the relations. `complete` records whether
    /// the outputs cover every position (no unexpanded wildcard remained).
    pub(super) fn with_query_outputs(self, query_outputs: Vec<OutputCol>, complete: bool) -> Scope {
        Scope {
            query_outputs,
            outputs_complete: complete,
            ..self
        }
    }

    /// The output-slot view this (sub)query exposes as a derived table / CTE:
    /// an explicit alias list (`AS d(x, y)`) renames positionally; otherwise
    /// each output keeps its inferred name (`None` for an anonymous output —
    /// it can't be referenced by name, but it still holds its position).
    /// `complete` carries [`Scope::outputs_complete`] through, so a positional
    /// consumer (wildcard expansion) knows whether the view can be trusted.
    pub(super) fn exposed(&self, alias: Option<&TableAlias>) -> Exposed {
        let alias_columns: Vec<&Ident> = alias
            .map(|a| a.columns.iter().map(|c| &c.name).collect())
            .unwrap_or_default();
        // An explicit column-alias list (`… AS d(x, y)`) declares the exposed
        // columns by position; past it, the body's own output names apply. Drive
        // by the longer of the two so the declared alias names survive even when
        // the body exposes no names (a `SELECT *` whose outputs are unexpanded) —
        // otherwise a reference to an aliased column would dangle as a phantom
        // unresolved read instead of binding to this derived relation.
        let len = alias_columns.len().max(self.query_outputs.len());
        let slots = (0..len)
            .map(|i| {
                alias_columns
                    .get(i)
                    .map(|n| (*n).clone())
                    .or_else(|| self.query_outputs.get(i).and_then(|o| o.name.clone()))
            })
            .collect();
        Exposed {
            slots,
            complete: self.outputs_complete,
        }
    }
}

impl Relation {
    /// The name this relation answers to in a qualifier: its alias, else a
    /// real table's bare name. A derived relation answers only to its alias.
    pub(super) fn exposed_name(&self) -> Option<&Ident> {
        match self {
            Relation::Table { alias, table, .. } => alias.as_ref().or(Some(&table.name)),
            Relation::Derived { alias, .. } | Relation::TableFunction { alias } => alias.as_ref(),
        }
    }

    /// The column names this relation is *known* to expose — a `Cataloged`
    /// table's columns, or a derived relation's named slots. An `Unknown`
    /// (catalog-free) table or an opaque table function has no known list, so it
    /// contributes nothing to a NATURAL join's schema-common columns.
    pub(super) fn known_columns(&self) -> Vec<&Ident> {
        match self {
            Relation::Table {
                columns: Columns::Cataloged(cols),
                ..
            } => cols.iter().collect(),
            Relation::Derived { columns, .. } => columns.names().collect(),
            Relation::Table {
                columns: Columns::Unknown,
                ..
            }
            | Relation::TableFunction { .. } => Vec::new(),
        }
    }
}
