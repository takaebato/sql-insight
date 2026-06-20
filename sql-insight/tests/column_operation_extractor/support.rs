//! Shared harness for the `column_operations` integration tests:
//! re-exports of the public surface plus the value-builder helpers and the
//! multiset-equality macro the thematic modules use.

pub use sql_insight::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
pub use sql_insight::extractor::{
    extract_column_operations, extract_column_operations_with_options, ColumnLineageEdge,
    ColumnLineageKind, ColumnOperation, ColumnTarget, ExtractorOptions, StatementKind,
};
pub use sql_insight::sqlparser::ast::Ident;
pub use sql_insight::sqlparser::dialect::{Dialect, GenericDialect};
pub use sql_insight::{ColumnRead, ColumnReference, ResolutionKind, TableReference};

/// Order-insensitive multiset equality for the `reads` / `lineage`
/// surfaces. The public API returns them in source order, but these tests
/// compare as multisets to stay span-agnostic and focus on membership +
/// multiplicity — the source-order guarantee itself is pinned separately by
/// `reads_are_returned_in_source_order`. Compares via `PartialEq`
/// (sqlparser `Ident` equality already ignores spans).
macro_rules! assert_unordered_eq {
    ($actual:expr, $expected:expr $(,)?) => {{
        let actual = $actual;
        let mut remaining = $expected;
        // Tie the element types so an empty-vs-empty comparison still infers.
        let _ = actual.iter().chain(remaining.iter()).count();
        assert_eq!(
            actual.len(),
            remaining.len(),
            "length mismatch\n  actual:   {actual:#?}\n  expected: {remaining:#?}"
        );
        for item in &actual {
            match remaining.iter().position(|e| e == item) {
                Some(i) => {
                    remaining.remove(i);
                }
                None => panic!("unexpected item not in expected: {item:#?}\n  actual: {actual:#?}"),
            }
        }
    }};
}

pub fn extract(sql: &str) -> ColumnOperation {
    let mut result = extract_column_operations(&GenericDialect {}, sql).unwrap();
    result.remove(0).unwrap()
}

pub fn table(name: &str) -> TableReference {
    TableReference {
        catalog: None,
        schema: None,
        name: name.into(),
    }
}

/// The canonical identity a catalog-matched table surfaces with. The
/// catalog-aware test modules all register tables under a `public`
/// schema, and a unique match canonicalizes the reference to that full
/// path — so `Cataloged` reads / writes / lineage carry `public.<name>`,
/// not the bare name written in the SQL. The canonical identity is
/// case-exact, so its segments surface **quoted** (the dialect's quote;
/// `"` for the GenericDialect these tests use). Used by `read_confirmed` /
/// `col_confirmed` and the catalog modules' write / relation helpers.
pub fn cataloged_table(name: &str) -> TableReference {
    TableReference {
        catalog: None,
        schema: Some(Ident::with_quote('"', "public")),
        name: Ident::with_quote('"', name),
    }
}

// Read-side helpers return `ColumnRead` (identity + ResolutionKind).
// `read` and `col` both default to `ResolutionKind::Inferred`, which is
// the catalog-less mode's natural resolution — most tests in this
// file run without a catalog, so the default minimises noise. Tests
// supplying a catalog override with `read_confirmed` / `col_confirmed`
// (which carry the canonicalized `public.<name>` identity).
pub fn read(table_name: &str, col: &str) -> ColumnRead {
    read_with(table_name, col, ResolutionKind::Inferred)
}

pub fn read_confirmed(table_name: &str, col: &str) -> ColumnRead {
    read_with_ref(cataloged_table(table_name), col, ResolutionKind::Cataloged)
}

pub fn read_with(table_name: &str, col: &str, resolution: ResolutionKind) -> ColumnRead {
    read_with_ref(table(table_name), col, resolution)
}

pub fn read_with_ref(
    table_ref: TableReference,
    col: &str,
    resolution: ResolutionKind,
) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: Some(table_ref),
            name: col.into(),
        },
        resolution,
    }
}

pub fn col(table_name: &str, name: &str) -> ColumnRead {
    read_with(table_name, name, ResolutionKind::Inferred)
}

pub fn col_confirmed(table_name: &str, name: &str) -> ColumnRead {
    read_with_ref(cataloged_table(table_name), name, ResolutionKind::Cataloged)
}

pub fn ambiguous(col: &str) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: None,
            name: col.into(),
        },
        resolution: ResolutionKind::Ambiguous,
    }
}

pub fn unresolved(col: &str) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: None,
            name: col.into(),
        },
        resolution: ResolutionKind::Unresolved,
    }
}

// Write-side helpers stay as `ColumnReference` — write targets come
// straight from SQL syntax and are always `ResolutionKind::Cataloged` by
// construction, so attaching a resolution field would be dead weight.
pub fn write(table_name: &str, col: &str) -> ColumnReference {
    ColumnReference {
        table: Some(table(table_name)),
        name: col.into(),
    }
}

pub fn out(name: &str, position: usize) -> ColumnTarget {
    ColumnTarget::QueryOutput {
        name: Some(name.into()),
        position,
    }
}

pub fn out_anon(position: usize) -> ColumnTarget {
    ColumnTarget::QueryOutput {
        name: None,
        position,
    }
}

pub fn relation(table_name: &str, col: &str) -> ColumnTarget {
    ColumnTarget::Relation(ColumnReference {
        table: Some(table(table_name)),
        name: col.into(),
    })
}

pub fn passthrough(source: ColumnRead, target: ColumnTarget) -> ColumnLineageEdge {
    ColumnLineageEdge {
        source,
        target,
        kind: ColumnLineageKind::Passthrough,
    }
}

pub fn transformation(source: ColumnRead, target: ColumnTarget) -> ColumnLineageEdge {
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
pub fn assert_column_ops(sql: &str, expected: ColumnOperation) {
    assert_nth_column_ops(sql, 0, expected);
}

/// Like `assert_column_ops` but for multi-statement batches —
/// targets the statement at `index`. Compose multiple calls to
/// pin down each statement in a batch independently.
pub fn assert_nth_column_ops(sql: &str, index: usize, expected: ColumnOperation) {
    let actual = extract_column_operations(&GenericDialect {}, sql)
        .unwrap()
        .into_iter()
        .nth(index)
        .unwrap_or_else(|| panic!("statement {index} missing in result for SQL: {sql}"))
        .unwrap();
    assert_column_ops_inner(sql, index, actual, expected);
}

pub fn assert_column_ops_inner(
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
    // `reads` / `lineage` come back in source order; these compare as
    // multisets to stay span-agnostic (the order is pinned separately), while
    // `writes` follow the source column order.
    assert_unordered_eq!(actual.reads, reads);
    assert_eq!(
        actual.writes, writes,
        "writes for SQL: {sql} (statement {index})"
    );
    assert_unordered_eq!(actual.lineage, lineage);
    let actual_kinds: Vec<_> = actual.diagnostics.iter().map(|d| d.kind.clone()).collect();
    let expected_kinds: Vec<_> = diagnostics.iter().map(|d| d.kind.clone()).collect();
    assert_eq!(
        actual_kinds, expected_kinds,
        "diagnostic kinds for SQL: {sql} (statement {index})"
    );
}

/// Placeholder `ColumnLevelDiagnostic` for `assert_column_ops.expected.diagnostics`.
/// Only the kind is compared; message and span are placeholders.
pub fn diag(kind: ColumnLevelDiagnosticKind) -> ColumnLevelDiagnostic {
    ColumnLevelDiagnostic {
        kind,
        message: String::new(),
        span: None,
    }
}
