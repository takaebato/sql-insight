//! The bound logical plan: a tree of relational operators with resolved
//! column references ([`BoundColumn`] / [`Binding`]) and expressions ([`Expr`]).
//!
//! Each node is a textbook relational operator (or a DML / DDL root). The
//! tree carries no pre-collapsed provenance: lineage is derived by tracing
//! output columns down the operators (the column-origin traversal), and the
//! read-vs-not distinction is `Binding` (set at bind), not a synthetic flag.

use sqlparser::ast::Ident;

use crate::reference::{ColumnWrite, ResolutionKind, TableRead, TableReference, TableWrite};

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

/// A base-table scan (leaf). `resolution` is how the catalog identified the
/// table. Column resolution consults the relation's exposed column list in the
/// binding scope ([`Columns`] on `Relation::Table`), not the scan, so the scan
/// itself carries no column list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scan {
    pub(crate) table: TableReference,
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

impl Columns {
    /// A catalog match's column list as scope knowledge — `table_match`
    /// returns an empty list for a miss (schema unknown), never a zero-column
    /// table, so empty maps to [`Unknown`](Columns::Unknown).
    pub(crate) fn from_catalog(columns: Vec<Ident>) -> Columns {
        if columns.is_empty() {
            Columns::Unknown
        } else {
            Columns::Cataloged(columns)
        }
    }
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
/// inputs' columns. LATERAL / dependent joins are not marked here — the right
/// side's visibility of the left is modelled at bind time via the correlation
/// scope stack, so the bound tree needs no `lateral` flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Join {
    pub(crate) left: Box<LogicalPlan>,
    pub(crate) right: Box<LogicalPlan>,
    pub(crate) on: Vec<Expr>,
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
/// `alias` is dropped from reads and traces to the origins of `args` — the
/// function's data inputs — at function granularity (see the `TableFunction`
/// arm of the origin trace). `args` are the clause / argument expressions
/// (reads, and the outputs' lineage sources). `input` is the wrapped inner
/// table (feeds data, e.g. a PIVOT source) or [`LogicalPlan::Empty`] for a
/// bare function.
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
    /// The FROM-clause alias (`c AS x` → `x`), distinct from `name` (the
    /// declared CTE it resolves to); `None` when referenced by its own name.
    /// It is the *exposed* name a qualified origin trace must match, so two
    /// references to one CTE under different aliases (`c x JOIN c y`) are told
    /// apart — without it a qualified output column expands through *every*
    /// reference and duplicates the lineage edge (`reads` already folds, as the
    /// body is walked once at the declaration; `origins` is demand-driven, so
    /// it needs the alias to prune the non-owning references).
    pub(crate) alias: Option<Ident>,
}

/// A `VALUES` row set: synthesised rows with no base columns of their own —
/// but not opaque: a column of the row set *is* the like-positioned cell of
/// every row, so positional consumers (an INSERT pairing, a reference through
/// an aliased `VALUES` relation, an `EXCLUDED` mapping) trace into the cell
/// expressions rather than stopping here. `columns` carries the declared
/// column names when the row set is exposed as a relation
/// (`(VALUES …) AS v(a, b)` / `WITH v (a, b) AS (VALUES …)`; empty otherwise)
/// so a *named* reference maps to its position first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Values {
    pub(crate) rows: Vec<Vec<Expr>>,
    pub(crate) columns: Vec<Ident>,
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
///
/// `source_wildcard` flags that the source projection **kept an unexpanded
/// wildcard** (`SELECT *, y` whose `*` couldn't expand): its column count and
/// positions are then indeterminate, so positional pairing with `columns`
/// would mis-attribute and the arity check would mis-fire. The
/// relation-lineage walker and the arity check skip when it is set — the
/// target columns still surface as `writes`, the `WildcardSuppressed`
/// diagnostic signals the gap. An *expanded* wildcard is just columns
/// (determinate positions), so it does not set this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Insert {
    pub(crate) target: TableWrite,
    /// Written target columns, each catalog-resolved against the target
    /// (explicit list — from an `(a, b)` list or an Oracle inline-view target's
    /// projection — or catalog-filled for a column-less INSERT).
    pub(crate) columns: Vec<ColumnWrite>,
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) returning: Vec<NamedExpr>,
    pub(crate) on_conflict: Vec<Assignment>,
    pub(crate) conflict_predicate: Vec<Expr>,
    /// An Oracle inline-view target's predicates — the WHERE
    /// (`INSERT INTO (SELECT … FROM t WHERE …) …`) plus, for a join view, the
    /// join `ON` conditions: filter reads against the view's relations — their
    /// columns read, but never originate a value.
    pub(crate) target_predicate: Vec<Expr>,
    /// An Oracle **join-view** target's companion relations — the joined
    /// tables other than the write target (`INSERT INTO (SELECT e.id FROM
    /// emp e JOIN dept d ON …) …` → `Scan(dept)`): scanned context that gates
    /// which rows the view exposes, so they surface as table *reads* — but
    /// they feed no data (the value path is `input`), so table lineage
    /// ignores them. [`Empty`](LogicalPlan::Empty) for every other INSERT.
    pub(crate) target_context: Box<LogicalPlan>,
    pub(crate) source_wildcard: bool,
}

