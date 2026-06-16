//! The binder: lowers a `sqlparser` AST into the bound [`Operator`] tree,
//! resolving every column reference against the visible output schema.
//!
//! Brick ② (minimal core): catalog-free SELECT / FROM / JOIN / WHERE /
//! projection over column refs and simple expressions. Tables bind `Open`
//! (any name resolves `Base { Inferred }`); a name owned by several
//! relations is `Ambiguous`, by none `Unresolved`. Catalog matching,
//! GROUP BY / clauses, derived tables / CTEs, set ops, and DML are later
//! bricks — unhandled constructs fall to [`Operator::Empty`] for now.

use sqlparser::ast::{
    Expr as SqlExpr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident,
    JoinConstraint, JoinOperator, Query, Select, SelectItem, SetExpr, Statement, TableFactor,
    TableWithJoins,
};

use super::operator::{Binding, ColRef, Columns, Expr, Filter, NamedExpr, Operator, Project, Scan};
use crate::casing::{CaseFold, IdentifierCasing};
use crate::catalog::Catalog;
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

/// A bind-time output column: the relation it's exposed under, its name, and
/// whether it's a base-table column (`Some` carries the canonical table +
/// resolution) or a produced / derived one (`None`). Scratch — never stored
/// on the [`Operator`] tree.
#[derive(Clone)]
struct SchemaCol {
    qualifier: Option<Ident>,
    /// `None` is an `Open` scan's wildcard slot (matches any name).
    name: Option<Ident>,
    base: Option<(TableReference, ResolutionKind)>,
}

/// The output columns a (sub)plan exposes for resolution. Bind-time scratch.
type Schema = Vec<SchemaCol>;

struct Binder<'a> {
    #[allow(dead_code)] // catalog matching arrives in brick ③ (currently all Open)
    catalog: Option<&'a Catalog>,
    casing: IdentifierCasing,
}

