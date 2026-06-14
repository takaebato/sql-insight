//! Plan-based table-level operation extraction: assembles the public
//! [`TableOperation`] from a bound [`Plan`](super::ir::Plan). It reuses
//! the resolver-independent [`classify_statement`] for the statement verb,
//! walks the plan for the `reads` / `writes` surfaces, and projects the
//! column-level diagnostics down to the table level. The differential
//! harness in [`super::extract`] pins it against the live resolver-based
//! extractor.
//!
//! `lineage` is a follow-up brick — table-level lineage has its own
//! subtleties (predicate-subquery exclusion, CTE transitivity) — so it is
//! left empty here and excluded from the differential comparison for now.

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
    match plan {
        Some(plan) => TableOperation {
            statement_kind,
            reads: super::extract::extract_table_reads(&plan),
            writes: super::extract::extract_table_writes(&plan),
            // Lineage is a follow-up brick (see module docs).
            lineage: Vec::new(),
            // Table-level diagnostics are the column-level ones projected
            // down (only `UnsupportedStatement` survives the projection).
            diagnostics: column_diagnostics
                .iter()
                .filter_map(|d| d.to_table_level())
                .collect(),
        },
        // Classified as supported but unbindable — treat as unsupported.
        None => unsupported(statement_kind, statement),
    }
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
