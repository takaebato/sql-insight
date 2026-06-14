//! Plan-based operation extraction: assembles the public
//! [`ColumnOperation`] from a bound [`Plan`](super::ir::Plan), the surface
//! the extractor switch will expose. It reuses the resolver-independent
//! [`classify_statement`] for the statement verb, walks the plan for the
//! `reads` / `writes` / `lineage` surfaces, and reports the tool-side
//! diagnostics. The differential harness in [`super::extract`] pins it
//! against the live resolver-based extractor.

use sqlparser::ast::Statement;

use crate::catalog::Catalog;
use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::extractor::{classify_statement, ColumnOperation, StatementKind};
use crate::resolver::IdentifierCasing;

/// Build the column-level operation for one statement from its bound plan.
/// A statement kind the binder doesn't model (or can't bind) yields an
/// empty operation carrying an `UnsupportedStatement` diagnostic, mirroring
/// the resolver-based extractor.
pub(crate) fn column_operation(
    statement: &Statement,
    catalog: Option<&Catalog>,
    casing: IdentifierCasing,
) -> ColumnOperation {
    let statement_kind = classify_statement(statement);
    if statement_kind == StatementKind::Unsupported {
        return unsupported(statement_kind, statement);
    }
    let (plan, diagnostics) = super::binder::build_with_diagnostics(statement, catalog, casing);
    // A supported statement kind that binds to no plan (`DROP` / `TRUNCATE`
    // and other structure-only DDL) has empty surfaces but is *not*
    // unsupported — it carries no diagnostic.
    let plan = plan.unwrap_or(super::ir::Plan::OpaqueLeaf);
    ColumnOperation {
        statement_kind,
        reads: super::extract::extract_reads(&plan),
        writes: super::extract::extract_writes(&plan),
        lineage: super::extract::extract_lineage(&plan),
        // The bind accumulates WildcardSuppressed for each suppressed
        // projection wildcard (nested ones included).
        diagnostics,
    }
}

fn unsupported(statement_kind: StatementKind, statement: &Statement) -> ColumnOperation {
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
