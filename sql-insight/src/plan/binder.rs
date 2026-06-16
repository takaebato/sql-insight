//! The binder: lowers a `sqlparser` AST into the bound [`Operator`] tree,
//! resolving every column reference against the bind-time scope.
//!
//! Bricks ③–⑤: catalog-aware SELECT / FROM / JOIN / WHERE / projection,
//! GROUP BY / HAVING / ORDER BY, derived tables, CTEs, and set operations.
//! A table factor is matched against the catalog (right-anchored,
//! dialect-cased) into a canonical identity with its `Known` columns and
//! table-level [`ResolutionKind`], or `Open` (catalog-free / miss / ambiguous).
//! Column resolution ranks the in-scope relations: a Known-witness over an Open
//! suspect downgrades to `Inferred`, several owners give `Ambiguous`, none
//! `Unresolved`, and a derived / CTE relation that exposes the column gives
//! `Derived`. GROUP BY / HAVING / ORDER BY resolve against the FROM relations
//! together with the projection outputs (clause-alias visibility): an
//! introduced alias becomes `Derived` (dropped from reads), an identity
//! passthrough falls through to the real column. Their reads ride on a filter /
//! sort above the projection. DML and the remaining breadth are later bricks —
//! unhandled constructs fall to [`Operator::Empty`].

use sqlparser::ast::{
    Expr as SqlExpr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr,
    GroupByWithModifier, Ident, JoinConstraint, JoinOperator, OrderBy, OrderByExpr, OrderByKind,
    Query, Select, SelectItem, SetExpr, Statement, TableAlias, TableFactor, TableWithJoins,
};

use super::operator::{Binding, ColRef, Columns, Expr, Filter, NamedExpr, Operator, Project, Scan};
use crate::casing::{CaseFold, IdentifierCasing};
use crate::catalog::{Catalog, CatalogTable};
use crate::reference::{ResolutionKind, TableReference};

/// Bind a statement into an [`Operator`] tree. Statement kinds not yet
/// modelled (everything but a top-level query, in this brick) yield
/// [`Operator::Empty`].
pub(crate) fn build(
    statement: &Statement,
    catalog: Option<&Catalog>,
    casing: IdentifierCasing,
) -> Operator {
    Binder {
        catalog,
        casing,
        ctes: Vec::new(),
        outer: Vec::new(),
    }
    .bind_statement(statement)
}

/// A CTE in scope: its name and the output column names it exposes (so a
/// `FROM cte` reference resolves through them). The body lives once on the
/// owning `With` node; a reference is a lightweight `CteRef`.
#[derive(Clone)]
struct CteEnv {
    name: Ident,
    columns: Vec<Ident>,
}

// ===== bind-time scope (scratch, relation-grouped) =======================

/// The relations visible at a point in the bind. Scratch — never stored on
/// the [`Operator`] tree.
#[derive(Default)]
struct Scope {
    relations: Vec<Relation>,
    /// The enclosing SELECT's output columns, visible to its own GROUP BY /
    /// HAVING / ORDER BY (clause-alias visibility). Empty at FROM-level
    /// resolution (WHERE / projection).
    outputs: Vec<OutputCol>,
}

/// A projection output column, for clause-alias resolution. `identity` marks a
/// bare passthrough (`SELECT a`): a clause reference to it falls through to
/// the real column (a read); a non-identity (introduced) alias resolves to the
/// output itself (`Binding::Derived`, dropped from reads).
struct OutputCol {
    name: Option<Ident>,
    identity: bool,
}

/// A relation in scope: its use-site alias (if any) and where its columns
/// come from. Cloned onto the correlation stack so an inner subquery can
/// resolve against enclosing relations.
#[derive(Clone)]
struct Relation {
    alias: Option<Ident>,
    source: RelSource,
}

#[derive(Clone)]
enum RelSource {
    /// A real table: its canonical identity, catalog column knowledge, and
    /// table-level resolution.
    Table {
        table: TableReference,
        columns: Columns,
        resolution: ResolutionKind,
    },
    /// A derived table / CTE reference: a synthetic relation exposing the named
    /// output columns of an inner query. A reference through it is
    /// `Binding::Derived` — the origin traversal traces into the producing
    /// sub-plan (`SubqueryAlias` / `CteRef`).
    Derived { columns: Vec<Ident> },
}

