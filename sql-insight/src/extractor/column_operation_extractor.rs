//! Extracts the column-level operations a SQL statement performs.
//!
//! Where [`extract_table_operations`](crate::extract_table_operations)
//! answers "what tables does this statement touch / write / flow", this
//! module answers the same questions at column granularity.
//!
//! The output mirrors `StatementTableOperations` — three parallel
//! surfaces (`reads`, `writes`, `flows`) — plus a small enrichment on
//! flow edges to distinguish passthrough projections from computed
//! expressions.
//!
//! **Status:** type skeleton only. The extractor currently returns an
//! empty [`StatementColumnOperations`] for every parsed statement;
//! column reference collection, scope-chain resolution, and `SELECT *`
//! expansion arrive in later phases.

use crate::catalog::Catalog;
use crate::error::Error;
use crate::extractor::operation_extractor::{
    OperationDiagnostic, OperationDiagnosticCode, StatementKind,
};
use crate::relation::TableReference;
use sqlparser::ast::{Ident, Statement};
use sqlparser::dialect::Dialect;
use sqlparser::parser::Parser;

/// Convenience function to extract column-level operations from SQL.
///
/// `catalog` is consulted for relation-level enrichment as well as
/// future column-level needs (`SELECT *` expansion, ambiguous
/// unqualified column resolution). Pass `None` for the lightest path —
/// the MVP does not consult the catalog yet, but the signature is fixed
/// so callers don't have to migrate when it does.
pub fn extract_column_operations(
    dialect: &dyn Dialect,
    sql: &str,
    catalog: Option<&dyn Catalog>,
) -> Result<Vec<Result<StatementColumnOperations, Error>>, Error> {
    ColumnOperationExtractor::extract(dialect, sql, catalog)
}

/// Column-level operations performed by a single SQL statement.
///
/// Mirrors [`StatementTableOperations`](crate::StatementTableOperations)
/// with the same three surfaces — `reads`, `writes`, `flows` — at
/// column granularity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementColumnOperations {
    pub statement_kind: StatementKind,
    pub reads: Vec<ColumnRead>,
    pub writes: Vec<ColumnWrite>,
    pub flows: Vec<ColumnFlow>,
    pub diagnostics: Vec<OperationDiagnostic>,
}

/// A column-level identity reference: an optional owning table plus the
/// column name.
///
/// `table` is `Option` because some column references cannot be
/// resolved structurally (ambiguous unqualified columns, references to
/// derived tables we do not yet expand, etc.) — in that case a
/// diagnostic accompanies the operation. Identity is name-based: two
/// `ColumnReference`s with the same `table` and `name` compare equal,
/// independent of where they appeared in the SQL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColumnReference {
    pub table: Option<TableReference>,
    pub name: Ident,
}

/// A column referenced as a Read source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnRead {
    pub column: ColumnReference,
}

/// A column that the statement writes to — an INSERT target column,
/// an UPDATE SET target, a MERGE WHEN clause target, or a column of
/// the new relation produced by CTAS / CREATE VIEW.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnWrite {
    pub column: ColumnReference,
}

/// A column-level flow edge: data from `source` contributes to
/// `target`. Emitted for both persisted-target statements (INSERT /
/// UPDATE / MERGE / CTAS / CREATE VIEW) and bare SELECT (where target
/// is a `ColumnTarget::QueryOutput`).
///
/// One edge per (source, target) pair: `SELECT a + b FROM t1` emits two
/// flows, both from `t1.a` and `t1.b` to the same query-output target,
/// each tagged `Computed`.
///
/// Statements that physically move data emit composed end-to-end flows
/// — `INSERT INTO t1 (col) SELECT b FROM t2` emits `t2.b → t1.col`
/// directly, with no intermediate query-output entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnFlow {
    pub source: ColumnReference,
    pub target: ColumnTarget,
    pub kind: ColumnFlowKind,
}

/// The target endpoint of a [`ColumnFlow`].
///
/// `Persisted` covers columns that live in a real relation (table or
/// view) and receive a value from the statement (INSERT target,
/// UPDATE SET target, MERGE INSERT/UPDATE target, CTAS / CREATE VIEW
/// output column).
///
/// `QueryOutput` covers transient columns produced by a SELECT
/// projection that is not piped into a persisted relation. `name`
/// follows the projection: the alias if explicit, the bare column name
/// if the projection is a single column, otherwise `None`. `position`
/// is always set so anonymous outputs can be identified.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ColumnTarget {
    Persisted(ColumnReference),
    QueryOutput {
        name: Option<Ident>,
        position: usize,
    },
}

