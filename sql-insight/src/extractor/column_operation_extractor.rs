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

use crate::catalog::Catalog;
use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::error::Error;
use crate::extractor::table_operation_extractor::StatementKind;
use crate::reference::{ColumnReference, TableReference};
use crate::resolver::{LineageTargetSpec, RawColumnRef, Resolution, Resolver};
use sqlparser::ast::{
    AlterTableOperation, AssignmentTarget, Ident, OnConflictAction, OnInsert, Statement,
    TableFactor,
};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to extract column-level operations from SQL.
///
/// `catalog` is consulted for relation-level enrichment as well as
/// future column-level needs (`SELECT *` expansion, ambiguous
/// unqualified column resolution). Pass `None` for the lightest path —
/// the MVP does not consult the catalog yet, but the signature is fixed
/// so callers don't have to migrate when it does.
///
/// ## Example
///
/// ```rust
/// use sql_insight::sqlparser::dialect::GenericDialect;
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
/// assert_eq!(ops.reads.len(), 1);
/// let read = &ops.reads[0];
/// assert_eq!(read.name.value, "a");
/// assert_eq!(read.table.as_ref().unwrap().name.value, "t1");
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
    catalog: Option<&dyn Catalog>,
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
    /// Columns read by the statement, in walk order. Occurrence-based:
    /// a column referenced more than once appears more than once
    /// (e.g. `SELECT a FROM t WHERE a > 0` yields `t.a` twice). A
    /// consumer wanting the distinct set dedups via a `HashSet`.
    pub reads: Vec<ColumnReference>,
    /// Columns written by the statement, in walk order. Occurrence-based
    /// like `reads`.
    pub writes: Vec<ColumnReference>,
    pub lineage: Vec<ColumnLineageEdge>,
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
    pub source: ColumnReference,
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

/// Extracts column-level operations from SQL.
#[derive(Default, Debug)]
pub struct ColumnOperationExtractor;

impl ColumnOperationExtractor {
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
        catalog: Option<&dyn Catalog>,
    ) -> Result<Vec<Result<ColumnOperation, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        Ok(statements
            .iter()
            .map(|s| Self::extract_from_statement(s, catalog))
            .collect())
    }

    pub fn extract_from_statement(
        statement: &Statement,
        catalog: Option<&dyn Catalog>,
    ) -> Result<ColumnOperation, Error> {
        let kind = super::table_operation_extractor::classify_statement(statement);
        let resolution = Resolver::resolve_statement(catalog, statement)?;

        // Start from resolver-level diagnostics; extractor adds its own
        // only when classify_statement detects an unsupported case the
        // resolver did not already report.
        let mut diagnostics = resolution.diagnostics.clone();

        if matches!(kind, StatementKind::Unsupported) {
            if !diagnostics
                .iter()
                .any(|d| matches!(d.kind, ColumnLevelDiagnosticKind::UnsupportedStatement))
            {
                diagnostics.push(ColumnLevelDiagnostic {
                    kind: ColumnLevelDiagnosticKind::UnsupportedStatement,
                    message: format!(
                        "Unsupported statement for column operation extraction: {}",
                        statement
                    ),
                    span: None,
                });
            }
            return Ok(ColumnOperation {
                statement_kind: kind,
                reads: Vec::new(),
                writes: Vec::new(),
                lineage: Vec::new(),
                diagnostics,
            });
        }

        let reads = collect_reads(&resolution);
        let writes = collect_writes(statement, &resolution)?;
        let lineage = extract_lineage(&resolution);

        Ok(ColumnOperation {
            statement_kind: kind,
            reads,
            writes,
            lineage,
            diagnostics,
        })
    }
}