impl Relation {
    /// The name this relation answers to in a qualifier: its alias, else a
    /// real table's bare name. A derived relation answers only to its alias.
    fn exposed_name(&self) -> Option<&Ident> {
        self.alias.as_ref().or(match &self.source {
            RelSource::Table { table, .. } => Some(&table.name),
            RelSource::Derived { .. } => None,
        })
    }
}

/// One candidate owner of a column reference, with the binding it would
/// contribute and whether it's a confirmed (catalog-listed) witness.
struct Candidate {
    binding: Binding,
    confirmed: bool,
}

struct Binder<'a> {
    catalog: Option<&'a Catalog>,
    casing: IdentifierCasing,
    /// CTEs in scope (declaration order, innermost `WITH` last).
    ctes: Vec<CteEnv>,
    /// Enclosing queries' relations (the correlation stack, outermost first)
    /// that an inner subquery's references fall through to.
    outer: Vec<Vec<Relation>>,
}

impl Binder<'_> {
    /// A child binder with a different CTE environment (sharing catalog /
    /// casing / correlation stack).
    fn with_ctes(&self, ctes: Vec<CteEnv>) -> Binder<'_> {
        Binder {
            catalog: self.catalog,
            casing: self.casing,
            ctes,
            outer: self.outer.clone(),
        }
    }

    /// A child binder with one more enclosing scope on the correlation stack
    /// (used when descending into a subquery in an expression / a LATERAL
    /// factor).
    fn with_outer(&self, relations: Vec<Relation>) -> Binder<'_> {
        let mut outer = self.outer.clone();
        outer.push(relations);
        Binder {
            catalog: self.catalog,
            casing: self.casing,
            ctes: self.ctes.clone(),
            outer,
        }
    }

    fn bind_statement(&self, statement: &Statement) -> Operator {
        match statement {
            Statement::Query(query) => self.bind_query(query).0,
            _ => Operator::Empty,
        }
    }

    /// Bind a query, returning the operator and its output scope (relations +
    /// outputs). A leading `WITH` is peeled first: each CTE binds in
    /// declaration order into an environment the later CTEs and the body
    /// resolve against; the bodies are owned by a `With` node, references are
    /// `CteRef`s.
    fn bind_query(&self, query: &Query) -> (Operator, Scope) {
        let Some(with) = &query.with else {
            return self.bind_query_body(query);
        };
        let mut env = self.ctes.clone();
        let mut declared = Vec::new();
        for cte in &with.cte_tables {
            let (mut plan, scope) = self.with_ctes(env.clone()).bind_query(&cte.query);
            let columns = exposed_columns(&scope.outputs, Some(&cte.alias));
            // An explicit `c (x, y)` column list renames the body's output
            // columns so a reference through the CTE traces to them.
            rename_outputs(&mut plan, &alias_column_names(&cte.alias));
            declared.push(super::operator::Cte {
                name: cte.alias.name.clone(),
                plan,
            });
            env.push(CteEnv {
                name: cte.alias.name.clone(),
                columns,
            });
        }
        let (body, scope) = self.with_ctes(env).bind_query_body(query);
        (
            Operator::With(super::operator::With {
                ctes: declared,
                body: Box::new(body),
            }),
            scope,
        )
    }

    /// Bind a query's body and trailing ORDER BY (the `WITH` is already in
    /// scope via `self.ctes`).
    fn bind_query_body(&self, query: &Query) -> (Operator, Scope) {
        let (mut op, scope) = self.bind_set_expr(&query.body);
        if let Some(order_by) = &query.order_by {
            let keys = self.order_by_keys(order_by, &scope);
            if !keys.is_empty() {
                op = sort(op, keys);
            }
        }
        (op, scope)
    }

    fn bind_set_expr(&self, body: &SetExpr) -> (Operator, Scope) {
        match body {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(query) => self.bind_query(query),
            // A set operation: result columns are the left operand's (names
            // from the left, positional merge).
            SetExpr::SetOperation { left, right, .. } => {
                let (l, scope) = self.bind_set_expr(left);
                let (r, _) = self.bind_set_expr(right);
                (
                    Operator::SetOp(super::operator::SetOp {
                        left: Box::new(l),
                        right: Box::new(r),
                    }),
                    scope,
                )
            }
            _ => (Operator::Empty, Scope::default()),
        }
    }

    /// Bind a SELECT, returning the operator and its clause scope (FROM
    /// relations + projection outputs) for a trailing ORDER BY to resolve
    /// against.
    fn bind_select(&self, select: &Select) -> (Operator, Scope) {
        let (from, scope) = self.bind_from(&select.from);
        // WHERE: a filter over the FROM (no output aliases visible — the
        // clause-phase rule, structural).
        let mut node = match &select.selection {
            Some(predicate) => Operator::Filter(Filter {
                input: Box::new(from),
                predicate: vec![self.bind_expr(predicate, &scope)],
            }),
            None => from,
        };
        // SELECT: the projection resolves against the FROM scope.
        let exprs: Vec<NamedExpr> = select
            .projection
            .iter()
            .filter_map(|item| self.bind_select_item(item, &scope))
            .collect();
        let outputs = self.output_cols(&exprs);
        node = Operator::Project(Project {
            input: Box::new(node),
            exprs,
        });
        // GROUP BY / HAVING / SORT BY see the output aliases (clause phase):
        // resolve against the FROM relations *plus* the outputs. Their columns
        // are reads, not lineage origins, so they ride on a filter / sort
        // above the projection (which the origin traversal peels through).
        let clause_scope = Scope {
            relations: scope.relations,
            outputs,
        };
        let mut clause_reads = self.group_by_keys(&select.group_by, &clause_scope);
        if let Some(having) = &select.having {
            clause_reads.push(self.bind_expr(having, &clause_scope));
        }
        if !clause_reads.is_empty() {
            node = Operator::Filter(Filter {
                input: Box::new(node),
                predicate: clause_reads,
            });
        }
        let sort_keys = self.order_by_expr_keys(&select.sort_by, &clause_scope);
        if !sort_keys.is_empty() {
            node = sort(node, sort_keys);
        }
        (node, clause_scope)
    }

    fn bind_from(&self, items: &[TableWithJoins]) -> (Operator, Scope) {
        let mut iter = items.iter();
        let Some(first) = iter.next() else {
            return (Operator::Empty, Scope::default());
        };
        let (mut node, mut scope) = self.bind_table_with_joins(first);
        // Comma-separated FROM items are a cross join.
        for twj in iter {
            let (right, right_scope) = self.bind_table_with_joins(twj);
            scope.relations.extend(right_scope.relations);
            node = join(node, right, Vec::new());
        }
        (node, scope)
    }

    fn bind_table_with_joins(&self, twj: &TableWithJoins) -> (Operator, Scope) {
        let (mut node, mut scope) = self.bind_table_factor(&twj.relation);
        for j in &twj.joins {
            let (right, right_scope) = self.bind_table_factor(&j.relation);
            scope.relations.extend(right_scope.relations);
            // The ON predicate resolves against both sides' columns.
            let on = join_on(&j.join_operator)
                .map(|e| self.bind_expr(e, &scope))
                .into_iter()
                .collect();
            node = join(node, right, on);
        }
        (node, scope)
    }

    fn bind_table_factor(&self, factor: &TableFactor) -> (Operator, Scope) {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                let Ok(written) = TableReference::try_from_name(name) else {
                    return (Operator::Empty, Scope::default());
                };
                let alias_name = alias.as_ref().map(|a| a.name.clone());
                // A bare name matching an in-scope CTE resolves to a `CteRef`
                // (the body lives once on the owning `With`) exposing the CTE's
                // output columns as a synthetic relation.
                if written.schema.is_none() && written.catalog.is_none() {
                    if let Some(cte) = self
                        .ctes
                        .iter()
                        .rev()
                        .find(|c| self.eq(self.casing.table_alias, &c.name, &written.name))
                    {
                        let relation = Relation {
                            alias: alias_name.or_else(|| Some(cte.name.clone())),
                            source: RelSource::Derived {
                                columns: cte.columns.clone(),
                            },
                        };
                        return (
                            Operator::CteRef(super::operator::CteRef {
                                name: cte.name.clone(),
                            }),
                            scope_of(relation),
                        );
                    }
                }
                let m = self.table_match(&written);
                let columns = if m.columns.is_empty() {
                    Columns::Open
                } else {
                    Columns::Known(m.columns)
                };
                let scan = Operator::Scan(Scan {
                    table: m.table.clone(),
                    columns: columns.clone(),
                    resolution: m.resolution,
                });
                let relation = Relation {
                    alias: alias_name,
                    source: RelSource::Table {
                        table: m.table,
                        columns,
                        resolution: m.resolution,
                    },
                };
                (scan, scope_of(relation))
            }
            // A derived table `(<subquery>) AS d`: bind the subquery, expose
            // its output columns as a synthetic relation under the alias.
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let (mut op, sub_scope) = self.bind_query(subquery);
                let columns = exposed_columns(&sub_scope.outputs, alias.as_ref());
                let relation = Relation {
                    alias: alias.as_ref().map(|a| a.name.clone()),
                    source: RelSource::Derived { columns },
                };
                let node = match alias {
                    Some(a) => {
                        rename_outputs(&mut op, &alias_column_names(a));
                        Operator::SubqueryAlias(super::operator::SubqueryAlias {
                            alias: a.name.clone(),
                            input: Box::new(op),
                        })
                    }
                    None => op,
                };
                (node, scope_of(relation))
            }
            // Table functions / nested joins are later bricks.
            _ => (Operator::Empty, Scope::default()),
        }
    }

    fn bind_select_item(&self, item: &SelectItem, scope: &Scope) -> Option<NamedExpr> {
        match item {
            SelectItem::UnnamedExpr(expr) => Some(NamedExpr {
                name: inferred_name(expr),
                expr: self.bind_expr(expr, scope),
            }),
            SelectItem::ExprWithAlias { expr, alias } => Some(NamedExpr {
                name: Some(alias.clone()),
                expr: self.bind_expr(expr, scope),
            }),
            // Wildcards are suppressed (a later brick records the diagnostic).
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => None,
        }
    }

    /// Resolve a `sqlparser` expression into a bound [`Expr`].
    fn bind_expr(&self, expr: &SqlExpr, scope: &Scope) -> Expr {
        match expr {
            SqlExpr::Identifier(id) => {
                Expr::Column(Box::new(self.resolve(std::slice::from_ref(id), scope)))
            }
            SqlExpr::CompoundIdentifier(parts) => {
                Expr::Column(Box::new(self.resolve(parts, scope)))
            }
            SqlExpr::Nested(inner) => self.bind_expr(inner, scope),
            SqlExpr::BinaryOp { left, right, .. } => Expr::Call {
                args: vec![self.bind_expr(left, scope), self.bind_expr(right, scope)],
            },
            SqlExpr::UnaryOp { expr, .. } => Expr::Call {
                args: vec![self.bind_expr(expr, scope)],
            },
            SqlExpr::Function(function) => Expr::Call {
                args: self.bind_function_args(function, scope),
            },
            // A scalar subquery (value position): its output flows into the
            // enclosing value.
            SqlExpr::Subquery(query) => Expr::Subquery(Box::new(self.bind_subquery(query, scope))),
            // A test (filter position): its columns are reads, never an origin.
            SqlExpr::Exists { subquery, .. } => {
                Expr::Exists(Box::new(self.bind_subquery(subquery, scope)))
            }
            SqlExpr::InSubquery { expr, subquery, .. } => Expr::InSubquery {
                expr: Box::new(self.bind_expr(expr, scope)),
                subquery: Box::new(self.bind_subquery(subquery, scope)),
            },
            // Literals and not-yet-modelled forms contribute no column refs.
            _ => Expr::Call { args: Vec::new() },
        }
    }

    /// Bind a subquery nested in an expression: its references resolve against
    /// its own FROM plus the containing scope's relations (pushed onto the
    /// correlation stack), so a correlated reference reaches outward.
    fn bind_subquery(&self, query: &Query, scope: &Scope) -> Operator {
        self.with_outer(scope.relations.clone()).bind_query(query).0
    }

    fn bind_function_args(&self, function: &Function, scope: &Scope) -> Vec<Expr> {
        let FunctionArguments::List(list) = &function.args else {
            return Vec::new();
        };
        list.args
            .iter()
            .filter_map(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                }
                | FunctionArg::ExprNamed {
                    arg: FunctionArgExpr::Expr(e),
                    ..
                } => Some(self.bind_expr(e, scope)),
                _ => None,
            })
            .collect()
    }

    // ===== clauses (GROUP BY / HAVING / ORDER BY) ========================

    /// The GROUP BY key expressions (plain + GROUPING SETS members), resolved
    /// against the clause scope. These are reads, not lineage origins.
    fn group_by_keys(&self, group_by: &GroupByExpr, scope: &Scope) -> Vec<Expr> {
        let mut keys = Vec::new();
        if let GroupByExpr::Expressions(exprs, modifiers) = group_by {
            let members = exprs.iter().chain(modifiers.iter().filter_map(|m| match m {
                GroupByWithModifier::GroupingSets(expr) => Some(expr),
                _ => None,
            }));
            for expr in members {
                keys.push(self.bind_expr(expr, scope));
            }
        }
        keys
    }

    /// The ORDER BY key expressions (a trailing `query.order_by`).
    fn order_by_keys(&self, order_by: &OrderBy, scope: &Scope) -> Vec<Expr> {
        let OrderByKind::Expressions(exprs) = &order_by.kind else {
            return Vec::new();
        };
        self.order_by_expr_keys(exprs, scope)
    }

    /// Bind a list of order-by expressions (`query.order_by` members or a
    /// `SELECT … SORT BY` list) as clause reads.
    fn order_by_expr_keys(&self, exprs: &[OrderByExpr], scope: &Scope) -> Vec<Expr> {
        exprs
            .iter()
            .map(|e| self.bind_expr(&e.expr, scope))
            .collect()
    }

    /// Summarise the projection outputs for clause-alias resolution. An output
    /// is *identity* iff it is a bare column under its own name.
    fn output_cols(&self, exprs: &[NamedExpr]) -> Vec<OutputCol> {
        exprs
            .iter()
            .map(|ne| {
                let identity = match &ne.expr {
                    Expr::Column(c) => ne
                        .name
                        .as_ref()
                        .is_some_and(|n| self.eq(self.casing.column, n, &c.name)),
                    _ => false,
                };
                OutputCol {
                    name: ne.name.clone(),
                    identity,
                }
            })
            .collect()
    }

    // ===== column resolution =============================================

    /// Resolve a dotted reference (`parts`) against the scope. Unqualified
    /// ranks every relation; qualified matches the qualifier first. Collapsed
    /// by [`pick`](Self::pick).
    fn resolve(&self, parts: &[Ident], scope: &Scope) -> ColRef {
        let name = parts.last().expect("a reference has at least one segment");
        // Clause-alias visibility (GROUP BY / HAVING / ORDER BY): a bare ref
        // naming an *introduced* output alias resolves to that output
        // (`Derived`, dropped from reads — the real dependency is at the
        // projection). An *identity* passthrough falls through to the real
        // column. (Empty `outputs` at FROM-level, so this is a no-op there.)
        if parts.len() == 1 {
            if let Some(out) = scope.outputs.iter().find(|o| {
                o.name
                    .as_ref()
                    .is_some_and(|n| self.eq(self.casing.column, n, name))
            }) {
                if !out.identity {
                    return ColRef {
                        qualifier: None,
                        name: name.clone(),
                        binding: Binding::Derived,
                    };
                }
            }
        }
        // Resolve against the current scope, then fall through the enclosing
        // scopes (correlation), innermost first, before giving up.
        let binding = self
            .resolve_in(parts, &scope.relations)
            .or_else(|| {
                self.outer
                    .iter()
                    .rev()
                    .find_map(|relations| self.resolve_in(parts, relations))
            })
            .unwrap_or(Binding::Unresolved);
        ColRef {
            qualifier: (parts.len() >= 2).then(|| parts[parts.len() - 2].clone()),
            name: name.clone(),
            binding,
        }
    }

    /// Resolve `parts` against one set of relations, returning `None` when none
    /// could own the column (so the caller falls through to an enclosing
    /// scope) and `Some(binding)` once at least one is a candidate (even if
    /// that collapses to `Ambiguous` — a name owned here doesn't escape).
    fn resolve_in(&self, parts: &[Ident], relations: &[Relation]) -> Option<Binding> {
        let name = parts.last()?;
        let candidates: Vec<Candidate> = if parts.len() == 1 {
            relations
                .iter()
                .filter_map(|rel| self.unqualified_candidate(rel, name))
                .collect()
        } else {
            let qualifier_parts = &parts[..parts.len() - 1];
            let qualifier_ref = TableReference::try_from_parts(qualifier_parts);
            relations
                .iter()
                .filter_map(|rel| {
                    self.qualified_candidate(rel, qualifier_parts, qualifier_ref.as_ref(), name)
                })
                .collect()
        };
        (!candidates.is_empty()).then(|| self.pick(candidates))
    }

    /// A relation is an unqualified candidate iff it could own `name`: a
    /// `Known` schema must list it (confirmed witness); an `Open` table always
    /// could (suspect).
    fn unqualified_candidate(&self, rel: &Relation, name: &Ident) -> Option<Candidate> {
        match &rel.source {
            RelSource::Table {
                table,
                columns: Columns::Known(cols),
                ..
            } => self.list_has(cols, name).then(|| Candidate {
                binding: base(table, ResolutionKind::Cataloged),
                confirmed: true,
            }),
            RelSource::Table {
                table,
                columns: Columns::Open,
                ..
            } => Some(Candidate {
                binding: base(table, ResolutionKind::Inferred),
                confirmed: false,
            }),
            // A derived relation owns the column iff it exposes it (confirmed
            // witness, like a Known table); the origin traversal collapses it.
            RelSource::Derived { columns } => self.list_has(columns, name).then_some(Candidate {
                binding: Binding::Derived,
                confirmed: true,
            }),
        }
    }

    /// A relation is a qualified candidate iff the qualifier matches it: a
    /// non-aliased real table by right-anchored path, anything else by its
    /// single exposed (alias) name. A `Known` table that doesn't list the
    /// column still resolves (`Inferred`) — the qualifier pins it.
    fn qualified_candidate(
        &self,
        rel: &Relation,
        qualifier_parts: &[Ident],
        qualifier_ref: Option<&TableReference>,
        name: &Ident,
    ) -> Option<Candidate> {
        let qualifier_ok = match &rel.source {
            RelSource::Table { table, .. } if rel.alias.is_none() => {
                qualifier_ref.is_some_and(|q| self.qualifier_matches_table(q, table))
            }
            _ => rel.exposed_name().is_some_and(|exposed| {
                matches!(qualifier_parts, [only] if self.eq(self.casing.table_alias, only, exposed))
            }),
        };
        if !qualifier_ok {
            return None;
        }
        match &rel.source {
            RelSource::Table {
                table,
                columns: Columns::Known(cols),
                ..
            } => {
                let confirmed = self.list_has(cols, name);
                let resolution = if confirmed {
                    ResolutionKind::Cataloged
                } else {
                    ResolutionKind::Inferred
                };
                Some(Candidate {
                    binding: base(table, resolution),
                    confirmed,
                })
            }
            RelSource::Table {
                table,
                columns: Columns::Open,
                ..
            } => Some(Candidate {
                binding: base(table, ResolutionKind::Inferred),
                confirmed: false,
            }),
            RelSource::Derived { columns } => self.list_has(columns, name).then_some(Candidate {
                binding: Binding::Derived,
                confirmed: true,
            }),
        }
    }

    /// Collapse candidates to a [`Binding`]: none → `Unresolved`; one → its
    /// binding verbatim; several with exactly one confirmed witness → that
    /// witness, downgraded to `Inferred` (Known-witness-over-Open); otherwise
    /// `Ambiguous`.
    fn pick(&self, candidates: Vec<Candidate>) -> Binding {
        match candidates.len() {
            0 => Binding::Unresolved,
            1 => candidates.into_iter().next().unwrap().binding,
            _ => {
                let mut confirmed = candidates.into_iter().filter(|c| c.confirmed);
                match (confirmed.next(), confirmed.next()) {
                    (Some(witness), None) => downgrade(witness.binding),
                    _ => Binding::Ambiguous,
                }
            }
        }
    }

    /// Match a written table reference against the catalog (after default-fill,
    /// right-anchored, dialect-cased). Unique hit → canonical identity + Known
    /// columns + `Cataloged`; several → written ref + `Ambiguous`; no catalog
    /// or no hit → written ref + `Inferred`.
    fn table_match(&self, written: &TableReference) -> TableMatch {
        let no_hit = |resolution| TableMatch {
            table: written.clone(),
            resolution,
            columns: Vec::new(),
        };
        let Some(catalog) = self.catalog else {
            return no_hit(ResolutionKind::Inferred);
        };
        let filled = fill_query_defaults(written, catalog);
        let fold = self.casing.table;
        let mut hits = catalog
            .tables()
            .iter()
            .filter(|t| catalog_table_matches(&filled, t, fold));
        let Some(first) = hits.next() else {
            return no_hit(ResolutionKind::Inferred);
        };
        if hits.next().is_some() {
            return no_hit(ResolutionKind::Ambiguous);
        }
        let columns = first
            .column_names()
            .iter()
            .map(|c| Ident::with_quote('"', c))
            .collect();
        TableMatch {
            table: canonical_ref(first),
            resolution: ResolutionKind::Cataloged,
            columns,
        }
    }

    /// Right-anchored match of a decoded qualifier against a real table's
    /// `catalog.schema.name`, under the dialect's table casing (an omitted
    /// qualifier segment is a wildcard).
    fn qualifier_matches_table(&self, qualifier: &TableReference, table: &TableReference) -> bool {
        let fold = self.casing.table;
        let opt_eq = |a: Option<&Ident>, b: Option<&Ident>| match (a, b) {
            (Some(x), Some(y)) => fold.normalize(x) == fold.normalize(y),
            _ => true,
        };
        fold.normalize(&qualifier.name) == fold.normalize(&table.name)
            && opt_eq(qualifier.schema.as_ref(), table.schema.as_ref())
            && opt_eq(qualifier.catalog.as_ref(), table.catalog.as_ref())
    }

    fn list_has(&self, columns: &[Ident], name: &Ident) -> bool {
        columns.iter().any(|c| self.eq(self.casing.column, c, name))
    }

    fn eq(&self, fold: CaseFold, a: &Ident, b: &Ident) -> bool {
        fold.normalize(a) == fold.normalize(b)
    }
}

