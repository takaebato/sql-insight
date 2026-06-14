//! The binder: lowers a `sqlparser` AST into the bound [`Plan`] IR,
//! resolving every column reference bottom-up.
//!
//! Resolution runs against a [`Scope`] threaded up through the bind (the
//! current node's output columns + open relations). The scope is
//! bind-time *scratch* — it is never stored on the [`Plan`], which keeps
//! only resolved provenance / reads. This is what makes the phase a
//! single bottom-up pass with no `O(n²)` schema recomputation.

use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, Join, JoinConstraint,
    JoinOperator, Query, Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins,
};

use super::ir::{BoundColumn, OpaqueLeaf, PassThrough, Plan, Project, Scan};
use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableReference};
use crate::resolver::IdentifierCasing;

/// Bind one statement into a [`Plan`], or `None` for statement kinds
/// this brick doesn't model yet (everything except `SELECT`-shaped
/// queries). The top-level scope is discarded — callers consume the
/// resolved tree, not the scope.
pub(crate) fn build(statement: &Statement, casing: IdentifierCasing) -> Option<Plan> {
    let binder = Binder { casing };
    match statement {
        Statement::Query(query) => Some(binder.bind_query(query).0),
        // DML / DDL are later bricks.
        _ => None,
    }
}

/// Bind-time resolution scope: the columns visible at a point in the
/// bind. `columns` are known, named columns with resolved provenance (a
/// `Project`'s outputs); `open` lists relations whose full column set is
/// unknown — a name not in `columns` could belong to any of them
/// (best-effort, catalog-free). Scratch: never stored on the [`Plan`].
pub(crate) struct Scope {
    columns: Vec<BoundColumn>,
    open: Vec<OpenRelation>,
}

impl Scope {
    fn empty() -> Self {
        Self {
            columns: Vec::new(),
            open: Vec::new(),
        }
    }

    /// Concatenate two scopes (the schema of a join / comma of their
    /// relations).
    fn merge(mut self, mut other: Scope) -> Scope {
        self.columns.append(&mut other.columns);
        self.open.append(&mut other.open);
        self
    }
}

/// A relation in a [`Scope`] whose column set is unknown.
struct OpenRelation {
    table: TableReference,
    alias: Option<Ident>,
}

impl OpenRelation {
    /// The name this relation answers to in a qualifier: its alias if
    /// aliased, otherwise its table's bare name.
    fn exposed_name(&self) -> &Ident {
        self.alias.as_ref().unwrap_or(&self.table.name)
    }
}

/// Carries the bind-time context. For now just the dialect casing; a
/// catalog and the correlation scope stack join it in later bricks.
struct Binder {
    casing: IdentifierCasing,
}

impl Binder {
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
        let input_scope = from_scope;
        let outputs: Vec<BoundColumn> = select
            .projection
            .iter()
            .filter_map(|item| self.bind_output_column(item, &input_scope))
            .collect();
        let output_scope = Scope {
            columns: outputs.clone(),
            open: Vec::new(),
        };
        (
            Plan::Project(Project {
                input: Box::new(input),
                outputs,
            }),
            output_scope,
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
                Ok(table) => {
                    let alias = alias.as_ref().map(|a| a.name.clone());
                    let scope = Scope {
                        columns: Vec::new(),
                        open: vec![OpenRelation {
                            table: table.clone(),
                            alias: alias.clone(),
                        }],
                    };
                    (
                        Plan::Scan(Scan {
                            table,
                            alias,
                            // Catalog-free brick: every real table is inferred.
                            resolution: ResolutionKind::Inferred,
                        }),
                        scope,
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
    /// Returns one `ColumnRead` per real source column it lands on (a
    /// single entry in this brick; multiple once USING fan-in / known
    /// multi-provenance columns land).
    fn resolve_ref(&self, parts: &[Ident], scope: &Scope) -> Vec<ColumnRead> {
        let Some(column) = parts.last() else {
            return Vec::new();
        };
        if parts.len() == 1 {
            // A known output column inherits its provenance directly.
            if let Some(known) = scope
                .columns
                .iter()
                .find(|c| self.name_eq(c.name.as_ref(), column))
            {
                return known.provenance.clone();
            }
            // Otherwise it must come from an open relation. Catalog-free,
            // any could own it: one → inferred, several → ambiguous.
            match scope.open.as_slice() {
                [] => vec![unresolved(column)],
                [only] => vec![inferred(&only.table, column)],
                _ => vec![ambiguous(column)],
            }
        } else {
            // Qualified: match the qualifier against an open relation's
            // exposed name. (Known-column qualifiers are a later brick.)
            let qualifier = &parts[parts.len() - 2];
            match scope
                .open
                .iter()
                .find(|rel| self.ident_eq(rel.exposed_name(), qualifier))
            {
                Some(rel) => vec![inferred(&rel.table, column)],
                None => vec![unresolved(column)],
            }
        }
    }

    fn name_eq(&self, name: Option<&Ident>, other: &Ident) -> bool {
        name.is_some_and(|n| self.casing.column.normalize(n) == self.casing.column.normalize(other))
    }

    fn ident_eq(&self, a: &Ident, b: &Ident) -> bool {
        self.casing.table_alias.normalize(a) == self.casing.table_alias.normalize(b)
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

fn inferred(table: &TableReference, column: &Ident) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: Some(table.clone()),
            name: column.clone(),
        },
        resolution: ResolutionKind::Inferred,
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
        build(&statements[0], casing).expect("supported statement")
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

    fn read(table: &str, column: &str) -> ColumnRead {
        inferred(&tref(table), &Ident::new(column))
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
                    passthrough_col("a", vec![read("t", "a")]),
                    passthrough_col("b", vec![read("t", "b")]),
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
                        reads: vec![read("x", "id"), read("y", "id")],
                    })],
                    reads: vec![read("y", "b")],
                })),
                outputs: vec![passthrough_col("a", vec![read("x", "a")])],
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

    #[test]
    fn projection_is_closed_over_its_outputs() {
        // A projection exposes exactly its named outputs.
        let Plan::Project(project) = bind_one("SELECT a, b FROM t") else {
            panic!("expected Project");
        };
        let names: Vec<_> = project
            .outputs
            .iter()
            .filter_map(|c| c.name.as_ref().map(|n| n.value.clone()))
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }
}
