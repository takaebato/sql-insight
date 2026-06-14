//! The bound logical-plan IR: a materialized, full-stack operator tree.
//!
//! These types are the *persistent* output of [`super::binder::build`] — they
//! carry only resolved data (each [`BoundColumn::provenance`] is already
//! collapsed to real base columns). The bind-time resolution scope
//! ([`super::binder::Scope`]) is scratch and lives in the binder, not
//! here.

use sqlparser::ast::Ident;

use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, TableReference};

/// One node in the bound operator tree.
///
/// `PassThrough` unifies join and every filter (their output is the
/// identity concatenation of their inputs); `Project` is the only
/// column-defining producer. `Scan` is the named leaf; `OpaqueLeaf`
/// covers leaves with no inspectable columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Scan(Scan),
    /// A leaf with no inspectable columns — `VALUES`, a table function,
    /// or any FROM item not modelled yet. It contributes no source
    /// columns (and no scope entry), so resolution / extraction skip it.
    OpaqueLeaf,
    PassThrough(PassThrough),
    Project(Project),
    SetOp(SetOp),
    Write(Write),
    With(With),
    /// A FROM-clause reference to an in-scope CTE. Its columns are
    /// resolved (and collapsed into the referencing value's provenance)
    /// at bind time through the scope, so the reference itself contributes
    /// nothing to extraction — the CTE body's reads / tables are counted
    /// once at the owning [`With`] node, never re-counted per reference.
    CteRef(CteRef),
}

/// A real stored table (leaf), identified by its (catalog-canonicalized)
/// reference. The use-site alias and catalog resolution live on the
/// bind-time scope, not here; column reads already carry their own
/// resolution, so the persisted node needs only the table identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) table: TableReference,
}

/// Join (N inputs) **and** every filter (WHERE / HAVING / JOIN ON):
/// output is the identity concatenation of the inputs; `reads` are the
/// columns the predicate referenced — filter position, never value /
/// lineage sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PassThrough {
    pub(crate) inputs: Vec<Plan>,
    pub(crate) reads: Vec<ColumnRead>,
    /// Sub-plans of subqueries in the predicate (a `WHERE … IN (SELECT …)`
    /// / `EXISTS (…)`): full bound plans so their tables / reads surface by
    /// walking, rather than being folded away. Filter position — never a
    /// lineage source.
    pub(crate) subqueries: Vec<Plan>,
}

/// The SELECT list — the only column-defining producer. Each output
/// column carries its pre-collapsed provenance, each source of which
/// holds its own composed lineage kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Project {
    pub(crate) input: Box<Plan>,
    pub(crate) outputs: Vec<BoundColumn>,
    /// Sub-plans of scalar subqueries in the projection expressions
    /// (`SELECT (SELECT …) …`): kept whole so their tables / reads surface
    /// by walking. Value position — each one's output is also folded into
    /// the owning output column's provenance (as a lineage source).
    pub(crate) subqueries: Vec<Plan>,
}

/// A set operation (UNION / INTERSECT / EXCEPT): the result columns are
/// the operands' columns merged positionally (name from the first
/// operand, provenance unioned across all).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SetOp {
    pub(crate) operands: Vec<Plan>,
}

/// A statement that writes a relation (INSERT / UPDATE / MERGE / CTAS /
/// CREATE VIEW): the source `input`'s output columns pair positionally
/// with `target_columns` to produce relation lineage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Write {
    pub(crate) target: TableReference,
    pub(crate) target_columns: Vec<Ident>,
    pub(crate) input: Box<Plan>,
}

/// A `WITH` clause: the CTEs it declares, kept as named sub-plans, plus
/// the `body` that resolves against them. Each declared body is bound
/// **once** and lives here regardless of how many references consume it
/// (or whether any do), so extraction walks it exactly once — a
/// reference is a lightweight [`CteRef`], never a clone of the body. This
/// is the shared-node model: SQL says a CTE is one named relation, so the
/// plan stores one sub-plan, mirroring how a recursive CTE keeps a single
/// shared work-table rather than inlining a copy per reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct With {
    /// The CTEs this clause declares, in declaration order.
    pub(crate) ctes: Vec<CtePlan>,
    pub(crate) body: Box<Plan>,
}

/// One declared CTE: its name paired with its bound body sub-plan. The
/// name links it to the [`CteRef`]s that consume it; the plan is walked
/// once (for reads / tables) at this declaration site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CtePlan {
    pub(crate) name: Ident,
    pub(crate) plan: Plan,
}

/// A FROM-clause reference to an in-scope CTE: just the CTE's name. Its
/// columns were resolved and collapsed into the referencing value's
/// provenance at bind time (via the scope), so this leaf contributes no
/// reads / lineage of its own — the body's reads are counted once at the
/// owning [`With`] node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CteRef {
    pub(crate) name: Ident,
}

/// One resolved output column: its (optional) name and the real base
/// columns it derives from. Each source is pre-collapsed to a real
/// column and carries its **composed** lineage kind — `Transformation`
/// if any step from that base column up to this output transformed the
/// value (so a passthrough output of a transforming derived column is
/// `Transformation`), else `Passthrough`. Lineage extraction emits one
/// edge per source straight from this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoundColumn {
    pub(crate) name: Option<Ident>,
    pub(crate) provenance: Vec<ProvenanceSource>,
}

/// One pre-collapsed lineage source of a [`BoundColumn`]: the real base
/// column read, paired with the composed kind of the path from it to the
/// output column.
///
/// `synthetic_origin` marks a source reached *through* a synthetic step —
/// a reference to a derived-table / CTE column, or to the query's own
/// output alias — rather than a fresh physical reference to a base
/// column. Such a source still carries the collapsed base column for
/// lineage, but the physical read it stands for was already counted at
/// the inner producer, so it is excluded from `reads` (which counts
/// physical references, each with its own source span).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProvenanceSource {
    pub(crate) read: ColumnRead,
    pub(crate) kind: ColumnLineageKind,
    pub(crate) synthetic_origin: bool,
}
