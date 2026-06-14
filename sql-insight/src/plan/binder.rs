//! The binder: lowers a `sqlparser` AST into the bound [`Plan`] IR,
//! resolving every column reference bottom-up.
//!
//! Resolution runs against a [`Scope`] threaded up through the bind (the
//! relations visible at the current node). The scope is bind-time
//! *scratch* — never stored on the [`Plan`], which keeps only resolved
//! provenance / reads. With a [`Catalog`] a relation's columns are
//! `Known` (resolution becomes strict — `Cataloged` hits, `Unresolved`
//! denials, narrowed candidates); catalog-free they are `Open` and
//! resolution is best-effort (`Inferred` / `Ambiguous`). The catalog
//! matching mirrors [`crate::resolver`]'s (ported here while the two
//! coexist; the differential harness pins them together).

use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, Join, JoinConstraint,
    JoinOperator, Query, Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins,
};

use super::ir::{BoundColumn, OpaqueLeaf, PassThrough, Plan, Project, Scan};
use crate::catalog::{Catalog, CatalogTable};
use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableReference};
use crate::resolver::{CaseFold, IdentifierCasing};

/// Bind one statement into a [`Plan`], or `None` for statement kinds
/// this brick doesn't model yet (everything except `SELECT`-shaped
/// queries). The top-level scope is discarded — callers consume the
/// resolved tree, not the scope.
pub(crate) fn build(
    statement: &Statement,
    catalog: Option<&Catalog>,
    casing: IdentifierCasing,
) -> Option<Plan> {
    let binder = Binder { catalog, casing };
    match statement {
        Statement::Query(query) => Some(binder.bind_query(query).0),
        // DML / DDL are later bricks.
        _ => None,
    }
}

/// Bind-time resolution scope: the relations visible at a point in the
/// bind. Scratch — never stored on the [`Plan`].
pub(crate) struct Scope {
    relations: Vec<Relation>,
}

impl Scope {
    fn empty() -> Self {
        Self {
            relations: Vec::new(),
        }
    }

    fn of(relation: Relation) -> Self {
        Self {
            relations: vec![relation],
        }
    }

    /// Concatenate two scopes (the relations of a join / comma).
    fn merge(mut self, mut other: Scope) -> Scope {
        self.relations.append(&mut other.relations);
        self
    }
}

/// A relation visible in a [`Scope`]: its (canonical) identity, use-site
/// alias, and column knowledge.
struct Relation {
    table: TableReference,
    alias: Option<Ident>,
    columns: RelationColumns,
}

impl Relation {
    /// The name this relation answers to in a qualifier: its alias if
    /// aliased, otherwise its table's bare name.
    fn exposed_name(&self) -> &Ident {
        self.alias.as_ref().unwrap_or(&self.table.name)
    }
}

/// What a relation exposes for resolution.
enum RelationColumns {
    /// Column set unknown (catalog-free, or a catalog miss / ambiguous
    /// match) — any name could plausibly belong here (`Inferred`).
    Open,
    /// Catalog-known columns (quoted = exact-match idents). A name in
    /// the list resolves `Cataloged`; a name absent means the relation
    /// can't own it.
    Known(Vec<Ident>),
}

/// One candidate owner of a column reference during resolution.
struct Candidate {
    table: TableReference,
    /// A `Known` schema lists the column (drives `Cataloged` vs
    /// `Inferred` and the Known-witness-over-Open tiebreaker).
    confirmed: bool,
}

/// Carries the bind-time context: the optional catalog and the dialect
/// casing. (A correlation scope stack joins it in a later brick.)
struct Binder<'a> {
    catalog: Option<&'a Catalog>,
    casing: IdentifierCasing,
}