/// `UPDATE target SET assignments [FROM ...] WHERE ...`: `input` carries the
/// (write-role) target, any FROM relations, and the predicate; each
/// assignment pairs an RHS with its target column for lineage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Update {
    /// The DML root — used for the unqualified-SET default, the sink read, and
    /// RETURNING scope. The *write* targets are per-assignment (`Assignment`),
    /// since a multi-table UPDATE writes the relation each SET qualifier names.
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
    pub(crate) targets: Vec<TableWrite>,
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) returning: Vec<NamedExpr>,
}

/// `MERGE INTO target USING source ON on WHEN ...`: the `clauses` drive
/// writes / lineage (UPDATE SET / INSERT VALUES). `returning` projects the
/// affected rows — the `RETURNING` (Snowflake) / `OUTPUT` (MSSQL) clause's
/// select items, over the target + source scope, like the other DML roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Merge {
    pub(crate) target: TableWrite,
    pub(crate) source: Box<LogicalPlan>,
    pub(crate) on: Vec<Expr>,
    pub(crate) clauses: Vec<MergeClause>,
    pub(crate) returning: Vec<NamedExpr>,
}

/// One `WHEN [NOT] MATCHED ...` action of a [`Merge`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MergeClause {
    /// `WHEN MATCHED ... UPDATE SET`.
    Update { assignments: Vec<Assignment> },
    /// `WHEN NOT MATCHED ... INSERT (columns) VALUES (values)`.
    Insert {
        columns: Vec<ColumnWrite>,
        values: Vec<Expr>,
    },
    /// `WHEN MATCHED ... DELETE` — removes rows, no writes / lineage.
    Delete,
}

/// `CREATE TABLE target (columns) AS <input>`: like [`Insert`] (source
/// columns pair with target columns) but creates the relation. `columns` holds
/// only the *explicit* column list and is empty for the implicit form
/// (`CREATE TABLE t AS …`); there the per-position target name is the source
/// output's own inferred name, resolved at the write / lineage surface so both
/// stay aligned with the source outputs (an anonymous output is unnameable, so
/// it contributes no write / lineage edge without shifting later positions).
///
/// `schema_source` is the table a `CREATE TABLE t LIKE src` / `... CLONE src`
/// shapes itself from (`input` is then [`LogicalPlan::Empty`]). Either way the
/// source is *read* (it surfaces in `reads`); they differ in data flow — see
/// [`SchemaSource`]. The new table's *columns* aren't known here (no wildcard
/// expansion), so no column read / write / lineage edge is produced.
///
/// `source_wildcard` mirrors [`Insert::source_wildcard`]: the source projection
/// holds an unexpanded wildcard, so an *explicit* column list can't be paired
/// positionally (the lineage walker skips it then). The implicit form follows
/// the source outputs' own names, so a wildcard there merely omits the
/// unexpanded columns without misattributing — `source_wildcard` only gates the
/// explicit case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreateTableAs {
    pub(crate) target: TableWrite,
    pub(crate) columns: Vec<Ident>,
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) schema_source: Option<SchemaSource>,
    pub(crate) source_wildcard: bool,
}