// ===== catalog matching ==================================================

/// The outcome of matching a written table reference against the catalog.
struct TableMatch {
    table: TableReference,
    resolution: ResolutionKind,
    columns: Vec<Ident>,
}

/// Fill a query reference's missing prefix segments from the catalog's
/// defaults (bare → schema then catalog).
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

/// Right-anchored, dialect-cased match of a (default-filled) query reference
/// against a registered table.
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

fn normalize_catalog(segment: &str, fold: CaseFold) -> String {
    fold.normalize(&Ident::with_quote('"', segment))
}

/// The surfaced canonical identity of a matched table: plain (unquoted) idents.
fn canonical_ref(table: &CatalogTable) -> TableReference {
    TableReference {
        catalog: table.catalog_segment().map(Ident::new),
        schema: Some(Ident::new(table.schema_segment())),
        name: Ident::new(table.name_segment()),
    }
}

// ===== small helpers =====================================================

fn base(table: &TableReference, resolution: ResolutionKind) -> Binding {
    Binding::Base {
        table: table.clone(),
        resolution,
    }
}

/// Downgrade a winning real-table witness to `Inferred` — adopted over Open
/// suspects without firm evidence.
fn downgrade(binding: Binding) -> Binding {
    match binding {
        Binding::Base { table, .. } => Binding::Base {
            table,
            resolution: ResolutionKind::Inferred,
        },
        other => other,
    }
}

