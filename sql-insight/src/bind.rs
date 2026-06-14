//! Incubating "bound plan" — design **B** from the 2026-06-14 grill: a
//! materialized, full-stack operator tree that the *bind* phase produces
//! from a `sqlparser` AST, resolving every column reference bottom-up.
//! Lineage / reads / writes extraction is meant to walk this tree
//! (dialect-agnostic), but that wiring does not exist yet — this module
//! is built **alongside** the current [`crate::resolver`] (strangler
//! migration) and is not reachable from any public API.
//!
//! Operator set (kept minimal; see the design memo): `Scan` /
//! `OpaqueLeaf` / `PassThrough` (join + every filter, output = identity)
//! / `Project` (the only column-defining producer; aggregates fold in as
//! `kind = Transformation`) / `Set` / `Write`. Each node carries its
//! output [`Schema`]; a `Project` output column's [`OutputColumn::provenance`]
//! is **pre-collapsed** to real base columns (each node unions its
//! children's provenance), so extraction is trivial.
//!
//! ## Current brick
//!
//! `SELECT` over `FROM` (single table, comma joins, `JOIN … ON`) with a
//! `WHERE` filter and a projection of column references / simple
//! expressions, resolved **catalog-free** (every resolved column is
//! [`ResolutionKind::Inferred`]; multi-candidate unqualified names are
//! `Ambiguous`, unknown qualifiers `Unresolved`). Catalog/open-schema
//! resolution, GROUP BY / HAVING / ORDER BY, set operations, CTE /
//! derived / subquery, DML `Write`, and `USING` fan-in are later bricks.
#![allow(dead_code)] // incubating: exercised by tests only until wired

use sqlparser::ast::{
    Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, Join, JoinConstraint,
    JoinOperator, Query, Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins,
};

use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableReference};
use crate::resolver::IdentifierCasing;

/// A node in the bound operator tree. Every variant except [`Write`]
/// exposes an output [`Schema`] via [`Node::schema`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    Scan(Scan),
    OpaqueLeaf(OpaqueLeaf),
    PassThrough(PassThrough),
    Project(Project),
    Set(Set),
    Write(Write),
}

/// A real stored table (leaf). Catalog-free, its column set is unknown,
/// so its schema is a single open relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) table: TableReference,
    /// Alias at this use-site, if any (drives qualifier matching).
    pub(crate) alias: Option<Ident>,
    /// How the catalog identified the table. Catalog-free → `Inferred`.
    pub(crate) resolution: ResolutionKind,
}

/// A relation with no inspectable column provenance — `VALUES`, a table
/// function, or any FROM item we don't model yet. Contributes a named
/// scope but no source columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpaqueLeaf {
    pub(crate) alias: Option<Ident>,
}

/// Join (N inputs) **and** every filter (WHERE / HAVING / JOIN ON):
/// output schema is the identity concatenation of the inputs' schemas;
/// `reads` are the columns the predicate referenced (filter position —
/// they never contribute value/lineage).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PassThrough {
    pub(crate) inputs: Vec<Node>,
    pub(crate) reads: Vec<ColumnRead>,
}

/// The SELECT list — the only column-defining producer. Each output
/// column carries its pre-collapsed provenance and a
/// passthrough/transformation kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Project {
    pub(crate) input: Box<Node>,
    pub(crate) outputs: Vec<OutputColumn>,
}

/// A set operation (UNION / INTERSECT / EXCEPT): the result schema is
/// the first operand's, with the others fanned in positionally. (Bind
/// for this is a later brick.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Set {
    pub(crate) operands: Vec<Node>,
}

/// A statement that writes a relation (INSERT / UPDATE / MERGE / CTAS /
/// CREATE VIEW): the source `input`'s output columns pair with
/// `target_columns` to produce relation lineage. (Bind is a later
/// brick.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Write {
    pub(crate) target: TableReference,
    pub(crate) target_columns: Vec<Ident>,
    pub(crate) input: Box<Node>,
}

/// One resolved output column: its (optional) name, the real base
/// columns it derives from (pre-collapsed provenance), and whether it
/// forwards a value unchanged or transforms it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutputColumn {
    pub(crate) name: Option<Ident>,
    pub(crate) provenance: Vec<ColumnRead>,
    pub(crate) kind: ColumnLineageKind,
}

/// The columns a node exposes upward.
///
/// `columns` are known, named columns with resolved provenance (a
/// `Project`'s outputs; catalog-backed `Scan`s in a later brick).
/// `open` lists relations whose full column set is unknown — an
/// unqualified or qualified name not in `columns` could belong to any of
/// them (best-effort, catalog-free).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Schema {
    pub(crate) columns: Vec<OutputColumn>,
    pub(crate) open: Vec<OpenRelation>,
}

