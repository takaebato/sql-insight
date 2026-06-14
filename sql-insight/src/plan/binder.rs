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
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr,
    GroupByWithModifier, Ident, Join, JoinConstraint, JoinOperator, OrderBy, OrderByKind, Query,
    Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins,
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
/// bind, plus (for the output-alias-visible clauses) the enclosing
/// SELECT's output columns. Scratch — never stored on the [`Plan`].
pub(crate) struct Scope {
    relations: Vec<Relation>,
    /// The enclosing SELECT's output columns, visible to its own
    /// GROUP BY / HAVING / ORDER BY (SQL alias visibility). Empty at
    /// FROM-level resolution (WHERE / projection / JOIN ON).
    outputs: Vec<BoundColumn>,
}

impl Scope {
    fn empty() -> Self {
        Self {
            relations: Vec::new(),
            outputs: Vec::new(),
        }
    }

    fn of(relation: Relation) -> Self {
        Self {
            relations: vec![relation],
            outputs: Vec::new(),
        }
    }

    /// Concatenate the relations of two scopes (a join / comma). Output
    /// columns aren't merged — they belong to a single SELECT.
    fn merge(mut self, mut other: Scope) -> Scope {
        self.relations.append(&mut other.relations);
        self
    }
}

/// A relation visible in a [`Scope`]: its use-site alias and where its
/// columns come from.
struct Relation {
    alias: Option<Ident>,
    source: RelationSource,
}

impl Relation {
    /// The name this relation answers to in a qualifier: its alias if
    /// aliased, otherwise a real table's bare name. A derived table with
    /// no alias answers to nothing (SQL requires the alias anyway).
    fn exposed_name(&self) -> Option<&Ident> {
        self.alias.as_ref().or(match &self.source {
            RelationSource::Table { table, .. } => Some(&table.name),
            RelationSource::Derived { .. } => None,
        })
    }
}

/// Where a relation's columns come from.
enum RelationSource {
    /// A real stored table: its canonical identity plus catalog column
    /// knowledge (`Open` catalog-free, `Known` with a catalog).
    Table {
        table: TableReference,
        columns: RelationColumns,
    },
    /// A derived table / subquery: its output columns, each already
    /// resolved to real base columns (pre-collapsed provenance).
    /// *Synthetic* — it has no table identity, so a reference through it
    /// surfaces the inner real columns, never the derived name itself.
    Derived { columns: Vec<BoundColumn> },
}

/// What a real table exposes for resolution.
enum RelationColumns {
    /// Column set unknown (catalog-free, or a catalog miss / ambiguous
    /// match) — any name could plausibly belong here (`Inferred`).
    Open,
    /// Catalog-known columns (quoted = exact-match idents). A name in
    /// the list resolves `Cataloged`; a name absent means the relation
    /// can't own it.
    Known(Vec<Ident>),
}