/// Map the resolver's pre-built `lineage_edges` 1:1 to public
/// `ColumnLineageEdge`. Sources go through scope-chain resolution; targets
/// are already fully spec'd by the resolver.
fn extract_lineage(resolution: &Resolution) -> Vec<ColumnLineageEdge> {
    resolution
        .lineage_edges
        .iter()
        .filter_map(|edge| {
            let source = resolve_raw_ref(&edge.source)?;
            let target = match &edge.target {
                LineageTargetSpec::QueryOutput { name, position } => ColumnTarget::QueryOutput {
                    name: name.clone(),
                    position: *position,
                },
                LineageTargetSpec::Relation { table, column } => {
                    ColumnTarget::Relation(ColumnReference {
                        table: Some(table.clone()),
                        name: column.clone(),
                    })
                }
            };
            Some(ColumnLineageEdge {
                source,
                target,
                kind: edge.kind,
            })
        })
        .collect()
}

/// Build a `ColumnReference` from a resolver-captured raw ref. The
/// resolver records owning-table resolution at walk time, so this is
/// a 1:1 read of `(resolved, parts.last())`. Refs whose owning
/// binding was synthetic at walk time are dropped upstream by the
/// resolver itself before they reach the extractor — see
/// `Resolution::real_column_refs`.
fn resolve_raw_ref(raw: &RawColumnRef) -> Option<ColumnReference> {
    let name = raw.parts.last()?.clone();
    Some(ColumnReference {
        table: raw.resolved.clone(),
        name,
    })
}

fn collect_reads(resolution: &Resolution) -> Vec<ColumnReference> {
    resolution
        .column_refs
        .iter()
        .filter_map(resolve_raw_ref)
        .collect()
}

/// Build a `ColumnReference` from a `CompoundIdentifier`'s parts —
/// used by UPDATE SET target parsing where the target's qualifier
/// hasn't been resolver-walked. The last part is the column name;
/// preceding parts decode into `TableReference` by length (1 / 2 / 3).
fn column_ref_from_parts(parts: &[Ident]) -> Option<ColumnReference> {
    let (col, table_parts) = match parts.split_last() {
        Some((col, rest)) if !rest.is_empty() => (col.clone(), rest),
        _ => return None,
    };
    let table = match table_parts.len() {
        1 => TableReference {
            catalog: None,
            schema: None,
            name: table_parts[0].clone(),
        },
        2 => TableReference {
            catalog: None,
            schema: Some(table_parts[0].clone()),
            name: table_parts[1].clone(),
        },
        3 => TableReference {
            catalog: Some(table_parts[0].clone()),
            schema: Some(table_parts[1].clone()),
            name: table_parts[2].clone(),
        },
        _ => return None,
    };
    Some(ColumnReference {
        table: Some(table),
        name: col,
    })
}

