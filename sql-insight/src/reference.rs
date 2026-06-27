//! Reference (identity) types shared by SQL inspection features.
//!
//! [`TableReference`] / [`ColumnReference`] are *qualified names* that
//! denote a table / column in a catalog or schema — pure identity, not
//! a relation (no tuples) nor a schema (no attribute types). They carry
//! only enough to name the thing and compare two names for equality.

use core::fmt;

use crate::casing::IdentifierCasing;
use crate::error::Error;
use sqlparser::ast::{Ident, Insert, ObjectName, TableFactor, TableObject};

/// Physical table identity — the `catalog.schema.name` triplet.
///
/// `TableReference` deliberately carries no alias: aliasing is a
/// use-site decoration, not part of a table's identity. Use-site alias
/// information, when needed, is carried by the structures that wrap a
/// `TableReference` (e.g. resolver bindings).
///
/// **Equality has two levels.** The derived `Eq` / `Hash` are
/// *structural* — case- and quote-sensitive, exact segments. That is the
/// right dedup when references come from catalog-backed analysis (matched
/// tables are canonicalized, so equal tables produce equal references) and
/// for direct cross-statement comparison. For catalog-free dedup, where
/// the same table may appear under fold-equivalent spellings (`users` vs
/// `USERS`), use [`identity_key`](Self::identity_key) /
/// [`same_table`](Self::same_table), which fold by a dialect's
/// [`IdentifierCasing`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TableReference {
    #[cfg_attr(
        feature = "serde",
        serde(serialize_with = "crate::serde_support::opt_ident")
    )]
    pub catalog: Option<Ident>,
    #[cfg_attr(
        feature = "serde",
        serde(serialize_with = "crate::serde_support::opt_ident")
    )]
    pub schema: Option<Ident>,
    #[cfg_attr(
        feature = "serde",
        serde(serialize_with = "crate::serde_support::ident")
    )]
    pub name: Ident,
}

/// One read-side occurrence of a [`TableReference`], pairing the
/// identity with how the resolver resolved it ([`ResolutionKind`]).
///
/// The table-granularity mirror of [`ColumnRead`]. Read-side surfaces
/// ([`TableOperation::reads`] and [`TableLineageEdge::source`]) use this
/// wrapper so each occurrence can carry resolution metadata while
/// [`TableReference`] stays identity-only. Write-side surfaces
/// ([`TableOperation::writes`], [`TableLineageEdge::target`]) stay bare
/// [`TableReference`] — write targets come straight from SQL syntax and
/// are trivially resolved by construction.
///
/// Unlike [`ColumnRead`], `reference` is **always present**: a table's
/// name is written out in the SQL, so even an
/// [`Ambiguous`](ResolutionKind::Ambiguous) table read (the catalog
/// holds several tables matching an under-qualified name) still surfaces
/// the reference as written. [`Unresolved`](ResolutionKind::Unresolved)
/// therefore never arises at table granularity — it is columns-only.
/// The resolution records how the catalog matched the table:
/// [`Cataloged`](ResolutionKind::Cataloged) for a unique registered hit,
/// [`Ambiguous`](ResolutionKind::Ambiguous) for several, and
/// [`Inferred`](ResolutionKind::Inferred) for a catalog miss or
/// catalog-less mode.
///
/// [`TableOperation::reads`]: crate::extractor::TableOperation::reads
/// [`TableOperation::writes`]: crate::extractor::TableOperation::writes
/// [`TableLineageEdge::source`]: crate::extractor::TableLineageEdge::source
/// [`TableLineageEdge::target`]: crate::extractor::TableLineageEdge::target
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TableRead {
    pub reference: TableReference,
    pub resolution: ResolutionKind,
}

/// One write-side occurrence of a [`TableReference`] — a DML / DDL write
/// target — pairing the identity with how the catalog matched it
/// ([`ResolutionKind`]).
///
/// The write-role counterpart of [`TableRead`], kept a distinct type so a
/// read can't be passed where a write is meant (and so the write side can
/// diverge later). The `resolution` carries the same catalog-match outcome a
/// scanned source would: [`Cataloged`](ResolutionKind::Cataloged) for a unique
/// registered hit, [`Ambiguous`](ResolutionKind::Ambiguous) for several, and
/// [`Inferred`](ResolutionKind::Inferred) for a catalog miss or catalog-less
/// mode — so the [`Cataloged`](ResolutionKind::Cataloged)-detects-catalog-aware
/// invariant holds on writes too. `reference` is always present (a target's
/// name is written out), so [`Unresolved`](ResolutionKind::Unresolved) never
/// arises here, exactly as for [`TableRead`].
///
/// [`TableOperation::writes`]: crate::extractor::TableOperation::writes
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TableWrite {
    pub reference: TableReference,
    pub resolution: ResolutionKind,
}