impl Binder<'_> {
    /// Bind a query, returning the plan node and its output scope.
    fn bind_query(&self, query: &Query) -> (Plan, Scope) {
        match query.body.as_ref() {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(inner) => self.bind_query(inner),
            // Set operations, VALUES, `WITH … <DML>` wrappers: later
            // bricks — an opaque relation with no columns for now.
            _ => (Plan::OpaqueLeaf(OpaqueLeaf { alias: None }), Scope::empty()),
        }
    }

    fn bind_select(&self, select: &Select) -> (Plan, Scope) {
        let (from, from_scope) = self.bind_from(&select.from);
        // WHERE wraps the FROM in a PassThrough: its reads resolve
        // against the FROM scope only (Project is above, so no aliases
        // are visible — the clause-phase rule, structurally).
        let input = match &select.selection {
            Some(predicate) => Plan::PassThrough(PassThrough {
                inputs: vec![from],
                reads: self.resolve_reads(predicate, &from_scope),
            }),
            None => from,
        };
        // PassThrough is identity, so the projection resolves against the
        // FROM scope either way.
        let outputs: Vec<BoundColumn> = select
            .projection
            .iter()
            .filter_map(|item| self.bind_output_column(item, &from_scope))
            .collect();
        // The query's output interface (a Known relation exposing these
        // columns) is only needed once derived tables / clause-phase
        // land — deferred, so return an empty output scope for now.
        (
            Plan::Project(Project {
                input: Box::new(input),
                outputs,
            }),
            Scope::empty(),
        )
    }

    fn bind_from(&self, items: &[TableWithJoins]) -> (Plan, Scope) {
        let mut bound: Vec<(Plan, Scope)> = items
            .iter()
            .map(|twj| self.bind_table_with_joins(twj))
            .collect();
        match bound.len() {
            // `SELECT 1` (no FROM) — an empty opaque source.
            0 => (Plan::OpaqueLeaf(OpaqueLeaf { alias: None }), Scope::empty()),
            1 => bound.pop().unwrap(),
            // Comma join: a PassThrough with no predicate.
            _ => {
                let mut scope = Scope::empty();
                let mut inputs = Vec::with_capacity(bound.len());
                for (node, node_scope) in bound {
                    inputs.push(node);
                    scope = scope.merge(node_scope);
                }
                (
                    Plan::PassThrough(PassThrough {
                        inputs,
                        reads: Vec::new(),
                    }),
                    scope,
                )
            }
        }
    }

    fn bind_table_with_joins(&self, twj: &TableWithJoins) -> (Plan, Scope) {
        let (mut node, mut scope) = self.bind_table_factor(&twj.relation);
        for join in &twj.joins {
            let (right, right_scope) = self.bind_table_factor(&join.relation);
            // The ON predicate sees both sides; resolve its reads against
            // the combined scope, which is also this PassThrough's output.
            let combined = scope.merge(right_scope);
            let reads = match join_constraint(join) {
                Some(JoinConstraint::On(expr)) => self.resolve_reads(expr, &combined),
                // USING fan-in / NATURAL are later bricks.
                _ => Vec::new(),
            };
            node = Plan::PassThrough(PassThrough {
                inputs: vec![node, right],
                reads,
            });
            scope = combined;
        }
        (node, scope)
    }

    fn bind_table_factor(&self, factor: &TableFactor) -> (Plan, Scope) {
        match factor {
            TableFactor::Table { name, alias, .. } => match TableReference::try_from_name(name) {
                Ok(written) => {
                    let alias = alias.as_ref().map(|a| a.name.clone());
                    // A unique catalog hit canonicalizes the identity and
                    // supplies the columns; a miss / ambiguous / no-catalog
                    // leaves it as written with an open column set.
                    let (table, columns) = match self.catalog_match(&written) {
                        Some((canonical, cols)) if !cols.is_empty() => {
                            (canonical, RelationColumns::Known(cols))
                        }
                        Some((canonical, _)) => (canonical, RelationColumns::Open),
                        None => (written, RelationColumns::Open),
                    };
                    let resolution = match columns {
                        RelationColumns::Known(_) => ResolutionKind::Cataloged,
                        RelationColumns::Open => ResolutionKind::Inferred,
                    };
                    let relation = Relation {
                        table: table.clone(),
                        alias: alias.clone(),
                        columns,
                    };
                    (
                        Plan::Scan(Scan {
                            table,
                            alias,
                            resolution,
                        }),
                        Scope::of(relation),
                    )
                }
                Err(_) => (Plan::OpaqueLeaf(OpaqueLeaf { alias: None }), Scope::empty()),
            },
            // Derived tables / table functions / pivots etc. are later
            // bricks; they expose no inspectable columns yet.
            _ => (Plan::OpaqueLeaf(OpaqueLeaf { alias: None }), Scope::empty()),
        }
    }

    /// Resolve one SELECT-list item against `scope`. Wildcards are
    /// skipped for now (`None`).
    fn bind_output_column(&self, item: &SelectItem, scope: &Scope) -> Option<BoundColumn> {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.clone())),
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => return None,
        };
        let mut refs = Vec::new();
        collect_column_refs(expr, &mut refs);
        let provenance = refs
            .iter()
            .flat_map(|parts| self.resolve_ref(parts, scope))
            .collect();
        let name = alias.or_else(|| inferred_output_name(expr));
        let kind = if is_single_column(expr) {
            ColumnLineageKind::Passthrough
        } else {
            ColumnLineageKind::Transformation
        };
        Some(BoundColumn {
            name,
            provenance,
            kind,
        })
    }

    /// Reads contributed by a predicate (WHERE / ON): every column it
    /// references, resolved against `scope`.
    fn resolve_reads(&self, expr: &Expr, scope: &Scope) -> Vec<ColumnRead> {
        let mut refs = Vec::new();
        collect_column_refs(expr, &mut refs);
        refs.iter()
            .flat_map(|parts| self.resolve_ref(parts, scope))
            .collect()
    }

    /// Resolve a single column reference to its pre-collapsed provenance.
    /// Mirrors the resolver: unqualified scans candidate relations
    /// (Known-witness over Open suspects); qualified matches the
    /// qualifier first. Catalog-free everything is `Open` → `Inferred` /
    /// `Ambiguous`.
    fn resolve_ref(&self, parts: &[Ident], scope: &Scope) -> Vec<ColumnRead> {
        let Some(column) = parts.last() else {
            return Vec::new();
        };
        let candidates: Vec<Candidate> = if parts.len() == 1 {
            scope
                .relations
                .iter()
                .filter_map(|rel| self.unqualified_candidate(rel, column))
                .collect()
        } else {
            let qualifier = &parts[parts.len() - 2];
            scope
                .relations
                .iter()
                .filter(|rel| self.ident_eq(rel.exposed_name(), qualifier))
                .map(|rel| Candidate {
                    table: rel.table.clone(),
                    confirmed: self.relation_lists(rel, column),
                })
                .collect()
        };
        vec![self.pick(candidates, column)]
    }

    /// A relation is an unqualified candidate iff it could own `column`:
    /// a `Known` schema must list it; an `Open` one always could.
    fn unqualified_candidate(&self, rel: &Relation, column: &Ident) -> Option<Candidate> {
        match &rel.columns {
            RelationColumns::Known(cols) => self.list_has(cols, column).then(|| Candidate {
                table: rel.table.clone(),
                confirmed: true,
            }),
            RelationColumns::Open => Some(Candidate {
                table: rel.table.clone(),
                confirmed: false,
            }),
        }
    }

    /// Whether a `Known` schema lists `column` (for qualified
    /// confirmation; `Open` never confirms).
    fn relation_lists(&self, rel: &Relation, column: &Ident) -> bool {
        match &rel.columns {
            RelationColumns::Known(cols) => self.list_has(cols, column),
            RelationColumns::Open => false,
        }
    }

    /// Collapse candidates to one resolved read (the resolver's rule):
    /// none → Unresolved; one → Cataloged if confirmed else Inferred;
    /// several with exactly one confirmed → that Known witness wins as
    /// Inferred; otherwise Ambiguous.
    fn pick(&self, candidates: Vec<Candidate>, column: &Ident) -> ColumnRead {
        match candidates.as_slice() {
            [] => unresolved(column),
            [only] => read(
                &only.table,
                column,
                if only.confirmed {
                    ResolutionKind::Cataloged
                } else {
                    ResolutionKind::Inferred
                },
            ),
            _ => {
                let confirmed: Vec<&Candidate> =
                    candidates.iter().filter(|c| c.confirmed).collect();
                match confirmed.as_slice() {
                    [witness] => read(&witness.table, column, ResolutionKind::Inferred),
                    _ => ambiguous(column),
                }
            }
        }
    }

    fn list_has(&self, columns: &[Ident], column: &Ident) -> bool {
        columns
            .iter()
            .any(|c| self.casing.column.normalize(c) == self.casing.column.normalize(column))
    }

    fn ident_eq(&self, a: &Ident, b: &Ident) -> bool {
        self.casing.table_alias.normalize(a) == self.casing.table_alias.normalize(b)
    }

    /// Match a query table reference against the catalog: a unique
    /// right-anchored, dialect-cased hit returns the registered table's
    /// canonical identity and column names. Mirrors the resolver's
    /// `catalog_match` (ambiguous / miss → `None`, best-effort open).
    fn catalog_match(&self, written: &TableReference) -> Option<(TableReference, Vec<Ident>)> {
        let catalog = self.catalog?;
        let filled = fill_query_defaults(written, catalog);
        let fold = self.casing.table;
        let mut hits = catalog
            .tables()
            .iter()
            .filter(|t| catalog_table_matches(&filled, t, fold));
        let first = hits.next()?;
        if hits.next().is_some() {
            // Ambiguous registration — stay best-effort (open).
            return None;
        }
        let columns = first
            .column_names()
            .iter()
            .map(|c| Ident::with_quote('"', c))
            .collect();
        Some((canonical_ref(first), columns))
    }
}

