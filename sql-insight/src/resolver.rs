//! Walks a `sqlparser` `Statement` once and produces a
//! [`Resolution`] carrying scope bindings, captured column
//! references, and lineage edges. Two post-passes
//! ([`Resolution::collapsed_lineage_edges`] and
//! [`Resolution::real_column_refs`]) refine the raw walk
//! data into the public extraction surfaces.
//!
//! Module layout (all sub-modules are crate-internal):
//!
//! - [`binding`]: scope arena, `Binding` enum, scope traversal,
//!   binder methods on `Resolver`.
//! - [`context`]: the scoped `with_*` helpers that save / restore
//!   `scope_kind` around a clause walk.
//! - [`column_ref`]: `RawColumnRef` and walk-time resolution of
//!   identifier parts to owning tables.
//! - [`projection`]: `ProjectionGroup` / `ProjectionItem` and the
//!   passthrough-vs-transformation classification helper.
//! - [`lineage`]: `LineageEdge` / `LineageTargetSpec` and the emit
//!   helpers that drive INSERT / CTAS / QueryOutput edge construction.
//! - [`collapse`]: post-walk passes that collapse synthetic
//!   relations out of the lineage chain and filter synthetic reads.
//! - [`rename`]: CTE / derived column-alias renaming.
//! - Walker modules ([`expr`], [`query`], [`statement`], [`table`]):
//!   `visit_*` methods on `Resolver`, one per major AST
//!   region.

mod binding;
mod collapse;
mod column_ref;
mod context;
mod lineage;
mod projection;
mod rename;

mod expr;
mod query;
mod statement;
mod table;

pub(crate) use binding::{
    Binding, RawTableRef, RelationSchema, Scope, ScopeId, ScopeKind, TableRefTarget, TableRole,
};
pub(crate) use column_ref::RawColumnRef;
pub(crate) use lineage::{LineageEdge, LineageTargetSpec};
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
/// arena and the raw column refs / lineage edges collected during the
/// walk, plus accumulated diagnostics. Two post-passes inside
/// [`Resolver::into_resolution`] refine
/// `column_refs` and `lineage_edges` before the resolution leaves the
/// resolver.
#[derive(Debug)]
pub(crate) struct Resolution {
    /// Column-level diagnostics accumulated during the walk
    /// (`WildcardSuppressed`, `AmbiguousColumn`, `UnresolvedColumn`,
    /// `UnsupportedStatement`). Table-level extractors project this
    /// down via `ColumnLevelDiagnostic::to_table_level`.
    pub(crate) diagnostics: Vec<ColumnLevelDiagnostic>,
    /// Finalized scope arena, indexed by [`ScopeId`]. Holds every
    /// scope created during the walk — post-passes (collapse,
    /// real-column-ref filtering) re-walk via id lookups.
    pub(crate) scopes: Vec<Scope>,
    /// Column refs that survive the synthetic-binding filter (see
    /// [`Resolution::real_column_refs`]).
    pub(crate) column_refs: Vec<RawColumnRef>,
    /// Lineage edges after end-to-end collapse through CTE / derived
    /// synthetics (see [`Resolution::collapsed_lineage_edges`]).
    pub(crate) lineage_edges: Vec<LineageEdge>,
    /// Every `FROM`-position use of a table-like source captured
    /// during the walk. Drives table-lineage collapse (see
    /// [`Resolution::collapsed_feeding_table_sources`]) — an entry per
    /// physical use, occurrence-based (no dedup), matching
    /// [`ColumnLineageEdge`] semantics.
    pub(crate) table_refs: Vec<RawTableRef>,
}

/// What `resolve_query` returns: the body's `output_schema` and the
/// body projections per top-level SELECT (one entry, or one per UNION
/// branch). Callers decide whether to emit `QueryOutput` edges
/// (default), pair positionally with relation target columns
/// (INSERT / CTAS), or bubble them through `SetExpr::Query`.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedQuery {
    pub(crate) output_schema: RelationSchema,
    pub(crate) projections: Vec<ProjectionGroup>,
    /// Arena id of the scope pushed for this query's body — exposed so
    /// callers binding the query as a synthetic relation (CTE / derived
    /// table) can record it on the binding for table-lineage
    /// collapse. Equals the scope that held the body's FROM
    /// bindings; pop has already happened by the time the caller sees
    /// this id, but the arena entry remains for post-pass lookups.
    pub(crate) body_scope: ScopeId,
}

/// The walker. Owns the scope stack, the in-progress refs / edges,
/// the current projection buffer, and the lexical `scope_kind`. All
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
    /// In-progress diagnostics buffer; moved into
    /// [`Resolution::diagnostics`] at `into_resolution`.
    diagnostics: Vec<ColumnLevelDiagnostic>,
    /// Active scope arena + stack. Pushes/pops during the walk,
    /// flattens into [`Resolution::scopes`] at `into_resolution`.
    scopes: ScopeStack,
    /// Column refs captured by `record_column_ref` in walk order.
    /// Post-pass filters synthetic-owned ones out into
    /// [`Resolution::column_refs`].
    column_refs: Vec<RawColumnRef>,
    /// Lineage edges emitted directly during the walk. Post-pass
    /// collapses through CTE / derived synthetics into
    /// [`Resolution::lineage_edges`].
    lineage_edges: Vec<LineageEdge>,
    /// In-progress `RawTableRef` buffer; moved into
    /// [`Resolution::table_refs`] at `into_resolution`. Emit happens at
    /// every `FROM`-position bind site (real tables, CTE references,
    /// true derived subqueries).
    table_refs: Vec<RawTableRef>,
    /// Per-query buffer of projection groups collected by
    /// `visit_select`. `resolve_query` swaps a fresh buffer in for
    /// the duration of its walk and packs the collected groups into
    /// the returned `ResolvedQuery`, so each query gets exactly its
    /// own projections.
    current_projections: Vec<ProjectionGroup>,
    /// Lexical context stamped onto every scope pushed while it is in
    /// effect: `Body` by default, flipped to `Predicate` by
    /// [`Resolver::with_filter_clause`] so subqueries nested in WHERE /
    /// HAVING / JOIN ON etc. are excluded from table-lineage. Propagates
    /// *through* subquery boundaries (a subquery in a predicate is itself
    /// predicate-position).
    scope_kind: ScopeKind,
}

impl<'a> Resolver<'a> {
    fn new(catalog: Option<&'a dyn Catalog>) -> Self {
        Self {
            catalog,
            diagnostics: Vec::new(),
            scopes: ScopeStack::default(),
            column_refs: Vec::new(),
            lineage_edges: Vec::new(),
            table_refs: Vec::new(),
            current_projections: Vec::new(),
            scope_kind: ScopeKind::Body,
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
            lineage_edges: self.lineage_edges,
            table_refs: self.table_refs,
        };
        // Two post-passes, both rely on the scope arena being final:
        // - collapse lineage edges so synthetic-binding (Cte/Derived)
        //   sources are collapsed with their body's source refs;
        // - filter column refs so synthetic-owned ones don't surface
        //   in the public reads list.
        resolution.lineage_edges = resolution.collapsed_lineage_edges();
        resolution.column_refs = resolution.real_column_refs();
        resolution
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnSchema;
    use crate::reference::TableReference;
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
                        name: c.to_string(),
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
                assert_eq!(cols[0].value, "id");
                assert_eq!(cols[1].value, "email");
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
