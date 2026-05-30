//! Walks a `sqlparser` `Statement` once and produces a
//! [`Resolution`] carrying scope bindings, captured refs, and lineage
//! edges. Two post-passes ([`Resolution::collapsed_lineage_edges`]
//! and [`Resolution::real_column_refs`]) refine the raw walk data
//! into the public extraction surfaces.
//!
//! Module layout (all sub-modules are crate-internal):
//!
//! - [`scope`]: `Scope`, `ScopeStack`, and the `with_*` helpers that
//!   save / restore scope state around a clause walk.
//! - [`binding`]: `Binding` enum + `BindingKey` + `bind_*` /
//!   lookup / diagnostic-record methods on `Resolver`.
//! - [`column_ref`]: `RawColumnRef` and walk-time resolution of
//!   identifier parts to owning tables.
//! - [`table_ref`]: `RawTableRef` / `TableRefTarget` and the
//!   `record_*_table_ref` constructors. Parallel to `column_ref`.
//! - [`body_output`]: `BodyOutput` / `OutputColumn` and the helpers
//!   that derive each output column's name / kind from a `SelectItem`.
//! - [`lineage`]: `LineageEdge` / `LineageTargetSpec` and the emit
//!   helpers that drive INSERT / CTAS / QueryOutput edge construction.
//! - [`resolution`]: `Resolution` struct + every `impl Resolution`
//!   method — public table queries and the post-walk collapse /
//!   filter passes.
//! - Walker modules ([`resolve_statement`], [`resolve_query`],
//!   [`resolve_expr`], [`resolve_table`]): `visit_*` methods on
//!   `Resolver`, one per major AST region. `resolve_query` also
//!   defines `ResolvedQuery`.

mod binding;
mod body_output;
mod column_ref;
mod lineage;
mod resolution;
mod scope;
mod table_ref;

mod resolve_expr;
mod resolve_query;
mod resolve_statement;
mod resolve_table;

pub(crate) use binding::{Binding, TableRole};
pub(crate) use body_output::{BodyOutput, OutputColumn};
pub(crate) use column_ref::RawColumnRef;
pub(crate) use lineage::{LineageEdge, LineageTargetSpec};
pub(crate) use resolution::Resolution;
pub(crate) use resolve_query::ResolvedQuery;
pub(crate) use scope::{Scope, ScopeId, ScopeKind};
pub(crate) use table_ref::{RawTableRef, TableRefTarget};

// Internal infrastructure used by walkers via `super::*`.
use scope::ScopeStack;

use sqlparser::ast::Statement;

use crate::catalog::Catalog;
use crate::diagnostic::ColumnLevelDiagnostic;
use crate::error::Error;

/// The walker. Owns the scope stack, the in-progress refs / edges,
/// the current branch buffer, and the lexical `scope_kind`. All
/// `visit_*` methods (in the walker sub-modules) and the various
/// `bind_*` / `record_*` / `with_*` helpers live as `impl` blocks
/// across the sub-modules — this is just the data shape and the
/// top-level entry point.
#[derive(Debug)]
pub(crate) struct Resolver<'a> {
    /// `None` means the resolver runs without external schema
    /// enrichment; tables bound here carry `output_columns: None`.
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
    /// Per-query buffer of branch-shaped output columns collected by
    /// `visit_select` (one inner `Vec` per branch). `resolve_query`
    /// swaps a fresh buffer in for the duration of its walk and packs
    /// the collected branches into the returned `ResolvedQuery`'s
    /// `output_columns`, so each query gets exactly its own branches.
    current_branches: Vec<Vec<OutputColumn>>,
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
            current_branches: Vec::new(),
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

    fn first_table_columns(resolution: &Resolution) -> Option<&Option<Vec<sqlparser::ast::Ident>>> {
        resolution
            .scopes
            .iter()
            .flat_map(|scope| scope.bindings.values())
            .find_map(|binding| match binding {
                Binding::Table { output_columns, .. } => Some(output_columns),
                _ => None,
            })
    }

    #[test]
    fn catalog_hit_populates_table_columns() {
        let catalog = TestCatalog::default().with("users", vec!["id", "email"]);
        let resolution = resolve("SELECT * FROM users", Some(&catalog));
        match first_table_columns(&resolution) {
            Some(Some(cols)) => {
                assert_eq!(cols.len(), 2);
                assert_eq!(cols[0].value, "id");
                assert_eq!(cols[1].value, "email");
            }
            other => panic!("expected Some(Some(...)), got {:?}", other),
        }
    }

    #[test]
    fn catalog_miss_leaves_columns_unknown() {
        let catalog = TestCatalog::default();
        let resolution = resolve("SELECT * FROM users", Some(&catalog));
        assert!(matches!(first_table_columns(&resolution), Some(None)));
    }

    #[test]
    fn no_catalog_leaves_columns_unknown() {
        let resolution = resolve("SELECT * FROM users", None);
        assert!(matches!(first_table_columns(&resolution), Some(None)));
    }

    #[test]
    fn catalog_lookup_ignores_alias() {
        let catalog = TestCatalog::default().with("users", vec!["id"]);
        let resolution = resolve("SELECT * FROM users AS u", Some(&catalog));
        assert!(matches!(first_table_columns(&resolution), Some(Some(_))));
    }
}