/// Fill a query reference's missing prefix segments from the catalog's
/// defaults before matching (bare → schema then catalog; catalog only
/// once a schema is present). Filled segments are quoted (exact).
fn fill_query_defaults(written: &TableReference, catalog: &Catalog) -> TableReference {
    let mut filled = written.clone();
    if filled.schema.is_none() {
        if let Some(schema) = catalog.default_schema_segment() {
            filled.schema = Some(Ident::with_quote('"', schema));
        }
    }
    if filled.catalog.is_none() && filled.schema.is_some() {
        if let Some(catalog_segment) = catalog.default_catalog_segment() {
            filled.catalog = Some(Ident::with_quote('"', catalog_segment));
        }
    }
    filled
}

/// Right-anchored, dialect-cased match of a (default-filled) query
/// reference against a registered table. Catalog identifiers are
/// compared as exact (quoted) — see the resolver's casing notes.
fn catalog_table_matches(query: &TableReference, table: &CatalogTable, fold: CaseFold) -> bool {
    if fold.normalize(&query.name) != normalize_catalog(table.name_segment(), fold) {
        return false;
    }
    if let Some(schema) = &query.schema {
        if fold.normalize(schema) != normalize_catalog(table.schema_segment(), fold) {
            return false;
        }
    }
    match (&query.catalog, table.catalog_segment()) {
        (Some(query_catalog), Some(table_catalog)) => {
            fold.normalize(query_catalog) == normalize_catalog(table_catalog, fold)
        }
        _ => true,
    }
}

