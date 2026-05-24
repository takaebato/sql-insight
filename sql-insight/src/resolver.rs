//! Walks a `sqlparser` `Statement` once and produces a
//! [`Resolution`] carrying scope bindings, captured column
//! references, and flow edges. Two post-passes
//! ([`Resolution::composed_flow_edges`] and
//! [`Resolution::real_column_refs`]) refine the raw walk
//! data into the public extraction surfaces.
//!
//! Module layout (all sub-modules are crate-internal):
//!
//! - [`binding`]: scope arena, `Binding` enum, scope traversal,
//!   binder methods on `Resolver`.
//! - [`context`]: `VisitContext` and the scoped `with_*` helpers
//!   that mutate it.
//! - [`column_ref`]: `RawColumnRef` and walk-time resolution of
//!   identifier parts to owning tables.
//! - [`projection`]: `ProjectionGroup` / `ProjectionItem` and the
//!   passthrough-vs-transformation classification helper.
//! - [`flow`]: `FlowEdge` / `FlowTargetSpec` and the emit helpers
//!   that drive INSERT / CTAS / QueryOutput edge construction.
//! - [`composition`]: post-walk passes that substitute synthetic
//!   sources and filter synthetic reads.
//! - [`rename`]: CTE / derived column-alias renaming.
//! - Walker modules ([`expr`], [`query`], [`statement`], [`table`]):
//!   `visit_*` methods on `Resolver`, one per major AST
//!   region.

mod binding;
mod column_ref;
mod composition;
mod context;
mod flow;
mod projection;
mod rename;

mod expr;
mod query;
mod statement;
mod table;

pub(crate) use binding::{Binding, Column, RelationSchema, Scope, ScopeId, ScopeKind, TableRole};
pub(crate) use column_ref::RawColumnRef;
pub(crate) use context::VisitContext;
pub(crate) use flow::{FlowEdge, FlowTargetSpec};
pub(crate) use projection::{ProjectionGroup, ProjectionItem};

// Internal helpers used by walkers via `super::*`. Some are
// resolver-internal infrastructure (`BindingKey`, `ScopeStack`,
// binding helpers); rename helpers are surfaced for the CTE /
// derived-table walkers in walker/query.rs and walker/table.rs.
use binding::ScopeStack;
pub(super) use rename::{rename_projection_groups, rename_relation_schema};

use sqlparser::ast::Statement;

use crate::catalog::Catalog;
use crate::diagnostic::ColumnLevelDiagnostic;
use crate::error::Error;

/// The end-of-walk result the resolver produces. Holds the scope
/// arena and the raw column refs / flow edges collected during the
/// walk, plus accumulated diagnostics. Two post-passes inside
/// [`Resolver::into_resolution`] refine
/// `column_refs` and `flow_edges` before the resolution leaves the
/// resolver.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct Resolution {
    pub(crate) diagnostics: Vec<ColumnLevelDiagnostic>,
    pub(crate) scopes: Vec<Scope>,
    /// Column refs that survive the synthetic-binding filter (see
    /// [`Resolution::real_column_refs`]).
    pub(crate) column_refs: Vec<RawColumnRef>,
    /// Flow edges after end-to-end composition through CTE / derived
    /// intermediates (see
    /// [`Resolution::composed_flow_edges`]).
    pub(crate) flow_edges: Vec<FlowEdge>,
}

/// What `resolve_query` returns: the scope id pushed for this query
/// (mostly informational), the body's `output_schema`, and the body
/// projections per top-level SELECT (one entry, or one per UNION
/// branch). Callers decide whether to emit `QueryOutput` edges
/// (default), pair positionally with persisted target columns
/// (INSERT / CTAS), or bubble them through `SetExpr::Query`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct ResolvedQuery {
    pub(crate) scope_id: ScopeId,
    pub(crate) output_schema: RelationSchema,
    pub(crate) projections: Vec<ProjectionGroup>,
}