/// Statement-specific write extraction. Covered:
/// - INSERT explicit column list → writes scoped to the INSERT target.
/// - UPDATE SET targets → writes scoped to the UPDATE target table
///   (qualifier is honored when the SET target is qualified, otherwise
///   the UPDATE head provides the table).
/// - CTAS / CREATE VIEW / ALTER VIEW → writes follow the created
///   relation's columns (explicit list when given, otherwise the
///   columns the resolver derived from the source projection — read
///   off the resolution's `Relation` lineage edges to that target).
///
/// MERGE WHEN clause writes are deferred.
fn collect_writes(
    statement: &Statement,
    resolution: &Resolution,
) -> Result<Vec<ColumnReference>, Error> {
    // `WITH cte AS (...) <DML>` parses as a top-level `Statement::Query`
    // wrapping a `SetExpr::{Insert|Update|Delete|Merge}` around the
    // real DML statement. Unwrap that here so writes follow the inner
    // verb, matching what `classify_statement` already does for kind.
    if let Statement::Query(query) = statement {
        use sqlparser::ast::SetExpr;
        if let SetExpr::Insert(inner)
        | SetExpr::Update(inner)
        | SetExpr::Delete(inner)
        | SetExpr::Merge(inner) = query.body.as_ref()
        {
            return collect_writes(inner, resolution);
        }
    }
    let mut writes = Vec::new();
    match statement {
        Statement::Insert(insert) => {
            let target = TableReference::try_from(insert)?;
            if !insert.columns.is_empty() {
                for col in &insert.columns {
                    writes.push(ColumnReference {
                        table: Some(target.clone()),
                        name: col.clone(),
                    });
                }
            } else {
                // INSERT without an explicit column list — when the
                // catalog provided the target schema, the resolver
                // emitted Relation lineage to each paired column. Read
                // those off to surface the implicit writes.
                writes.extend(relation_target_writes(&target, resolution));
            }
            // ON CONFLICT DO UPDATE SET / ON DUPLICATE KEY UPDATE
            // assignment targets become writes too — each SET column
            // is updated on conflict, same role as a standalone UPDATE
            // SET target.
            writes.extend(insert_on_action_writes(insert, &target));
        }
        Statement::Update(update) => {
            let default_table = match &update.table.relation {
                TableFactor::Table { .. } => {
                    Some(TableReference::try_from(&update.table.relation)?)
                }
                _ => None,
            };
            for assignment in &update.assignments {
                if let Some(column) =
                    column_ref_from_assignment_target(&assignment.target, default_table.as_ref())
                {
                    writes.push(column);
                }
            }
        }
        // Only CTAS (`CREATE TABLE ... AS query`) writes data; plain
        // `CREATE TABLE t (a INT, ...)` is pure DDL and falls through to
        // the no-op arm below.
        Statement::CreateTable(ct) if ct.query.is_some() => {
            let target = TableReference::try_from(&ct.name)?;
            let explicit: Vec<Ident> = ct.columns.iter().map(|c| c.name.clone()).collect();
            writes.extend(created_writes(&target, &explicit, resolution));
        }
        Statement::CreateView(cv) => {
            let target = TableReference::try_from(&cv.name)?;
            let explicit: Vec<Ident> = cv.columns.iter().map(|c| c.name.clone()).collect();
            writes.extend(created_writes(&target, &explicit, resolution));
        }
        Statement::AlterView { name, columns, .. } => {
            let target = TableReference::try_from(name)?;
            writes.extend(created_writes(&target, columns, resolution));
        }
        Statement::AlterTable(alter) => {
            let target = TableReference::try_from(&alter.name)?;
            for op in &alter.operations {
                for col_name in alter_table_op_target_columns(op) {
                    writes.push(ColumnReference {
                        table: Some(target.clone()),
                        name: col_name,
                    });
                }
            }
        }
        Statement::Merge(merge) => {
            use sqlparser::ast::MergeAction;
            let target = match &merge.table {
                TableFactor::Table { .. } => TableReference::try_from(&merge.table).ok(),
                _ => None,
            };
            for clause in &merge.clauses {
                match &clause.action {
                    MergeAction::Insert(insert_expr) => {
                        let Some(target) = &target else { continue };
                        for col_obj in &insert_expr.columns {
                            let Some(ident) = col_obj.0.last().and_then(|p| p.as_ident()) else {
                                continue;
                            };
                            writes.push(ColumnReference {
                                table: Some(target.clone()),
                                name: ident.clone(),
                            });
                        }
                    }
                    MergeAction::Update(update_expr) => {
                        for assignment in &update_expr.assignments {
                            if let Some(column) = column_ref_from_assignment_target(
                                &assignment.target,
                                target.as_ref(),
                            ) {
                                writes.push(column);
                            }
                        }
                    }
                    MergeAction::Delete { .. } => {}
                }
            }
        }
        _ => {}
    }
    Ok(writes)
}

/// Writes for a CREATE-as-style target: when an explicit column list
/// is given, use it verbatim; otherwise delegate to
/// [`relation_target_writes`] to recover the columns from the
/// resolver's lineage edges.
fn created_writes(
    target: &TableReference,
    explicit: &[Ident],
    resolution: &Resolution,
) -> Vec<ColumnReference> {
    if !explicit.is_empty() {
        return explicit
            .iter()
            .map(|c| ColumnReference {
                table: Some(target.clone()),
                name: c.clone(),
            })
            .collect();
    }
    relation_target_writes(target, resolution)
}