/// One candidate owner of a column reference during resolution, carrying
/// the provenance it would contribute.
struct Candidate {
    /// The real base columns this candidate resolves the reference to:
    /// one entry for a real table, the derived column's full (already
    /// collapsed) provenance for a synthetic one.
    provenance: Vec<ColumnRead>,
    /// A `Known` schema lists the column (or a derived relation exposes
    /// it) — drives the Known-witness-over-Open tiebreaker.
    confirmed: bool,
    /// The candidate is a derived / synthetic relation. Its provenance is
    /// already collapsed to real columns, so the witness tiebreaker keeps
    /// it verbatim rather than downgrading to `Inferred`: a real table's
    /// own ref is downgraded, but a synthetic relation's inner refs keep
    /// their resolution (the synthetic name never surfaces anyway).
    synthetic: bool,
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
        let (body, scope) = match query.body.as_ref() {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(inner) => self.bind_query(inner),
            // Set operations, VALUES, `WITH … <DML>` wrappers: later
            // bricks — an opaque relation with no columns for now.
            _ => (Plan::OpaqueLeaf(OpaqueLeaf { alias: None }), Scope::empty()),
        };
        // A trailing ORDER BY sits above the body and sees its output
        // aliases (resolved against the body's output scope).
        let plan = match &query.order_by {
            Some(order_by) => {
                let reads = self.order_by_reads(order_by, &scope);
                wrap_reads(body, reads)
            }
            None => body,
        };
        (plan, scope)
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
        let project = Plan::Project(Project {
            input: Box::new(input),
            outputs: outputs.clone(),
        });
        // GROUP BY / HAVING / SORT BY see the output aliases (clause
        // phase): resolve against the FROM relations *plus* the outputs.
        let clause_scope = Scope {
            relations: from_scope.relations,
            outputs,
        };
        let mut clause_reads = self.group_by_reads(&select.group_by, &clause_scope);
        if let Some(having) = &select.having {
            clause_reads.extend(self.resolve_reads(having, &clause_scope));
        }
        for sort in &select.sort_by {
            clause_reads.extend(self.resolve_reads(&sort.expr, &clause_scope));
        }
        // A trailing top-level ORDER BY also resolves against this scope,
        // so hand it back to `bind_query`.
        (wrap_reads(project, clause_reads), clause_scope)
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
                        alias: alias.clone(),
                        source: RelationSource::Table {
                            table: table.clone(),
                            columns,
                        },
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
            // A derived table `(<subquery>) AS d`: bind the subquery and
            // expose its output columns as a synthetic relation. Those
            // outputs already carry collapsed provenance, so an outer
            // reference through `d` surfaces the inner real columns —
            // collapse falls out of construction. The subquery's plan is
            // this factor's plan (an input to the enclosing operators).
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let (plan, sub_scope) = self.bind_query(subquery);
                let relation = Relation {
                    alias: alias.as_ref().map(|a| a.name.clone()),
                    source: RelationSource::Derived {
                        columns: sub_scope.outputs,
                    },
                };
                (plan, Scope::of(relation))
            }
            // Table functions / pivots / VALUES etc. are later bricks;
            // they expose no inspectable columns yet.
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
            // Output-alias visibility (GROUP BY / HAVING / ORDER BY): a
            // bare name matching an enclosing output column is a
            // reference to that output — return its provenance so an
            // introduced alias resolves to its real sources instead of a
            // phantom stored column. Empty at FROM-level (no outputs).
            if let Some(output) = scope
                .outputs
                .iter()
                .find(|c| self.name_matches(c.name.as_ref(), column))
            {
                return output.provenance.clone();
            }
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
                .filter(|rel| {
                    rel.exposed_name()
                        .is_some_and(|n| self.ident_eq(n, qualifier))
                })
                .filter_map(|rel| self.qualified_candidate(rel, column))
                .collect()
        };
        self.pick(candidates, column)
    }

    /// Reads contributed by GROUP BY (plain keys + ROLLUP / CUBE /
    /// GROUPING SETS members + a `GROUPING SETS` modifier), resolved
    /// against `scope` (which carries the output aliases).
    fn group_by_reads(&self, group_by: &GroupByExpr, scope: &Scope) -> Vec<ColumnRead> {
        let mut refs = Vec::new();
        if let GroupByExpr::Expressions(exprs, modifiers) = group_by {
            for expr in exprs {
                collect_column_refs(expr, &mut refs);
            }
            for modifier in modifiers {
                if let GroupByWithModifier::GroupingSets(expr) = modifier {
                    collect_column_refs(expr, &mut refs);
                }
            }
        }
        refs.iter()
            .flat_map(|parts| self.resolve_ref(parts, scope))
            .collect()
    }

    /// Reads contributed by an ORDER BY, resolved against `scope`.
    fn order_by_reads(&self, order_by: &OrderBy, scope: &Scope) -> Vec<ColumnRead> {
        let OrderByKind::Expressions(exprs) = &order_by.kind else {
            return Vec::new();
        };
        let mut refs = Vec::new();
        for expr in exprs {
            collect_column_refs(&expr.expr, &mut refs);
        }
        refs.iter()
            .flat_map(|parts| self.resolve_ref(parts, scope))
            .collect()
    }

    fn name_matches(&self, name: Option<&Ident>, other: &Ident) -> bool {
        name.is_some_and(|n| self.casing.column.normalize(n) == self.casing.column.normalize(other))
    }

    /// A relation is an unqualified candidate iff it could own `column`:
    /// a `Known` schema must list it; an `Open` real table always could;
    /// a derived relation must expose it.
    fn unqualified_candidate(&self, rel: &Relation, column: &Ident) -> Option<Candidate> {
        match &rel.source {
            RelationSource::Table {
                table,
                columns: RelationColumns::Known(cols),
            } => self.list_has(cols, column).then(|| Candidate {
                provenance: vec![read(table, column, ResolutionKind::Cataloged)],
                confirmed: true,
                synthetic: false,
            }),
            RelationSource::Table {
                table,
                columns: RelationColumns::Open,
            } => Some(Candidate {
                provenance: vec![read(table, column, ResolutionKind::Inferred)],
                confirmed: false,
                synthetic: false,
            }),
            RelationSource::Derived { columns } => self.derived_candidate(columns, column),
        }
    }

    /// A candidate for a qualified ref whose qualifier already matched
    /// `rel`. Differs from the unqualified case only for a `Known` table
    /// that doesn't list the column: the qualifier pins the relation, so
    /// it still resolves (`Inferred`) rather than dropping out. A derived
    /// relation that doesn't expose the column contributes nothing.
    fn qualified_candidate(&self, rel: &Relation, column: &Ident) -> Option<Candidate> {
        match &rel.source {
            RelationSource::Table {
                table,
                columns: RelationColumns::Known(cols),
            } => {
                let confirmed = self.list_has(cols, column);
                Some(Candidate {
                    provenance: vec![read(
                        table,
                        column,
                        if confirmed {
                            ResolutionKind::Cataloged
                        } else {
                            ResolutionKind::Inferred
                        },
                    )],
                    confirmed,
                    synthetic: false,
                })
            }
            RelationSource::Table {
                table,
                columns: RelationColumns::Open,
            } => Some(Candidate {
                provenance: vec![read(table, column, ResolutionKind::Inferred)],
                confirmed: false,
                synthetic: false,
            }),
            RelationSource::Derived { columns } => self.derived_candidate(columns, column),
        }
    }

    /// A derived relation is a candidate iff it exposes an output column
    /// named `column`; its (already collapsed) provenance is the
    /// candidate's. Synthetic — the witness tiebreaker keeps it verbatim.
    fn derived_candidate(&self, columns: &[BoundColumn], column: &Ident) -> Option<Candidate> {
        columns
            .iter()
            .find(|c| self.name_matches(c.name.as_ref(), column))
            .map(|c| Candidate {
                provenance: c.provenance.clone(),
                confirmed: true,
                synthetic: true,
            })
    }

    /// Collapse candidates to the reference's provenance (the resolver's
    /// rule): none → `Unresolved`; one → its provenance verbatim (already
    /// `Cataloged` / `Inferred` / collapsed); several with exactly one
    /// confirmed → that Known witness wins (a real table downgrades to
    /// `Inferred`, a synthetic one keeps its provenance); otherwise
    /// `Ambiguous`.
    fn pick(&self, candidates: Vec<Candidate>, column: &Ident) -> Vec<ColumnRead> {
        if candidates.is_empty() {
            return vec![unresolved(column)];
        }
        if candidates.len() == 1 {
            return candidates.into_iter().next().unwrap().provenance;
        }
        let mut confirmed = candidates.into_iter().filter(|c| c.confirmed);
        match (confirmed.next(), confirmed.next()) {
            (Some(witness), None) => {
                if witness.synthetic {
                    witness.provenance
                } else {
                    downgrade_to_inferred(witness.provenance)
                }
            }
            _ => vec![ambiguous(column)],
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
        // GROUP BY ROLLUP / CUBE / GROUPING SETS — each grouping set is a
        // list of expressions.
        Expr::Rollup(sets) | Expr::Cube(sets) | Expr::GroupingSets(sets) => {
            for set in sets {
                for expr in set {
                    collect_column_refs(expr, out);
                }
            }
        }
        _ => {}
    }
}