/// The walker. Owns the scope stack, the in-progress refs / edges,
/// the current projection buffer, and the [`VisitContext`]. All
/// `visit_*` methods (in the walker sub-modules) and the various
/// `bind_*` / `record_*` / `with_*` helpers live as `impl` blocks
/// across the sub-modules — this is just the data shape and the
/// top-level entry point.
#[derive(Debug)]
pub(crate) struct Resolver<'a> {
    /// `None` means the resolver runs without external schema
    /// enrichment; table schemas stay `RelationSchema::Unknown` in
    /// that case.
    catalog: Option<&'a dyn Catalog>,
    diagnostics: Vec<ColumnLevelDiagnostic>,
    scopes: ScopeStack,
    column_refs: Vec<RawColumnRef>,
    flow_edges: Vec<FlowEdge>,
    /// Per-query buffer of projection groups collected by
    /// `visit_select`. `resolve_query` swaps a fresh buffer in for
    /// the duration of its walk and packs the collected groups into
    /// the returned `ResolvedQuery`, so each query gets exactly its
    /// own projections.
    current_projections: Vec<ProjectionGroup>,
    /// Lexical walking context (`scope_kind`). See [`VisitContext`].
    ctx: VisitContext,
}

impl<'a> Resolver<'a> {
    fn new(catalog: Option<&'a dyn Catalog>) -> Self {
        Self {
            catalog,
            diagnostics: Vec::new(),
            scopes: ScopeStack::default(),
            column_refs: Vec::new(),
            flow_edges: Vec::new(),
            current_projections: Vec::new(),
            ctx: VisitContext::default(),
        }
    }

    pub(crate) fn resolve_statement(
        catalog: Option<&'a dyn Catalog>,
        statement: &Statement,
    ) -> Result<Resolution, Error> {
        let mut resolver = Self::new(catalog);
        resolver.visit_statement(statement)?;
        Ok(resolver.into_resolution())
    }

    fn into_resolution(self) -> Resolution {
        let mut resolution = Resolution {
            diagnostics: self.diagnostics,
            scopes: self.scopes.into_scopes(),
            column_refs: self.column_refs,
            flow_edges: self.flow_edges,
        };
        // Two post-passes, both rely on the scope arena being final:
        // - compose flow edges so synthetic-binding (Cte/Derived)
        //   sources are substituted with their body's source refs;
        // - filter column refs so synthetic-owned ones don't surface
        //   in the public reads list.
        resolution.flow_edges = resolution.composed_flow_edges();
        resolution.column_refs = resolution.real_column_refs();
        resolution
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnSchema;
    use crate::relation::TableReference;
    use sqlparser::ast::Ident;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;
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
                        name: Ident::new(*c),
                    })
                    .collect()
            })
        }
    }

    fn resolve(sql: &str, catalog: Option<&dyn Catalog>) -> Resolution {
        let dialect = GenericDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        Resolver::resolve_statement(catalog, &statements[0]).unwrap()
    }

    fn first_table_schema(resolution: &Resolution) -> Option<&RelationSchema> {
        resolution
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.values())
            .find_map(|binding| match binding {
                Binding::Table { schema, .. } => Some(schema),
                _ => None,
            })
    }

    #[test]
    fn catalog_hit_populates_table_schema() {
        let catalog = TestCatalog::default().with("users", vec!["id", "email"]);
        let resolution = resolve("SELECT * FROM users", Some(&catalog));
        match first_table_schema(&resolution) {
            Some(RelationSchema::Known(cols)) => {
                assert_eq!(cols.len(), 2);
                assert_eq!(cols[0].name.value, "id");
                assert_eq!(cols[1].name.value, "email");
            }
            other => panic!("expected RelationSchema::Known(...), got {:?}", other),
        }
    }

    #[test]
    fn catalog_miss_keeps_schema_unknown() {
        let catalog = TestCatalog::default();
        let resolution = resolve("SELECT * FROM users", Some(&catalog));
        assert!(matches!(
            first_table_schema(&resolution),
            Some(RelationSchema::Unknown)
        ));
    }

    #[test]
    fn no_catalog_keeps_schema_unknown() {
        let resolution = resolve("SELECT * FROM users", None);
        assert!(matches!(
            first_table_schema(&resolution),
            Some(RelationSchema::Unknown)
        ));
    }

    #[test]
    fn catalog_lookup_ignores_alias() {
        let catalog = TestCatalog::default().with("users", vec!["id"]);
        let resolution = resolve("SELECT * FROM users AS u", Some(&catalog));
        assert!(matches!(
            first_table_schema(&resolution),
            Some(RelationSchema::Known(_))
        ));
    }
}