/// Fold a catalog-side string as an exact (quoted) identifier.
fn normalize_catalog(segment: &str, fold: CaseFold) -> String {
    fold.normalize(&Ident::with_quote('"', segment))
}

/// The surfaced canonical identity of a matched table: plain (unquoted)
/// idents so reads / writes compare naturally.
fn canonical_ref(table: &CatalogTable) -> TableReference {
    TableReference {
        catalog: table.catalog_segment().map(Ident::new),
        schema: Some(Ident::new(table.schema_segment())),
        name: Ident::new(table.name_segment()),
    }
}

fn join_constraint(join: &Join) -> Option<&JoinConstraint> {
    match &join.join_operator {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::CrossJoin(c)
        | JoinOperator::Semi(c)
        | JoinOperator::LeftSemi(c)
        | JoinOperator::RightSemi(c)
        | JoinOperator::Anti(c)
        | JoinOperator::LeftAnti(c)
        | JoinOperator::RightAnti(c)
        | JoinOperator::StraightJoin(c) => Some(c),
        JoinOperator::AsOf { constraint, .. } => Some(constraint),
        JoinOperator::CrossApply | JoinOperator::OuterApply => None,
    }
}

/// Collect the column references inside an expression, each as its raw
/// identifier path. Best-effort over common expression shapes;
/// unmodelled variants contribute nothing (only ever a missed read,
/// never a wrong one).
fn collect_column_refs(expr: &Expr, out: &mut Vec<Vec<Ident>>) {
    match expr {
        Expr::Identifier(id) => out.push(vec![id.clone()]),
        Expr::CompoundIdentifier(ids) => out.push(ids.clone()),
        Expr::BinaryOp { left, right, .. } => {
            collect_column_refs(left, out);
            collect_column_refs(right, out);
        }
        Expr::UnaryOp { expr, .. } | Expr::Nested(expr) | Expr::Cast { expr, .. } => {
            collect_column_refs(expr, out)
        }
        Expr::Function(function) => collect_function_refs(function, out),
        _ => {}
    }
}

