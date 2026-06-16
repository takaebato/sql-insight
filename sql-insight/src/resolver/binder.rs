//! The binder: lowers a `sqlparser` AST into the bound [`Plan`] IR,
//! resolving every column reference bottom-up.
//!
//! Resolution runs against a [`Scope`] threaded up through the bind (the
//! relations visible at the current node). The scope is bind-time
//! *scratch* — never stored on the [`Plan`], which keeps only resolved
//! provenance / reads. With a [`Catalog`] a relation's columns are
//! `Known` (resolution becomes strict — `Cataloged` hits, `Unresolved`
//! denials, narrowed candidates); catalog-free they are `Open` and
//! resolution is best-effort (`Inferred` / `Ambiguous`). Catalog matching
//! is right-anchored and dialect-cased (via [`crate::casing`]).

use sqlparser::ast::{
    AccessExpr, AlterTable, AlterTableOperation, Array, ConnectByKind, CreateTable, CreateView,
    Cte, Delete, DictionaryField, Distinct, Expr, FromTable, Function, FunctionArg,
    FunctionArgExpr, FunctionArgumentClause, FunctionArgumentList, FunctionArguments, GroupByExpr,
    GroupByWithModifier, Ident, Insert, Join, JoinConstraint, JoinOperator, LimitClause,
    ListAggOnOverflow, Map, Merge, MergeAction, MergeInsertKind, NamedWindowExpr, ObjectName,
    ObjectType, OnConflictAction, OnInsert, OrderBy, OrderByExpr, OrderByKind, PipeOperator, Query,
    Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Statement, Subscript, Table,
    TableAlias, TableFactor, TableObject, TableWithJoins, TopQuantity, Update, UpdateTableFromKind,
    Values, WindowFrameBound, WindowSpec, WindowType,
};

use std::cell::RefCell;

// Bind-time vocabulary and scratch (types + their helpers).
mod catalog_match;
mod collect;
mod helpers;
mod scope;

// The bind engine, split by clause family. Each is an `impl Binder<'_>`
// block over `use super::*;`; this root keeps only the shared context
// (the `Binder` struct, its constructors, and the entry points).
mod expr;
mod query;
mod resolve;
mod statement;

use self::catalog_match::{canonical_ref, catalog_table_matches, fill_query_defaults, TableMatch};
use self::collect::{BoundValue, ExprCollector};
use self::helpers::{
    ambiguous, assignment_target_columns, object_name_last_ident, passthrough, read, unresolved,
    wrap_reads,
};
use self::scope::{CteRelation, Relation, RelationColumns, RelationSource, Scope};
use super::ir::{
    BoundColumn, CtePlan, CteRef, DeletePlan, PassThrough, Plan, Project, ProvenanceSource, Scan,
    ScanRole, SetOp, With, Write,
};
use crate::casing::IdentifierCasing;
use crate::catalog::Catalog;
use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ResolutionKind, TableReference};
use sqlparser::tokenizer::Span;

/// Bind one statement into a [`Plan`] (or `None` for statement kinds not
/// modelled — queries and the data-moving DML / DDL are; other DDL and
/// session statements aren't), returning the column-level diagnostics the
/// bind accumulated (currently `WildcardSuppressed` for each suppressed
/// projection wildcard). The diagnostics buffer is shared across child
/// binders (CTE bodies, subqueries), so nested wildcards are reported too.
/// The top-level scope is discarded — callers consume the resolved tree.
pub(crate) fn build_with_diagnostics(
    statement: &Statement,
    catalog: Option<&Catalog>,
    casing: IdentifierCasing,
) -> (Option<Plan>, Vec<ColumnLevelDiagnostic>) {
    let diagnostics = RefCell::new(Vec::new());
    let binder = Binder {
        catalog,
        casing,
        ctes: Vec::new(),
        outer_scopes: Vec::new(),
        diagnostics: &diagnostics,
    };
    let plan = binder.bind_statement(statement);
    (plan, diagnostics.into_inner())
}

/// Like [`build_with_diagnostics`] but folds an unbound statement (a
/// supported but structure-only kind, or one that fails to bind) to an
/// empty [`Plan::OpaqueLeaf`], so callers always get a walkable plan
/// without handling `None` or naming the IR. The extractors assemble the
/// public operations from this.
pub(crate) fn build_plan(
    statement: &Statement,
    catalog: Option<&Catalog>,
    casing: IdentifierCasing,
) -> (Plan, Vec<ColumnLevelDiagnostic>) {
    let (plan, diagnostics) = build_with_diagnostics(statement, catalog, casing);
    (plan.unwrap_or(Plan::OpaqueLeaf), diagnostics)
}

/// Carries the bind-time context: the optional catalog, the dialect
/// casing, the common table expressions in scope (accumulated in
/// declaration order, innermost `WITH` last), and the enclosing queries'
/// relations (the correlation stack, outermost first) that an inner
/// subquery's references fall through to.
struct Binder<'a> {
    catalog: Option<&'a Catalog>,
    casing: IdentifierCasing,
    ctes: Vec<CteRelation>,
    outer_scopes: Vec<Vec<Relation>>,
    /// Column-level diagnostics accumulated during the bind, shared across
    /// child binders (CTE bodies / subqueries) so nested ones surface too.
    diagnostics: &'a RefCell<Vec<ColumnLevelDiagnostic>>,
}

impl Binder<'_> {
    /// A child binder sharing this one's catalog / casing, with the given
    /// CTE environment and correlation stack.
    fn child(&self, ctes: Vec<CteRelation>, outer_scopes: Vec<Vec<Relation>>) -> Binder<'_> {
        Binder {
            catalog: self.catalog,
            casing: self.casing,
            ctes,
            outer_scopes,
            diagnostics: self.diagnostics,
        }
    }

    /// A child binder with a different CTE environment (extending scope
    /// across a `WITH`); the correlation stack carries over unchanged.
    fn with_ctes(&self, ctes: Vec<CteRelation>) -> Binder<'_> {
        self.child(ctes, self.outer_scopes.clone())
    }

    /// A child binder with one more enclosing scope on the correlation
    /// stack (used when descending into a subquery in an expression).
    fn with_outer_scope(&self, relations: Vec<Relation>) -> Binder<'_> {
        let mut outer_scopes = self.outer_scopes.clone();
        outer_scopes.push(relations);
        self.child(self.ctes.clone(), outer_scopes)
    }
}

#[cfg(test)]
mod tests;
