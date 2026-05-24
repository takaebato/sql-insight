//! Extracts the column-level operations a SQL statement performs.
//!
//! Where [`extract_table_operations`](crate::extract_table_operations)
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
//!   derived table, or table function (synthetic intermediates, not
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
//! - `lineage`: per-projection-item edges for SELECT (target =
//!   `QueryOutput { name, position }`), positionally paired
//!   `source-column → target-column` edges for INSERT (explicit
//!   column list, or — when the catalog provides the target's
//!   schema — the catalog columns; one ProjectionGroup per UNION
//!   branch, each paired against the same target columns), and
//!   per-assignment edges for
//!   UPDATE SET. Sources that reference CTEs or derived tables are
//!   composed end-to-end — references substitute through the
//!   intermediate's body projections recursively, so a SELECT through
//!   a chain of CTEs surfaces lineage whose sources are the underlying
//!   base tables. Each edge is tagged with a `ColumnLineageKind`:
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
/// use sql_insight::{
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
/// Mirrors [`TableOperation`](crate::TableOperation)
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
/// Statements that physically move data emit composed end-to-end lineage
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
mod tests {
    use super::*;
    use sqlparser::dialect::GenericDialect;

    fn extract(sql: &str) -> ColumnOperation {
        let mut result = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
        result.remove(0).unwrap()
    }

    fn table(name: &str) -> TableReference {
        TableReference {
            catalog: None,
            schema: None,
            name: name.into(),
        }
    }

    // reads / writes are now plain `Vec<ColumnReference>` (occurrence
    // based, no clause kind), so all the read/write builders return a
    // `ColumnReference`. `read` and `col` are interchangeable; both are
    // kept for callsite readability (`read` in reads lists, `col` as a
    // lineage source / target inner).
    fn read(table_name: &str, col: &str) -> ColumnReference {
        ColumnReference {
            table: Some(table(table_name)),
            name: col.into(),
        }
    }

    fn write(table_name: &str, col: &str) -> ColumnReference {
        ColumnReference {
            table: Some(table(table_name)),
            name: col.into(),
        }
    }

    fn unresolved(col: &str) -> ColumnReference {
        ColumnReference {
            table: None,
            name: col.into(),
        }
    }

    fn out(name: &str, position: usize) -> ColumnTarget {
        ColumnTarget::QueryOutput {
            name: Some(name.into()),
            position,
        }
    }

    fn out_anon(position: usize) -> ColumnTarget {
        ColumnTarget::QueryOutput {
            name: None,
            position,
        }
    }

    fn relation(table_name: &str, col: &str) -> ColumnTarget {
        ColumnTarget::Relation(ColumnReference {
            table: Some(table(table_name)),
            name: col.into(),
        })
    }

    fn col(table_name: &str, name: &str) -> ColumnReference {
        ColumnReference {
            table: Some(table(table_name)),
            name: name.into(),
        }
    }

    fn passthrough(source: ColumnReference, target: ColumnTarget) -> ColumnLineageEdge {
        ColumnLineageEdge {
            source,
            target,
            kind: ColumnLineageKind::Passthrough,
        }
    }

    fn transformation(source: ColumnReference, target: ColumnTarget) -> ColumnLineageEdge {
        ColumnLineageEdge {
            source,
            target,
            kind: ColumnLineageKind::Transformation,
        }
    }

    /// Whole-value-ish assertion: pin down the full
    /// `ColumnOperation` for `sql`. reads / writes / lineage /
    /// statement_kind compare strictly; diagnostics compare by **kind
    /// sequence only** so message wording and span coordinates aren't
    /// baked into the expected value.
    fn assert_column_ops(sql: &str, expected: ColumnOperation) {
        assert_nth_column_ops(sql, 0, expected);
    }

    /// Like `assert_column_ops` but for multi-statement batches —
    /// targets the statement at `index`. Compose multiple calls to
    /// pin down each statement in a batch independently.
    fn assert_nth_column_ops(sql: &str, index: usize, expected: ColumnOperation) {
        let actual = extract_column_operations(&GenericDialect {}, sql, None)
            .unwrap()
            .into_iter()
            .nth(index)
            .unwrap_or_else(|| panic!("statement {index} missing in result for SQL: {sql}"))
            .unwrap();
        assert_column_ops_inner(sql, index, actual, expected);
    }

    fn assert_column_ops_inner(
        sql: &str,
        index: usize,
        actual: ColumnOperation,
        expected: ColumnOperation,
    ) {
        let ColumnOperation {
            statement_kind,
            reads,
            writes,
            lineage,
            diagnostics,
        } = expected;
        assert_eq!(
            actual.statement_kind, statement_kind,
            "kind for SQL: {sql} (statement {index})"
        );
        assert_eq!(
            actual.reads, reads,
            "reads for SQL: {sql} (statement {index})"
        );
        assert_eq!(
            actual.writes, writes,
            "writes for SQL: {sql} (statement {index})"
        );
        assert_eq!(
            actual.lineage, lineage,
            "lineage for SQL: {sql} (statement {index})"
        );
        let actual_kinds: Vec<_> = actual.diagnostics.iter().map(|d| d.kind.clone()).collect();
        let expected_kinds: Vec<_> = diagnostics.iter().map(|d| d.kind.clone()).collect();
        assert_eq!(
            actual_kinds, expected_kinds,
            "diagnostic kinds for SQL: {sql} (statement {index})"
        );
    }

    /// Placeholder `ColumnLevelDiagnostic` for `assert_column_ops.expected.diagnostics`.
    /// Only the kind is compared; message and span are placeholders.
    fn diag(kind: ColumnLevelDiagnosticKind) -> ColumnLevelDiagnostic {
        ColumnLevelDiagnostic {
            kind,
            message: String::new(),
            span: None,
        }
    }

    mod reads {
        use super::*;