/// The `CREATE TABLE t LIKE src` / `CLONE src` shape source on a
/// [`CreateTableAs`]. The `source` is always read; `copies_data` distinguishes
/// the two: `LIKE` copies only the column definitions (zero rows) — a schema
/// dependency with no data flow — while `CLONE` (Snowflake / BigQuery) copies
/// the data too, so it also feeds `source → target` table lineage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SchemaSource {
    pub(crate) source: TableRead,
    pub(crate) copies_data: bool,
}

/// `CREATE VIEW target (columns) AS <input>`. `columns` is the explicit column
/// list only (empty for the implicit form), resolved like [`CreateTableAs`].
/// `source_wildcard` plays the same role as on [`CreateTableAs`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreateView {
    pub(crate) target: TableWrite,
    pub(crate) columns: Vec<Ident>,
    pub(crate) input: Box<LogicalPlan>,
    pub(crate) source_wildcard: bool,
}

/// `ALTER TABLE target ...`: the column-naming operations' `columns` are
/// writes; no reads or lineage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AlterTable {
    pub(crate) target: TableWrite,
    pub(crate) columns: Vec<Ident>,
}

/// `DROP TABLE/VIEW` / `TRUNCATE`: names relations as write targets; no
/// reads, lineage, or column-level writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Drop {
    pub(crate) targets: Vec<TableWrite>,
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
    /// `then` results and `else_result` are value (origins).
    Case {
        when: Vec<Expr>,
        then: Vec<Expr>,
        else_result: Option<Box<Expr>>,
    },
    /// A window function: `arg` is value; the `partition` / `order` keys are
    /// filter (row positioning).
    Window {
        arg: Box<Expr>,
        partition: Vec<Expr>,
        order: Vec<Expr>,
    },
    /// A subquery in value position: the `output`-th output column's origins
    /// flow into the enclosing value (as a `Transformation`). `output` is 0 for
    /// a scalar `(SELECT …)`; a tuple assignment `SET (a, b) = (SELECT x, y …)`
    /// binds one per target column, each projecting its positional output.
    Subquery {
        plan: Box<LogicalPlan>,
        output: usize,
    },
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
    /// A wildcard-expansion-synthesized reference to the `index`-th output
    /// slot of the derived relation exposed as `qualifier` — **positional by
    /// construction** (the expansion enumerates the producer's slots), so a
    /// duplicate output name (`SELECT o.id, c.id` in the producer) or an
    /// anonymous one can't misattribute the trace the way a name-keyed
    /// `Derived` lookup would. Never minted for written SQL: a *written*
    /// reference resolves by name (`Expr::Column`). Like any `Derived` ref it
    /// is not a read (the physical read is counted at the inner producer);
    /// `origins` traces it to the producer's `index`-th output.
    DerivedSlot {
        qualifier: Option<Ident>,
        index: usize,
    },
}

/// Structural accessors over an [`Expr`]: which sub-expressions and sub-plans
/// sit in *value* position (feed into the value the expression produces, so
/// `origins` traces them) versus *filter* position (read but never originate
/// a value — `CASE` conditions, window partition / order keys, `EXISTS` / `IN`
/// tests). Mirrors the `own_exprs` / `children` shape the plan side already
/// exposes: each walker chooses which combination it cares about (e.g. `reads`
/// takes both positions, `lineage`'s `expr_feeding` takes only value sub-plans
/// and value operands), so the value/filter classification lives in one place
/// instead of being re-derived in every walker.
impl Expr {
    /// Operands in **value** position — the parts that feed into the value an
    /// expression produces. `origins` traces these (composing them with the
    /// arm's lineage kind); `reads` walks them like any other sub-expression.
    pub(super) fn value_operands(&self) -> Vec<&Expr> {
        match self {
            Expr::Call { args } => args.iter().collect(),
            Expr::Case {
                then, else_result, ..
            } => {
                let mut v: Vec<&Expr> = then.iter().collect();
                if let Some(e) = else_result {
                    v.push(e);
                }
                v
            }
            Expr::Window { arg, .. } => vec![arg],
            // `a IN (subquery)`: the LHS is a value operand — the IN result is a
            // boolean transformation of it (`origins_of_expr` traces it the same
            // way). The subquery side is the membership test, a filter
            // (`filter_subplans`).
            Expr::InSubquery { expr, .. } => vec![expr],
            Expr::Column(_)
            | Expr::Subquery { .. }
            | Expr::Exists(_)
            | Expr::Filter(_)
            | Expr::Fanin(_)
            | Expr::DerivedSlot { .. } => Vec::new(),
        }
    }

