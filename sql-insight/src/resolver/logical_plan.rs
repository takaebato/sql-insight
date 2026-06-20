//! The bound logical plan: a tree of relational operators with resolved
//! column references ([`BoundColumn`] / [`Binding`]) and expressions ([`Expr`]).
//!
//! Each node is a textbook relational operator (or a DML / DDL root). The
//! tree carries no pre-collapsed provenance: lineage is derived by tracing
//! output columns down the operators (the column-origin traversal), and the
//! read-vs-not distinction is `Binding` (set at bind), not a synthetic flag.

use sqlparser::ast::Ident;

use crate::reference::{ResolutionKind, TableReference};

// ===== the logical plan ==================================================

/// A node of the bound logical plan — a relational operator, or a DML / DDL
/// root. The enum is recursive, so a value is both a node and the subtree it
/// roots (hence the type name `LogicalPlan` for the whole tree). Children are
/// owned `Box<LogicalPlan>`; CTE sharing and recursion use a symbolic
/// [`CteRef`] (by name), so the tree stays acyclic and `Eq`-able.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LogicalPlan {
    // --- relational (query) operators ---
    Scan(Scan),
    Filter(Filter),
    Join(Join),
    Aggregate(Aggregate),
    Projection(Projection),
    Sort(Sort),
    SetOp(SetOp),
    SubqueryAlias(SubqueryAlias),
    TableFunction(TableFunction),
    With(With),
    CteRef(CteRef),
    Values(Values),
    /// A FROM-less source (`SELECT 1`): one row, no columns.
    Empty,
    // --- DML / DDL roots ---
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    Merge(Merge),
    CreateTableAs(CreateTableAs),
    CreateView(CreateView),
    AlterTable(AlterTable),
    Drop(Drop),
}

/// A base-table scan (leaf). `columns` is the catalog-known list
/// ([`Columns::Cataloged`]) or [`Columns::Unknown`] (catalog-free / miss / ambiguous);
/// `resolution` is how the catalog identified the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) table: TableReference,
    pub(crate) columns: Columns,
    pub(crate) resolution: ResolutionKind,
}

/// What a [`Scan`] exposes for column resolution: an authoritative
/// (catalog-confirmed) column list, or none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Columns {
    /// Catalog-confirmed column names — an authoritative, *closed* list. A
    /// name in it resolves `Base { Cataloged }`; a name absent means the
    /// relation can't own it (it's ruled out as a candidate).
    Cataloged(Vec<Ident>),
    /// The column set is unknown (catalog-free, or a catalog miss / ambiguous
    /// match) — an open world: any name could plausibly belong, so the
    /// relation is a best-effort suspect (`Base { Inferred }`).
    Unknown,
}

/// Selection (σ): rows passing `predicate` flow through unchanged. The
/// predicate's columns are reads but never lineage origins — it is filter
/// position, and `origins` never traces here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Filter {
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) predicate: Vec<Expr>,
}

/// Join (⋈) of two inputs on `on`. Output is the concatenation of both
/// inputs' columns. `lateral` marks a LATERAL / dependent join: the right
/// resolves against the left (only then can the right reference left columns).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Join {
    pub(crate) left: Box<LogicalPlan>,
    pub(crate) right: Box<LogicalPlan>,
    pub(crate) on: Vec<Expr>,
    pub(crate) lateral: bool,
}

/// Aggregate (Γ): the `GROUP BY` grouping over its input, sitting below the
/// [`Projection`] in canonical evaluation order (`Scan → WHERE → Aggregate →
/// HAVING → Projection`). `group_by` holds the grouping-key expressions — reads
/// that pick the groups, never a value origin (the aggregate functions
/// themselves, `sum(x)`, are projection expressions counted at the `Projection`,
/// so a grouped column referenced both in `SELECT` and `GROUP BY` is counted
/// at each, occurrence-based). A reference resolves against the FROM scope, not
/// this node's output, so it stays a base read rather than tracing through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Aggregate {
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) group_by: Vec<Expr>,
}

/// Projection (π): the SELECT list — the column-defining operator. Each
/// output column is a named expression over the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Projection {
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) exprs: Vec<NamedExpr>,
}