/// Wrap `plan` in a filter `PassThrough` carrying `reads`, or return it
/// unchanged when there are none.
fn wrap_reads(plan: Plan, reads: Vec<ColumnRead>) -> Plan {
    if reads.is_empty() {
        plan
    } else {
        Plan::PassThrough(PassThrough {
            inputs: vec![plan],
            reads,
        })
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

/// Downgrade a real-table witness's provenance to `Inferred` — the
/// Known-witness-over-Open tiebreaker adopts it without firm evidence.
/// (Synthetic witnesses skip this: their inner refs keep their own
/// resolution since the synthetic name never surfaces.)
fn downgrade_to_inferred(provenance: Vec<ColumnRead>) -> Vec<ColumnRead> {
    provenance
        .into_iter()
        .map(|mut read| {
            read.resolution = ResolutionKind::Inferred;
            read
        })
        .collect()
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
    fn derived_table_exposes_inner_columns_collapsed() {
        // `(SELECT a AS x FROM t) d` becomes a synthetic relation whose
        // output column `x` already carries `t.a` as provenance. The outer
        // `d.x` resolves to that — collapse falls out of construction, so
        // both Projects carry the same inner real column.
        assert_eq!(
            bind_one("SELECT d.x FROM (SELECT a AS x FROM t) d"),
            Plan::Project(Project {
                input: Box::new(Plan::Project(Project {
                    input: Box::new(scan("t")),
                    outputs: vec![passthrough_col("x", vec![inferred("t", "a")])],
                })),
                outputs: vec![passthrough_col("x", vec![inferred("t", "a")])],
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
