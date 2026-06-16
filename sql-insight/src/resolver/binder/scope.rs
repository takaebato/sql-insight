//! The bind-time resolution vocabulary: the scratch types the [`Binder`]
//! threads while walking the AST. None of these are stored on the
//! [`Plan`](crate::resolver::ir::Plan) — they exist only during the bind, so a
//! reference can be resolved against the relations / outputs visible at
//! each point. Fields are `pub(super)` (resolver-internal): the binder
//! constructs and reads them directly.
//!
//! [`Binder`]: super::Binder

use sqlparser::ast::Ident;

use crate::reference::TableReference;
use crate::resolver::ir::BoundColumn;

/// Bind-time resolution scope: the relations visible at a point in the
/// bind, plus (for the output-alias-visible clauses) the enclosing
/// SELECT's output columns. Scratch — never stored on the [`Plan`](crate::resolver::ir::Plan).
#[derive(Clone, Default)]
pub(super) struct Scope {
    pub(super) relations: Vec<Relation>,
    /// The enclosing SELECT's output columns, visible to its own
    /// GROUP BY / HAVING / ORDER BY (SQL alias visibility). Empty at
    /// FROM-level resolution (WHERE / projection / JOIN ON).
    pub(super) outputs: Vec<BoundColumn>,
    /// `JOIN … USING (col)` merge columns in scope: an unqualified
    /// reference to one fans in to every relation that could own it
    /// (one source per side) instead of resolving ambiguously.
    pub(super) merge_columns: Vec<Ident>,
}

impl Scope {
    pub(super) fn empty() -> Self {
        Self {
            relations: Vec::new(),
            outputs: Vec::new(),
            merge_columns: Vec::new(),
        }
    }

    pub(super) fn of(relation: Relation) -> Self {
        Self {
            relations: vec![relation],
            outputs: Vec::new(),
            merge_columns: Vec::new(),
        }
    }

    /// Concatenate the relations (and USING merge columns) of two scopes
    /// (a join / comma). Output columns aren't merged — they belong to a
    /// single SELECT.
    pub(super) fn merge(mut self, mut other: Scope) -> Scope {
        self.relations.append(&mut other.relations);
        self.merge_columns.append(&mut other.merge_columns);
        self
    }
}

/// A relation visible in a [`Scope`]: its use-site alias and where its
/// columns come from. Cloned into the binder's outer-scope stack so a
/// correlated subquery can resolve against enclosing relations.
#[derive(Clone)]
pub(super) struct Relation {
    pub(super) alias: Option<Ident>,
    pub(super) source: RelationSource,
}

impl Relation {
    /// The name this relation answers to in a qualifier: its alias if
    /// aliased, otherwise a real table's bare name. A derived table with
    /// no alias answers to nothing (SQL requires the alias anyway).
    pub(super) fn exposed_name(&self) -> Option<&Ident> {
        self.alias.as_ref().or(match &self.source {
            RelationSource::Table { table, .. } => Some(&table.name),
            RelationSource::Derived { .. } | RelationSource::TableFunction => None,
        })
    }
}

/// Where a relation's columns come from.
#[derive(Clone)]
pub(super) enum RelationSource {
    /// A real stored table: its canonical identity plus catalog column
    /// knowledge (`Open` catalog-free, `Known` with a catalog).
    Table {
        table: TableReference,
        columns: RelationColumns,
    },
    /// A derived table / subquery: its output columns, each already
    /// resolved to real base columns (pre-collapsed provenance).
    /// *Synthetic* — it has no table identity, so a reference through it
    /// surfaces the inner real columns, never the derived name itself.
    Derived { columns: Vec<BoundColumn> },
    /// A table function / `UNNEST` / `PIVOT` / `JSON_TABLE` output: a
    /// synthetic relation whose columns are opaque (dynamically produced).
    /// A reference qualified by its alias resolves *to it* but contributes
    /// nothing (the produced columns aren't stored), so it neither surfaces
    /// as a read nor as a lineage source — its inputs are read separately
    /// from the function's argument expressions.
    TableFunction,
}

/// What a real table exposes for resolution.
#[derive(Clone)]
pub(super) enum RelationColumns {
    /// Column set unknown (catalog-free, or a catalog miss / ambiguous
    /// match) — any name could plausibly belong here (`Inferred`).
    Open,
    /// Catalog-known columns (quoted = exact-match idents). A name in
    /// the list resolves `Cataloged`; a name absent means the relation
    /// can't own it.
    Known(Vec<Ident>),
}

/// A common table expression in scope: a named synthetic relation. Bound
/// once at its `WITH` declaration. This is the bind-time *resolution*
/// entry — just the name and the exposed `outputs` (already collapsed like
/// a derived table). The body sub-plan itself lives on the
/// [`With`](crate::resolver::ir::With) node (so it is walked once regardless of
/// reference count); a FROM reference resolves through these `outputs` and
/// emits a lightweight [`CteRef`](crate::resolver::ir::CteRef), never a clone of the
/// body.
#[derive(Clone)]
pub(super) struct CteRelation {
    pub(super) name: Ident,
    pub(super) outputs: Vec<BoundColumn>,
}