/// Sort (ORDER BY): `keys` are reads (row positioning), columns pass
/// through unchanged. Sits above [`Projection`] in canonical order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sort {
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) keys: Vec<Expr>,
}

/// A set operation (UNION / INTERSECT / EXCEPT): result columns are the two
/// operands' columns merged positionally (names from the left). Chains nest
/// (`a UNION b UNION c` = `SetOp(SetOp(a, b), c)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SetOp {
    pub(crate) left: Box<LogicalPlan>,
    pub(crate) right: Box<LogicalPlan>,
}

/// A derived table / aliased subquery (`(<subquery>) AS d`): exposes the
/// input's output columns re-qualified under `alias`. A relation boundary —
/// a reference through it is not a base read (the base read happened inside);
/// `origins` traces into the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubqueryAlias {
    pub(crate) alias: Ident,
    pub(crate) input: Box<LogicalPlan>,
}

/// An opaque table-producing factor: a table function (`f(args)` / `UNNEST` /
/// `JSON_TABLE` / …), or a `PIVOT` / `UNPIVOT` / `MATCH_RECOGNIZE` wrapping an
/// inner table. Its produced columns are dynamic, so a reference through its
/// `alias` is a synthetic lineage source (the alias as table, dropped from
/// reads). `args` are the clause / argument expressions (reads). `input` is the
/// wrapped inner table (feeds data, e.g. a PIVOT source) or [`LogicalPlan::Empty`]
/// for a bare function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TableFunction {
    pub(crate) alias: Option<Ident>,
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) args: Vec<Expr>,
}

/// A `WITH` clause: the CTEs it declares (in declaration order) plus the
/// `body` that resolves against them. Each CTE body is owned here and walked
/// once regardless of reference count; references are lightweight [`CteRef`]s.
/// Declaration order gives each CTE visibility of the earlier ones (so a
/// chained CTE resolves unambiguously).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct With {
    pub(crate) ctes: Vec<Cte>,
    pub(crate) body: Box<LogicalPlan>,
}

/// One declared CTE: its name paired with its bound body. The name links it
/// to the [`CteRef`]s that consume it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cte {
    pub(crate) name: Ident,
    pub(crate) body: LogicalPlan,
}

/// A FROM-clause reference to an in-scope CTE, by name (the body lives once
/// on the owning [`With`]). `origins` expands it into that body; reads count
/// it nowhere (the body's reads are counted once at the declaration). A
/// recursive self-reference terminates via an active-set during traversal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CteRef {
    pub(crate) name: Ident,
}

/// A `VALUES` row set: synthesised rows with no base columns. The row
/// expressions are reads (and feed positionally when this is a write source).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Values {
    pub(crate) rows: Vec<Vec<Expr>>,
}

/// `INSERT INTO target (columns) <input>`: the source `input`'s output
/// columns pair positionally with `columns` for relation lineage.
/// `returning` projects the written relation; `on_conflict` is the
/// `DO UPDATE SET` / `ON DUPLICATE KEY UPDATE` action (extra writes, each
/// `value → target.col`), and `conflict_predicate` is its optional
/// `DO UPDATE … WHERE` (filter reads, non-feeding). A conflict value may
/// reference the `EXCLUDED` pseudo-table (the proposed row) — resolved to a
/// `Derived` ref qualified `excluded`, traced to the source's like-positioned
/// output column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Insert {
    pub(crate) target: TableReference,
    pub(crate) columns: Vec<Ident>,
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) returning: Vec<NamedExpr>,
    pub(crate) on_conflict: Vec<Assignment>,
    pub(crate) conflict_predicate: Vec<Expr>,
}

/// `UPDATE target SET assignments [FROM ...] WHERE ...`: `input` carries the
/// (write-role) target, any FROM relations, and the predicate; each
/// assignment pairs an RHS with its target column for lineage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Update {
    pub(crate) target: TableReference,
    pub(crate) assignments: Vec<Assignment>,
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) returning: Vec<NamedExpr>,
}