    /// Operands in **filter** position — reads but never value origins.
    pub(super) fn filter_operands(&self) -> Vec<&Expr> {
        match self {
            Expr::Case { when, .. } => when.iter().collect(),
            Expr::Window {
                partition, order, ..
            } => partition.iter().chain(order).collect(),
            Expr::Filter(exprs) => exprs.iter().collect(),
            Expr::Column(_)
            | Expr::Call { .. }
            | Expr::Subquery { .. }
            | Expr::Exists(_)
            | Expr::InSubquery { .. }
            | Expr::Fanin(_)
            | Expr::DerivedSlot { .. } => Vec::new(),
        }
    }

    /// Sub-plans in **value** position — a scalar `(SELECT …)` whose first
    /// output column flows into the enclosing value (`origins` follows it
    /// composed with `Transformation`; table lineage's `feeding_scans` follows
    /// it for the source-table feed).
    pub(super) fn value_subplans(&self) -> Vec<&LogicalPlan> {
        match self {
            Expr::Subquery { plan, output: 0 } => vec![plan],
            // `output > 0` is a tuple `SET (a, b) = (SELECT …)` clone: every
            // target column clones the same subquery, differing only in which
            // output it projects. Expose the shared plan once (at output 0
            // above) so reads / source-feeds stay occurrence-correct; column
            // lineage traces each output separately via `origins`, not here.
            Expr::Subquery { .. }
            | Expr::Column(_)
            | Expr::Call { .. }
            | Expr::Case { .. }
            | Expr::Window { .. }
            | Expr::Exists(_)
            | Expr::InSubquery { .. }
            | Expr::Filter(_)
            | Expr::Fanin(_)
            | Expr::DerivedSlot { .. } => Vec::new(),
        }
    }

    /// Sub-plans in **filter** position — `EXISTS (subquery)` and the right
    /// operand of `expr IN (subquery)`. Their scans are reads but feed no
    /// value into the enclosing expression.
    pub(super) fn filter_subplans(&self) -> Vec<&LogicalPlan> {
        match self {
            Expr::Exists(plan) => vec![plan],
            Expr::InSubquery { subquery, .. } => vec![subquery],
            Expr::Column(_)
            | Expr::Call { .. }
            | Expr::Case { .. }
            | Expr::Window { .. }
            | Expr::Subquery { .. }
            | Expr::Filter(_)
            | Expr::Fanin(_)
            | Expr::DerivedSlot { .. } => Vec::new(),
        }
    }
}

/// A named output item — a projection item, an aggregate, or a `RETURNING`
/// item: one expression under one or several output names. The expression is
/// always a real [`Expr`] and always exists exactly once, so `reads` count it
/// once by construction; a multi-name item (a *fan*) occupies several adjacent
/// output **positions**, so a list's item index is **not** its output position
/// — positional and named access go through the slot view ([`output_slots`] /
/// [`slot_count`] / [`resolved_output_expr`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NamedExpr {
    pub(crate) names: OutputNames,
    pub(crate) expr: Expr,
}