/// Scan the resolution's `Relation` lineage edges for any pointing at
/// `target`, returning a deduped `ColumnWrite` per unique column
/// name. Used by both CREATE-as-style writes derivation and INSERT
/// without an explicit column list (where the catalog-provided
/// schema let the resolver pair source projections positionally).
fn relation_target_writes(
    target: &TableReference,
    resolution: &Resolution,
) -> Vec<ColumnReference> {
    let mut seen: Vec<Ident> = Vec::new();
    for edge in &resolution.lineage_edges {
        if let LineageTargetSpec::Relation { table, column } = &edge.target {
            if table == target && !seen.iter().any(|n| n.value == column.value) {
                seen.push(column.clone());
            }
        }
    }
    seen.into_iter()
        .map(|name| ColumnReference {
            table: Some(target.clone()),
            name,
        })
        .collect()
}

/// Extract the column names an ALTER TABLE operation writes to.
/// Schema-level changes (AddConstraint, DropConstraint, partition /
/// projection ops, RENAME TABLE, etc.) return empty — they don't
/// affect named columns. Rename / change return BOTH the old and new
/// names so the lineage surface records both ends of the rename.
fn alter_table_op_target_columns(op: &AlterTableOperation) -> Vec<Ident> {
    match op {
        AlterTableOperation::AddColumn { column_def, .. } => vec![column_def.name.clone()],
        AlterTableOperation::DropColumn { column_names, .. } => column_names.clone(),
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => vec![old_column_name.clone(), new_column_name.clone()],
        AlterTableOperation::ChangeColumn {
            old_name, new_name, ..
        } => {
            if old_name == new_name {
                vec![old_name.clone()]
            } else {
                vec![old_name.clone(), new_name.clone()]
            }
        }
        AlterTableOperation::ModifyColumn { col_name, .. } => vec![col_name.clone()],
        AlterTableOperation::AlterColumn { column_name, .. } => vec![column_name.clone()],
        _ => Vec::new(),
    }
}

/// Surface ON CONFLICT DO UPDATE SET / ON DUPLICATE KEY UPDATE
/// assignment targets as writes on the INSERT target table.
/// Returns an empty `Vec` when the INSERT carries no on-clause, or
/// when the on-clause is `DO NOTHING` (no SET targets to surface).
fn insert_on_action_writes(
    insert: &sqlparser::ast::Insert,
    target: &TableReference,
) -> Vec<ColumnReference> {
    let assignments: &[sqlparser::ast::Assignment] = match insert.on.as_ref() {
        Some(OnInsert::DuplicateKeyUpdate(a)) => a,
        Some(OnInsert::OnConflict(c)) => match &c.action {
            OnConflictAction::DoUpdate(do_update) => &do_update.assignments,
            OnConflictAction::DoNothing => return Vec::new(),
        },
        // `OnInsert` is `#[non_exhaustive]` — unknown variants
        // surface no writes until we model them explicitly.
        Some(_) => return Vec::new(),
        None => return Vec::new(),
    };
    assignments
        .iter()
        .filter_map(|a| column_ref_from_assignment_target(&a.target, Some(target)))
        .collect()
}

/// Resolve a SET assignment target to a `ColumnReference`. If the
/// target is qualified (`t1.a`), the qualifier wins; otherwise the
/// `default_table` (the UPDATE head) provides the table.
fn column_ref_from_assignment_target(
    target: &AssignmentTarget,
    default_table: Option<&TableReference>,
) -> Option<ColumnReference> {
    let name = match target {
        AssignmentTarget::ColumnName(name) => name,
        AssignmentTarget::Tuple(_) => return None,
    };
    let idents: Vec<Ident> = name
        .0
        .iter()
        .map(|p| p.as_ident().cloned())
        .collect::<Option<Vec<_>>>()?;
    match idents.len() {
        1 => Some(ColumnReference {
            table: default_table.cloned(),
            name: idents.into_iter().next().unwrap(),
        }),
        2..=4 => column_ref_from_parts(&idents),
        _ => None,
    }
}

#[cfg(test)]
#[path = "column_operation_extractor_tests.rs"]
mod tests;
