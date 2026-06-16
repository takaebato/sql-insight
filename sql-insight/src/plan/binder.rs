//! The binder: lowers a `sqlparser` AST into the bound [`Operator`] tree,
//! resolving every column reference against the bind-time scope.
//!
//! Brick ③: catalog-aware SELECT / FROM / JOIN / WHERE / projection. A table
//! factor is matched against the catalog (right-anchored, dialect-cased) into
//! a canonical identity + `Known` columns + [`ResolutionKind`], or `Open`
//! (catalog-free / miss / ambiguous). Column resolution ranks the in-scope
//! relations (Known-witness over Open suspect → `Inferred` downgrade;
//! several owners → `Ambiguous`; none → `Unresolved`). Clauses (GROUP BY /
//! ORDER BY), derived tables / CTEs, set ops, and DML are later bricks —
//! unhandled constructs fall to [`Operator::Empty`].

use sqlparser::ast::{
    Expr as SqlExpr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident,
    JoinConstraint, JoinOperator, Query, Select, SelectItem, SetExpr, Statement, TableFactor,
    TableWithJoins,
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
    Binder { catalog, casing }.bind_statement(statement)
}

// ===== bind-time scope (scratch, relation-grouped) =======================

/// The relations visible at a point in the bind. Scratch — never stored on
/// the [`Operator`] tree.
#[derive(Default)]
struct Scope {
    relations: Vec<Relation>,
}

/// A relation in scope: its use-site alias (if any) and where its columns
/// come from.
struct Relation {
    alias: Option<Ident>,
    source: RelSource,
}

enum RelSource {
    /// A real table: its canonical identity, catalog column knowledge, and
    /// table-level resolution. (Derived / CTE / table-function relations are
    /// later bricks.)
    Table {
        table: TableReference,
        columns: Columns,
        resolution: ResolutionKind,
    },
}

impl Relation {
    /// The name this relation answers to in a qualifier: its alias, else a
    /// real table's bare name.
    fn exposed_name(&self) -> Option<&Ident> {
        self.alias.as_ref().or(match &self.source {
            RelSource::Table { table, .. } => Some(&table.name),
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
}

impl Binder<'_> {
    fn bind_statement(&self, statement: &Statement) -> Operator {
        match statement {
            Statement::Query(query) => self.bind_query(query),
            _ => Operator::Empty,
        }
    }

    fn bind_query(&self, query: &Query) -> Operator {
        self.bind_set_expr(&query.body)
    }

    fn bind_set_expr(&self, body: &SetExpr) -> Operator {
        match body {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(query) => self.bind_query(query),
            _ => Operator::Empty,
        }
    }

    fn bind_select(&self, select: &Select) -> Operator {
        let (from, scope) = self.bind_from(&select.from);
        // WHERE: a filter over the FROM; predicate columns are reads, never
        // lineage origins.
        let node = match &select.selection {
            Some(predicate) => Operator::Filter(Filter {
                input: Box::new(from),
                predicate: vec![self.bind_expr(predicate, &scope)],
            }),
            None => from,
        };
        // SELECT: the projection, resolved against the FROM scope.
        let exprs: Vec<NamedExpr> = select
            .projection
            .iter()
            .filter_map(|item| self.bind_select_item(item, &scope))
            .collect();
        Operator::Project(Project {
            input: Box::new(node),
            exprs,
        })
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
                    alias: alias.as_ref().map(|a| a.name.clone()),
                    source: RelSource::Table {
                        table: m.table,
                        columns,
                        resolution: m.resolution,
                    },
                };
                (
                    scan,
                    Scope {
                        relations: vec![relation],
                    },
                )
            }
            // Derived tables / table functions / nested joins are later bricks.
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
            // Literals and not-yet-modelled forms contribute no column refs.
            _ => Expr::Call { args: Vec::new() },
        }
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

    // ===== column resolution =============================================

    /// Resolve a dotted reference (`parts`) against the scope. Unqualified
    /// ranks every relation; qualified matches the qualifier first. Collapsed
    /// by [`pick`](Self::pick).
    fn resolve(&self, parts: &[Ident], scope: &Scope) -> ColRef {
        let name = parts.last().expect("a reference has at least one segment");
        let binding = if parts.len() == 1 {
            let candidates = scope
                .relations
                .iter()
                .filter_map(|rel| self.unqualified_candidate(rel, name))
                .collect();
            self.pick(candidates)
        } else {
            let qualifier_parts = &parts[..parts.len() - 1];
            let qualifier_ref = TableReference::try_from_parts(qualifier_parts);
            let candidates = scope
                .relations
                .iter()
                .filter_map(|rel| {
                    self.qualified_candidate(rel, qualifier_parts, qualifier_ref.as_ref(), name)
                })
                .collect();
            self.pick(candidates)
        };
        ColRef {
            qualifier: (parts.len() >= 2).then(|| parts[parts.len() - 2].clone()),
            name: name.clone(),
            binding,
        }
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