impl Binder<'_> {
    fn bind_statement(&self, statement: &Statement) -> Operator {
        match statement {
            Statement::Query(query) => self.bind_query(query).0,
            _ => Operator::Empty,
        }
    }

    fn bind_query(&self, query: &Query) -> (Operator, Schema) {
        self.bind_set_expr(&query.body)
    }

    fn bind_set_expr(&self, body: &SetExpr) -> (Operator, Schema) {
        match body {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(query) => self.bind_query(query),
            _ => (Operator::Empty, Vec::new()),
        }
    }

    fn bind_select(&self, select: &Select) -> (Operator, Schema) {
        let (from, from_schema) = self.bind_from(&select.from);
        // WHERE: a filter over the FROM; predicate columns are reads, never
        // lineage origins.
        let node = match &select.selection {
            Some(predicate) => Operator::Filter(Filter {
                input: Box::new(from),
                predicate: vec![self.bind_expr(predicate, &from_schema)],
            }),
            None => from,
        };
        // SELECT: the projection, resolved against the FROM schema.
        let exprs: Vec<NamedExpr> = select
            .projection
            .iter()
            .filter_map(|item| self.bind_select_item(item, &from_schema))
            .collect();
        let out = project_schema(&exprs);
        let project = Operator::Project(Project {
            input: Box::new(node),
            exprs,
        });
        (project, out)
    }

    fn bind_from(&self, items: &[TableWithJoins]) -> (Operator, Schema) {
        let mut iter = items.iter();
        let Some(first) = iter.next() else {
            return (Operator::Empty, Vec::new());
        };
        let (mut node, mut schema) = self.bind_table_with_joins(first);
        // Comma-separated FROM items are a cross join.
        for twj in iter {
            let (right, right_schema) = self.bind_table_with_joins(twj);
            schema.extend(right_schema);
            node = join(node, right, Vec::new());
        }
        (node, schema)
    }

    fn bind_table_with_joins(&self, twj: &TableWithJoins) -> (Operator, Schema) {
        let (mut node, mut schema) = self.bind_table_factor(&twj.relation);
        for j in &twj.joins {
            let (right, right_schema) = self.bind_table_factor(&j.relation);
            schema.extend(right_schema);
            // The ON predicate resolves against both sides' columns.
            let on = join_on(&j.join_operator)
                .map(|e| self.bind_expr(e, &schema))
                .into_iter()
                .collect();
            node = join(node, right, on);
        }
        (node, schema)
    }

    fn bind_table_factor(&self, factor: &TableFactor) -> (Operator, Schema) {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                let Ok(table) = TableReference::try_from_name(name) else {
                    return (Operator::Empty, Vec::new());
                };
                // Catalog-free for now: every table is `Open` (any name
                // resolves `Inferred`).
                let qualifier = alias
                    .as_ref()
                    .map(|a| a.name.clone())
                    .unwrap_or_else(|| table.name.clone());
                let scan = Operator::Scan(Scan {
                    table: table.clone(),
                    columns: Columns::Open,
                    resolution: ResolutionKind::Inferred,
                });
                let schema = vec![SchemaCol {
                    qualifier: Some(qualifier),
                    name: None,
                    base: Some((table, ResolutionKind::Inferred)),
                }];
                (scan, schema)
            }
            // Derived tables / table functions / nested joins are later bricks.
            _ => (Operator::Empty, Vec::new()),
        }
    }

    fn bind_select_item(&self, item: &SelectItem, schema: &Schema) -> Option<NamedExpr> {
        match item {
            SelectItem::UnnamedExpr(expr) => Some(NamedExpr {
                name: inferred_name(expr),
                expr: self.bind_expr(expr, schema),
            }),
            SelectItem::ExprWithAlias { expr, alias } => Some(NamedExpr {
                name: Some(alias.clone()),
                expr: self.bind_expr(expr, schema),
            }),
            // Wildcards are suppressed (a later brick records the diagnostic).
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => None,
        }
    }

    /// Resolve a `sqlparser` expression into a bound [`Expr`]. Column refs
    /// resolve against `schema`; functions / operators become a `Call`
    /// (transformation over value args); other forms are later bricks.
    fn bind_expr(&self, expr: &SqlExpr, schema: &Schema) -> Expr {
        match expr {
            SqlExpr::Identifier(id) => Expr::Column(Box::new(self.resolve(None, id, schema))),
            SqlExpr::CompoundIdentifier(parts) => {
                let name = parts.last().expect("compound identifier is non-empty");
                let qualifier = (parts.len() >= 2).then(|| parts[parts.len() - 2].clone());
                Expr::Column(Box::new(self.resolve(qualifier.as_ref(), name, schema)))
            }
            SqlExpr::Nested(inner) => self.bind_expr(inner, schema),
            SqlExpr::BinaryOp { left, right, .. } => Expr::Call {
                args: vec![self.bind_expr(left, schema), self.bind_expr(right, schema)],
            },
            SqlExpr::UnaryOp { expr, .. } => Expr::Call {
                args: vec![self.bind_expr(expr, schema)],
            },
            SqlExpr::Function(function) => Expr::Call {
                args: self.bind_function_args(function, schema),
            },
            // Literals and not-yet-modelled forms contribute no column refs.
            _ => Expr::Call { args: Vec::new() },
        }
    }

    fn bind_function_args(&self, function: &Function, schema: &Schema) -> Vec<Expr> {
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
                } => Some(self.bind_expr(e, schema)),
                _ => None,
            })
            .collect()
    }

    /// Resolve a (qualifier, name) against the visible schema. Catalog-free:
    /// one matching relation → `Base { Inferred }`; several → `Ambiguous`;
    /// none → `Unresolved`. (The Known-witness / catalog tiebreaker is brick ③.)
    fn resolve(&self, qualifier: Option<&Ident>, name: &Ident, schema: &Schema) -> ColRef {
        let candidates: Vec<&SchemaCol> = schema
            .iter()
            .filter(|c| {
                qualifier.is_none_or(|q| {
                    c.qualifier
                        .as_ref()
                        .is_some_and(|cq| self.eq(self.casing.table_alias, cq, q))
                })
            })
            .filter(|c| match &c.name {
                Some(n) => self.eq(self.casing.column, n, name),
                None => true, // Open wildcard slot matches any name
            })
            .collect();
        let binding = match candidates.as_slice() {
            [] => Binding::Unresolved,
            [c] => match &c.base {
                Some((table, resolution)) => Binding::Base {
                    table: table.clone(),
                    resolution: *resolution,
                },
                None => Binding::Derived,
            },
            _ => Binding::Ambiguous,
        };
        ColRef {
            qualifier: qualifier.cloned(),
            name: name.clone(),
            binding,
        }
    }

    fn eq(&self, fold: CaseFold, a: &Ident, b: &Ident) -> bool {
        fold.normalize(a) == fold.normalize(b)
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

/// The output schema a `Project` exposes: each output column by name, marked
/// derived (`base = None`) — a reference to it is not a base-table read.
fn project_schema(exprs: &[NamedExpr]) -> Schema {
    exprs
        .iter()
        .map(|ne| SchemaCol {
            qualifier: None,
            name: ne.name.clone(),
            base: None,
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
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;

    fn bind(sql: &str) -> Operator {
        let statements = Parser::parse_sql(&GenericDialect {}, sql).unwrap();
        let casing = IdentifierCasing::for_dialect(&GenericDialect {});
        build(&statements[0], None, casing)
    }

    /// The single column of a `Project` whose expr is a bare `Column`.
    fn only_column(op: &Operator) -> &ColRef {
        let Operator::Project(p) = op else {
            panic!("expected Project, got {op:?}")
        };
        match &p.exprs[..] {
            [NamedExpr {
                expr: Expr::Column(c),
                ..
            }] => c,
            other => panic!("expected one column expr, got {other:?}"),
        }
    }

    #[test]
    fn select_from_single_table_resolves_base() {
        let op = bind("SELECT a FROM t");
        let Operator::Project(p) = &op else { panic!() };
        assert!(matches!(&*p.input, Operator::Scan(s) if s.table.name.value == "t"));
        let c = only_column(&op);
        assert_eq!(c.name.value, "a");
        assert!(matches!(&c.binding, Binding::Base { table, resolution }
            if table.name.value == "t" && *resolution == ResolutionKind::Inferred));
    }

    #[test]
    fn where_wraps_a_filter_below_the_project() {
        let op = bind("SELECT a FROM t WHERE b > 0");
        let Operator::Project(p) = &op else { panic!() };
        let Operator::Filter(f) = &*p.input else {
            panic!("expected Filter below Project, got {:?}", p.input)
        };
        assert!(matches!(&*f.input, Operator::Scan(_)));
        assert_eq!(f.predicate.len(), 1);
    }

    #[test]
    fn unqualified_over_two_open_tables_is_ambiguous() {
        // catalog-free: `a` could be t1's or t2's → Ambiguous.
        let op = bind("SELECT a FROM t1 JOIN t2 ON t1.id = t2.id");
        let Operator::Project(p) = &op else { panic!() };
        assert!(matches!(&*p.input, Operator::Join(_)));
        assert!(matches!(only_column(&op).binding, Binding::Ambiguous));
    }

    #[test]
    fn qualified_ref_pins_one_relation() {
        let op = bind("SELECT t1.a FROM t1 JOIN t2 ON t1.id = t2.id");
        assert!(
            matches!(&only_column(&op).binding, Binding::Base { table, .. }
            if table.name.value == "t1")
        );
    }

    #[test]
    fn unknown_no_from_is_unresolved() {
        let op = bind("SELECT a");
        assert!(matches!(only_column(&op).binding, Binding::Unresolved));
    }

    #[test]
    fn expression_becomes_a_call() {
        let op = bind("SELECT a + b AS s FROM t");
        let Operator::Project(p) = &op else { panic!() };
        match &p.exprs[..] {
            [NamedExpr {
                name: Some(n),
                expr: Expr::Call { args },
            }] => {
                assert_eq!(n.value, "s");
                assert_eq!(args.len(), 2); // a, b
            }
            other => panic!("expected one Call expr, got {other:?}"),
        }
    }
}
