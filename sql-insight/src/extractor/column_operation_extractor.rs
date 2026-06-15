//! Extracts the column-level operations a SQL statement performs.
//!
//! Where [`extract_table_operations`](crate::extractor::extract_table_operations)
//! answers "what tables does this statement touch / write / lineage", this
//! module answers the same questions at column granularity.
//!
//! The output mirrors `TableOperation` — three parallel
//! surfaces (`reads`, `writes`, `lineage`) — plus a small enrichment on
//! lineage edges to distinguish passthrough projections from
//! value-changing transformations.
//!
//! **Current coverage** (column tracking is rolling in incrementally):
//! - `reads`: qualified column references decompose directly to
//!   `TableReference + name`; unqualified ones are resolved against
//!   the scope chain at walk time. A unique candidate binding wins;
//!   0 or 2+ candidates leave `table: None` (the column name still
//!   surfaces). References whose walk-time owning binding was a CTE,
//!   derived table, or table function (synthetic relations, not
//!   real storage) are dropped from reads — only references to real
//!   tables or unresolved names surface. `reads` is a plain
//!   occurrence list of `ColumnReference`s in walk order: a column
//!   referenced more than once appears more than once, with no
//!   syntactic clause tag. (Whether a reference contributes a value
//!   or merely influences the result — e.g. a `WHERE` predicate — is
//!   recovered structurally: value contributors are `lineage` sources,
//!   filter-only columns are in `reads` but not `lineage`.)
//! - `writes`: INSERT target columns (explicit list when given;
//!   when omitted and the catalog provides the target's schema,
//!   the columns the resolver paired with source projections via
//!   the catalog), UPDATE SET targets scoped to the UPDATE table,
//!   CTAS / CREATE VIEW / ALTER VIEW target columns (explicit
//!   column list when provided, else the names the resolver derived
//!   from the source projection), and MERGE WHEN-clause writes
//!   (UPDATE SET targets and INSERT column lists, with the same
//!   catalog fallback for column-list-less INSERT).
//! - `lineage`: per-output-column edges for SELECT (target =
//!   `QueryOutput { name, position }`), positionally paired
//!   `source-column → target-column` edges for INSERT (explicit
//!   column list, or — when the catalog provides the target's
//!   schema — the catalog columns; one branch per UNION arm, each
//!   paired against the same target columns), and
//!   per-assignment edges for
//!   UPDATE SET. Sources that reference CTEs or derived tables are
//!   collapsed end-to-end — references recurse through the
//!   synthetic's body projections, so a SELECT through a chain of
//!   CTEs surfaces lineage whose sources are the underlying real
//!   tables. Each edge is tagged with a `ColumnLineageKind`:
//!   `Passthrough` (the value is forwarded unchanged — a bare column
//!   ref, rename included) or `Transformation` (any expression that
//!   changes the value: arithmetic, function calls, aggregates,
//!   window functions, CASE, casts, …). Composition yields
//!   `Transformation` whenever any step in a CTE / derived chain is a
//!   transformation. CTAS / CREATE
//!   VIEW / ALTER VIEW emit `Relation`-target lineage from source
//!   projections to the created relation's columns. MERGE emits
//!   per-clause `Relation`-target lineage for WHEN MATCHED UPDATE
//!   (per assignment) and
//!   WHEN NOT MATCHED INSERT VALUES (positional pair with the INSERT
//!   column list); DELETE actions emit nothing. Column-list-less
//!   INSERT SELECT is deferred.
//!
//! **Strictness scales with the catalog.** Without a catalog, Table
//! bindings have `Unknown` schemas and unqualified refs to a
//! single-table scope resolve unconditionally (best-effort, matches
//! the implicit promise of `catalog: None`). With a catalog, Table
//! schemas come back `Known(cols)` and unqualified refs only resolve
//! when the candidate's schema actually lists the column — column
//! typos that would otherwise silently resolve become unresolved.