/// `DELETE`: removes rows from `targets`; `input` carries the FROM / USING
/// relations and the predicate. No column lineage (rows go wholesale);
/// `returning` projects the deleted rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Delete {
    pub(crate) targets: Vec<TableReference>,
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) returning: Vec<NamedExpr>,
}

/// `MERGE INTO target USING source ON on WHEN ...`: the `clauses` drive
/// writes / lineage (UPDATE SET / INSERT VALUES).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Merge {
    pub(crate) target: TableReference,
    pub(crate) source: Box<LogicalPlan>,
    pub(crate) on: Vec<Expr>,
    pub(crate) clauses: Vec<MergeClause>,
}

/// One `WHEN [NOT] MATCHED ...` action of a [`Merge`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MergeClause {
    /// `WHEN MATCHED ... UPDATE SET`.
    Update { assignments: Vec<Assignment> },
    /// `WHEN NOT MATCHED ... INSERT (columns) VALUES (values)`.
    Insert {
        columns: Vec<Ident>,
        values: Vec<Expr>,
    },
    /// `WHEN MATCHED ... DELETE` — removes rows, no writes / lineage.
    Delete,
}

/// `CREATE TABLE target (columns) AS <input>`: like [`Insert`] (source
/// columns pair with target columns) but creates the relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreateTableAs {
    pub(crate) target: TableReference,
    pub(crate) columns: Vec<Ident>,
    pub(crate) input: Box<LogicalPlan>,
}

/// `CREATE VIEW target (columns) AS <input>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreateView {
    pub(crate) target: TableReference,
    pub(crate) columns: Vec<Ident>,
    pub(crate) input: Box<LogicalPlan>,
}

/// `ALTER TABLE target ...`: the column-naming operations' `columns` are
/// writes; no reads or lineage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AlterTable {
    pub(crate) target: TableReference,
    pub(crate) columns: Vec<Ident>,
}

/// `DROP TABLE/VIEW` / `TRUNCATE`: names relations as write targets; no
/// reads, lineage, or column-level writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Drop {
    pub(crate) targets: Vec<TableReference>,
}

// ===== expressions =======================================================

/// A resolved expression. Column references are resolved to a [`BoundColumn`];
/// the construct-specific variants carry the value/filter split structurally
/// — `origins` traces the **value** operands (composing the lineage kind) and
/// skips the **filter** ones, while `reads` walks every column reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Expr {
    /// A column reference (value position; a bare column → `Passthrough`).
    /// Boxed: a resolved [`BoundColumn`] carries several spanned identifiers, so
    /// it dwarfs the other variants if stored inline.
    Column(Box<BoundColumn>),
    /// A function / operator / cast — a transformation over its value `args`.
    Call { args: Vec<Expr> },
    /// `CASE`: the `when` conditions are filter (reads, not origins); the
    /// `then` results and `else_` are value (origins).
    Case {
        when: Vec<Expr>,
        then: Vec<Expr>,
        else_: Option<Box<Expr>>,
    },
    /// A window function: `arg` is value; the `partition` / `order` keys are
    /// filter (row positioning).
    Window {
        arg: Box<Expr>,
        partition: Vec<Expr>,
        order: Vec<Expr>,
    },
    /// A scalar subquery (value position): its first output column's origins
    /// flow into the enclosing value (as a `Transformation`).
    Subquery(Box<LogicalPlan>),
    /// `EXISTS (subquery)` (filter position): a test, never a value origin.
    Exists(Box<LogicalPlan>),
    /// `expr IN (subquery)` (filter position).
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<LogicalPlan>,
    },
    /// Filter-position operands that are reads but never value origins — for
    /// the suppressed parts a construct-specific variant doesn't already cover
    /// (an `ANY` / `ALL` right operand, an aggregate `FILTER` / `ORDER BY`
    /// key). `reads` walks them; `origins` skips them.
    Filter(Vec<Expr>),
    /// A `JOIN … USING (col)` merge column referenced unqualified: it has no
    /// single owner, so it fans in to every joined relation that could own it
    /// — each a `Passthrough` read / origin (one per side, not an ambiguous
    /// `table: None`).
    Fanin(Vec<BoundColumn>),
}