/// A column-level identity reference: an optional owning table plus the
/// column name.
///
/// `table` is `Option` because a column the resolver couldn't pin to a
/// single owning table — [`Ambiguous`](ResolutionKind::Ambiguous) or
/// [`Unresolved`](ResolutionKind::Unresolved) (see
/// [`ColumnRead::resolution`] for *why*) — still surfaces its name with
/// `table: None`. Identity is name-based: two `ColumnReference`s with the
/// same `table` and `name` compare equal, independent of where they
/// appeared in the SQL or how the resolver placed them. (For dialect-aware
/// equality, see [`identity_key`](Self::identity_key).)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ColumnReference {
    pub table: Option<TableReference>,
    #[cfg_attr(
        feature = "serde",
        serde(serialize_with = "crate::serde_support::ident")
    )]
    pub name: Ident,
}

/// One read-side occurrence of a [`ColumnReference`], pairing the
/// identity with how the resolver resolved it ([`ResolutionKind`]).
///
/// Read-side surfaces ([`ColumnOperation::reads`] and
/// [`ColumnLineageEdge::source`]) use this wrapper so the same column
/// referenced twice can carry per-occurrence resolution metadata
/// without breaking [`ColumnReference`]'s identity-only contract.
/// Write-side surfaces ([`ColumnOperation::writes`],
/// [`ColumnTarget::Relation`]) stay as bare [`ColumnReference`] —
/// targets come straight from SQL syntax and are always
/// [`ResolutionKind::Cataloged`]-or-trivially-resolved by construction,
/// so the field would be dead weight there.
///
/// [`ColumnOperation::reads`]: crate::extractor::ColumnOperation::reads
/// [`ColumnOperation::writes`]: crate::extractor::ColumnOperation::writes
/// [`ColumnLineageEdge::source`]: crate::extractor::ColumnLineageEdge::source
/// [`ColumnTarget::Relation`]: crate::extractor::ColumnTarget::Relation
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ColumnRead {
    pub reference: ColumnReference,
    pub resolution: ResolutionKind,
}

/// How a reference was resolved — "what kind of resolution backs this
/// `(table, name)` placement?".
///
/// Catalog-less mode runs as an *inference mode*: every real-table
/// binding's schema is unknown, so a single-candidate resolution
/// is best-effort, not catalog-backed. CTE and derived bodies do carry
/// known schemas (the resolver derives them from the body's
/// projection), but those refs are synthetic and dropped from the
/// public reads / lineage by the resolver's post-pass.
///
/// `Ambiguous` and `Unresolved` are the two failure modes. Both come
/// with `table: None` on the [`ColumnReference`]; the variant tells
/// the consumer *why* the resolver gave up. (`Unresolved` arises only
/// for columns — a table reference always has a name present.)
///
/// # Invariants
///
/// - **Catalog-less mode → no public `Cataloged`**: every surviving
///   non-synthetic ref points at an unknown real table, so the
///   strongest claim the resolver can make is
///   [`Inferred`](Self::Inferred). Catalog-aware analysis is
///   therefore detectable by the presence of `Cataloged`.
/// - **Catalog-aware mode does not imply `Cataloged`**: catalogs are
///   often partial. Refs against tables the catalog doesn't cover,
///   or against a real unknown table that won a multi-candidate
///   tiebreaker over known ones, both still come back as
///   [`Inferred`](Self::Inferred).
///
/// # How each variant arises
///
/// | Situation | ResolutionKind |
/// |---|---|
/// | catalog-less, real unknown table, sole candidate | [`Inferred`](Self::Inferred) |
/// | catalog-less, two real unknown tables in scope | [`Ambiguous`](Self::Ambiguous) |
/// | catalog-less, CTE known body confirms the column | (internal `Cataloged`; synthetic, dropped) |
/// | catalog-less, CTE known body denies the column (`SELECT typo FROM cte` where cte = `[id]`) | [`Unresolved`](Self::Unresolved) |
/// | catalog-aware, known binding lists the column | [`Cataloged`](Self::Cataloged) |
/// | catalog-aware, known binding *doesn't* list the column | [`Unresolved`](Self::Unresolved) |
/// | catalog-aware, one known confirms + one unknown suspect (known-witness-over-unknown-suspects) | [`Inferred`](Self::Inferred) |
/// | catalog-aware, two or more known schemas confirm | [`Ambiguous`](Self::Ambiguous) |
/// | qualified `t.col` where `t` is unknown | [`Inferred`](Self::Inferred) |
/// | qualified `t.col` where `t` is known and lists `col` | [`Cataloged`](Self::Cataloged) |
///
/// # Consumer guidance
///
/// - **Strict mode validation**: a fully resolved, catalog-confirmed
///   statement satisfies
///   `op.diagnostics.is_empty() && op.reads.iter().all(|r| r.resolution == ResolutionKind::Cataloged)`.
/// - **DFD / CRUD comprehension**: treat
///   [`Cataloged`](Self::Cataloged) and [`Inferred`](Self::Inferred)
///   interchangeably as "resolved" (use the `(table, name)` pair);
///   treat [`Ambiguous`](Self::Ambiguous) and
///   [`Unresolved`](Self::Unresolved) as "incomplete".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum ResolutionKind {
    /// Backed by a known schema that lists the column / names the
    /// table. On the public surface this means a catalog (or registry)
    /// entry backed the reference. Internally a CTE / derived body's
    /// known schema also yields this variant on a synthetic ref, but
    /// the post-pass drops those — so consumers only ever see
    /// `Cataloged` for catalog-backed real references.
    Cataloged,
    /// Resolution succeeded by assuming the reference exists where the
    /// resolver placed it: an unknown-schema binding adopted as the
    /// sole candidate, a qualified reference whose qualifier alone
    /// determined the table, or a known witness winning over
    /// unknown suspects in a multi-candidate scope. All defensible
    /// inferences in catalog-less or partial-catalog mode, but not
    /// proven.
    Inferred,
    /// Multiple plausible candidates and the resolver couldn't pick
    /// one: either two-or-more known schemas confirmed the column
    /// (genuine ambiguity), or every candidate was an unknown
    /// suspect with no tiebreaker. `ColumnReference.table` is `None`.
    Ambiguous,
    /// No in-scope binding could plausibly own the column: either
    /// every known schema in scope explicitly denied it, or the
    /// scope chain held no bindings at all. `ColumnReference.table`
    /// is `None`. Columns only.
    Unresolved,
}

