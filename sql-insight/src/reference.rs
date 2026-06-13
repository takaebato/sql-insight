//! Reference (identity) types shared by SQL inspection features.
//!
//! [`TableReference`] / [`ColumnReference`] are *qualified names* that
//! denote a table / column in a catalog or schema — pure identity, not
//! a relation (no tuples) nor a schema (no attribute types). They carry
//! only enough to name the thing and compare two names for equality.

use core::fmt;

use crate::error::Error;
use sqlparser::ast::{Ident, Insert, ObjectName, TableFactor, TableObject};

/// Physical table identity — the `catalog.schema.name` triplet.
///
/// `TableReference` deliberately carries no alias: aliasing is a
/// use-site decoration, not part of a table's identity. Two SQL
/// fragments that reference the same physical table produce equal
/// `TableReference`s regardless of how they alias it, so `HashSet` /
/// `HashMap` dedup behaves intuitively and cross-statement comparison
/// is direct. Use-site alias information, when needed, is carried by
/// the structures that wrap a `TableReference` (e.g. resolver bindings).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TableReference {
    pub catalog: Option<Ident>,
    pub schema: Option<Ident>,
    pub name: Ident,
}

/// A column-level identity reference: an optional owning table plus the
/// column name.
///
/// `table` is `Option` because some column references cannot be
/// resolved structurally (ambiguous unqualified columns, references to
/// derived tables we do not yet expand, etc.) — the accompanying
/// [`ColumnRead::confidence`] surfaces *why*. Identity is name-based:
/// two `ColumnReference`s with the same `table` and `name` compare
/// equal, independent of where they appeared in the SQL or with what
/// confidence the resolver placed them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColumnReference {
    pub table: Option<TableReference>,
    pub name: Ident,
}

/// One read-side occurrence of a [`ColumnReference`], pairing the
/// identity with the resolver's [`Confidence`] in that placement.
///
/// Read-side surfaces ([`ColumnOperation::reads`] and
/// [`ColumnLineageEdge::source`]) use this wrapper so the same column
/// referenced twice can carry per-occurrence resolution metadata
/// without breaking [`ColumnReference`]'s identity-only contract.
/// Write-side surfaces ([`ColumnOperation::writes`],
/// [`ColumnTarget::Relation`]) stay as bare [`ColumnReference`] —
/// targets come straight from SQL syntax and are always
/// [`Confidence::Confirmed`] by construction, so the field would be
/// dead weight there.
///
/// [`ColumnOperation::reads`]: crate::extractor::ColumnOperation::reads
/// [`ColumnOperation::writes`]: crate::extractor::ColumnOperation::writes
/// [`ColumnLineageEdge::source`]: crate::extractor::ColumnLineageEdge::source
/// [`ColumnTarget::Relation`]: crate::extractor::ColumnTarget::Relation
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColumnRead {
    pub reference: ColumnReference,
    pub confidence: Confidence,
}

/// The resolver's confidence in a column reference's placement —
/// "how sure are we that this `(table, name)` is right?".
///
/// Catalog-less mode runs as an *inference mode*: every real-table
/// binding's schema is `Unknown`, so a single-candidate resolution
/// is best-effort, not confirmed. CTE and derived bodies do carry
/// `Known` schemas (the resolver derives them from the body's
/// projection), but those refs are synthetic and dropped from the
/// public reads / lineage by the resolver's post-pass.
///
/// `Ambiguous` and `Unresolved` are the two failure modes. Both come
/// with `table: None` on the [`ColumnReference`]; the variant tells
/// the consumer *why* the resolver gave up.
///
/// # Invariants
///
/// - **Catalog-less mode → no public `Confirmed`**: every surviving
///   non-synthetic ref points at an `Unknown` real table, so the
///   strongest claim the resolver can make is
///   [`Inferred`](Self::Inferred). Catalog-aware analysis is
///   therefore detectable by the presence of `Confirmed`.
/// - **Catalog-aware mode does not imply `Confirmed`**: catalogs are
///   often partial. Refs against tables the catalog doesn't cover,
///   or against a real `Unknown` table that won a multi-candidate
///   tiebreaker over `Known` ones, both still come back as
///   [`Inferred`](Self::Inferred).
///
/// # How each variant arises
///
/// | Situation | Confidence |
/// |---|---|
/// | catalog-less, real `Unknown` table, sole candidate | [`Inferred`](Self::Inferred) |
/// | catalog-less, two real `Unknown` tables in scope | [`Ambiguous`](Self::Ambiguous) |
/// | catalog-less, CTE `Known` body confirms the column | (internal `Confirmed`; synthetic, dropped) |
/// | catalog-less, CTE `Known` body denies the column (`SELECT typo FROM cte` where cte = `[id]`) | [`Unresolved`](Self::Unresolved) |
/// | catalog-aware, `Known` binding lists the column | [`Confirmed`](Self::Confirmed) |
/// | catalog-aware, `Known` binding *doesn't* list the column | [`Unresolved`](Self::Unresolved) |
/// | catalog-aware, one `Known` confirms + one `Unknown` suspect (Known-witness-over-Unknown-suspects) | [`Inferred`](Self::Inferred) |
/// | catalog-aware, two or more `Known` schemas confirm | [`Ambiguous`](Self::Ambiguous) |
/// | qualified `t.col` where `t` is `Unknown` | [`Inferred`](Self::Inferred) |
/// | qualified `t.col` where `t` is `Known` and lists `col` | [`Confirmed`](Self::Confirmed) |
///
/// # Consumer guidance
///
/// - **Strict mode validation**: a fully resolved, catalog-confirmed
///   statement satisfies
///   `op.diagnostics.is_empty() && op.reads.iter().all(|r| r.confidence == Confidence::Confirmed)`.
/// - **DFD / CRUD comprehension**: treat
///   [`Confirmed`](Self::Confirmed) and [`Inferred`](Self::Inferred)
///   interchangeably as "resolved" (use the `(table, name)` pair);
///   treat [`Ambiguous`](Self::Ambiguous) and
///   [`Unresolved`](Self::Unresolved) as "incomplete".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Confidence {
    /// A `Known`-schema binding (catalog hit, or a CTE / derived body
    /// the resolver could fully walk) positively confirmed the column
    /// at this reference. The strongest claim the resolver makes.
    Confirmed,
    /// Resolution succeeded by assuming the column exists where the
    /// resolver placed it: an `Unknown`-schema binding adopted as the
    /// sole candidate, a qualified reference whose qualifier alone
    /// determined the table, or a `Known` witness winning over
    /// `Unknown` suspects in a multi-candidate scope. All defensible
    /// inferences in catalog-less or partial-catalog mode, but not
    /// proven.
    Inferred,
    /// Multiple plausible candidates and the resolver couldn't pick
    /// one: either two-or-more `Known` schemas confirmed the column
    /// (genuine ambiguity), or every candidate was an `Unknown`
    /// suspect with no tiebreaker. `ColumnReference.table` is `None`.
    Ambiguous,
    /// No in-scope binding could plausibly own the column: either
    /// every `Known` schema in scope explicitly denied it, or the
    /// scope chain held no bindings at all. `ColumnReference.table`
    /// is `None`.
    Unresolved,
}