fn join(left: Operator, right: Operator, on: Vec<Expr>) -> Operator {
    Operator::Join(super::operator::Join {
        left: Box::new(left),
        right: Box::new(right),
        on,
        lateral: false,
    })
}

fn sort(input: Operator, keys: Vec<Expr>) -> Operator {
    Operator::Sort(super::operator::Sort {
        input: Box::new(input),
        keys,
    })
}

fn scope_of(relation: Relation) -> Scope {
    Scope {
        relations: vec![relation],
        ..Scope::default()
    }
}

/// The names of a table alias's explicit column list (`AS d(x, y)`), empty if
/// none.
fn alias_column_names(alias: &TableAlias) -> Vec<Ident> {
    alias.columns.iter().map(|c| c.name.clone()).collect()
}

/// Rename a (sub)plan's output columns positionally to `names` (an explicit
/// `AS d(x, y)` column list). A no-op when `names` is empty. Descends through
/// the clause layers / `With` to the producing `Project` / `Aggregate`; a
/// `SetOp` renames both operands.
fn rename_outputs(op: &mut Operator, names: &[Ident]) {
    if names.is_empty() {
        return;
    }
    match op {
        Operator::Project(p) => {
            for (ne, n) in p.exprs.iter_mut().zip(names) {
                ne.name = Some(n.clone());
            }
        }
        Operator::Aggregate(a) => {
            for (ne, n) in a.group_by.iter_mut().chain(&mut a.aggregates).zip(names) {
                ne.name = Some(n.clone());
            }
        }
        Operator::Sort(s) => rename_outputs(&mut s.input, names),
        Operator::Filter(f) => rename_outputs(&mut f.input, names),
        Operator::With(w) => rename_outputs(&mut w.body, names),
        Operator::SetOp(so) => {
            rename_outputs(&mut so.left, names);
            rename_outputs(&mut so.right, names);
        }
        _ => {}
    }
}