/// A relation in a [`Schema`] whose column set is unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OpenRelation {
    pub(crate) table: TableReference,
    pub(crate) alias: Option<Ident>,
}

impl OpenRelation {
    /// The name this relation answers to in a qualifier: its alias if
    /// aliased, otherwise its table's bare name.
    fn exposed_name(&self) -> &Ident {
        self.alias.as_ref().unwrap_or(&self.table.name)
    }
}

impl Node {
    /// This node's output schema, computed structurally.
    pub(crate) fn schema(&self) -> Schema {
        match self {
            Node::Scan(scan) => Schema {
                columns: Vec::new(),
                open: vec![OpenRelation {
                    table: scan.table.clone(),
                    alias: scan.alias.clone(),
                }],
            },
            Node::OpaqueLeaf(_) => Schema::default(),
            // Join / filter pass their inputs' columns through unchanged.
            Node::PassThrough(pt) => {
                let mut schema = Schema::default();
                for input in &pt.inputs {
                    let mut child = input.schema();
                    schema.columns.append(&mut child.columns);
                    schema.open.append(&mut child.open);
                }
                schema
            }
            // A projection is closed: it exposes exactly its outputs.
            Node::Project(project) => Schema {
                columns: project.outputs.clone(),
                open: Vec::new(),
            },
            // Result schema follows the first operand (SQL's rule).
            Node::Set(set) => set.operands.first().map(Node::schema).unwrap_or_default(),
            Node::Write(write) => write.input.schema(),
        }
    }
}

/// Entry point: bind one statement into a [`Node`], or `None` for
/// statement kinds this brick doesn't model yet (everything except
/// `SELECT`-shaped queries).
pub(crate) fn bind_statement(statement: &Statement, casing: IdentifierCasing) -> Option<Node> {
    let binder = Binder { casing };
    match statement {
        Statement::Query(query) => Some(binder.bind_query(query)),
        // DML / DDL are later bricks.
        _ => None,
    }
}

/// Carries the bind-time context. For now just the dialect casing; a
/// catalog and the correlation scope stack join it in later bricks.
struct Binder {
    casing: IdentifierCasing,
}

impl Binder {
    fn bind_query(&self, query: &Query) -> Node {
        match query.body.as_ref() {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(inner) => self.bind_query(inner),
            // Set operations, VALUES, and `WITH … <DML>` wrappers are
            // later bricks — surface an opaque relation for now.
            _ => Node::OpaqueLeaf(OpaqueLeaf { alias: None }),
        }
    }

    fn bind_select(&self, select: &Select) -> Node {
        let from = self.bind_from(&select.from);
        // WHERE wraps the FROM in a PassThrough: its reads resolve
        // against the FROM schema only (no SELECT aliases — Project is
        // above), which is exactly the clause-phase rule, structurally.
        let input = match &select.selection {
            Some(predicate) => {
                let reads = self.resolve_reads(predicate, &from.schema());
                Node::PassThrough(PassThrough {
                    inputs: vec![from],
                    reads,
                })
            }
            None => from,
        };
        let input_schema = input.schema();
        let outputs = select
            .projection
            .iter()
            .filter_map(|item| self.bind_output_column(item, &input_schema))
            .collect();
        Node::Project(Project {
            input: Box::new(input),
            outputs,
        })
    }

    fn bind_from(&self, items: &[TableWithJoins]) -> Node {
        let mut nodes: Vec<Node> = items
            .iter()
            .map(|twj| self.bind_table_with_joins(twj))
            .collect();
        match nodes.len() {
            // `SELECT 1` (no FROM) — an empty opaque source.
            0 => Node::OpaqueLeaf(OpaqueLeaf { alias: None }),
            1 => nodes.pop().unwrap(),
            // Comma join: a PassThrough with no predicate.
            _ => Node::PassThrough(PassThrough {
                inputs: nodes,
                reads: Vec::new(),
            }),
        }
    }

    fn bind_table_with_joins(&self, twj: &TableWithJoins) -> Node {
        let mut node = self.bind_table_factor(&twj.relation);
        for join in &twj.joins {
            let right = self.bind_table_factor(&join.relation);
            // The ON predicate sees both sides, so resolve its reads
            // against the combined schema.
            let reads = match join_constraint(join) {
                Some(JoinConstraint::On(expr)) => {
                    let mut combined = node.schema();
                    let mut right_schema = right.schema();
                    combined.columns.append(&mut right_schema.columns);
                    combined.open.append(&mut right_schema.open);
                    self.resolve_reads(expr, &combined)
                }
                // USING fan-in / NATURAL are later bricks.
                _ => Vec::new(),
            };
            node = Node::PassThrough(PassThrough {
                inputs: vec![node, right],
                reads,
            });
        }
        node
    }