impl TableReference {
    pub(crate) fn try_from_name(name: &ObjectName) -> Result<Self, Error> {
        // Every part must be a plain identifier. A non-identifier part — e.g.
        // Snowflake's `IDENTIFIER('t')`, a function-computed name — makes the
        // reference unrepresentable; `as_ident` is `None` there, so the
        // all-or-nothing `collect` yields `None` and we return `Err` rather
        // than `unwrap`-panicking (callers drop it best-effort).
        let parts = name
            .0
            .iter()
            .map(|part| part.as_ident())
            .collect::<Option<Vec<&Ident>>>()
            .ok_or_else(|| {
                Error::AnalysisError(format!(
                    "table name `{name}` is not a plain identifier path"
                ))
            })?;
        match parts.as_slice() {
            [] => Err(Error::AnalysisError(
                "ObjectName has no identifiers".to_string(),
            )),
            [n] => Ok(TableReference {
                catalog: None,
                schema: None,
                name: (*n).clone(),
            }),
            [schema, n] => Ok(TableReference {
                catalog: None,
                schema: Some((*schema).clone()),
                name: (*n).clone(),
            }),
            [catalog, schema, n] => Ok(TableReference {
                catalog: Some((*catalog).clone()),
                schema: Some((*schema).clone()),
                name: (*n).clone(),
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

impl fmt::Display for ColumnReference {
    /// `table.column` when the owning table is known (the table renders as
    /// its own [`TableReference`] path), otherwise just `column`. Mirrors
    /// [`TableReference`]'s `Display` for the column-identity case.
    ///
    /// ```rust
    /// use sql_insight::{ColumnReference, TableReference};
    ///
    /// let qualified = ColumnReference {
    ///     table: Some(TableReference {
    ///         catalog: None,
    ///         schema: Some("public".into()),
    ///         name: "users".into(),
    ///     }),
    ///     name: "id".into(),
    /// };
    /// assert_eq!(qualified.to_string(), "public.users.id");
    ///
    /// let bare = ColumnReference { table: None, name: "id".into() };
    /// assert_eq!(bare.to_string(), "id");
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.table {
            Some(table) => write!(f, "{table}.{}", self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

/// An opaque, dialect-aware identity key for a [`TableReference`].
///
/// Two references whose keys are equal denote the same table *under the
/// given dialect's case-folding* — e.g. `users` and `USERS` share a key in
/// PostgreSQL, but not in a case-sensitive dialect. Use it to deduplicate
/// references catalog-free, where the structural `Eq` / `Hash` on
/// `TableReference` (case-sensitive, quote-sensitive) would over-count
/// fold-equivalent spellings. (With a catalog, matched references are
/// already canonicalized, so structural dedup suffices.)
///
/// The key is **identity**, not wildcard matching: every present segment is
/// significant, so a bare `users` and a qualified `public.users` have
/// *different* keys (they are different identities). The folded text is not
/// observable — only equality / hashing.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TableIdentityKey {
    catalog: Option<String>,
    schema: Option<String>,
    name: String,
}

/// An opaque, dialect-aware identity key for a [`ColumnReference`] — the
/// [`TableIdentityKey`] of its owning table (if any, folded by the table
/// rule) plus the column name folded by the column rule. See
/// [`TableIdentityKey`] for the identity-vs-matching and opacity notes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColumnIdentityKey {
    table: Option<TableIdentityKey>,
    name: String,
}

impl TableReference {
    /// The dialect-aware [`TableIdentityKey`] for this reference: each
    /// segment folded by `casing`'s table rule. Equal keys denote the same
    /// table under that dialect's casing.
    ///
    /// This is the **catalog-free dedup key**. The structural `Eq` on
    /// `TableReference` is exact (case- and quote-sensitive), so without a
    /// catalog to canonicalize spellings it over-counts `users` and `USERS`
    /// as two tables; folding by the dialect's casing collapses them.
    ///
    /// ```rust
    /// use std::collections::HashSet;
    /// use sql_insight::{CaseRule, IdentifierCasing, TableReference};
    ///
    /// let users = TableReference { catalog: None, schema: None, name: "users".into() };
    /// let upper = TableReference { catalog: None, schema: None, name: "USERS".into() };
    ///
    /// // Structural equality is exact — these read as two different tables.
    /// assert_ne!(users, upper);
    ///
    /// // Under a case-folding dialect (here lower-folding, e.g. PostgreSQL)
    /// // they share one identity.
    /// let casing = IdentifierCasing::uniform(CaseRule::Lower);
    /// assert!(users.same_table(&upper, &casing));
    ///
    /// // So a fold-keyed set counts the table once, where a structural
    /// // `HashSet<TableReference>` would count two.
    /// let distinct: HashSet<_> = [&users, &upper]
    ///     .iter()
    ///     .map(|t| t.identity_key(&casing))
    ///     .collect();
    /// assert_eq!(distinct.len(), 1);
    ///
    /// // Identity, not wildcard: a bare name and a schema-qualified one stay
    /// // distinct (different identities, not a prefix match).
    /// let qualified = TableReference {
    ///     catalog: None,
    ///     schema: Some("public".into()),
    ///     name: "users".into(),
    /// };
    /// assert!(!users.same_table(&qualified, &casing));
    /// ```
    pub fn identity_key(&self, casing: &IdentifierCasing) -> TableIdentityKey {
        let fold = |ident: &Ident| casing.table.normalize(ident);
        TableIdentityKey {
            catalog: self.catalog.as_ref().map(&fold),
            schema: self.schema.as_ref().map(&fold),
            name: fold(&self.name),
        }
    }

    /// Whether `self` and `other` denote the same table under `casing` —
    /// equivalent to comparing their [`identity_key`](Self::identity_key)s.
    pub fn same_table(&self, other: &Self, casing: &IdentifierCasing) -> bool {
        self.identity_key(casing) == other.identity_key(casing)
    }
}

impl ColumnReference {
    /// The dialect-aware [`ColumnIdentityKey`] for this reference: the
    /// owning table folded by the table rule, the column name by the column
    /// rule. Equal keys denote the same column under that dialect's casing.
    ///
    /// Like [`TableReference::identity_key`] this is the catalog-free dedup
    /// key — folding both the owning table and the column name (by their
    /// separate rules, which a dialect can set apart). The owning table is
    /// part of the identity: same column name, different table → different
    /// column.
    ///
    /// ```rust
    /// use sql_insight::{CaseRule, ColumnReference, IdentifierCasing, TableReference};
    ///
    /// let owned = |t: &str| Some(TableReference { catalog: None, schema: None, name: t.into() });
    /// let lower = ColumnReference { table: owned("users"), name: "id".into() };
    /// let upper = ColumnReference { table: owned("USERS"), name: "ID".into() };
    ///
    /// // Structural equality is exact; a case-folding casing merges them.
    /// assert_ne!(lower, upper);
    /// let casing = IdentifierCasing::uniform(CaseRule::Insensitive);
    /// assert!(lower.same_column(&upper, &casing));
    ///
    /// // A different owning table is a different column, same name or not.
    /// let other = ColumnReference { table: owned("accounts"), name: "id".into() };
    /// assert!(!lower.same_column(&other, &casing));
    /// ```
    pub fn identity_key(&self, casing: &IdentifierCasing) -> ColumnIdentityKey {
        ColumnIdentityKey {
            table: self.table.as_ref().map(|t| t.identity_key(casing)),
            name: casing.column.normalize(&self.name),
        }
    }

    /// Whether `self` and `other` denote the same column under `casing` —
    /// equivalent to comparing their [`identity_key`](Self::identity_key)s.
    pub fn same_column(&self, other: &Self, casing: &IdentifierCasing) -> bool {
        self.identity_key(casing) == other.identity_key(casing)
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