/// The output name(s) of one [`NamedExpr`] item.
///
/// `Fan` is Spark's multi-column alias `expr AS (a, b, …)`: one expression
/// producing several output columns (the one-source-many-outputs dual of
/// [`Expr::Fanin`]). Keeping the fan **inside one item** — instead of
/// splitting it into per-name entries linked by references — makes the
/// sharing structural: the names and the expression can never separate (no
/// dangling reference under any reorder / filter), lineage fans out by
/// expanding the names, and the expression is walked once wherever items are
/// walked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OutputNames {
    /// An ordinary item: the explicit alias, else the inferred name (a bare
    /// column's own name), else `None` (anonymous).
    Single(Option<Ident>),
    /// A multi-column alias — always explicit identifiers, one output
    /// position per name.
    Fan(Vec<Ident>),
}

impl OutputNames {
    /// How many output positions this item occupies.
    pub(crate) fn count(&self) -> usize {
        match self {
            OutputNames::Single(_) => 1,
            OutputNames::Fan(names) => names.len(),
        }
    }

    /// The name at the item's `j`-th position (inner `None` = anonymous;
    /// outer `None` = out of range).
    pub(crate) fn get(&self, j: usize) -> Option<Option<&Ident>> {
        match self {
            OutputNames::Single(name) => (j == 0).then_some(name.as_ref()),
            OutputNames::Fan(names) => names.get(j).map(Some),
        }
    }
}

/// The number of output **positions** an output list produces — the item
/// count plus fan expansion (`SELECT id, explode(a) AS (k, v)` = 2 items,
/// 3 positions).
pub(super) fn slot_count(outputs: &[NamedExpr]) -> usize {
    outputs.iter().map(|ne| ne.names.count()).sum()
}

/// Iterate an output list position by position as `(name, expr)` pairs — a
/// fan yields its shared expression once per name. The single positional /
/// named access path over outputs (query-output lineage, DML relation
/// pairing, RETURNING, CTAS / CREATE VIEW, set-operation / subquery /
/// EXCLUDED traces, derived-table name traces), so every position of a fan
/// traces exactly like its siblings.
pub(super) fn output_slots(outputs: &[NamedExpr]) -> impl Iterator<Item = (Option<&Ident>, &Expr)> {
    outputs
        .iter()
        .flat_map(|ne| (0..ne.names.count()).map(|j| (ne.names.get(j).flatten(), &ne.expr)))
}

/// The value expression at output **position** `i` (`None` = out of range).
pub(super) fn resolved_output_expr(outputs: &[NamedExpr], i: usize) -> Option<&Expr> {
    output_slots(outputs).nth(i).map(|(_, e)| e)
}

