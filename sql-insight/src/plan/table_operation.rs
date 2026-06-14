//! Plan-based table-level operation extraction: assembles the public
//! [`TableOperation`] from a bound [`Plan`](super::ir::Plan). It reuses
//! the resolver-independent [`classify_statement`] for the statement verb,
//! walks the plan for the `reads` / `writes` / `lineage` surfaces, and
//! projects the column-level diagnostics down to the table level. The
//! differential harness in [`super::extract`] pins it against the live
//! resolver-based extractor.

use sqlparser::ast::Statement;

use crate::catalog::Catalog;
use crate::diagnostic::{TableLevelDiagnostic, TableLevelDiagnosticKind};
use crate::extractor::{classify_statement, StatementKind, TableOperation};
use crate::resolver::IdentifierCasing;

/// Build the table-level operation for one statement from its bound plan.
/// A statement kind the binder doesn't model (or can't bind) yields an
/// empty operation carrying an `UnsupportedStatement` diagnostic, mirroring
/// the resolver-based extractor.
pub(crate) fn table_operation(
    statement: &Statement,
    catalog: Option<&Catalog>,
    casing: IdentifierCasing,
) -> TableOperation {
    let statement_kind = classify_statement(statement);
    if statement_kind == StatementKind::Unsupported {
        return unsupported(statement_kind, statement);
    }
    let (plan, column_diagnostics) =
        super::binder::build_with_diagnostics(statement, catalog, casing);
    let Some(plan) = plan else {
        // Classified as supported but unbindable — treat as unsupported.
        return unsupported(statement_kind, statement);
    };
    // Lineage is only for statements that move data into a target. A
    // column-less INSERT and a DELETE both bind to a `Write`, so the
    // structural walk can't tell them apart — gate on the kind.
    let lineage = if moves_data(&statement_kind) {
        super::extract::extract_table_lineage(&plan)
    } else {
        Vec::new()
    };
    TableOperation {
        statement_kind,
        reads: super::extract::extract_table_reads(&plan),
        writes: super::extract::extract_table_writes(&plan),
        lineage,
        // Table-level diagnostics are the column-level ones projected down
        // (only `UnsupportedStatement` survives the projection).
        diagnostics: column_diagnostics
            .iter()
            .filter_map(|d| d.to_table_level())
            .collect(),
    }
}

/// Whether a statement physically moves data into its target (so it emits
/// table lineage). `DELETE` / `DROP` / `TRUNCATE` / `ALTER TABLE` touch a
/// target but feed it nothing; a bare `SELECT` has no target.
fn moves_data(kind: &StatementKind) -> bool {
    matches!(
        kind,
        StatementKind::Insert
            | StatementKind::Update
            | StatementKind::Merge
            | StatementKind::CreateTable
            | StatementKind::CreateView
            | StatementKind::AlterView
    )
}

fn unsupported(statement_kind: StatementKind, statement: &Statement) -> TableOperation {
    TableOperation {
        statement_kind,
        reads: Vec::new(),
        writes: Vec::new(),
        lineage: Vec::new(),
        diagnostics: vec![TableLevelDiagnostic {
            kind: TableLevelDiagnosticKind::UnsupportedStatement,
            message: format!("Unsupported statement for plan-based extraction: {statement}"),
            span: None,
        }],
    }
}