impl TableReference {
    pub(crate) fn try_from_name(name: &ObjectName) -> Result<Self, Error> {
        match name.0.len() {
            0 => Err(Error::AnalysisError(
                "ObjectName has no identifiers".to_string(),
            )),
            1 => Ok(TableReference {
                catalog: None,
                schema: None,
                name: name.0[0].as_ident().unwrap().clone(),
            }),
            2 => Ok(TableReference {
                catalog: None,
                schema: Some(name.0[0].as_ident().unwrap().clone()),
                name: name.0[1].as_ident().unwrap().clone(),
            }),
            3 => Ok(TableReference {
                catalog: Some(name.0[0].as_ident().unwrap().clone()),
                schema: Some(name.0[1].as_ident().unwrap().clone()),
                name: name.0[2].as_ident().unwrap().clone(),
            }),
            _ => Err(Error::AnalysisError(
                "Too many identifiers provided".to_string(),
            )),
        }
    }

    /// Format a slice of `TableReference`s as a comma-separated string
    /// (e.g. `"t1, schema.t2, catalog.schema.t3"`). Shared by the
    /// table-extractor `Display` surfaces.
    pub(crate) fn format_list(tables: &[Self]) -> String {
        tables
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Decode an `[Ident]` slice into a `TableReference`. 1 element =
    /// bare name, 2 = `schema.name`, 3 = `catalog.schema.name`. Returns
    /// `None` for 0 or 4+ parts. Use [`Self::try_from_name`] when the
    /// input is an [`ObjectName`] (4+ parts surface as `Error` there).
    pub(crate) fn try_from_parts(parts: &[Ident]) -> Option<Self> {
        match parts {
            [name] => Some(TableReference {
                catalog: None,
                schema: None,
                name: name.clone(),
            }),
            [schema, name] => Some(TableReference {
                catalog: None,
                schema: Some(schema.clone()),
                name: name.clone(),
            }),
            [catalog, schema, name] => Some(TableReference {
                catalog: Some(catalog.clone()),
                schema: Some(schema.clone()),
                name: name.clone(),
            }),
            _ => None,
        }
    }

    /// Parse an INSERT statement's target into (identity, alias) pair.
    pub(crate) fn from_insert_with_alias(value: &Insert) -> Result<(Self, Option<Ident>), Error> {
        let name = match &value.table {
            TableObject::TableName(object_name) => object_name,
            TableObject::TableFunction(function) => &function.name,
        };
        Ok((Self::try_from_name(name)?, value.table_alias.clone()))
    }

    /// Parse a `TableFactor::Table` into (identity, alias) pair. Other
    /// `TableFactor` variants (Derived / NestedJoin / Pivot / Unpivot /
    /// MatchRecognize / TableFunction / Function) do not name a stored
    /// table, so they surface as an `AnalysisError`.
    pub(crate) fn from_table_factor_with_alias(
        table: &TableFactor,
    ) -> Result<(Self, Option<Ident>), Error> {
        match table {
            TableFactor::Table { name, alias, .. } => Ok((
                Self::try_from_name(name)?,
                alias.as_ref().map(|a| a.name.clone()),
            )),
            _ => Err(Error::AnalysisError(
                "TableFactor variant other than Table cannot be converted to a TableReference"
                    .to_string(),
            )),
        }
    }
}

impl fmt::Display for TableReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let Some(catalog) = &self.catalog {
            parts.push(catalog.to_string());
        }
        if let Some(schema) = &self.schema {
            parts.push(schema.to_string());
        }
        parts.push(self.name.to_string());
        write!(f, "{}", parts.join("."))
    }
}

impl TryFrom<&Insert> for TableReference {
    type Error = Error;

    fn try_from(value: &Insert) -> Result<Self, Self::Error> {
        Self::from_insert_with_alias(value).map(|(table, _)| table)
    }
}

impl TryFrom<&TableFactor> for TableReference {
    type Error = Error;

    fn try_from(table: &TableFactor) -> Result<Self, Self::Error> {
        Self::from_table_factor_with_alias(table).map(|(table, _)| table)
    }
}

impl TryFrom<&ObjectName> for TableReference {
    type Error = Error;

    fn try_from(obj_name: &ObjectName) -> Result<Self, Self::Error> {
        Self::try_from_name(obj_name)
    }
}