/// A `col = expr` assignment (UPDATE SET, MySQL INSERT … SET, ON CONFLICT
/// DO UPDATE SET, MERGE UPDATE). `target` carries the *resolved* table it
/// writes — the DML root for a single-table statement, or the relation a
/// qualifier names in a multi-table `UPDATE t1 JOIN t2 SET t2.col = …` — so a
/// write / lineage edge attributes to the right table rather than assuming the
/// root. `target_resolution` is how the catalog matched that table (captured
/// where the qualifier is resolved), so an UPDATE's per-target table write
/// surfaces its [`ResolutionKind`] without the walker re-deriving it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Assignment {
    /// The written column, catalog-resolved against its target table
    /// (`target.resolution` is the *column*-level match).
    pub(crate) target: ColumnWrite,
    /// How the catalog matched the write-target *table* (for `table_writes`).
    pub(crate) target_resolution: ResolutionKind,
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
/// `Base` / `Unresolved` / `Ambiguous`, drops `Derived` (the physical read was
/// counted at the inner producer) and `Local` (not a column at all); the origin
/// traversal traces `Derived` into its producer and treats the rest as
/// terminal (`Local` contributing nothing).
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
    /// A lambda parameter (`x` in `x -> x + 1`) — a *local* binding, not a
    /// table column, so it is neither a read nor a lineage origin. Bare
    /// references to an in-scope lambda parameter resolve here, shadowing any
    /// real column of the same name within the lambda body.
    Local,
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
            .chain(i.target_predicate.iter())
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
            exprs.extend(m.returning.iter().map(|ne| &ne.expr));
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
        LogicalPlan::Insert(i) => vec![&i.input, &i.target_context],
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
    // Every sub-plan at this expression (value + filter), then recurse through
    // every sub-expression (value + filter) — `own_expr_subplans` wants the
    // complete set of nested plans regardless of position.
    out.extend(expr.value_subplans());
    out.extend(expr.filter_subplans());
    for child in expr
        .value_operands()
        .into_iter()
        .chain(expr.filter_operands())
    {
        collect_subplans(child, out);
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

/// Every DML / DDL write **root** a statement carries: the peeled outer root,
/// plus each *data-modifying* CTE body (`WITH c AS (INSERT … RETURNING …) …`) —
/// recursively, so multiple sibling and nested data-modifying CTEs are all
/// collected. A read-only body / CTE contributes nothing. Backs the write /
/// CRUD / lineage walkers, since one statement can have more than one write
/// root. (A data-modifying CTE nested inside another root's *source* query is
/// not reached — only CTE declarations are descended, like the binder's shape.)
pub(super) fn dml_roots(op: &LogicalPlan) -> Vec<&LogicalPlan> {
    let mut out = Vec::new();
    collect_dml_roots(op, &mut out);
    out
}

/// Whether a node is a DML / DDL write root (the variants [`dml_roots`]
/// collects) — used to tell a read-only outer body (which emits `QueryOutput`
/// lineage) from a write root (which emits target lineage).
pub(super) fn is_dml_root(op: &LogicalPlan) -> bool {
    matches!(
        op,
        LogicalPlan::Insert(_)
            | LogicalPlan::Update(_)
            | LogicalPlan::Delete(_)
            | LogicalPlan::Merge(_)
            | LogicalPlan::CreateTableAs(_)
            | LogicalPlan::CreateView(_)
            | LogicalPlan::AlterTable(_)
            | LogicalPlan::Drop(_)
    )
}

fn collect_dml_roots<'a>(op: &'a LogicalPlan, out: &mut Vec<&'a LogicalPlan>) {
    match op {
        LogicalPlan::With(w) => {
            for cte in &w.ctes {
                collect_dml_roots(&cte.body, out);
            }
            collect_dml_roots(&w.body, out);
        }
        LogicalPlan::Insert(_)
        | LogicalPlan::Update(_)
        | LogicalPlan::Delete(_)
        | LogicalPlan::Merge(_)
        | LogicalPlan::CreateTableAs(_)
        | LogicalPlan::CreateView(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => out.push(op),
        // Read-only / structural roots write nothing — listed explicitly so a
        // new `LogicalPlan` variant forces a write-root decision here.
        LogicalPlan::Scan(_)
        | LogicalPlan::Filter(_)
        | LogicalPlan::Join(_)
        | LogicalPlan::Aggregate(_)
        | LogicalPlan::Projection(_)
        | LogicalPlan::Sort(_)
        | LogicalPlan::SetOp(_)
        | LogicalPlan::SubqueryAlias(_)
        | LogicalPlan::TableFunction(_)
        | LogicalPlan::CteRef(_)
        | LogicalPlan::Values(_)
        | LogicalPlan::Empty => {}
    }
}

/// Pre-order walk of the structural tree — structural children, the sub-plans
/// nested in this node's own expressions (a `WHERE … IN (SELECT …)` / scalar
/// subquery), and on a `With` each declared CTE body — invoking `f` at every
/// operator. The shared shape every [`super`]-side walker that visits "every
/// LogicalPlan reachable from a statement" can be a `f`-customisation over.
pub(super) fn walk_plan(op: &LogicalPlan, f: &mut impl FnMut(&LogicalPlan)) {
    f(op);
    for child in children(op) {
        walk_plan(child, f);
    }
    for sub in own_expr_subplans(op) {
        walk_plan(sub, f);
    }
    if let LogicalPlan::With(w) = op {
        for cte in &w.ctes {
            walk_plan(&cte.body, f);
        }
    }
}
