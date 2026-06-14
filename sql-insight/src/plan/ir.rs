//! The bound logical-plan IR: a materialized, full-stack operator tree.
//!
//! These types are the *persistent* output of [`super::binder::build`] — they
//! carry only resolved data (each [`BoundColumn::provenance`] is already
//! collapsed to real base columns). The bind-time resolution scope
//! ([`super::binder::Scope`]) is scratch and lives in the binder, not
//! here.

use sqlparser::ast::Ident;

use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ResolutionKind, TableReference};

/// One node in the bound operator tree.
///
/// `PassThrough` unifies join and every filter (their output is the
/// identity concatenation of their inputs); `Project` is the only
/// column-defining producer. `Scan` is the named leaf; `OpaqueLeaf`
/// covers leaves with no inspectable columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Scan(Scan),
    OpaqueLeaf(OpaqueLeaf),
    PassThrough(PassThrough),
    Project(Project),
    SetOp(SetOp),
    Write(Write),
}

/// A real stored table (leaf). Catalog-free its column set is unknown,
/// so resolution treats it as an open relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) table: TableReference,
    /// Alias at this use-site, if any (drives qualifier matching).
    pub(crate) alias: Option<Ident>,
    /// How the catalog identified the table. Catalog-free → `Inferred`.
    pub(crate) resolution: ResolutionKind,
}

/// A relation with no inspectable column provenance — `VALUES`, a table
/// function, or any FROM item not modelled yet. Contributes a (possibly
/// aliased) scope entry but no source columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpaqueLeaf {
    pub(crate) alias: Option<Ident>,
}

/// Join (N inputs) **and** every filter (WHERE / HAVING / JOIN ON):
/// output is the identity concatenation of the inputs; `reads` are the
/// columns the predicate referenced — filter position, never value /
/// lineage sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PassThrough {
    pub(crate) inputs: Vec<Plan>,
    pub(crate) reads: Vec<ColumnRead>,
}

/// The SELECT list — the only column-defining producer. Each output
/// column carries its pre-collapsed provenance and a
/// passthrough/transformation kind (aggregates fold in as
/// `Transformation`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Project {
    pub(crate) input: Box<Plan>,
    pub(crate) outputs: Vec<BoundColumn>,
}

/// A set operation (UNION / INTERSECT / EXCEPT): the result schema is
/// the first operand's, with the others fanned in positionally. (Bind
/// for this is a later brick.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SetOp {
    pub(crate) operands: Vec<Plan>,
}

/// A statement that writes a relation (INSERT / UPDATE / MERGE / CTAS /
/// CREATE VIEW): the source `input`'s output columns pair with
/// `target_columns` to produce relation lineage. (Bind is a later
/// brick.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Write {
    pub(crate) target: TableReference,
    pub(crate) target_columns: Vec<Ident>,
    pub(crate) input: Box<Plan>,
}

/// One resolved output column: its (optional) name, the real base
/// columns it derives from (pre-collapsed provenance), and whether it
/// forwards a value unchanged or transforms it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoundColumn {
    pub(crate) name: Option<Ident>,
    pub(crate) provenance: Vec<ColumnRead>,
    pub(crate) kind: ColumnLineageKind,
}