/// A named output expression — a projection item, an aggregate, a group key,
/// or a `RETURNING` item. `name` is the explicit alias, else the inferred
/// name (a bare column's own name), else `None` (anonymous).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamedExpr {
    pub(crate) name: Option<Ident>,
    pub(crate) expr: Expr,
}

/// A `col = expr` assignment (UPDATE SET, MySQL INSERT … SET, ON CONFLICT
/// DO UPDATE SET, MERGE UPDATE).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Assignment {
    pub(crate) target: Ident,
    pub(crate) value: Expr,
}

/// A resolved column reference. `name` keeps the original [`Ident`] (its span
/// feeds the public read's source location). `binding` is the bind-time
/// resolution outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoundColumn {
    pub(crate) qualifier: Option<Ident>,
    pub(crate) name: Ident,
    pub(crate) binding: Binding,
}

/// What a column reference resolved to, decided at bind. `reads` keeps
/// `Base` / `Unresolved` / `Ambiguous` and drops `Derived` (the physical read
/// was counted at the inner producer); the origin traversal follows all four
/// (`Derived` recurses into the producing relation, collapsing lazily).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Binding {
    /// A real base-table column (the table is canonicalised; `resolution` is
    /// `Cataloged` for a catalog hit, else `Inferred`).
    Base {
        table: TableReference,
        resolution: ResolutionKind,
    },
    /// A CTE / derived-table / computed (Projection or Aggregate output) column —
    /// not a physical read; the origin traversal traces through it.
    Derived,
    /// No candidate owner.
    Unresolved,
    /// Several candidate owners.
    Ambiguous,
}

// ===== tree navigation ====================================================
// Shared structural accessors over the plan, used by every extraction module
// (`reads` / `lineage` / `origins` / `tables`). They describe the plan's
// *shape* — which child operators and which own expressions a node has — so
// they live with the type rather than in any one extraction concern.