        #[test]
        fn qualified_select_collects_qualified_reads() {
            assert_column_ops(
                "SELECT t1.a, t1.b FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t1", "b"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn qualified_join_collects_reads_from_both_sides() {
            // Resolver walks FROM (including JOIN ON) before the projection,
            // so the predicate columns appear ahead of the projected ones —
            // and are tagged Filter while projection refs are Projection.
            assert_column_ops(
                "SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.id = t2.id",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "id"),
                        read("t2", "id"),
                        read("t1", "a"),
                        read("t2", "b"),
                    ],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t2", "b"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn qualified_ref_through_alias_resolves_to_real_table() {
            // `u` is an alias of `t1`; the qualified ref `u.a` resolves
            // to the alias-free real table `t1`, matching how an
            // unqualified ref resolves. Alias is use-site decoration,
            // not part of the column's identity.
            assert_column_ops(
                "SELECT u.a FROM t1 AS u",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn qualified_refs_through_aliases_on_both_join_sides_resolve_to_real_tables() {
            // Implicit aliases (`t1 a`, `t2 b`) on both join sides; every
            // qualified ref canonicalizes to its real table. JOIN ON is
            // walked during FROM, so the predicate reads precede the
            // projection reads.
            assert_column_ops(
                "SELECT a.x, b.y FROM t1 a JOIN t2 b ON a.id = b.id",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "id"),
                        read("t2", "id"),
                        read("t1", "x"),
                        read("t2", "y"),
                    ],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "x"), out("x", 0)),
                        passthrough(col("t2", "y"), out("y", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aliased_filter_ref_resolves_to_real_table_and_stays_out_of_lineage() {
            // A WHERE-only column through an alias resolves to the real
            // table for `reads`, but a filter column is not a value
            // contributor, so it never appears in `lineage`.
            assert_column_ops(
                "SELECT u.a FROM t1 AS u WHERE u.b > 0",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn schema_qualified_ref_resolves_to_schema_dot_table() {
            let table_ref = TableReference {
                catalog: None,
                schema: Some("s1".into()),
                name: "t1".into(),
            };
            assert_column_ops(
                "SELECT s1.t1.a FROM s1.t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![ColumnReference {
                        table: Some(table_ref.clone()),
                        name: "a".into(),
                    }],
                    writes: vec![],
                    lineage: vec![passthrough(
                        ColumnReference {
                            table: Some(table_ref),
                            name: "a".into(),
                        },
                        out("a", 0),
                    )],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_qualified_ref_resolves_to_catalog_dot_schema_dot_table() {
            // `c1.s1.t1.a` — 4-part ref. parts.last() is the column;
            // the preceding 3 parts decode into TableReference's
            // catalog / schema / name fields.
            let table_ref = TableReference {
                catalog: Some("c1".into()),
                schema: Some("s1".into()),
                name: "t1".into(),
            };
            assert_column_ops(
                "SELECT c1.s1.t1.a FROM c1.s1.t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![ColumnReference {
                        table: Some(table_ref.clone()),
                        name: "a".into(),
                    }],
                    writes: vec![],
                    lineage: vec![passthrough(
                        ColumnReference {
                            table: Some(table_ref),
                            name: "a".into(),
                        },
                        out("a", 0),
                    )],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unqualified_ref_against_catalog_qualified_table_inherits_full_qualifier() {
            // `SELECT a FROM c1.s1.t1` — the unqualified `a` resolves
            // to the catalog-qualified binding, picking up the full
            // qualifier in the ColumnReference.
            let table_ref = TableReference {
                catalog: Some("c1".into()),
                schema: Some("s1".into()),
                name: "t1".into(),
            };
            assert_column_ops(
                "SELECT a FROM c1.s1.t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![ColumnReference {
                        table: Some(table_ref.clone()),
                        name: "a".into(),
                    }],
                    writes: vec![],
                    lineage: vec![passthrough(
                        ColumnReference {
                            table: Some(table_ref),
                            name: "a".into(),
                        },
                        out("a", 0),
                    )],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn five_part_ref_overshoots_qualifier_decoder_and_is_unresolved() {
            // sqlparser parses `extra.c1.s1.t1.a` into 5 parts. The
            // qualifier decoder caps at 3 parts (catalog / schema /
            // name) — anything longer is a struct-field access on a
            // fully qualified column, which we don't model. The ref
            // is recorded with `table: None`.
            assert_column_ops(
                "SELECT extra.c1.s1.t1.a FROM c1.s1.t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![unresolved("a")],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: None,
                            name: "a".into(),
                        },
                        target: out("a", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn where_predicate_qualified_ref_is_a_read() {
            assert_column_ops(
                "SELECT t1.a FROM t1 WHERE t1.b > 0",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unqualified_single_table_resolves_to_that_table() {
            assert_column_ops(
                "SELECT a, b FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t1", "b"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unqualified_in_where_resolves_to_single_table() {
            assert_column_ops(
                "SELECT a FROM t1 WHERE b > 0",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unqualified_with_multiple_tables_stays_unresolved() {
            // Two `Unknown`-schema tables — without a catalog the resolver
            // cannot tell which `a` belongs to, so the ref surfaces with
            // `table: None`. The lineage source also stays unresolved.
            assert_column_ops(
                "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id"), read("t2", "id"), unresolved("a")],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: None,
                            name: "a".into(),
                        },
                        target: out("a", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unqualified_uses_alias_binding_but_returns_real_table() {
            // Alias is just a binding key; the resolver returns the
            // alias-free TableReference of the binding's underlying table.
            assert_column_ops(
                "SELECT a FROM t1 AS u",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_ref_does_not_surface_in_reads() {
            // The outer `id` resolves to the cte binding (a synthetic
            // intermediate, not real storage), so it's dropped from reads.
            // Reads surface only references with real Table owners or
            // unresolved column names. `unknown_col` doesn't match the
            // cte's Known schema [id], so it surfaces unresolved
            // (table: None) AND fires an UnresolvedColumn diagnostic.
            assert_column_ops(
                "WITH cte AS (SELECT id FROM t1) SELECT id, unknown_col FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id"), unresolved("unknown_col")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "id"), out("id", 0)),
                        ColumnLineageEdge {
                            source: ColumnReference {
                                table: None,
                                name: "unknown_col".into(),
                            },
                            target: out("unknown_col", 1),
                            kind: ColumnLineageKind::Passthrough,
                        },
                    ],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnresolvedColumn)],
                },
            );
        }

        #[test]
        fn derived_table_ref_does_not_surface_in_reads() {
            // Outer `id` resolves to derived alias `d` — synthetic, dropped.
            // Only the inner SELECT's t1.id is a real read.
            assert_column_ops(
                "SELECT id FROM (SELECT id FROM t1) AS d",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn unqualified_inner_scope_shadows_outer() {
            // Inner subquery has its own t2 in scope; the unqualified `y`
            // inside the IN-subquery resolves to t2 even though t1 is
            // also in the outer scope. Standard SQL inner-shadows-outer.
            // The predicate subquery emits no lineage (it feeds a filter);
            // it still surfaces its refs in reads. The outer `*` is a
            // suppressed wildcard, so there is no lineage at all.
            assert_column_ops(
                "SELECT * FROM t1 WHERE id IN (SELECT id FROM t2 WHERE y > 0)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id"), read("t2", "id"), read("t2", "y")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
                },
            );
        }

        #[test]
        fn unqualified_correlated_walks_to_outer_when_inner_has_no_candidate() {
            // Inner CTE has Known schema [zz]; `outer_col` doesn't fit it,
            // so resolution walks to the outer scope and picks the t1
            // (Unknown) binding. The predicate subquery emits no lineage;
            // the outer `*` is a suppressed wildcard, so no lineage at all.
            assert_column_ops(
                "SELECT * FROM t1 WHERE id IN (\
                WITH inner_cte AS (SELECT zz FROM t1) \
                SELECT zz FROM inner_cte WHERE outer_col > 0)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id"), read("t1", "zz"), read("t1", "outer_col")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
                },
            );
        }
    }

    mod writes {
        use super::*;

        #[test]
        fn insert_with_explicit_columns_writes_those_columns_on_target() {
            assert_column_ops(
                "INSERT INTO t1 (a, b) VALUES (1, 2)",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![write("t1", "a"), write("t1", "b")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_records_target_writes_and_qualified_source_reads() {
            assert_column_ops(
                "INSERT INTO t1 (a) SELECT t2.b FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t2", "b")],
                    writes: vec![write("t1", "a")],
                    lineage: vec![passthrough(col("t2", "b"), relation("t1", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_without_explicit_columns_yields_no_writes() {
            // Without an explicit column list AND without a catalog, the
            // resolver can't pair source projections to target columns;
            // writes / lineage stay empty.
            assert_column_ops(
                "INSERT INTO t1 SELECT t2.b FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t2", "b")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_targets_become_writes_on_update_table() {
            assert_column_ops(
                "UPDATE t1 SET a = 1",
                ColumnOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![],
                    writes: vec![write("t1", "a")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_qualified_target_keeps_qualifier() {
            assert_column_ops(
                "UPDATE t1 SET t1.a = 1",
                ColumnOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![],
                    writes: vec![write("t1", "a")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_rhs_qualified_ref_is_a_read() {
            // SET RHS is value-producing (Projection-like); WHERE refs are
            // Filter-tagged.
            assert_column_ops(
                "UPDATE t1 SET a = t2.b FROM t2 WHERE t1.id = t2.id",
                ColumnOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![read("t2", "b"), read("t1", "id"), read("t2", "id")],
                    writes: vec![write("t1", "a")],
                    lineage: vec![passthrough(col("t2", "b"), relation("t1", "a"))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod delete {
        use super::*;

        #[test]
        fn delete_qualified_predicate_is_a_read() {
            assert_column_ops(
                "DELETE FROM t1 WHERE t1.id = 5",
                ColumnOperation {
                    statement_kind: StatementKind::Delete,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }
    }

    // Columns from every clause (projection / WHERE / GROUP BY /
    // ORDER BY / OVER / CASE / HAVING / …) surface in `reads` as plain
    // occurrence entries — `reads` no longer tags a syntactic clause.
    // These tests pin down WHICH refs surface (occurrence-based, dups
    // kept) and the lineage they produce.
    mod reads_by_clause {
        use super::*;

        #[test]
        fn same_column_in_projection_and_where_is_two_reads() {
            // The two textual `a` references each get their own `reads`
            // entry (occurrence-based — duplicates are kept).
            assert_column_ops(
                "SELECT a FROM t1 WHERE a > 0",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "a")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn predicate_subquery_surfaces_reads_but_no_lineage() {
            // The IN-subquery feeds a filter, so it emits NO lineage
            // (Option B: nested subqueries resolve raw, no intermediate
            // QueryOutput edge). Its refs (s.id, s.flag) still surface
            // in reads. Only the outer projection `a` contributes a lineage edge.
            assert_column_ops(
                "SELECT a FROM t WHERE id IN (SELECT id FROM s WHERE flag = 1)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t", "a"),
                        read("t", "id"),
                        read("s", "id"),
                        read("s", "flag"),
                    ],
                    writes: vec![],
                    lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn scalar_subquery_in_projection_feeds_only_outer() {
            // `SELECT a, (SELECT max(x) FROM s) AS m FROM t`:
            //  - the scalar subquery does NOT emit its own QueryOutput
            //    edge (Option B: raw resolve). Its source `s.x` is
            //    captured by the enclosing projection item, which emits
            //    the single meaningful edge `s.x → out("m", 1)`,
            //    Transformation (the item is a subquery expression).
            //  - `a` is a plain passthrough at position 0.
            assert_column_ops(
                "SELECT a, (SELECT max(x) FROM s) AS m FROM t",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t", "a"), read("s", "x")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t", "a"), out("a", 0)),
                        transformation(col("s", "x"), out("m", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn is_null_predicate_ref_surfaces_as_read() {
            // `WHERE x IS NULL` — x surfaces in reads like any other
            // WHERE ref; it is not a lineage source (predicate-only).
            assert_column_ops(
                "SELECT a FROM t1 WHERE b IS NULL",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn is_not_null_predicate_ref_surfaces_as_read() {
            assert_column_ops(
                "SELECT a FROM t1 WHERE b IS NOT NULL",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn group_by_ref_surfaces_as_read() {
            assert_column_ops(
                "SELECT a, COUNT(*) FROM t1 GROUP BY a",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "a")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn order_by_ref_surfaces_as_read() {
            assert_column_ops(
                "SELECT a FROM t1 ORDER BY b",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn group_by_and_having_refs_both_surface() {
            // `a` (projection + GROUP BY) and `b` (HAVING) all surface.
            // Walk order: projection → HAVING → GROUP BY (the visitor
            // hits HAVING before GROUP BY), so the read order reflects
            // that, not the textual SQL order.
            assert_column_ops(
                "SELECT a FROM t1 GROUP BY a HAVING SUM(b) > 0",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b"), read("t1", "a")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn group_by_rollup_modifier_refs_surface() {
            assert_column_ops(
                "SELECT a, b FROM t1 GROUP BY ROLLUP(a, b)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "a"),
                        read("t1", "b"),
                        read("t1", "a"),
                        read("t1", "b"),
                    ],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t1", "b"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn group_by_cube_modifier_refs_surface() {
            assert_column_ops(
                "SELECT a, b FROM t1 GROUP BY CUBE(a, b)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "a"),
                        read("t1", "b"),
                        read("t1", "a"),
                        read("t1", "b"),
                    ],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t1", "b"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn group_by_grouping_sets_walks_each_set_member() {
            // GROUPING SETS ((a, b), (a), ()) — every named column
            // inside any set surfaces as a read. The empty set
            // contributes nothing.
            assert_column_ops(
                "SELECT a, b FROM t1 GROUP BY GROUPING SETS ((a, b), (a), ())",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "a"),
                        read("t1", "b"),
                        read("t1", "a"),
                        read("t1", "b"),
                        read("t1", "a"),
                    ],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t1", "b"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn group_by_mixed_plain_and_rollup_collects_both() {
            // `GROUP BY a, ROLLUP(b, c)` — `a` is a plain GROUP BY ref;
            // `b`, `c` are inside the ROLLUP expression. All three
            // surface as reads.
            assert_column_ops(
                "SELECT a, b, c FROM t1 GROUP BY a, ROLLUP(b, c)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "a"),
                        read("t1", "b"),
                        read("t1", "c"),
                        read("t1", "a"),
                        read("t1", "b"),
                        read("t1", "c"),
                    ],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t1", "b"), out("b", 1)),
                        passthrough(col("t1", "c"), out("c", 2)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn subquery_in_group_by_surfaces_reads_but_no_inner_lineage() {
            // GROUP BY (SELECT z FROM s) — the subquery's `z` surfaces in
            // reads, but the subquery emits no lineage (Option B: raw
            // resolve, no intermediate QueryOutput). Only the outer
            // projection `a` contributes a lineage edge.
            assert_column_ops(
                "SELECT a FROM t GROUP BY (SELECT z FROM s)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t", "a"), read("s", "z")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn case_in_projection_refs_surface_and_transform() {
            // Condition (`a`), THEN (`b`), and ELSE (`c`) all surface as
            // reads and feed into the CASE output as Transformation.
            assert_column_ops(
                "SELECT CASE WHEN a > 0 THEN b ELSE c END FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b"), read("t1", "c")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "a"), out_anon(0)),
                        transformation(col("t1", "b"), out_anon(0)),
                        transformation(col("t1", "c"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn case_in_where_refs_surface_as_reads() {
            // The CASE sits in WHERE: its condition (`x`) and results
            // (`y`, `z`) surface as reads (not lineage sources — the CASE
            // feeds a predicate). `b` is the outer projection.
            assert_column_ops(
                "SELECT b FROM t WHERE CASE WHEN x > 0 THEN y ELSE z END = 1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t", "b"),
                        read("t", "x"),
                        read("t", "y"),
                        read("t", "z"),
                    ],
                    writes: vec![],
                    lineage: vec![passthrough(col("t", "b"), out("b", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn scalar_subquery_in_case_condition_composes_to_outer_only() {
            // A scalar subquery in a CASE condition emits no lineage of its
            // own (Option B: raw resolve). The outer CASE projection
            // item captures the subquery's refs (`s.x` from its
            // projection, `s.y` from its WHERE) as its source refs, so
            // both feed into the outer anonymous output as
            // Transformation. Refs still surface in reads.
            assert_column_ops(
                "SELECT CASE WHEN (SELECT x FROM s WHERE y > 0) IS NULL THEN 1 END FROM t",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("s", "x"), read("s", "y")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("s", "x"), out_anon(0)),
                        transformation(col("s", "y"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn simple_case_operand_and_results_surface() {
            // `CASE x WHEN 1 THEN a WHEN 2 THEN b END` — the operand
            // `x` and the results `a` / `b` all surface as reads and
            // feed into the CASE output as Transformation.
            assert_column_ops(
                "SELECT CASE x WHEN 1 THEN a WHEN 2 THEN b END FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "x"), read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "x"), out_anon(0)),
                        transformation(col("t1", "a"), out_anon(0)),
                        transformation(col("t1", "b"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn simple_case_with_column_when_pattern_all_surface() {
            // `CASE x WHEN y THEN a ELSE b END` — operand `x`,
            // WHEN-pattern `y`, and results `a` / `b` all surface as
            // reads and feed into the CASE output as Transformation.
            assert_column_ops(
                "SELECT CASE x WHEN y THEN a ELSE b END FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "x"),
                        read("t1", "y"),
                        read("t1", "a"),
                        read("t1", "b"),
                    ],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "x"), out_anon(0)),
                        transformation(col("t1", "y"), out_anon(0)),
                        transformation(col("t1", "a"), out_anon(0)),
                        transformation(col("t1", "b"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn window_partition_by_refs_surface_and_transform() {
            // OVER (PARTITION BY p) — both the aggregate arg `x` and
            // the partition key `p` surface as reads, and both feed
            // into the window output as Transformation (the whole
            // SUM(...) OVER (...) expression is value-changing).
            assert_column_ops(
                "SELECT SUM(x) OVER (PARTITION BY p) FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "x"), read("t1", "p")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "x"), out_anon(0)),
                        transformation(col("t1", "p"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn window_order_by_refs_surface_and_transform() {
            assert_column_ops(
                "SELECT SUM(x) OVER (ORDER BY o) FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "x"), read("t1", "o")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "x"), out_anon(0)),
                        transformation(col("t1", "o"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn window_partition_and_order_refs_all_surface_and_transform() {
            assert_column_ops(
                "SELECT SUM(x) OVER (PARTITION BY p ORDER BY o) FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "x"), read("t1", "p"), read("t1", "o")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "x"), out_anon(0)),
                        transformation(col("t1", "p"), out_anon(0)),
                        transformation(col("t1", "o"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn window_with_literal_frame_bounds_does_not_add_refs() {
            // Frame bounds with literal integers (`3 PRECEDING`,
            // `CURRENT ROW`) walk via visit_expr but produce no
            // column refs — same shape as the no-frame version.
            assert_column_ops(
                "SELECT SUM(x) OVER (PARTITION BY p ORDER BY o \
                                     ROWS BETWEEN 3 PRECEDING AND CURRENT ROW) FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "x"), read("t1", "p"), read("t1", "o")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "x"), out_anon(0)),
                        transformation(col("t1", "p"), out_anon(0)),
                        transformation(col("t1", "o"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn window_with_unbounded_frame_bounds_does_not_add_refs() {
            // UNBOUNDED PRECEDING / UNBOUNDED FOLLOWING are bound
            // variants without an associated expr — visit_window_frame_bound
            // returns Ok without walking anything.
            assert_column_ops(
                "SELECT SUM(x) OVER (ORDER BY o \
                                     ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) \
                 FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "x"), read("t1", "o")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "x"), out_anon(0)),
                        transformation(col("t1", "o"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn merge_on_clause_refs_surface_as_reads_not_lineage() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET t.a = s.a",
                ColumnOperation {
                    statement_kind: StatementKind::Merge,
                    reads: vec![read("t", "id"), read("s", "id"), read("s", "a")],
                    writes: vec![write("t", "a")],
                    lineage: vec![passthrough(col("s", "a"), relation("t", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn create_table_definitions_are_not_writes() {
            assert_column_ops(
                "CREATE TABLE t1 (a INT, b INT)",
                ColumnOperation {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod diagnostics {
        use super::*;

        #[test]
        fn unsupported_statement_reports_diagnostic() {
            assert_column_ops(
                "CREATE INDEX idx ON t1 (a)",
                ColumnOperation {
                    statement_kind: StatementKind::Unsupported,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
                },
            );
        }

        #[test]
        fn wildcard_in_projection_reports_diagnostic() {
            // Whole-value pin-down on the structural shape; assert_column_ops
            // compares diagnostics by kind only. The message text and span
            // coordinates are verified separately below since this test's
            // *purpose* is to confirm both are populated.
            let ops = extract("SELECT * FROM t1");
            assert_column_ops(
                "SELECT * FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
                },
            );
            // Span info ("at L1:C8") is duplicated in message and surfaced
            // as structured data for programmatic consumers.
            assert!(
                ops.diagnostics[0].message.contains("at L1:C8"),
                "expected span suffix in message, got: {}",
                ops.diagnostics[0].message
            );
            let span = ops.diagnostics[0]
                .span
                .expect("wildcard token carries a span");
            assert_eq!(span.start.line, 1);
            assert_eq!(span.start.column, 8);
        }

        #[test]
        fn qualified_wildcard_in_projection_reports_diagnostic() {
            assert_column_ops(
                "SELECT t1.* FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
                },
            );
        }

        #[test]
        fn multiple_statements_produce_multiple_results() {
            let sql = "SELECT t1.a FROM t1; SELECT t2.b FROM t2";
            assert_nth_column_ops(
                sql,
                0,
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
            assert_nth_column_ops(
                sql,
                1,
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t2", "b")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t2", "b"), out("b", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn wildcard_select_yields_no_column_ops() {
            assert_column_ops(
                "SELECT * FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
                },
            );
        }
    }

    mod lineage {
        use super::*;

        #[test]
        fn select_bare_column_emits_passthrough_edge_to_query_output() {
            assert_column_ops(
                "SELECT a FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn select_aliased_column_uses_alias_as_output_name() {
            assert_column_ops(
                "SELECT a AS x FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("x", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn select_arithmetic_emits_one_transformation_edge_per_source() {
            assert_column_ops(
                "SELECT a + b FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "a"), out_anon(0)),
                        transformation(col("t1", "b"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn select_mixed_projection_separates_targets_by_position() {
            assert_column_ops(
                "SELECT a, a + b FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        transformation(col("t1", "a"), out_anon(1)),
                        transformation(col("t1", "b"), out_anon(1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn select_qualified_ref_in_expression_resolves_directly() {
            assert_column_ops(
                "SELECT t1.a + t1.b AS sum FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "a"), out("sum", 0)),
                        transformation(col("t1", "b"), out("sum", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_pairs_target_cols_positionally() {
            assert_column_ops(
                "INSERT INTO t1 (a, b) SELECT x, y FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t2", "x"), read("t2", "y")],
                    writes: vec![write("t1", "a"), write("t1", "b")],
                    lineage: vec![
                        passthrough(col("t2", "x"), relation("t1", "a")),
                        passthrough(col("t2", "y"), relation("t1", "b")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_transformation_marks_kind_per_source() {
            assert_column_ops(
                "INSERT INTO t1 (a) SELECT x + y FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t2", "x"), read("t2", "y")],
                    writes: vec![write("t1", "a")],
                    lineage: vec![
                        transformation(col("t2", "x"), relation("t1", "a")),
                        transformation(col("t2", "y"), relation("t1", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_union_pairs_both_branches_with_target_cols() {
            // Both UNION branches feed the same INSERT target positions,
            // so each branch's projection should pair `position N → t.col_N`.
            assert_column_ops(
                "INSERT INTO t1 (a, b) \
                 SELECT x, y FROM t2 \
                 UNION ALL \
                 SELECT p, q FROM t3",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![
                        read("t2", "x"),
                        read("t2", "y"),
                        read("t3", "p"),
                        read("t3", "q"),
                    ],
                    writes: vec![write("t1", "a"), write("t1", "b")],
                    lineage: vec![
                        passthrough(col("t2", "x"), relation("t1", "a")),
                        passthrough(col("t2", "y"), relation("t1", "b")),
                        passthrough(col("t3", "p"), relation("t1", "a")),
                        passthrough(col("t3", "q"), relation("t1", "b")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_without_explicit_cols_emits_no_lineage() {
            // Target column names would need catalog-driven positional
            // mapping; without catalog the resolver emits nothing.
            assert_column_ops(
                "INSERT INTO t1 SELECT x FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t2", "x")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_values_with_literals_emits_no_lineage() {
            assert_column_ops(
                "INSERT INTO t1 (a, b) VALUES (1, 2)",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![write("t1", "a"), write("t1", "b")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_literal_emits_no_lineage() {
            assert_column_ops(
                "UPDATE t1 SET a = 1",
                ColumnOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![],
                    writes: vec![write("t1", "a")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn delete_emits_no_lineage() {
            assert_column_ops(
                "DELETE FROM t1 WHERE id = 5",
                ColumnOperation {
                    statement_kind: StatementKind::Delete,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn wildcard_select_emits_no_lineage() {
            assert_column_ops(
                "SELECT * FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
                },
            );
        }

        #[test]
        fn update_set_passthrough_lineage() {
            assert_column_ops(
                "UPDATE t1 SET a = b",
                ColumnOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![read("t1", "b")],
                    writes: vec![write("t1", "a")],
                    lineage: vec![passthrough(col("t1", "b"), relation("t1", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_transformation_lineage() {
            assert_column_ops(
                "UPDATE t1 SET a = b + 1",
                ColumnOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![read("t1", "b")],
                    writes: vec![write("t1", "a")],
                    lineage: vec![transformation(col("t1", "b"), relation("t1", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn update_set_with_qualified_rhs_resolves_to_other_table() {
            assert_column_ops(
                "UPDATE t1 SET a = t2.b FROM t2 WHERE t1.id = t2.id",
                ColumnOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![read("t2", "b"), read("t1", "id"), read("t2", "id")],
                    writes: vec![write("t1", "a")],
                    lineage: vec![passthrough(col("t2", "b"), relation("t1", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_call_in_projection_emits_transformation_edge() {
            assert_column_ops(
                "SELECT SUM(a) FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    lineage: vec![transformation(col("t1", "a"), out_anon(0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_with_alias_carries_aliased_name() {
            assert_column_ops(
                "SELECT COUNT(b) AS n FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "b")],
                    writes: vec![],
                    lineage: vec![transformation(col("t1", "b"), out("n", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_wrapped_in_expression_is_transformation() {
            // `SUM(a) + 1` is a value-changing expression, so the lineage edge
            // is Transformation — same kind a bare aggregate call would
            // produce, since the model no longer sub-classifies them.
            assert_column_ops(
                "SELECT SUM(a) + 1 FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    lineage: vec![transformation(col("t1", "a"), out_anon(0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_in_insert_select_propagates_transformation() {
            assert_column_ops(
                "INSERT INTO t2 (n) SELECT COUNT(a) FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t1", "a")],
                    writes: vec![write("t2", "n")],
                    lineage: vec![transformation(col("t1", "a"), relation("t2", "n"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_aggregate_composes_to_outer_as_transformation() {
            // CTE body's `s` is Transformation (SUM(a)); outer's bare `s`
            // would be Passthrough, but composition keeps the chain a
            // Transformation (any transforming step dominates).
            assert_column_ops(
                "WITH cte AS (SELECT SUM(a) AS s FROM t1) SELECT s FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    lineage: vec![transformation(col("t1", "a"), out("s", 0))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod cte_derived_rename {
        use super::*;

        #[test]
        fn cte_column_rename_composes_through_renamed_name() {
            // Outer `a` refers to cte's renamed column at position 0,
            // which body-positionally is `x` from t. Composition follows
            // the renamed name back to the body item, then to t.x.
            // Reads surface only the real-table ref (CTE binding is
            // synthetic, dropped).
            assert_column_ops(
                "WITH cte (a) AS (SELECT x FROM t) SELECT a FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t", "x")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t", "x"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_column_alias_matched_case_insensitively() {
            // The CTE projects `x AS Foo`; the outer query references it
            // as unquoted `foo`. Composition's name-match folds both
            // sides to the same key, so `foo` composes back to the real
            // source `t1.x`.
            assert_column_ops(
                "WITH cte AS (SELECT x AS Foo FROM t1) SELECT foo FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "x")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "x"), out("foo", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_column_rename_partial_keeps_remaining_body_names() {
            // Rename `(p)` covers position 0 only. Position 1's body name
            // `y` survives; outer can reference `p` or `y`.
            assert_column_ops(
                "WITH cte (p) AS (SELECT x, y FROM t) SELECT p, y FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t", "x"), read("t", "y")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t", "x"), out("p", 0)),
                        passthrough(col("t", "y"), out("y", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn derived_table_column_rename_composes() {
            // `(SELECT x FROM t) AS d(a)` — outer `a` resolves via d's
            // renamed column at position 0 → body item x → t.x.
            assert_column_ops(
                "SELECT a FROM (SELECT x FROM t) d(a)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t", "x")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t", "x"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_column_rename_into_insert() {
            // `INSERT INTO t2 (col) WITH cte(a) AS (SELECT x FROM t1)
            //  SELECT a FROM cte` composes through both the CTE rename
            //  and the INSERT pairing: t1.x → t2.col.
            assert_column_ops(
                "INSERT INTO t2 (col) WITH cte (a) AS (SELECT x FROM t1) \
                 SELECT a FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t1", "x")],
                    writes: vec![write("t2", "col")],
                    lineage: vec![passthrough(col("t1", "x"), relation("t2", "col"))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod with_in_dml {
        //! `WITH cte AS (...) <DML>` — Postgres / Sqlite / standard
        //! SQL syntax for binding CTEs visible to a DML statement.
        //! sqlparser typically parses these as Query-with-WITH at the
        //! source level for INSERT, and wraps Update / Delete in
        //! various ways. These tests pin down what actually surfaces
        //! through the resolver.
        use super::*;

        #[test]
        fn with_in_insert_select_composes_cte_to_target() {
            assert_column_ops(
                "WITH cte AS (SELECT x FROM s) INSERT INTO t (a) SELECT x FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "x")],
                    writes: vec![write("t", "a")],
                    lineage: vec![passthrough(col("s", "x"), relation("t", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn with_in_update_via_scalar_subquery_composes() {
            // CTE referenced from the SET RHS scalar subquery. The
            // subquery emits no QueryOutput edge of its own (Option B);
            // the UPDATE SET assignment captures its source (composed
            // through cte to s.x) and emits the single Relation edge.
            // Transformation (the value is derived through max + the
            // subquery wrapping).
            assert_column_ops(
                "WITH cte AS (SELECT max(x) AS m FROM s) \
                 UPDATE t SET a = (SELECT m FROM cte) WHERE id = 1",
                ColumnOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![read("s", "x"), read("t", "id")],
                    writes: vec![write("t", "a")],
                    lineage: vec![transformation(col("s", "x"), relation("t", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn with_in_delete_via_predicate_subquery_keeps_cte_source_as_read() {
            // The DELETE target `t` lives in its own scope (the SetExpr
            // DML scope), so the outer predicate `id` resolves
            // unambiguously to `t`. The predicate subquery feeds a
            // filter, so it emits no lineage (Option B); its refs (s.id
            // via the cte) still surface in reads. DELETE has no column
            // lineage of its own — so lineage is empty.
            assert_column_ops(
                "WITH cte AS (SELECT id FROM s WHERE flag) \
                 DELETE FROM t WHERE id IN (SELECT id FROM cte)",
                ColumnOperation {
                    statement_kind: StatementKind::Delete,
                    reads: vec![read("s", "id"), read("s", "flag"), read("t", "id")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn with_multiple_ctes_chained_into_insert() {
            // Two CTEs where `b` references `a`. INSERT then pulls
            // from `b`. Composition walks back through both layers
            // to the base table.
            assert_column_ops(
                "WITH a AS (SELECT id FROM t1), \
                      b AS (SELECT id + 1 AS x FROM a) \
                 INSERT INTO t2 (col) SELECT x FROM b",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t1", "id")],
                    writes: vec![write("t2", "col")],
                    lineage: vec![transformation(col("t1", "id"), relation("t2", "col"))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod merge {
        use super::*;

        #[test]
        fn merge_when_matched_update_emits_lineage_and_write() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET t.a = s.a",
                ColumnOperation {
                    statement_kind: StatementKind::Merge,
                    reads: vec![read("t", "id"), read("s", "id"), read("s", "a")],
                    writes: vec![write("t", "a")],
                    lineage: vec![passthrough(col("s", "a"), relation("t", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn merge_when_not_matched_insert_emits_lineage_and_write() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id \
                 WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a)",
                ColumnOperation {
                    statement_kind: StatementKind::Merge,
                    reads: vec![
                        read("t", "id"),
                        read("s", "id"),
                        read("s", "id"),
                        read("s", "a"),
                    ],
                    writes: vec![write("t", "id"), write("t", "a")],
                    lineage: vec![
                        passthrough(col("s", "id"), relation("t", "id")),
                        passthrough(col("s", "a"), relation("t", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn merge_delete_action_emits_no_lineage_no_write() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE",
                ColumnOperation {
                    statement_kind: StatementKind::Merge,
                    reads: vec![read("t", "id"), read("s", "id")],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn merge_combined_clauses_emit_per_clause_lineage_and_writes() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id \
                 WHEN MATCHED THEN UPDATE SET t.a = s.a \
                 WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a)",
                ColumnOperation {
                    statement_kind: StatementKind::Merge,
                    reads: vec![
                        read("t", "id"),
                        read("s", "id"),
                        read("s", "a"),
                        read("s", "id"),
                        read("s", "a"),
                    ],
                    writes: vec![write("t", "a"), write("t", "id"), write("t", "a")],
                    lineage: vec![
                        passthrough(col("s", "a"), relation("t", "a")),
                        passthrough(col("s", "id"), relation("t", "id")),
                        passthrough(col("s", "a"), relation("t", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn merge_update_transformation_kind_propagates() {
            assert_column_ops(
                "MERGE INTO t USING s ON t.id = s.id \
                 WHEN MATCHED THEN UPDATE SET t.a = s.a + 1",
                ColumnOperation {
                    statement_kind: StatementKind::Merge,
                    reads: vec![read("t", "id"), read("s", "id"), read("s", "a")],
                    writes: vec![write("t", "a")],
                    lineage: vec![transformation(col("s", "a"), relation("t", "a"))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod ctas_view {
        use super::*;

        #[test]
        fn ctas_pairs_source_projection_with_inferred_column_names() {
            // CREATE TABLE AS SELECT — no explicit column list, so target
            // columns follow the source projection's inferred names
            // (alias > bare ident).
            assert_column_ops(
                "CREATE TABLE t AS SELECT x AS a, y FROM s",
                ColumnOperation {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![read("s", "x"), read("s", "y")],
                    writes: vec![write("t", "a"), write("t", "y")],
                    lineage: vec![
                        passthrough(col("s", "x"), relation("t", "a")),
                        passthrough(col("s", "y"), relation("t", "y")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn ctas_with_explicit_columns_overrides_projection_names() {
            // Explicit column list wins over inferred names.
            assert_column_ops(
                "CREATE TABLE t (p INT, q INT) AS SELECT x, y FROM s",
                ColumnOperation {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![read("s", "x"), read("s", "y")],
                    writes: vec![write("t", "p"), write("t", "q")],
                    lineage: vec![
                        passthrough(col("s", "x"), relation("t", "p")),
                        passthrough(col("s", "y"), relation("t", "q")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn ctas_propagates_transformation_kind() {
            assert_column_ops(
                "CREATE TABLE t AS SELECT SUM(x) AS total FROM s",
                ColumnOperation {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![read("s", "x")],
                    writes: vec![write("t", "total")],
                    lineage: vec![transformation(col("s", "x"), relation("t", "total"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn create_view_pairs_source_projection() {
            assert_column_ops(
                "CREATE VIEW v AS SELECT x AS a, y FROM s",
                ColumnOperation {
                    statement_kind: StatementKind::CreateView,
                    reads: vec![read("s", "x"), read("s", "y")],
                    writes: vec![write("v", "a"), write("v", "y")],
                    lineage: vec![
                        passthrough(col("s", "x"), relation("v", "a")),
                        passthrough(col("s", "y"), relation("v", "y")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn create_view_with_explicit_columns_uses_list() {
            assert_column_ops(
                "CREATE VIEW v (a, b) AS SELECT x, y FROM s",
                ColumnOperation {
                    statement_kind: StatementKind::CreateView,
                    reads: vec![read("s", "x"), read("s", "y")],
                    writes: vec![write("v", "a"), write("v", "b")],
                    lineage: vec![
                        passthrough(col("s", "x"), relation("v", "a")),
                        passthrough(col("s", "y"), relation("v", "b")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn alter_view_pairs_replacement_query_projection() {
            assert_column_ops(
                "ALTER VIEW v AS SELECT x AS a FROM s",
                ColumnOperation {
                    statement_kind: StatementKind::AlterView,
                    reads: vec![read("s", "x")],
                    writes: vec![write("v", "a")],
                    lineage: vec![passthrough(col("s", "x"), relation("v", "a"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn ctas_unnamed_projection_yields_no_paired_lineage() {
            // `SELECT 1` has no column ref and no inferable name, so the
            // CTAS source produces no lineage / no write for that slot.
            assert_column_ops(
                "CREATE TABLE t AS SELECT 1 FROM s",
                ColumnOperation {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_with_distinct_args_marker() {
            // COUNT(DISTINCT user_id) — an aggregate call, so the source
            // feeds into the output as a Transformation.
            assert_column_ops(
                "SELECT COUNT(DISTINCT user_id) FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "user_id")],
                    writes: vec![],
                    lineage: vec![transformation(col("t1", "user_id"), out_anon(0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn aggregate_with_filter_clause_marker() {
            // SUM(x) FILTER (WHERE y > 0) — both `x` and `y` surface as
            // reads, and both feed into the aggregate's output as
            // Transformation. Anything mentioned inside the aggregate's
            // syntactic boundary (args + FILTER predicate) is a lineage
            // source, not just the bare argument.
            assert_column_ops(
                "SELECT SUM(x) FILTER (WHERE y > 0) FROM t1",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "x"), read("t1", "y")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "x"), out_anon(0)),
                        transformation(col("t1", "y"), out_anon(0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_aggregate_then_outer_expression_still_transformation() {
            // Outer wraps the CTE column in an expression (s + 1) —
            // composition: outer Transformation × inner Transformation =
            // Transformation.
            assert_column_ops(
                "WITH cte AS (SELECT SUM(a) AS s FROM t1) SELECT s + 1 FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    lineage: vec![transformation(col("t1", "a"), out_anon(0))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod composition {
        use super::*;

        #[test]
        fn cte_passthrough_composes_to_base_table() {
            // The outer edge's source `id` resolves to cte, then composes
            // through the CTE body's projection back to t1.id. No
            // intermediate cte.id → out edge survives.
            assert_column_ops(
                "WITH cte AS (SELECT id FROM t1) SELECT id FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_transformation_propagates_kind_after_composition() {
            // CTE body's `sum` is a transformation of a, b. Outer's bare
            // `sum` composes back into two edges, each Transformation
            // because the body item is (outer.bare && item.bare = false).
            assert_column_ops(
                "WITH cte AS (SELECT a + b AS sum FROM t1) SELECT sum FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "a"), out("sum", 0)),
                        transformation(col("t1", "b"), out("sum", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_to_insert_composes_end_to_end() {
            // Composition reaches past the CTE boundary into the INSERT
            // target — t1.id → t2.x directly, no cte.id step.
            assert_column_ops(
                "INSERT INTO t2 (x) WITH cte AS (SELECT id FROM t1) SELECT id FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t1", "id")],
                    writes: vec![write("t2", "x")],
                    lineage: vec![passthrough(col("t1", "id"), relation("t2", "x"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_chain_composes_through_all_levels() {
            // a → b → outer: outer's `b.id` composes via b's body back to
            // a, then via a's body back to t1. Outer is qualified because
            // having both `a` and `b` in scope with the same column name
            // makes the unqualified form ambiguous under our scope model
            // (outer SELECT sees both CTE bindings, not just b).
            assert_column_ops(
                "WITH a AS (SELECT id FROM t1), b AS (SELECT id FROM a) SELECT b.id FROM b",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn derived_table_composes_to_base_table() {
            // The outer projection's `col` composes through derived `d`'s
            // body (a + b AS col) into two Transformation edges on t1.
            assert_column_ops(
                "SELECT col FROM (SELECT a + b AS col FROM t1) d",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t1", "b")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "a"), out("col", 0)),
                        transformation(col("t1", "b"), out("col", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn cte_referenced_twice_composes_each_use() {
            // Each cte reference in the projection composes independently
            // back to t1.id.
            assert_column_ops(
                "WITH cte AS (SELECT id FROM t1) SELECT cte.id AS a, cte.id AS b FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "id"), out("a", 0)),
                        passthrough(col("t1", "id"), out("b", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn recursive_cte_does_not_panic_and_skips_composition() {
            // Recursive CTEs don't carry body_projections (fixpoint is
            // deferred), so composition falls back to leaving the lineage edge
            // source pointing at the CTE binding (`r.id`) rather than
            // tracing into a base table. Reads still get the synthetic
            // filter, so only `t1.id` from the non-recursive branch
            // surfaces in reads. No infinite recursion either.
            assert_column_ops(
                "WITH RECURSIVE r AS (SELECT id FROM t1 UNION SELECT id FROM r) SELECT id FROM r",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: Some(TableReference {
                                catalog: None,
                                schema: None,
                                name: "r".into(),
                            }),
                            name: "id".into(),
                        },
                        target: out("id", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod set_operations {
        use super::*;

        #[test]
        fn union_two_branches_emit_query_output_per_branch() {
            // Each branch contributes its own ProjectionGroup, so both
            // branches' projections fan out independently into
            // QueryOutput edges. Position is per-group, so both land at
            // position 0; name follows each branch's own projection.
            assert_column_ops(
                "SELECT a FROM t1 UNION SELECT b FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_all_behaves_same_as_union() {
            // UNION ALL only differs from UNION at runtime (dedup vs
            // not); structurally the resolver should treat them identically.
            assert_column_ops(
                "SELECT a FROM t1 UNION ALL SELECT b FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn intersect_behaves_same_as_union() {
            assert_column_ops(
                "SELECT a FROM t1 INTERSECT SELECT b FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn except_behaves_same_as_union() {
            assert_column_ops(
                "SELECT a FROM t1 EXCEPT SELECT b FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn three_way_union_emits_one_lineage_edge_per_branch() {
            // Chained UNION parses left-associatively as
            // `(t1 UNION t2) UNION t3`, so the resolver recursively
            // visits each base SELECT and each contributes its own group.
            assert_column_ops(
                "SELECT a FROM t1 UNION SELECT b FROM t2 UNION SELECT c FROM t3",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b"), read("t3", "c")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t2", "b"), out("b", 0)),
                        passthrough(col("t3", "c"), out("c", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_with_where_classifies_per_branch_kind() {
            // Each branch's WHERE is its own filter scope, so each
            // branch produces a Projection read plus a Filter read for
            // its own column.
            assert_column_ops(
                "SELECT a FROM t1 WHERE a > 0 UNION SELECT b FROM t2 WHERE b < 10",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "a"),
                        read("t1", "a"),
                        read("t2", "b"),
                        read("t2", "b"),
                    ],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_mixed_passthrough_and_transformation_kinds() {
            // Branch lineage kinds are independent. Left passthrough, right
            // transformation; both contribute to the same output position.
            assert_column_ops(
                "SELECT a FROM t1 UNION SELECT b + 1 AS a FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        transformation(col("t2", "b"), out("a", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_with_aggregate_branch_emits_transformation_edge() {
            assert_column_ops(
                "SELECT id FROM t1 UNION SELECT COUNT(id) AS id FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id"), read("t2", "id")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "id"), out("id", 0)),
                        transformation(col("t2", "id"), out("id", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_in_subquery_composes_both_branches_to_outer() {
            // The inner UNION lives in a derived subquery; the outer
            // SELECT projects from it and composes back to the base
            // tables of both branches — no intermediate QueryOutput
            // edge for the subquery survives.
            assert_column_ops(
                "SELECT x FROM (SELECT a AS x FROM t1 UNION SELECT b AS x FROM t2) sub",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("x", 0)),
                        passthrough(col("t2", "b"), out("x", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_in_cte_composes_to_outer_use() {
            // CTE body is a UNION. Outer SELECT pulls `x` from the cte.
            // Composition should walk back through both branches to t1/t2.
            assert_column_ops(
                "WITH cte AS (SELECT a AS x FROM t1 UNION SELECT b AS x FROM t2) \
                 SELECT x FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("x", 0)),
                        passthrough(col("t2", "b"), out("x", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn ctas_with_union_body_pairs_left_branch_names_for_all_branches() {
            // CTAS schema follows the LEFT branch's projection names
            // (SQL standard). The inferred-name path uses the first
            // ProjectionGroup's item names for every branch's
            // positional pairing — same as INSERT-SELECT-UNION. So:
            //   - writes: only `dst.a` (left branch's name)
            //   - lineage: BOTH branches feed `Relation(dst.a)`
            assert_column_ops(
                "CREATE TABLE dst AS SELECT a FROM t1 UNION SELECT b FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![write("dst", "a")],
                    lineage: vec![
                        passthrough(col("t1", "a"), relation("dst", "a")),
                        passthrough(col("t2", "b"), relation("dst", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn ctas_with_explicit_columns_and_union_body_pairs_left_target_for_all_branches() {
            // When CTAS specifies its own column list, both branches
            // pair positionally against the same target columns — same
            // pattern as INSERT-SELECT-UNION.
            assert_column_ops(
                "CREATE TABLE dst (x INT) AS SELECT a FROM t1 UNION SELECT b FROM t2",
                ColumnOperation {
                    statement_kind: StatementKind::CreateTable,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![write("dst", "x")],
                    lineage: vec![
                        passthrough(col("t1", "a"), relation("dst", "x")),
                        passthrough(col("t2", "b"), relation("dst", "x")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_with_trailing_order_by_ref_is_unresolved() {
            // ORDER BY on the whole UNION is visited in the outer query
            // scope, AFTER both branch scopes have been popped. The
            // ORDER BY column refers to a UNION output column, not a
            // base table — so `a` resolves to None (no in-scope
            // binding).
            assert_column_ops(
                "SELECT a FROM t1 UNION SELECT b FROM t2 ORDER BY a",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b"), unresolved("a")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn union_with_trailing_limit_literal_adds_nothing() {
            // LIMIT 10 is a literal — no column refs, no extra lineage.
            assert_column_ops(
                "SELECT a FROM t1 UNION SELECT b FROM t2 LIMIT 10",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t1", "a"), out("a", 0)),
                        passthrough(col("t2", "b"), out("b", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod join_using_and_natural {
        //! USING / NATURAL JOIN merge expansion is documented as
        //! future work (see the module-level note in
        //! column_operation_extractor). These tests pin down the
        //! *current* shape so when USING / NATURAL JOIN expansion lands
        //! (merged refs splitting into both source tables), the diff
        //! will surface here.
        use super::*;

        #[test]
        fn join_using_id_in_projection_is_unresolved_due_to_ambiguity() {
            // `id` in the projection is unqualified with two candidate
            // tables (t1, t2) — the resolver leaves it unresolved
            // (`table: None`) because no catalog disambiguates and
            // USING is not yet expanded into a merged-column binding.
            assert_column_ops(
                "SELECT id FROM t1 JOIN t2 USING (id)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![unresolved("id")],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: None,
                            name: "id".into(),
                        },
                        target: out("id", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn join_using_id_in_projection_and_where_yields_two_independent_unresolved_refs() {
            // The same `id` ref in projection vs. WHERE produces two
            // SEPARATE RawColumnRefs, each with a single-kind `kinds`
            // vec. There is no merge into one ref-with-multi-kinds
            // here — that would require resolver-level tracking of
            // ref identity across clauses, which we don't do.
            assert_column_ops(
                "SELECT id FROM t1 JOIN t2 USING (id) WHERE id > 0",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![unresolved("id"), unresolved("id")],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: None,
                            name: "id".into(),
                        },
                        target: out("id", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn join_using_qualified_id_resolves_to_named_table() {
            // Qualifying the ref sidesteps the USING ambiguity: `t1.id`
            // resolves to t1 unambiguously. Use this in real-world
            // queries until USING expansion is available.
            assert_column_ops(
                "SELECT t1.id FROM t1 JOIN t2 USING (id)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn natural_join_no_catalog_leaves_unqualified_refs_unresolved() {
            // NATURAL JOIN's merge set comes from the intersection of
            // both tables' column lists — only knowable with a
            // catalog. Without one, the resolver doesn't expand, and
            // unqualified `id` is multi-candidate-unresolved (same
            // shape as plain JOIN ON without USING).
            assert_column_ops(
                "SELECT id FROM t1 NATURAL JOIN t2",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![unresolved("id")],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: None,
                            name: "id".into(),
                        },
                        target: out("id", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod lateral_and_correlation {
        use super::*;

        #[test]
        fn lateral_subquery_resolves_inner_ref_to_inner_table() {
            // The existing-style LATERAL: the inner subquery only
            // references its own tables. The outer FROM joins it as
            // a derived source. The inner `id` resolves to t1 from
            // the LATERAL subquery's own scope.
            assert_column_ops(
                "SELECT d.id FROM LATERAL (SELECT id FROM t1) AS d JOIN t2 ON d.id = t2.id",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id"), read("t2", "id")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn lateral_with_outer_scope_reference_resolves_via_scope_chain() {
            // The interesting LATERAL case: the inner subquery references
            // `t1.x` from the OUTER FROM. Without LATERAL this is invalid
            // SQL, but the resolver doesn't enforce LATERAL semantics —
            // it walks the scope chain regardless.
            assert_column_ops(
                "SELECT sub.x FROM t1, LATERAL (SELECT t1.a + t2.b AS x FROM t2) sub",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "a"), out("x", 0)),
                        transformation(col("t2", "b"), out("x", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn non_lateral_derived_also_resolves_outer_ref_permissively() {
            // The resolver doesn't distinguish LATERAL from non-LATERAL
            // — both walk the scope chain identically. This is more
            // lenient than strict SQL semantics (where this should be
            // an error), but reasonable for lineage purposes: a
            // best-effort resolution is more useful than silently
            // dropping the reference.
            assert_column_ops(
                "SELECT sub.x FROM t1, (SELECT t1.a + t2.b AS x FROM t2) sub",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "b")],
                    writes: vec![],
                    lineage: vec![
                        transformation(col("t1", "a"), out("x", 0)),
                        transformation(col("t2", "b"), out("x", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn correlated_where_subquery_resolves_outer_ref() {
            // Classic correlated subquery in WHERE: the inner SELECT
            // references the outer t1.id. The resolver walks the
            // scope chain to find t1.id in the outer scope.
            assert_column_ops(
                "SELECT a FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.fk = t1.id)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "fk"), read("t1", "id")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod on_conflict {
        //! ON CONFLICT (Postgres / Sqlite) and ON DUPLICATE KEY UPDATE
        //! (MySQL) both sit in `Insert.on: Option<OnInsert>`. The
        //! resolver walks both, with subtle differences:
        //!
        //! - Postgres: `EXCLUDED.<col>` is a pseudo-table for the
        //!   would-be-inserted row. Bound as synthetic so refs
        //!   through it filter out of `reads` but still emit valid
        //!   Relation lineage edges into the target. The synthetic
        //!   binding's columns mirror the INSERT target's columns.
        //! - MySQL: `VALUES(<col>)` is a function-call form for the
        //!   same concept. No EXCLUDED binding (it would make
        //!   unqualified refs ambiguous against the INSERT target);
        //!   the inner ref resolves to the INSERT target like a
        //!   regular self-reference.
        //!
        //! DO UPDATE SET targets become writes on the INSERT target
        //! table — same role as a standalone UPDATE SET. The optional
        //! DO UPDATE WHERE clause walks in filter context.
        use super::*;
        use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};

        fn assert_column_ops_with_dialect(
            sql: &str,
            dialect: &dyn sqlparser::dialect::Dialect,
            expected: ColumnOperation,
        ) {
            let actual = extract_column_operations(dialect, sql, None)
                .unwrap()
                .into_iter()
                .next()
                .unwrap_or_else(|| panic!("no statements in result for SQL: {sql}"))
                .unwrap();
            assert_column_ops_inner(sql, 0, actual, expected);
        }

        /// Construct a `ColumnReference` for the synthetic EXCLUDED
        /// pseudo-table — used only as a Source in lineage edges, not
        /// as a real table.
        fn excluded(name: &str) -> ColumnReference {
            ColumnReference {
                table: Some(TableReference {
                    catalog: None,
                    schema: None,
                    name: "EXCLUDED".into(),
                }),
                name: name.into(),
            }
        }

        #[test]
        fn pg_on_conflict_do_update_set_excluded_emits_lineage_and_write() {
            // DO UPDATE SET b = EXCLUDED.b
            //   - writes: t.a, t.b from INSERT columns plus another
            //     t.b for the SET target.
            //   - reads: empty (EXCLUDED is synthetic-filtered;
            //     VALUES (1, 2) are literals).
            //   - lineage: EXCLUDED.b → Relation(t.b), Passthrough.
            assert_column_ops_with_dialect(
                "INSERT INTO t (a, b) VALUES (1, 2) ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
                &PostgreSqlDialect {},
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                    lineage: vec![passthrough(excluded("b"), relation("t", "b"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn pg_on_conflict_do_nothing_is_indistinguishable_from_plain_insert() {
            assert_column_ops_with_dialect(
                "INSERT INTO t (a, b) VALUES (1, 2) ON CONFLICT (a) DO NOTHING",
                &PostgreSqlDialect {},
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![write("t", "a"), write("t", "b")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn pg_insert_select_with_on_conflict_composes_excluded_to_source() {
            // EXCLUDED's body_projections come from the INSERT source
            // renamed to the target columns positionally. So
            // `EXCLUDED.b` composes through to the source's position-1
            // projection (`y` from s) — the conflict-action lineage edge
            // bottoms out at the same base table as the
            // source-projection lineage edge.
            assert_column_ops_with_dialect(
                "INSERT INTO t (a, b) SELECT x, y FROM s \
                 ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
                &PostgreSqlDialect {},
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "x"), read("s", "y")],
                    writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                    lineage: vec![
                        passthrough(col("s", "x"), relation("t", "a")),
                        passthrough(col("s", "y"), relation("t", "b")),
                        passthrough(col("s", "y"), relation("t", "b")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn mysql_on_duplicate_key_update_values_func_self_references_target() {
            // MySQL `VALUES(<col>)` is the implicit-row form. Without
            // an EXCLUDED binding, the inner `b` ref resolves to t.b
            // (the INSERT target). Result: t.b shows up as a read
            // (the VALUES function call is a value-changing wrapper) and
            // the SET clause adds a Relation-target lineage edge t.b → t.b.
            assert_column_ops_with_dialect(
                "INSERT INTO t (a, b) VALUES (1, 2) \
                 ON DUPLICATE KEY UPDATE b = VALUES(b)",
                &MySqlDialect {},
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t", "b")],
                    writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                    lineage: vec![transformation(col("t", "b"), relation("t", "b"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn pg_insert_union_with_on_conflict_excluded_fans_out_to_each_branch() {
            // The source has TWO ProjectionGroups (one per UNION
            // branch), so EXCLUDED's body_projections also have two
            // groups — each with a position-0 item named after the
            // INSERT target column. `EXCLUDED.a` then composes to
            // BOTH branches' position-0 source refs.
            assert_column_ops_with_dialect(
                "INSERT INTO t (a) SELECT x FROM s1 UNION SELECT y FROM s2 \
                 ON CONFLICT (a) DO UPDATE SET a = EXCLUDED.a",
                &PostgreSqlDialect {},
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s1", "x"), read("s2", "y")],
                    writes: vec![write("t", "a"), write("t", "a")],
                    lineage: vec![
                        passthrough(col("s1", "x"), relation("t", "a")),
                        passthrough(col("s2", "y"), relation("t", "a")),
                        passthrough(col("s1", "x"), relation("t", "a")),
                        passthrough(col("s2", "y"), relation("t", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn pg_insert_aggregate_with_on_conflict_excluded_keeps_transformation_kind() {
            // SUM(x) makes the source projection a Transformation. When
            // EXCLUDED.total composes back, compose_lineage_kinds keeps the
            // transforming step → lineage kind stays Transformation even on
            // the conflict-action path.
            assert_column_ops_with_dialect(
                "INSERT INTO t (total) SELECT SUM(x) FROM s \
                 ON CONFLICT (id) DO UPDATE SET total = EXCLUDED.total",
                &PostgreSqlDialect {},
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "x")],
                    writes: vec![write("t", "total"), write("t", "total")],
                    lineage: vec![
                        transformation(col("s", "x"), relation("t", "total")),
                        transformation(col("s", "x"), relation("t", "total")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn pg_on_conflict_do_update_with_where_clause_emits_read() {
            // DO UPDATE ... WHERE walks in filter context: `t.a` in the
            // WHERE expression surfaces as a read but not a lineage source.
            assert_column_ops_with_dialect(
                "INSERT INTO t (a, b) VALUES (1, 2) \
                 ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b WHERE t.a > 0",
                &PostgreSqlDialect {},
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t", "a")],
                    writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                    lineage: vec![passthrough(excluded("b"), relation("t", "b"))],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod values_as_relation {
        //! `VALUES` can stand in for a row-source in three positions:
        //! - INSERT … VALUES (already covered in `lineage` / `on_conflict`)
        //! - SELECT … FROM (VALUES …) AS t(x, y)   — derived table
        //! - WITH cte(x, y) AS (VALUES …) SELECT … — CTE body
        //!
        //! VALUES doesn't carry projection items the resolver can
        //! capture (literals have no source refs), so lineage from these
        //! variants bottom out at the synthetic binding — no
        //! composition to a base table is possible.
        use super::*;

        #[test]
        fn values_as_derived_table_with_aliases_emits_synthetic_refs_only() {
            // The derived table `t` carries schema [x, y] from the
            // alias rename, but its body_projections are empty (VALUES
            // contributes no ProjectionItems). So `t.x` is recorded as
            // a synthetic ref pointing at the derived binding; reads
            // filter it out, and lineage keeps `t.x` as the source
            // (composition can't substitute further).
            assert_column_ops(
                "SELECT x, y FROM (VALUES (1, 'a'), (2, 'b')) AS t(x, y)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![
                        ColumnLineageEdge {
                            source: ColumnReference {
                                table: Some(TableReference {
                                    catalog: None,
                                    schema: None,
                                    name: "t".into(),
                                }),
                                name: "x".into(),
                            },
                            target: out("x", 0),
                            kind: ColumnLineageKind::Passthrough,
                        },
                        ColumnLineageEdge {
                            source: ColumnReference {
                                table: Some(TableReference {
                                    catalog: None,
                                    schema: None,
                                    name: "t".into(),
                                }),
                                name: "y".into(),
                            },
                            target: out("y", 1),
                            kind: ColumnLineageKind::Passthrough,
                        },
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn values_as_cte_body_with_aliases_emits_synthetic_refs_only() {
            assert_column_ops(
                "WITH cte(id, val) AS (VALUES (1, 'a'), (2, 'b')) SELECT id FROM cte",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: Some(TableReference {
                                catalog: None,
                                schema: None,
                                name: "cte".into(),
                            }),
                            name: "id".into(),
                        },
                        target: out("id", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn values_with_column_ref_in_row_picks_up_outer_ref() {
            // A column ref inside a VALUES row (rare in practice but
            // syntactically valid) does get walked and surfaces in
            // reads — the outer table `t1` is in scope of the derived
            // table per the resolver's permissive scope-chain rule.
            assert_column_ops(
                "SELECT v.x FROM t1, (VALUES (t1.a)) AS v(x)",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: Some(TableReference {
                                catalog: None,
                                schema: None,
                                name: "v".into(),
                            }),
                            name: "x".into(),
                        },
                        target: out("x", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod alter_table {
        //! ALTER TABLE produces column-level writes for column-naming
        //! operations: ADD COLUMN, DROP COLUMN, RENAME COLUMN, CHANGE
        //! COLUMN, MODIFY COLUMN, ALTER COLUMN. RENAME / CHANGE surface
        //! BOTH the old and new names — both ends of the rename are
        //! useful for downstream lineage consumers tracking column
        //! history. Schema-level operations (constraints, partitions,
        //! RENAME TABLE) contribute no column writes.
        use super::*;

        #[test]
        fn alter_table_add_column_emits_write() {
            assert_column_ops(
                "ALTER TABLE t ADD COLUMN c INT",
                ColumnOperation {
                    statement_kind: StatementKind::AlterTable,
                    reads: vec![],
                    writes: vec![write("t", "c")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn alter_table_drop_column_emits_write() {
            assert_column_ops(
                "ALTER TABLE t DROP COLUMN c",
                ColumnOperation {
                    statement_kind: StatementKind::AlterTable,
                    reads: vec![],
                    writes: vec![write("t", "c")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn alter_table_rename_column_emits_both_old_and_new() {
            // RENAME moves data from old to new; surface both for
            // downstream consumers tracking column history.
            assert_column_ops(
                "ALTER TABLE t RENAME COLUMN a TO b",
                ColumnOperation {
                    statement_kind: StatementKind::AlterTable,
                    reads: vec![],
                    writes: vec![write("t", "a"), write("t", "b")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn alter_table_alter_column_emits_write_for_target_column() {
            assert_column_ops(
                "ALTER TABLE t ALTER COLUMN a SET NOT NULL",
                ColumnOperation {
                    statement_kind: StatementKind::AlterTable,
                    reads: vec![],
                    writes: vec![write("t", "a")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn alter_table_multiple_ops_collects_all_target_columns() {
            // sqlparser parses multi-op ALTER as a single statement
            // with `operations: Vec<AlterTableOperation>`.
            assert_column_ops(
                "ALTER TABLE t ADD COLUMN c INT, DROP COLUMN d",
                ColumnOperation {
                    statement_kind: StatementKind::AlterTable,
                    reads: vec![],
                    writes: vec![write("t", "c"), write("t", "d")],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn alter_table_add_constraint_emits_no_column_writes() {
            // AddConstraint is schema-level — no column-level writes
            // surface (the table itself stays in table_op writes).
            assert_column_ops(
                "ALTER TABLE t ADD CONSTRAINT uq UNIQUE (a)",
                ColumnOperation {
                    statement_kind: StatementKind::AlterTable,
                    reads: vec![],
                    writes: vec![],
                    lineage: vec![],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod returning {
        //! `RETURNING <select_items>` on INSERT / UPDATE / DELETE
        //! (Postgres / Sqlite extension) projects from the affected
        //! rows of the target table — treated like a top-level SELECT
        //! projection: each item contributes refs to `reads` and a
        //! `QueryOutput` lineage edge. Walked BEFORE the ON-clause for
        //! INSERT so any EXCLUDED binding doesn't ambify unqualified
        //! refs that collide with INSERT column names.
        use super::*;

        #[test]
        fn insert_values_with_returning_emits_target_reads_and_query_output() {
            assert_column_ops(
                "INSERT INTO t (a, b) VALUES (1, 2) RETURNING id",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t", "id")],
                    writes: vec![write("t", "a"), write("t", "b")],
                    lineage: vec![passthrough(col("t", "id"), out("id", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn returning_aliased_uses_alias_as_output_name() {
            assert_column_ops(
                "INSERT INTO t (a) VALUES (1) RETURNING id AS pk",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t", "id")],
                    writes: vec![write("t", "a")],
                    lineage: vec![passthrough(col("t", "id"), out("pk", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn returning_with_expression_marks_kind_transformation() {
            assert_column_ops(
                "INSERT INTO t (a) VALUES (1) RETURNING id + 1 AS bumped",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("t", "id")],
                    writes: vec![write("t", "a")],
                    lineage: vec![transformation(col("t", "id"), out("bumped", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn returning_wildcard_records_wildcard_suppressed_diagnostic() {
            assert_column_ops(
                "INSERT INTO t (a) VALUES (1) RETURNING *",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![],
                    writes: vec![write("t", "a")],
                    lineage: vec![],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
                },
            );
        }

        #[test]
        fn update_returning_walks_target_columns() {
            assert_column_ops(
                "UPDATE t SET a = b + 1 WHERE id = 5 RETURNING id, a",
                ColumnOperation {
                    statement_kind: StatementKind::Update,
                    reads: vec![
                        read("t", "b"),
                        read("t", "id"),
                        read("t", "id"),
                        read("t", "a"),
                    ],
                    writes: vec![write("t", "a")],
                    lineage: vec![
                        transformation(col("t", "b"), relation("t", "a")),
                        passthrough(col("t", "id"), out("id", 0)),
                        passthrough(col("t", "a"), out("a", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn delete_returning_walks_target_columns() {
            assert_column_ops(
                "DELETE FROM t WHERE id = 5 RETURNING id, val",
                ColumnOperation {
                    statement_kind: StatementKind::Delete,
                    reads: vec![read("t", "id"), read("t", "id"), read("t", "val")],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("t", "id"), out("id", 0)),
                        passthrough(col("t", "val"), out("val", 1)),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn insert_select_with_returning_keeps_source_lineage_and_target_returning() {
            // Source SELECT's tables are out of scope by the time
            // RETURNING walks (their nested scope was popped after
            // resolve_query). So RETURNING refs resolve to the target
            // table alone, even when the bare name `id` exists in the
            // source too.
            assert_column_ops(
                "INSERT INTO t (a) SELECT x FROM s RETURNING id",
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "x"), read("t", "id")],
                    writes: vec![write("t", "a")],
                    lineage: vec![
                        passthrough(col("s", "x"), relation("t", "a")),
                        passthrough(col("t", "id"), out("id", 0)),
                    ],
                    diagnostics: vec![],
                },
            );
        }
    }

    mod catalog_strict {
        use super::*;
        use crate::catalog::{Catalog, ColumnSchema};
        use sqlparser::ast::Ident;
        use std::collections::HashMap;

        #[derive(Debug, Default)]
        struct TestCatalog {
            tables: HashMap<String, Vec<&'static str>>,
        }

        impl TestCatalog {
            fn with(mut self, name: &str, cols: Vec<&'static str>) -> Self {
                self.tables.insert(name.to_string(), cols);
                self
            }
        }

        impl Catalog for TestCatalog {
            fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>> {
                self.tables.get(table.name.value.as_str()).map(|cols| {
                    cols.iter()
                        .map(|c| ColumnSchema {
                            name: c.to_string(),
                        })
                        .collect()
                })
            }
        }

        fn assert_column_ops_with_catalog(
            sql: &str,
            catalog: &dyn Catalog,
            expected: ColumnOperation,
        ) {
            let actual = extract_column_operations(&GenericDialect {}, sql, Some(catalog))
                .unwrap()
                .into_iter()
                .next()
                .unwrap()
                .unwrap();
            assert_column_ops_inner(sql, 0, actual, expected);
        }

        #[test]
        fn catalog_known_schema_rejects_columns_not_in_table() {
            // Without catalog `SELECT a FROM t1` resolves a → t1.a
            // unconditionally (single Unknown binding heuristic). With
            // a catalog that says t1's columns are [x, y], `a` cannot
            // come from t1 — it surfaces as unresolved and fires
            // UnresolvedColumn.
            let catalog = TestCatalog::default().with("t1", vec!["x", "y"]);
            assert_column_ops_with_catalog(
                "SELECT a FROM t1",
                &catalog,
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![unresolved("a")],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: None,
                            name: "a".into(),
                        },
                        target: out("a", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnresolvedColumn)],
                },
            );
        }

        #[test]
        fn catalog_known_schema_resolves_columns_present_in_table() {
            let catalog = TestCatalog::default().with("t1", vec!["a", "b"]);
            assert_column_ops_with_catalog(
                "SELECT a FROM t1",
                &catalog,
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_resolves_unquoted_ref_case_insensitively() {
            // The catalog declares `id` (lowercase); an unquoted `ID`
            // folds to the same key, so it resolves to t1. The column
            // name surfaces as written (`ID`) — folding governs matching,
            // not the surfaced identity.
            let catalog = TestCatalog::default().with("t1", vec!["id"]);
            assert_column_ops_with_catalog(
                "SELECT ID FROM t1",
                &catalog,
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "ID")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "ID"), out("ID", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_does_not_match_quoted_ref_against_unquoted_column() {
            // A quoted `"ID"` matches exactly (case-sensitive), so it does
            // not match the catalog's `id`; it stays unresolved and fires
            // UnresolvedColumn. Placed in WHERE so it is a read but not a
            // lineage source.
            let catalog = TestCatalog::default().with("t1", vec!["a", "id"]);
            assert_column_ops_with_catalog(
                r#"SELECT a FROM t1 WHERE "ID" > 0"#,
                &catalog,
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![
                        read("t1", "a"),
                        ColumnReference {
                            table: None,
                            name: Ident::with_quote('"', "ID"),
                        },
                    ],
                    writes: vec![],
                    lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnresolvedColumn)],
                },
            );
        }

        #[test]
        fn catalog_insert_without_explicit_columns_pairs_via_catalog_schema() {
            // INSERT INTO t SELECT a, b FROM s — no explicit column
            // list. With t = [x, y, z] in catalog, the resolver pairs
            // source projections positionally (s.a → t.x, s.b → t.y).
            // Unpaired catalog cols (z) get no lineage / no write.
            let catalog = TestCatalog::default().with("t", vec!["x", "y", "z"]);
            assert_column_ops_with_catalog(
                "INSERT INTO t SELECT a, b FROM s",
                &catalog,
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "a"), read("s", "b")],
                    writes: vec![write("t", "x"), write("t", "y")],
                    lineage: vec![
                        passthrough(col("s", "a"), relation("t", "x")),
                        passthrough(col("s", "b"), relation("t", "y")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_insert_without_explicit_columns_source_longer_than_target() {
            // 3 source projections vs t = [x, y] — pair what fits,
            // surplus source column gets no lineage.
            let catalog = TestCatalog::default().with("t", vec!["x", "y"]);
            assert_column_ops_with_catalog(
                "INSERT INTO t SELECT a, b, c FROM s",
                &catalog,
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "a"), read("s", "b"), read("s", "c")],
                    writes: vec![write("t", "x"), write("t", "y")],
                    lineage: vec![
                        passthrough(col("s", "a"), relation("t", "x")),
                        passthrough(col("s", "b"), relation("t", "y")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_insert_explicit_columns_override_catalog_schema() {
            // Explicit (q) wins over catalog [x, y, z].
            let catalog = TestCatalog::default().with("t", vec!["x", "y", "z"]);
            assert_column_ops_with_catalog(
                "INSERT INTO t (q) SELECT a FROM s",
                &catalog,
                ColumnOperation {
                    statement_kind: StatementKind::Insert,
                    reads: vec![read("s", "a")],
                    writes: vec![write("t", "q")],
                    lineage: vec![passthrough(col("s", "a"), relation("t", "q"))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_merge_not_matched_insert_no_cols_pairs_via_catalog() {
            // Same catalog fallback applies to MERGE's INSERT clause:
            // lineage is paired via catalog. Surprise surfaced by whole-
            // value compare: writes stay empty for catalog-paired MERGE
            // INSERT — only `INSERT (cols) VALUES (...)` with an
            // explicit column list populates writes.
            let catalog = TestCatalog::default().with("t", vec!["id", "a"]);
            assert_column_ops_with_catalog(
                "MERGE INTO t USING s ON t.id = s.id \
                 WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.a)",
                &catalog,
                ColumnOperation {
                    statement_kind: StatementKind::Merge,
                    reads: vec![
                        read("t", "id"),
                        read("s", "id"),
                        read("s", "id"),
                        read("s", "a"),
                    ],
                    writes: vec![],
                    lineage: vec![
                        passthrough(col("s", "id"), relation("t", "id")),
                        passthrough(col("s", "a"), relation("t", "a")),
                    ],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_disambiguates_join_unqualified_ref() {
            // Both tables are Known via catalog; only t2 has `a`, so
            // unqualified `a` in `t1 JOIN t2` resolves to t2 (no
            // catalog: same SQL would be ambiguous).
            let catalog = TestCatalog::default()
                .with("t1", vec!["id"])
                .with("t2", vec!["id", "a"]);
            assert_column_ops_with_catalog(
                "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id",
                &catalog,
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id"), read("t2", "id"), read("t2", "a")],
                    writes: vec![],
                    lineage: vec![passthrough(col("t2", "a"), out("a", 0))],
                    diagnostics: vec![],
                },
            );
        }

        #[test]
        fn catalog_confirmed_ambiguity_reports_diagnostic() {
            // Both tables Known and both declare `a`. ColumnLevelDiagnostic must
            // fire — without catalog the same query is silently
            // ambiguous (no diagnostic) since Unknown schemas could
            // contain anything. assert_column_ops compares diagnostics
            // by kind only; the message-content checks are kept inline
            // since they're this test's specific purpose.
            let catalog = TestCatalog::default()
                .with("t1", vec!["a"])
                .with("t2", vec!["a"]);
            assert_column_ops_with_catalog(
                "SELECT a FROM t1 JOIN t2 ON t1.a = t2.a",
                &catalog,
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "a"), read("t2", "a"), unresolved("a")],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: None,
                            name: "a".into(),
                        },
                        target: out("a", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::AmbiguousColumn)],
                },
            );
            // Specific message-content checks for this test's purpose.
            let ops = extract_column_operations(
                &GenericDialect {},
                "SELECT a FROM t1 JOIN t2 ON t1.a = t2.a",
                Some(&catalog),
            )
            .unwrap();
            let ops = ops.into_iter().next().unwrap().unwrap();
            let amb = ops
                .diagnostics
                .iter()
                .find(|d| matches!(d.kind, ColumnLevelDiagnosticKind::AmbiguousColumn))
                .expect("AmbiguousColumn must fire");
            assert!(amb.message.contains("ambiguous column `a`"));
            assert!(amb.message.contains("t1"));
            assert!(amb.message.contains("t2"));
        }

        #[test]
        fn catalog_unresolved_unqualified_reports_diagnostic() {
            // Catalog says t1 has [x, y]; unqualified `z` belongs to
            // nothing in scope — UnresolvedColumn fires.
            let catalog = TestCatalog::default().with("t1", vec!["x", "y"]);
            assert_column_ops_with_catalog(
                "SELECT z FROM t1",
                &catalog,
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![unresolved("z")],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: None,
                            name: "z".into(),
                        },
                        target: out("z", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnresolvedColumn)],
                },
            );
            // Message-content check for this test's purpose.
            let ops =
                extract_column_operations(&GenericDialect {}, "SELECT z FROM t1", Some(&catalog))
                    .unwrap();
            let ops = ops.into_iter().next().unwrap().unwrap();
            let unr = ops
                .diagnostics
                .iter()
                .find(|d| matches!(d.kind, ColumnLevelDiagnosticKind::UnresolvedColumn))
                .expect("UnresolvedColumn must fire");
            assert!(unr.message.contains("unresolved column `z`"));
        }

        #[test]
        fn no_catalog_unqualified_is_silent_even_when_ambiguous_shape() {
            // No catalog → all schemas are Unknown → resolver can't
            // tell whether `a` is genuinely in both t1 and t2, only one,
            // or neither. Two diagnostic kinds are intentionally
            // suppressed in this mode: AmbiguousColumn (no confirmed
            // matches) and UnresolvedColumn (no Known schemas in scope).
            // The resolution itself still returns None for the column,
            // and the lineage source is also unresolved.
            assert_column_ops(
                "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id",
                ColumnOperation {
                    statement_kind: StatementKind::Select,
                    reads: vec![read("t1", "id"), read("t2", "id"), unresolved("a")],
                    writes: vec![],
                    lineage: vec![ColumnLineageEdge {
                        source: ColumnReference {
                            table: None,
                            name: "a".into(),
                        },
                        target: out("a", 0),
                        kind: ColumnLineageKind::Passthrough,
                    }],
                    diagnostics: vec![],
                },
            );
        }
    }
}