use crate::casing::IdentifierCasing;
use crate::catalog::Catalog;
use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::error::Error;
use crate::extractor::{classify_statement, StatementKind};
use crate::reference::{ColumnRead, ColumnReference};
use sqlparser::ast::{Ident, Statement};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to extract column-level operations from SQL.
///
/// `catalog` is an optional schema registry (see [`Catalog`]). When
/// supplied, table references are matched against it (right-anchored,
/// dialect-cased): a
/// unique hit canonicalizes the surfaced identity to the registered
/// path and supplies the column list, turning column resolution strict
/// (a column the schema doesn't list surfaces as
/// [`Unresolved`](crate::ResolutionKind::Unresolved), and an unqualified
/// name two known schemas both declare as
/// [`Ambiguous`](crate::ResolutionKind::Ambiguous)). Pass `None` for
/// best-effort, catalog-free resolution (every resolved read is
/// [`Inferred`](crate::ResolutionKind::Inferred)).
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
/// use sql_insight::ResolutionKind;
/// use sql_insight::extractor::{
///     extract_column_operations, ColumnLineageKind, ColumnTarget, StatementKind,
/// };
///
/// let dialect = GenericDialect {};
/// let result =
///     extract_column_operations(&dialect, "SELECT a FROM t1", None).unwrap();
/// let ops = result[0].as_ref().unwrap();
///
/// // SELECT contributes reads + lineage but no writes.
/// assert_eq!(ops.statement_kind, StatementKind::Select);
/// assert!(ops.writes.is_empty());
///
/// // `t1.a` surfaces as a single read, walk-time resolved to t1.
/// // Catalog-less mode → resolution is `Inferred` (we adopted the
/// // sole `Unknown`-schema candidate without firm evidence).
/// assert_eq!(ops.reads.len(), 1);
/// let read = &ops.reads[0];
/// assert_eq!(read.reference.name.value, "a");
/// assert_eq!(read.reference.table.as_ref().unwrap().name.value, "t1");
/// assert_eq!(read.resolution, ResolutionKind::Inferred);
///
/// // The projection emits one lineage edge into the SELECT's QueryOutput slot,
/// // marked Passthrough (no expression wrapping the column).
/// assert_eq!(ops.lineage.len(), 1);
/// let edge = &ops.lineage[0];
/// assert_eq!(edge.kind, ColumnLineageKind::Passthrough);
/// match &edge.target {
///     ColumnTarget::QueryOutput { name, position } => {
///         assert_eq!(name.as_ref().unwrap().value, "a");
///         assert_eq!(*position, 0);
///     }
///     other => panic!("expected QueryOutput, got {other:?}"),
/// }
/// ```
pub fn extract_column_operations(
    dialect: &dyn Dialect,
    sql: &str,
    catalog: Option<&Catalog>,
) -> Result<Vec<Result<ColumnOperation, Error>>, Error> {
    ColumnOperationExtractor::extract(dialect, sql, catalog)
}

/// Column-level operations performed by a single SQL statement.
///
/// Mirrors [`TableOperation`](crate::extractor::TableOperation)
/// with the same three surfaces — `reads`, `writes`, `lineage` — at
/// column granularity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnOperation {
    pub statement_kind: StatementKind,
    /// Columns read by the statement. Occurrence-based: a column
    /// referenced more than once appears more than once (e.g.
    /// `SELECT a FROM t WHERE a > 0` yields `t.a` twice). Each entry pairs
    /// the [`ColumnReference`] identity with its
    /// [`ResolutionKind`](crate::ResolutionKind). **Order is not
    /// contractual** — it reflects an internal traversal and may change
    /// between versions; occurrence count is preserved, and each entry
    /// carries its source span, so a consumer wanting source-text order
    /// sorts by `reference.name.span` and one wanting the distinct identity
    /// set dedups `reads.iter().map(|r| &r.reference)` via a `HashSet`.
    pub reads: Vec<ColumnRead>,
    /// Columns written by the statement, in source (column-list) order.
    /// Occurrence-based like `reads`. Write targets come straight from SQL
    /// syntax and are always `ResolutionKind::Cataloged` by construction,
    /// so the resolution field is elided here.
    pub writes: Vec<ColumnReference>,
    /// Lineage edges. Statements that physically move data emit collapsed
    /// end-to-end edges (source → `ColumnTarget::Relation`); a bare
    /// `SELECT` emits source → `ColumnTarget::QueryOutput` edges. **Order
    /// is not contractual** (occurrence / multiplicity is preserved);
    /// consumers compare as a set / multiset.
    pub lineage: Vec<ColumnLineageEdge>,
    /// Column-level diagnostics: wildcard suppression plus the
    /// `UnsupportedStatement` projection inherited from table
    /// granularity. Per-reference resolution outcomes surface on
    /// `reads[i].resolution` instead.
    pub diagnostics: Vec<ColumnLevelDiagnostic>,
}

/// A column-level lineage edge: data from `source` contributes to
/// `target`. Emitted for both relation-target statements (INSERT /
/// UPDATE / MERGE / CTAS / CREATE VIEW, target = `ColumnTarget::Relation`)
/// and bare SELECT (target = `ColumnTarget::QueryOutput`).
///
/// One edge per (source, target) pair: `SELECT a + b FROM t1` emits two
/// edges, from `t1.a` and `t1.b` to the same query-output target, each
/// tagged `Transformation`.
///
/// Statements that physically move data emit collapsed end-to-end lineage
/// — `INSERT INTO t1 (col) SELECT b FROM t2` emits `t2.b → t1.col`
/// directly, with no intermediate query-output entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnLineageEdge {
    /// The column the lineage edge flows from, paired with the
    /// resolver's [`ResolutionKind`](crate::ResolutionKind) in that placement.
    /// `source.reference` is the inner (post-collapse) real-table
    /// reference; `source.resolution` follows that inner reference's
    /// classification rather than the outer synthetic step's.
    pub source: ColumnRead,
    pub target: ColumnTarget,
    pub kind: ColumnLineageKind,
}