/// A node's own expressions (not its children's): predicates, projection /
/// grouping / sort keys, table-function args, `VALUES` rows, and a DML root's
/// RETURNING / SET / MERGE-WHEN / conflict expressions.
pub(super) fn own_exprs(op: &LogicalPlan) -> Vec<&Expr> {
    match op {
        LogicalPlan::Filter(f) => f.predicate.iter().collect(),
        LogicalPlan::Join(j) => j.on.iter().collect(),
        LogicalPlan::Projection(p) => p.exprs.iter().map(|ne| &ne.expr).collect(),
        // The grouping keys are reads (they pick groups, never originate a
        // value); the aggregate functions live in the enclosing Projection.
        LogicalPlan::Aggregate(a) => a.group_by.iter().collect(),
        LogicalPlan::Sort(s) => s.keys.iter().collect(),
        // A table function's argument expressions are reads.
        LogicalPlan::TableFunction(tf) => tf.args.iter().collect(),
        LogicalPlan::Values(v) => v.rows.iter().flatten().collect(),
        // RETURNING items, conflict-action SET values, and the conflict
        // predicate are all reads (an `EXCLUDED` ref within them is `Derived`,
        // so dropped).
        LogicalPlan::Insert(i) => i
            .returning
            .iter()
            .map(|ne| &ne.expr)
            .chain(i.on_conflict.iter().map(|a| &a.value))
            .chain(i.conflict_predicate.iter())
            .collect(),
        LogicalPlan::Update(u) => u
            .assignments
            .iter()
            .map(|a| &a.value)
            .chain(u.returning.iter().map(|ne| &ne.expr))
            .collect(),
        LogicalPlan::Delete(d) => d.returning.iter().map(|ne| &ne.expr).collect(),
        // MERGE: the ON / per-clause predicates (filter reads) plus each WHEN
        // action's value expressions (SET RHS / INSERT values).
        LogicalPlan::Merge(m) => {
            let mut exprs: Vec<&Expr> = m.on.iter().collect();
            for clause in &m.clauses {
                match clause {
                    MergeClause::Update { assignments } => {
                        exprs.extend(assignments.iter().map(|a| &a.value));
                    }
                    MergeClause::Insert { values, .. } => exprs.extend(values.iter()),
                    MergeClause::Delete => {}
                }
            }
            exprs
        }
        LogicalPlan::Scan(_)
        | LogicalPlan::SubqueryAlias(_)
        | LogicalPlan::SetOp(_)
        | LogicalPlan::With(_)
        | LogicalPlan::CteRef(_)
        | LogicalPlan::Empty
        | LogicalPlan::CreateTableAs(_)
        | LogicalPlan::CreateView(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => Vec::new(),
    }
}

/// A node's structural child operators (not those nested in expressions — those
/// are reached via [`own_expr_subplans`]; not CTE bodies — `With` is handled
/// specially by each walk).
pub(super) fn children(op: &LogicalPlan) -> Vec<&LogicalPlan> {
    match op {
        LogicalPlan::Filter(f) => vec![&f.input],
        LogicalPlan::Join(j) => vec![&j.left, &j.right],
        LogicalPlan::Aggregate(a) => vec![&a.input],
        LogicalPlan::Projection(p) => vec![&p.input],
        LogicalPlan::Sort(s) => vec![&s.input],
        LogicalPlan::SetOp(so) => vec![&so.left, &so.right],
        LogicalPlan::SubqueryAlias(sa) => vec![&sa.input],
        // The inner (wrapped) table of a PIVOT / … is a child; the function
        // args are own expressions.
        LogicalPlan::TableFunction(tf) => vec![&tf.input],
        LogicalPlan::With(w) => vec![&w.body],
        LogicalPlan::Insert(i) => vec![&i.input],
        LogicalPlan::Update(u) => vec![&u.input],
        LogicalPlan::Delete(d) => vec![&d.input],
        LogicalPlan::Merge(m) => vec![&m.source],
        LogicalPlan::CreateTableAs(c) => vec![&c.input],
        LogicalPlan::CreateView(c) => vec![&c.input],
        LogicalPlan::Scan(_)
        | LogicalPlan::CteRef(_)
        | LogicalPlan::Values(_)
        | LogicalPlan::Empty
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => Vec::new(),
    }
}

/// The sub-plans appearing in a node's *own* expressions (a `WHERE … IN
/// (SELECT …)` / scalar subquery).
pub(super) fn own_expr_subplans(op: &LogicalPlan) -> Vec<&LogicalPlan> {
    let mut out = Vec::new();
    for expr in own_exprs(op) {
        collect_subplans(expr, &mut out);
    }
    out
}

fn collect_subplans<'a>(expr: &'a Expr, out: &mut Vec<&'a LogicalPlan>) {
    match expr {
        Expr::Column(_) => {}
        Expr::Call { args } => args.iter().for_each(|e| collect_subplans(e, out)),
        Expr::Case { when, then, else_ } => {
            when.iter()
                .chain(then)
                .for_each(|e| collect_subplans(e, out));
            if let Some(e) = else_ {
                collect_subplans(e, out);
            }
        }
        Expr::Window {
            arg,
            partition,
            order,
        } => {
            collect_subplans(arg, out);
            partition
                .iter()
                .chain(order)
                .for_each(|e| collect_subplans(e, out));
        }
        Expr::Subquery(plan) | Expr::Exists(plan) => out.push(plan),
        Expr::InSubquery { expr, subquery } => {
            collect_subplans(expr, out);
            out.push(subquery);
        }
        Expr::Filter(exprs) => exprs.iter().for_each(|e| collect_subplans(e, out)),
        Expr::Fanin(_) => {}
    }
}

/// Peel leading `With` nodes to the wrapped root (a query or DML root).
pub(super) fn peel_with(op: &LogicalPlan) -> &LogicalPlan {
    let mut node = op;
    while let LogicalPlan::With(w) = node {
        node = &w.body;
    }
    node
}

/// Identifier equality for matching a resolved reference against a producer's
/// output / relation name. The binder already resolved the reference with the
/// dialect's column / alias fold, and no dialect treats column or alias names
/// case-*sensitively* (they fold Upper / Lower / Insensitive), so an ASCII
/// case-insensitive compare matches every policy without threading the casing
/// into the walk.
pub(super) fn idents_eq(a: &Ident, b: &Ident) -> bool {
    a.value.eq_ignore_ascii_case(&b.value)
}