/// The output column names a derived table / CTE exposes: an explicit column
/// alias list (`AS d(x, y)`) renames positionally; otherwise each output keeps
/// its own inferred name (anonymous outputs with no alias are unnameable, so
/// dropped — they can't be referenced).
fn exposed_columns(outputs: &[OutputCol], alias: Option<&TableAlias>) -> Vec<Ident> {
    let alias_columns: Vec<&Ident> = alias
        .map(|a| a.columns.iter().map(|c| &c.name).collect())
        .unwrap_or_default();
    outputs
        .iter()
        .enumerate()
        .filter_map(|(i, o)| {
            alias_columns
                .get(i)
                .map(|n| (*n).clone())
                .or_else(|| o.name.clone())
        })
        .collect()
}

/// The name SQL infers for an unaliased projection item: a bare column keeps
/// its own name; anything else is anonymous.
fn inferred_name(expr: &SqlExpr) -> Option<Ident> {
    match expr {
        SqlExpr::Identifier(id) => Some(id.clone()),
        SqlExpr::CompoundIdentifier(parts) => parts.last().cloned(),
        _ => None,
    }
}

/// The `ON` predicate of a join operator, if any.
fn join_on(op: &JoinOperator) -> Option<&SqlExpr> {
    let constraint = match op {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c) => c,
        _ => return None,
    };
    match constraint {
        JoinConstraint::On(expr) => Some(expr),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, CatalogTable};
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    fn bind(sql: &str) -> Operator {
        bind_cat(sql, None)
    }

    fn bind_cat(sql: &str, catalog: Option<&Catalog>) -> Operator {
        let statements = Parser::parse_sql(&GenericDialect {}, sql).unwrap();
        let casing = IdentifierCasing::for_dialect(&GenericDialect {});
        build(&statements[0], catalog, casing)
    }

    fn only_binding(op: &Operator) -> &Binding {
        let Operator::Project(p) = op else {
            panic!("expected Project, got {op:?}")
        };
        match &p.exprs[..] {
            [NamedExpr {
                expr: Expr::Column(c),
                ..
            }] => &c.binding,
            other => panic!("expected one column expr, got {other:?}"),
        }
    }

    #[test]
    fn catalog_free_single_table_is_inferred() {
        assert!(matches!(only_binding(&bind("SELECT a FROM t")),
            Binding::Base { table, resolution }
            if table.name.value == "t" && *resolution == ResolutionKind::Inferred));
    }

    #[test]
    fn catalog_known_hit_is_cataloged_and_canonical() {
        let cat =
            Catalog::new().table(CatalogTable::new("public", "users").columns(["id", "name"]));
        let op = bind_cat("SELECT name FROM users", Some(&cat));
        match only_binding(&op) {
            Binding::Base { table, resolution } => {
                assert_eq!(table.name.value, "users");
                assert_eq!(table.schema.as_ref().unwrap().value, "public"); // canonicalized
                assert_eq!(*resolution, ResolutionKind::Cataloged);
            }
            other => panic!("expected Base Cataloged, got {other:?}"),
        }
    }

    #[test]
    fn catalog_known_miss_is_unresolved() {
        let cat =
            Catalog::new().table(CatalogTable::new("public", "users").columns(["id", "name"]));
        assert!(matches!(
            only_binding(&bind_cat("SELECT nonexistent FROM users", Some(&cat))),
            Binding::Unresolved
        ));
    }

    #[test]
    fn known_witness_over_open_downgrades_to_inferred() {
        // `known_t` lists `a`; `open_t` is not in the catalog → Open suspect.
        let cat = Catalog::new().table(CatalogTable::new("public", "known_t").columns(["a", "b"]));
        let op = bind_cat(
            "SELECT a FROM known_t JOIN open_t ON known_t.b = open_t.k",
            Some(&cat),
        );
        match only_binding(&op) {
            Binding::Base { table, resolution } => {
                assert_eq!(table.name.value, "known_t");
                assert_eq!(*resolution, ResolutionKind::Inferred); // downgraded
            }
            other => panic!("expected Base Inferred (downgraded), got {other:?}"),
        }
    }

    #[test]
    fn two_known_owners_is_ambiguous() {
        let cat = Catalog::new()
            .table(CatalogTable::new("public", "t1").columns(["id"]))
            .table(CatalogTable::new("public", "t2").columns(["id"]));
        assert!(matches!(
            only_binding(&bind_cat(
                "SELECT id FROM t1 JOIN t2 ON t1.id = t2.id",
                Some(&cat)
            )),
            Binding::Ambiguous
        ));
    }
}