    fn bind_table_factor(&self, factor: &TableFactor) -> Node {
        match factor {
            TableFactor::Table { name, alias, .. } => match TableReference::try_from_name(name) {
                Ok(table) => Node::Scan(Scan {
                    table,
                    alias: alias.as_ref().map(|a| a.name.clone()),
                    // Catalog-free brick: every real table is inferred.
                    resolution: ResolutionKind::Inferred,
                }),
                Err(_) => Node::OpaqueLeaf(OpaqueLeaf { alias: None }),
            },
            // Derived tables / table functions / pivots etc. are later
            // bricks; they expose no inspectable columns yet.
            _ => Node::OpaqueLeaf(OpaqueLeaf { alias: None }),
        }
    }

    /// Resolve one output column (a SELECT-list item) against `schema`.
    /// Wildcards are skipped for now (`None`).
    fn bind_output_column(&self, item: &SelectItem, schema: &Schema) -> Option<OutputColumn> {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.clone())),
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => return None,
        };
        let mut refs = Vec::new();
        collect_column_refs(expr, &mut refs);
        let provenance = refs
            .iter()
            .flat_map(|parts| self.resolve_ref(parts, schema))
            .collect();
        let name = alias.or_else(|| inferred_output_name(expr));
        let kind = if is_single_column(expr) {
            ColumnLineageKind::Passthrough
        } else {
            ColumnLineageKind::Transformation
        };
        Some(OutputColumn {
            name,
            provenance,
            kind,
        })
    }

    /// Reads contributed by a predicate (WHERE / ON): every column it
    /// references, resolved against `schema`.
    fn resolve_reads(&self, expr: &Expr, schema: &Schema) -> Vec<ColumnRead> {
        let mut refs = Vec::new();
        collect_column_refs(expr, &mut refs);
        refs.iter()
            .flat_map(|parts| self.resolve_ref(parts, schema))
            .collect()
    }

    /// Resolve a single column reference to its pre-collapsed provenance.
    /// Returns one `ColumnRead` per real source column it lands on (a
    /// single entry in this brick; multiple once USING fan-in / known
    /// multi-provenance columns land).
    fn resolve_ref(&self, parts: &[Ident], schema: &Schema) -> Vec<ColumnRead> {
        let Some(column) = parts.last() else {
            return Vec::new();
        };
        if parts.len() == 1 {
            // A known output column inherits its provenance directly.
            if let Some(known) = schema
                .columns
                .iter()
                .find(|c| self.name_eq(c.name.as_ref(), column))
            {
                return known.provenance.clone();
            }
            // Otherwise it must come from an open relation. Catalog-free,
            // any could own it: one → inferred, several → ambiguous.
            match schema.open.as_slice() {
                [] => vec![unresolved(column)],
                [only] => vec![inferred(&only.table, column)],
                _ => vec![ambiguous(column)],
            }
        } else {
            // Qualified: match the qualifier against an open relation's
            // exposed name. (Known-column qualifiers are a later brick.)
            let qualifier = &parts[parts.len() - 2];
            match schema
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

/// Walk the bound tree and collect every column read it expresses: each
/// `Project` output column's (pre-collapsed) provenance plus every
/// `PassThrough`'s filter reads. Occurrence-based and unordered relative
/// to the old resolver — the differential harness compares the *set* of
/// real reads, so order / multiplicity differences (inherent to
/// pre-collapse) don't register as regressions.
pub(crate) fn extract_reads(node: &Node) -> Vec<ColumnRead> {
    let mut reads = Vec::new();
    collect_reads(node, &mut reads);
    reads
}

fn collect_reads(node: &Node, out: &mut Vec<ColumnRead>) {
    match node {
        Node::Scan(_) | Node::OpaqueLeaf(_) => {}
        Node::PassThrough(pt) => {
            out.extend(pt.reads.iter().cloned());
            for input in &pt.inputs {
                collect_reads(input, out);
            }
        }
        Node::Project(project) => {
            for column in &project.outputs {
                out.extend(column.provenance.iter().cloned());
            }
            collect_reads(&project.input, out);
        }
        Node::Set(set) => {
            for operand in &set.operands {
                collect_reads(operand, out);
            }
        }
        Node::Write(write) => collect_reads(&write.input, out),
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
/// identifier path. Best-effort over the common expression shapes;
/// unmodelled variants contribute nothing (they only cost a missed
/// read, never a wrong one).
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

    fn bind_one(sql: &str) -> Node {
        let dialect = GenericDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let casing = IdentifierCasing::for_dialect(&dialect);
        bind_statement(&statements[0], casing).expect("supported statement")
    }

    fn tref(name: &str) -> TableReference {
        TableReference {
            catalog: None,
            schema: None,
            name: name.into(),
        }
    }

    fn scan(name: &str) -> Node {
        Node::Scan(Scan {
            table: tref(name),
            alias: None,
            resolution: ResolutionKind::Inferred,
        })
    }

    fn read(table: &str, column: &str) -> ColumnRead {
        inferred(&tref(table), &Ident::new(column))
    }

    fn passthrough_col(name: &str, provenance: Vec<ColumnRead>) -> OutputColumn {
        OutputColumn {
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
            Node::Project(Project {
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
        // FROM x JOIN y ON … is one PassThrough (join), WHERE wraps it in
        // another. The projection's qualified `x.a` resolves to x.
        assert_eq!(
            bind_one("SELECT x.a FROM x JOIN y ON x.id = y.id WHERE y.b > 0"),
            Node::Project(Project {
                input: Box::new(Node::PassThrough(PassThrough {
                    inputs: vec![Node::PassThrough(PassThrough {
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
        let bound = bind_one("SELECT a FROM x JOIN y ON x.id = y.id");
        let Node::Project(project) = &bound else {
            panic!("expected Project, got {bound:?}");
        };
        assert_eq!(
            project.outputs,
            vec![OutputColumn {
                name: Some(Ident::new("a")),
                provenance: vec![ambiguous(&Ident::new("a"))],
                kind: ColumnLineageKind::Passthrough,
            }]
        );
    }

    #[test]
    fn project_schema_is_closed_over_its_outputs() {
        // A projection exposes exactly its output columns (no open
        // relations leak upward).
        let schema = bind_one("SELECT a, b FROM t").schema();
        let names: Vec<_> = schema
            .columns
            .iter()
            .filter_map(|c| c.name.as_ref().map(|n| n.value.clone()))
            .collect();
        assert_eq!(names, vec!["a", "b"]);
        assert!(schema.open.is_empty());
    }
}

/// Differential harness (the strangler safety net): for SQL the bind
/// brick covers, the **set** of real column reads it produces must match
/// the current resolver-based `extract_column_operations`. As bind grows
/// (lineage, writes, more clauses), this corpus and the compared
/// surfaces grow with it; a set mismatch flags a regression to classify.
#[cfg(test)]
mod differential {
    use super::*;
    use crate::extractor::extract_column_operations;
    use sqlparser::dialect::GenericDialect;
    use sqlparser::parser::Parser;
    use std::collections::HashSet;

    fn bind_one(sql: &str) -> Node {
        let dialect = GenericDialect {};
        let statements = Parser::parse_sql(&dialect, sql).unwrap();
        let casing = IdentifierCasing::for_dialect(&dialect);
        bind_statement(&statements[0], casing).expect("supported statement")
    }

    /// SQL fully within the current brick's coverage (single SELECT,
    /// FROM / comma / `JOIN … ON`, WHERE, column / simple-expr
    /// projection — catalog-free).
    fn covered_corpus() -> &'static [&'static str] {
        &[
            "SELECT a FROM t",
            "SELECT a, b FROM t",
            "SELECT t.a FROM t",
            "SELECT a, b FROM t WHERE c > 0",
            "SELECT a + b AS s FROM t",
            "SELECT x.a, y.b FROM x JOIN y ON x.id = y.id",
            "SELECT a FROM x JOIN y ON x.id = y.id",
            "SELECT a FROM x JOIN y ON x.id = y.id WHERE x.c > 0",
            "SELECT p.a FROM p, q WHERE p.id = q.id",
        ]
    }

    fn read_set(reads: &[ColumnRead]) -> HashSet<ColumnRead> {
        reads.iter().cloned().collect()
    }

    fn old_reads(sql: &str) -> Vec<ColumnRead> {
        let dialect = GenericDialect {};
        extract_column_operations(&dialect, sql, None)
            .unwrap()
            .remove(0)
            .unwrap()
            .reads
    }

    #[test]
    fn bind_reads_set_matches_resolver_on_covered_corpus() {
        for sql in covered_corpus() {
            let bound = bind_one(sql);
            let new_set = read_set(&extract_reads(&bound));
            let old_set = read_set(&old_reads(sql));
            assert_eq!(
                new_set, old_set,
                "read-set mismatch for: {sql}\n  bind: {new_set:?}\n  old:  {old_set:?}"
            );
        }
    }
}