/// How a source column contributes to its target.
///
/// MVP carries two variants:
/// - `Passthrough` — the source value is forwarded unchanged
///   (`SELECT a FROM t1`, `INSERT INTO t1 (a) SELECT b FROM t2`).
/// - `Computed` — the source feeds an expression that produces the
///   target (`SELECT a + b FROM t1`, both `a` and `b` are `Computed`).
///
/// More variants (`Aggregation`, plus predicate-influence kinds like
/// `Filter` / `Join` / `GroupBy` / `Sort` / `Window` / `Conditional`)
/// will be added incrementally as later phases tighten the
/// classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColumnFlowKind {
    Passthrough,
    Computed,
}

/// Extracts column-level operations from SQL.
#[derive(Default, Debug)]
pub struct ColumnOperationExtractor;

impl ColumnOperationExtractor {
    pub fn extract(
        dialect: &dyn Dialect,
        sql: &str,
        catalog: Option<&dyn Catalog>,
    ) -> Result<Vec<Result<StatementColumnOperations, Error>>, Error> {
        let statements = Parser::parse_sql(dialect, sql)?;
        Ok(statements
            .iter()
            .map(|s| Self::extract_from_statement(s, catalog))
            .collect())
    }

    pub fn extract_from_statement(
        statement: &Statement,
        _catalog: Option<&dyn Catalog>,
    ) -> Result<StatementColumnOperations, Error> {
        let kind = super::operation_extractor::classify_statement(statement);
        let mut diagnostics = Vec::new();
        if matches!(kind, StatementKind::Unsupported) {
            diagnostics.push(OperationDiagnostic {
                code: OperationDiagnosticCode::UnsupportedStatement,
                message: format!(
                    "Unsupported statement for column operation extraction: {}",
                    statement
                ),
            });
        }
        Ok(StatementColumnOperations {
            statement_kind: kind,
            reads: Vec::new(),
            writes: Vec::new(),
            flows: Vec::new(),
            diagnostics,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::GenericDialect;

    fn extract(sql: &str) -> StatementColumnOperations {
        let mut result = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
        result.remove(0).unwrap()
    }

    #[test]
    fn select_yields_empty_lists() {
        let ops = extract("SELECT a, b FROM t1");
        assert_eq!(ops.statement_kind, StatementKind::Select);
        assert!(ops.reads.is_empty());
        assert!(ops.writes.is_empty());
        assert!(ops.flows.is_empty());
        assert!(ops.diagnostics.is_empty());
    }

    #[test]
    fn insert_yields_empty_lists() {
        let ops = extract("INSERT INTO t1 (a) SELECT b FROM t2");
        assert_eq!(ops.statement_kind, StatementKind::Insert);
        assert!(ops.reads.is_empty());
        assert!(ops.writes.is_empty());
        assert!(ops.flows.is_empty());
    }

    #[test]
    fn update_yields_empty_lists() {
        let ops = extract("UPDATE t1 SET a = 1");
        assert_eq!(ops.statement_kind, StatementKind::Update);
        assert!(ops.reads.is_empty());
        assert!(ops.writes.is_empty());
        assert!(ops.flows.is_empty());
    }

    #[test]
    fn unsupported_statement_reports_diagnostic() {
        let ops = extract("CREATE INDEX idx ON t1 (a)");
        assert_eq!(ops.statement_kind, StatementKind::Unsupported);
        assert_eq!(ops.diagnostics.len(), 1);
        assert_eq!(
            ops.diagnostics[0].code,
            OperationDiagnosticCode::UnsupportedStatement
        );
    }

    #[test]
    fn multiple_statements_produce_multiple_results() {
        let result = extract_column_operations(
            &GenericDialect {},
            "SELECT a FROM t1; SELECT b FROM t2",
            None,
        )
        .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0].as_ref().unwrap().statement_kind,
            StatementKind::Select
        );
        assert_eq!(
            result[1].as_ref().unwrap().statement_kind,
            StatementKind::Select
        );
    }
}