/// The target endpoint of a [`ColumnLineageEdge`].
///
/// `Relation` covers columns that live in a named relation — a table
/// or a view, both modelled identically as a `table`-qualified
/// `ColumnReference` — and receive a value from the statement (INSERT
/// target, UPDATE SET target, MERGE INSERT/UPDATE target, CTAS / CREATE
/// VIEW output column).
///
/// `QueryOutput` covers transient columns produced by a top-level
/// SELECT projection that is not piped into a named relation. `name`
/// follows the projection: the alias if explicit, the bare column name
/// if the projection is a single column, otherwise `None`. `position`
/// is always set so anonymous outputs can be identified.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ColumnTarget {
    /// A column in a real relation receiving the inbound lineage edge — INSERT /
    /// UPDATE / MERGE target columns, or columns of the new relation
    /// produced by CTAS / CREATE VIEW / ALTER VIEW.
    Relation(ColumnReference),
    /// A transient column produced by a top-level SELECT projection
    /// that is not piped into a named relation. `name` follows
    /// the projection's explicit alias or inferred single-column name
    /// (`None` for expressions without a clear name); `position` is
    /// always set so anonymous outputs remain identifiable.
    QueryOutput {
        name: Option<Ident>,
        position: usize,
    },
}

/// How a source column contributes to its target — the one clean,
/// exclusive distinction: is the value forwarded unchanged, or
/// derived?
///
/// - `Passthrough` — the source value is forwarded unchanged
///   (`SELECT a FROM t1`, `INSERT INTO t1 (a) SELECT b FROM t2`). A
///   rename (`SELECT a AS b`) is still `Passthrough`; detect it by
///   comparing the source `name` to the target `name`.
/// - `Transformation` — the source feeds any expression that changes
///   the value: arithmetic, function calls, CASE branches, casts,
///   aggregates (`SUM`, `STRING_AGG`), window functions, etc.
///
/// Finer sub-classification of `Transformation` (aggregate vs scalar,
/// cardinality, etc.) is deliberately not modelled here — it is lossy
/// for edge cases (window aggregates, value-preserving `STRING_AGG`)
/// and not load-bearing for the core dependency / impact-analysis use
/// case. A finer variant can be added later if a concrete consumer
/// needs it (a breaking change while the crate is pre-1.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ColumnLineageKind {
    /// Source value is forwarded unchanged. Composition stays
    /// `Passthrough` only when every step in the chain is also
    /// `Passthrough`.
    Passthrough,
    /// Source feeds an expression that changes the value. Composition
    /// yields `Transformation` whenever any step in the chain is a
    /// transformation.
    Transformation,
}

/// Struct-style entry point. Equivalent to the free
/// [`extract_column_operations`] function.
#[derive(Default, Debug)]
pub struct ColumnOperationExtractor;

impl ColumnOperationExtractor {
    /// Same as the free [`extract_column_operations`] function — kept
    /// for users who prefer the struct-style API.
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
        catalog: Option<&Catalog>,
    ) -> Result<Vec<Result<ColumnOperation, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        let casing = IdentifierCasing::for_dialect(dialect);
        Ok(statements
            .iter()
            .map(|s| Self::extract_from_statement(s, catalog, casing))
            .collect())
    }

    /// Assemble the column operation from the bound plan: classify the
    /// verb, bind the statement, and walk the plan for `reads` / `writes` /
    /// `lineage`. A kind the binder can't model yields an empty operation
    /// with an `UnsupportedStatement` diagnostic; a supported but
    /// structure-only kind (e.g. `DROP`) is empty without a diagnostic.
    fn extract_from_statement(
        statement: &Statement,
        catalog: Option<&Catalog>,
        casing: IdentifierCasing,
    ) -> Result<ColumnOperation, Error> {
        let statement_kind = classify_statement(statement);
        if statement_kind == StatementKind::Unsupported {
            return Ok(unsupported_column_operation(statement_kind, statement));
        }
        let (plan, diagnostics) = crate::resolver::build_plan(statement, catalog, casing);
        Ok(ColumnOperation {
            statement_kind,
            reads: crate::resolver::extract_reads(&plan),
            writes: crate::resolver::extract_writes(&plan),
            lineage: crate::resolver::extract_lineage(&plan),
            // The bind accumulates `WildcardSuppressed` / `TooManyTableQualifiers`.
            diagnostics,
        })
    }
}

fn unsupported_column_operation(
    statement_kind: StatementKind,
    statement: &Statement,
) -> ColumnOperation {
    ColumnOperation {
        statement_kind,
        reads: Vec::new(),
        writes: Vec::new(),
        lineage: Vec::new(),
        diagnostics: vec![ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::UnsupportedStatement,
            message: format!("Unsupported statement for plan-based extraction: {statement}"),
            span: None,
        }],
    }
}

#[cfg(test)]
#[path = "column_operation_extractor_tests.rs"]
mod tests;