fn collect_function_refs(function: &Function, out: &mut Vec<Vec<Ident>>) {
    if let FunctionArguments::List(list) = &function.args {
        for arg in &list.args {
            let inner = match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                }
                | FunctionArg::ExprNamed {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                } => Some(expr),
                _ => None,
            };
            if let Some(expr) = inner {
                collect_column_refs(expr, out);
            }
        }
    }
}

/// The output name SQL infers for an unaliased projection item: a bare
/// column keeps its own name; anything else is anonymous.
fn inferred_output_name(expr: &Expr) -> Option<Ident> {
    match expr {
        Expr::Identifier(id) => Some(id.clone()),
        Expr::CompoundIdentifier(ids) => ids.last().cloned(),
        _ => None,
    }
}

fn is_single_column(expr: &Expr) -> bool {
    matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
}

fn read(table: &TableReference, column: &Ident, resolution: ResolutionKind) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: Some(table.clone()),
            name: column.clone(),
        },
        resolution,
    }
}

fn ambiguous(column: &Ident) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: None,
            name: column.clone(),
        },
        resolution: ResolutionKind::Ambiguous,
    }
}

fn unresolved(column: &Ident) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: None,
            name: column.clone(),
        },
        resolution: ResolutionKind::Unresolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    fn bind_one(sql: &str) -> Plan {
        let dialect = GenericDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let casing = IdentifierCasing::for_dialect(&dialect);
        build(&statements[0], None, casing).expect("supported statement")
    }

    fn tref(name: &str) -> TableReference {
        TableReference {
            catalog: None,
            schema: None,
            name: name.into(),
        }
    }

    fn scan(name: &str) -> Plan {
        Plan::Scan(Scan {
            table: tref(name),
            alias: None,
            resolution: ResolutionKind::Inferred,
        })
    }

    fn inferred(table: &str, column: &str) -> ColumnRead {
        read(&tref(table), &Ident::new(column), ResolutionKind::Inferred)
    }

    fn passthrough_col(name: &str, provenance: Vec<ColumnRead>) -> BoundColumn {
        BoundColumn {
            name: Some(Ident::new(name)),
            provenance,
            kind: ColumnLineageKind::Passthrough,
        }
    }

    #[test]
    fn single_table_projection() {
        // Project over a Scan; each bare column is an Inferred read of t,
        // forwarded as a passthrough output.
        assert_eq!(
            bind_one("SELECT a, b FROM t"),
            Plan::Project(Project {
                input: Box::new(scan("t")),
                outputs: vec![
                    passthrough_col("a", vec![inferred("t", "a")]),
                    passthrough_col("b", vec![inferred("t", "b")]),
                ],
            })
        );
    }

    #[test]
    fn join_on_and_where_become_passthrough_reads() {
        // FROM x JOIN y ON … is one PassThrough (join); WHERE wraps it in
        // another. The projection's qualified `x.a` resolves to x.
        assert_eq!(
            bind_one("SELECT x.a FROM x JOIN y ON x.id = y.id WHERE y.b > 0"),
            Plan::Project(Project {
                input: Box::new(Plan::PassThrough(PassThrough {
                    inputs: vec![Plan::PassThrough(PassThrough {
                        inputs: vec![scan("x"), scan("y")],
                        reads: vec![inferred("x", "id"), inferred("y", "id")],
                    })],
                    reads: vec![inferred("y", "b")],
                })),
                outputs: vec![passthrough_col("a", vec![inferred("x", "a")])],
            })
        );
    }

    #[test]
    fn unqualified_ref_over_join_is_ambiguous() {
        // Two open relations in scope and no catalog → an unqualified
        // `a` can't be pinned to one, so its provenance is Ambiguous.
        let Plan::Project(project) = bind_one("SELECT a FROM x JOIN y ON x.id = y.id") else {
            panic!("expected Project");
        };
        assert_eq!(
            project.outputs,
            vec![BoundColumn {
                name: Some(Ident::new("a")),
                provenance: vec![ambiguous(&Ident::new("a"))],
                kind: ColumnLineageKind::Passthrough,
            }]
        );
    }
}
