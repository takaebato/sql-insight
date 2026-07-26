//! The column-origin traversal: tracing an expression's **value** to the base
//! columns it derives from, each with a composed [`ColumnLineageKind`]. This is
//! the `getColumnOrigins`-style core that [`super::lineage`] drives to build
//! the `source → target` edges.
//!
//! The value-vs-filter split falls out of this trace for free: filter-position
//! operands (CASE conditions, window partition / order keys, EXISTS / IN tests)
//! are *not* traced — they surface as reads, never as origins.

use sqlparser::ast::Ident;

use super::logical_plan::{
    output_slots, resolved_output_expr, Binding, BoundColumn, Cte, Expr, LogicalPlan, NamedExpr,
    Values,
};
use super::reads::column_read;
use crate::casing::{CaseRule, IdentifierCasing};
use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ColumnWrite};

/// CTE environment for expanding `CteRef`s during a trace, plus the
/// active-set that terminates a recursive self-reference and the dialect
/// casing for name comparisons (alias matching vs column matching use
/// different rules — e.g. ClickHouse folds neither, the generic dialect
/// folds both).
pub(super) struct TraceContext<'a> {
    pub(super) ctes: Vec<&'a Cte>,
    pub(super) active: Vec<String>,
    pub(super) casing: IdentifierCasing,
}

impl<'a> TraceContext<'a> {
    pub(super) fn new(op: &'a LogicalPlan, casing: IdentifierCasing) -> Self {
        // Collect leading `With` declarations so a `CteRef` on a traced path
        // resolves to its body.
        let mut ctes = Vec::new();
        let mut node = op;
        while let LogicalPlan::With(w) = node {
            ctes.extend(w.ctes.iter());
            node = &w.body;
        }
        TraceContext {
            ctes,
            active: Vec::new(),
            casing,
        }
    }

    /// Compare two identifiers under the dialect's *column* fold (output names,
    /// EXCLUDED column matching) — sensitive on ClickHouse, otherwise lenient.
    fn eq_column(&self, a: &Ident, b: &Ident) -> bool {
        self.casing.column.normalize(a) == self.casing.column.normalize(b)
    }

    /// Compare two identifiers under the dialect's *alias / CTE* fold (CTE
    /// names, derived-table / table-function / CTE-ref qualifiers).
    fn eq_alias(&self, a: &Ident, b: &Ident) -> bool {
        self.casing.table_alias.normalize(a) == self.casing.table_alias.normalize(b)
    }

    /// Run `f` with a `With`'s declarations pushed onto the CTE env, popping
    /// them after so a sibling subtree doesn't see them (the balanced
    /// push/truncate every nested-`With` walk shares).
    pub(super) fn with_decls<R>(
        &mut self,
        ctes: &'a [Cte],
        f: impl FnOnce(&mut TraceContext<'a>) -> R,
    ) -> R {
        let added = ctes.len();
        ctes.iter().for_each(|c| self.ctes.push(c));
        let r = f(self);
        self.ctes.truncate(self.ctes.len() - added);
        r
    }

    /// Like [`with_decls`](Self::with_decls) but for already-borrowed CTE
    /// declarations — an [`Operand`]'s accumulated in-scope CTEs (the `With`s
    /// peeled above it). Pushed for the duration of `f`, popped after.
    pub(super) fn with_cte_refs<R>(
        &mut self,
        ctes: &[&'a Cte],
        f: impl FnOnce(&mut TraceContext<'a>) -> R,
    ) -> R {
        let added = ctes.len();
        self.ctes.extend_from_slice(ctes);
        let r = f(self);
        self.ctes.truncate(self.ctes.len() - added);
        r
    }

    /// Resolve a `CteRef` by name and run `f` on its body with the name marked
    /// active (popped after), so a recursive self-reference terminates. Returns
    /// `None` — and never calls `f` — for an unknown name or an already-active
    /// self-reference (the recursion-termination case). Centralises the subtle
    /// active-set bookkeeping every walk that expands a `CteRef` shares.
    pub(super) fn enter_cte<R>(
        &mut self,
        name: &Ident,
        f: impl FnOnce(&mut TraceContext<'a>, &'a LogicalPlan) -> R,
    ) -> Option<R> {
        if self.active.iter().any(|n| n == &name.value) {
            return None; // recursive self-reference — terminate
        }
        let alias_fold = self.casing.table_alias;
        let cte = self
            .ctes
            .iter()
            .rev()
            .find(|c| alias_fold.normalize(&c.name) == alias_fold.normalize(name))
            .copied()?;
        self.active.push(name.value.clone());
        let r = f(self, &cte.body);
        self.active.pop();
        Some(r)
    }
}

/// The base-column origins of an expression's **value**, each with the
/// composed lineage kind. Filter-position operands (CASE conditions, window
/// keys, EXISTS / IN tests) are not traced — they are reads, not origins.
pub(super) fn origins_of_expr<'a>(
    expr: &'a Expr,
    input: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match expr {
        Expr::Column(c) => origins_of_ref(c, input, context),
        Expr::Call { args } => {
            transform(args.iter().flat_map(|e| origins_of_expr(e, input, context)))
        }
        Expr::Case {
            then, else_result, ..
        } => {
            // `when` conditions are filter — only the results are value.
            let mut sources: Vec<_> = then
                .iter()
                .flat_map(|e| origins_of_expr(e, input, context))
                .collect();
            if let Some(e) = else_result {
                sources.extend(origins_of_expr(e, input, context));
            }
            transform(sources)
        }
        // The function argument is value; partition / order keys are filter.
        Expr::Window { arg, .. } => transform(origins_of_expr(arg, input, context)),
        // A subquery's `output`-th column flows as a transformation.
        Expr::Subquery { plan, output } => {
            transform(subquery_output_origins(plan, *output, context))
        }
        // A merge-column fan-in: each owning side is its own origin, traced
        // like a column ref — a real-table side is a `Passthrough` base read,
        // a derived / CTE side traces into its producing subquery (so a
        // `(SELECT id FROM s) d JOIN t USING (id)` yields both `s.id` and
        // `t.id`, not just one).
        Expr::Fanin(refs) => refs
            .iter()
            .flat_map(|c| origins_of_ref(c, input, context))
            .collect(),
        // `a IN (subquery)`: the LHS is a value operand — `a IN (…)` is a
        // boolean transformation of it, like `a IN (list)`. The subquery's
        // columns are a membership test (filter), contributing no origin.
        Expr::InSubquery { expr, .. } => transform(origins_of_expr(expr, input, context)),
        // A wildcard-expanded slot over a derived relation: trace the
        // producer's `index`-th output — position, not name, so a duplicate /
        // anonymous output name in the producer can't misattribute.
        Expr::DerivedSlot { qualifier, index } => {
            origins_of_slot(input, qualifier.as_ref(), *index, context)
        }
        // Tests / suppressed operands contribute no value origin (reads only).
        Expr::Exists(_) | Expr::Filter(_) => Vec::new(),
    }
}

/// The origins of a single bound column reference (shared by the `Column` and
/// `Fanin` arms): a `Base` / unresolved / ambiguous ref is its own
/// `Passthrough` origin; a `Derived` ref traces through the producer that
/// defines it (composing the lineage kind end-to-end).
fn origins_of_ref<'a>(
    c: &'a BoundColumn,
    input: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match &c.binding {
        Binding::Base { .. } | Binding::Unresolved | Binding::Ambiguous => column_read(c)
            .map(|r| vec![(r, ColumnLineageKind::Passthrough)])
            .unwrap_or_default(),
        Binding::Derived => origins_into(input, c.qualifier.as_ref(), &c.name, context),
        // A lambda parameter is a local with no base column — no origin.
        Binding::Local => Vec::new(),
    }
}

/// The origins of a wildcard-expanded slot ([`Expr::DerivedSlot`]): find the
/// derived relation the slot was minted against — walking the producing
/// operator tree exactly like [`origins_into`], with the same qualifier
/// guards at the relation boundaries — then trace its `index`-th output
/// positionally ([`trace_nth_output`], fanning across set-operation
/// branches). A VALUES-backed relation traces straight into its cells
/// ([`values_cell_origins`]).
///
/// A slot with no qualifier was minted against an *unaliased* derived table —
/// expansion only allows that as the scope's sole relation, so the inline
/// `Projection` / `SetOp` / `Values` reached here is unambiguously the
/// producer.
fn origins_of_slot<'a>(
    op: &'a LogicalPlan,
    qualifier: Option<&Ident>,
    index: usize,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match op {
        // The unaliased producer reached inline (no relation boundary to match
        // a qualifier against). A qualified slot never claims here — its
        // producer sits behind a `SubqueryAlias` / `CteRef` boundary below.
        LogicalPlan::Projection(_) | LogicalPlan::SetOp(_) => {
            if qualifier.is_some() {
                return Vec::new();
            }
            trace_nth_output(&output_operands(op), index, context)
        }
        LogicalPlan::Values(v) => {
            if qualifier.is_some() {
                return Vec::new();
            }
            values_cell_origins(v, index, op, context)
        }
        LogicalPlan::Aggregate(a) => origins_of_slot(&a.input, qualifier, index, context),
        LogicalPlan::Filter(f) => origins_of_slot(&f.input, qualifier, index, context),
        LogicalPlan::Sort(s) => origins_of_slot(&s.input, qualifier, index, context),
        LogicalPlan::Join(j) => {
            // An *unqualified* slot over a join is a pipe passthrough / star
            // slot above a `|> JOIN`, whose output concatenates the left
            // block's slots then the right side's columns — the index
            // belongs to exactly one side, split at the left side's full
            // width ([`slot_width`], which sums a stacked join's both sides
            // — the first-operand count undercounted there and misrouted a
            // middle join's slots to the outer right side). Letting both
            // sides answer at the same index — the qualified behaviour
            // below — would misclaim: a derived right side has no qualifier
            // guard to stop it from answering for a left slot. An
            // uncountable left side can't locate the split, so nothing is
            // claimed rather than guessed.
            if qualifier.is_none() {
                return match slot_width(&j.left) {
                    Some(n) if index < n => origins_of_slot(&j.left, qualifier, index, context),
                    Some(n) => origins_of_slot(&j.right, qualifier, index - n, context),
                    None => Vec::new(),
                };
            }
            // A qualified slot resolves by relation boundary: each side's
            // `SubqueryAlias` / `CteRef` guard admits exactly one owner.
            let mut o = origins_of_slot(&j.left, qualifier, index, context);
            o.extend(origins_of_slot(&j.right, qualifier, index, context));
            o
        }
        LogicalPlan::SubqueryAlias(sa) => {
            if !qualifier.is_none_or(|q| context.eq_alias(q, &sa.alias)) {
                Vec::new()
            } else if let Some(values) = values_node(&sa.input) {
                values_cell_origins(values, index, &sa.input, context)
            } else {
                trace_nth_output(&output_operands(&sa.input), index, context)
            }
        }
        LogicalPlan::With(w) => context.with_decls(&w.ctes, |context| {
            origins_of_slot(&w.body, qualifier, index, context)
        }),
        // A `CteRef` boundary: only the reference whose exposed name the
        // qualifier matches descends (same guard as the named trace, so a
        // self-join of one CTE doesn't duplicate the edge). `output_operands`
        // + `Operand::trace` register the body's own `WITH` declarations.
        LogicalPlan::CteRef(r) => {
            let exposed = r.alias.as_ref().unwrap_or(&r.name);
            if !qualifier.is_none_or(|q| context.eq_alias(q, exposed)) {
                return Vec::new();
            }
            context
                .enter_cte(&r.name, |context, body| {
                    if let Some(values) = values_node(body) {
                        values_cell_origins(values, index, body, context)
                    } else {
                        trace_nth_output(&output_operands(body), index, context)
                    }
                })
                .unwrap_or_default()
        }
        // A table function reached here is always a walked-past join side:
        // expansion never mints a slot over one (its shape is unknown), and a
        // qualifier naming both it and the slot's own derived relation would
        // have failed the unique-match guard at expansion. Nothing to claim.
        // Not positional producers otherwise: a raw scan on a walked-past
        // join side, and DML / DDL roots.
        LogicalPlan::TableFunction(_) | LogicalPlan::Scan(_) | LogicalPlan::Empty => Vec::new(),
        LogicalPlan::Insert(_)
        | LogicalPlan::Update(_)
        | LogicalPlan::Delete(_)
        | LogicalPlan::Merge(_)
        | LogicalPlan::CreateTableAs(_)
        | LogicalPlan::CreateView(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => Vec::new(),
    }
}

/// The origins of a `VALUES`-backed relation's column at `position`: the
/// like-positioned cell of **every** row, each traced like any value. A
/// literal cell contributes nothing — a constant column has no source,
/// exactly like `SELECT 1 AS a` — while a subquery / correlated cell reaches
/// its real columns. So a `VALUES` relation is *reducible*, unlike a table
/// function (whose output only exists at run time): no synthetic source.
fn values_cell_origins<'a>(
    values: &'a Values,
    position: usize,
    input: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    let mut out = Vec::new();
    for row in &values.rows {
        if let Some(cell) = row.get(position) {
            out.extend(origins_of_expr(cell, input, context));
        }
    }
    out
}

/// The origins of a *named* reference through a `VALUES`-backed relation:
/// map the name to its declared-column position (`(VALUES …) AS v(a, b)` —
/// the only way such a column is nameable), then trace the cells.
fn values_named_origins<'a>(
    values: &'a Values,
    name: &Ident,
    input: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match values
        .columns
        .iter()
        .position(|c| context.eq_column(c, name))
    {
        Some(position) => values_cell_origins(values, position, input, context),
        None => Vec::new(),
    }
}

/// The `Values` row set a (sub)plan's rows come from (peeling the clause
/// layers / a leading `With`), `None` for any real relation — consumers trace
/// into its cells ([`values_cell_origins`] / [`values_named_origins`]).
fn values_node(op: &LogicalPlan) -> Option<&Values> {
    match op {
        LogicalPlan::Values(v) => Some(v),
        // Only the clause-layer wrappers `Values` can sit beneath are peeled —
        // an ORDER BY / WHERE on a VALUES, and a leading WITH. Anything else
        // is a real relation (a Scan, a Projection rewriting the row shape, a
        // derived alias on top), and its columns *do* have a base to collapse
        // to. Listed explicitly so a new operator added between `Values` and
        // its clause layers needs an explicit decision here.
        LogicalPlan::Sort(s) => values_node(&s.input),
        LogicalPlan::Filter(f) => values_node(&f.input),
        LogicalPlan::With(w) => values_node(&w.body),
        LogicalPlan::Scan(_)
        | LogicalPlan::Join(_)
        | LogicalPlan::Aggregate(_)
        | LogicalPlan::Projection(_)
        | LogicalPlan::SetOp(_)
        | LogicalPlan::SubqueryAlias(_)
        | LogicalPlan::TableFunction(_)
        | LogicalPlan::CteRef(_)
        | LogicalPlan::Empty
        | LogicalPlan::Insert(_)
        | LogicalPlan::Update(_)
        | LogicalPlan::Delete(_)
        | LogicalPlan::Merge(_)
        | LogicalPlan::CreateTableAs(_)
        | LogicalPlan::CreateView(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => None,
    }
}

/// Trace the named output column of `op` down to its base origins (used to
/// expand a `Derived` reference through its producing operator).
fn origins_into<'a>(
    op: &'a LogicalPlan,
    qualifier: Option<&Ident>,
    name: &Ident,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match op {
        LogicalPlan::Projection(p) => {
            // An inline producer is an *unaliased* derived table (no relation
            // boundary to match a qualifier against), so a qualified
            // reference never claims it by name — its producer sits behind a
            // `SubqueryAlias` / `CteRef` boundary (the slot trace has the
            // same guard). Without this, `x.a` over `(SELECT a FROM s),
            // (SELECT a FROM u) AS x` also traced into the unaliased `s`
            // producer.
            if qualifier.is_some() {
                return Vec::new();
            }
            // Resolve by name to a position, then through any multi-alias
            // back-reference — `d.v` over `explode(arr) AS (k, v)` traces the
            // head expression, reaching `arr`.
            match named_position(&p.exprs, name, context.casing.column)
                .and_then(|i| resolved_output_expr(&p.exprs, i))
            {
                Some(expr) => origins_of_expr(expr, &p.input, context),
                None => Vec::new(),
            }
        }
        // The projection resolves against the FROM scope, so a column is a
        // base ref that returns directly, not a named `Aggregate` output —
        // a `Derived` ref tracing down just passes through to the input.
        LogicalPlan::Aggregate(a) => origins_into(&a.input, qualifier, name, context),
        LogicalPlan::Filter(f) => origins_into(&f.input, qualifier, name, context),
        LogicalPlan::Sort(s) => origins_into(&s.input, qualifier, name, context),
        LogicalPlan::Join(j) => {
            let mut o = origins_into(&j.left, qualifier, name, context);
            o.extend(origins_into(&j.right, qualifier, name, context));
            o
        }
        LogicalPlan::SubqueryAlias(sa) => {
            if !qualifier.is_none_or(|q| context.eq_alias(q, &sa.alias)) {
                Vec::new()
            } else if let Some(values) = values_node(&sa.input) {
                // A `(VALUES …) AS t(x)` column is the like-positioned cell of
                // every row — trace into the cells, not a synthetic stop.
                values_named_origins(values, name, &sa.input, context)
            } else {
                origins_into(&sa.input, None, name, context)
            }
        }
        // A table function's output data comes from its inputs — the argument
        // expressions (`UNNEST(t.arr)` emits `t.arr`'s elements; a PIVOT's
        // aggregate expressions carry the inner columns) — so a ref through
        // its alias traces to the arguments' origins, at **function
        // granularity**: every output column derives from every argument
        // (`Transformation`), the same coarseness as a scalar call
        // `f(a, b) AS x`. Which argument feeds which output column is
        // function semantics the SQL text doesn't carry (a multi-array
        // `UNNEST(a, b)` zips: column i ← array i) — a per-function
        // refinement can narrow this later. Constant arguments contribute
        // nothing, so a `generate_series(1, 10)` output has no source,
        // exactly like `SELECT 1`.
        LogicalPlan::TableFunction(tf) => match &tf.alias {
            Some(alias) if qualifier.is_none_or(|q| context.eq_alias(q, alias)) => {
                table_function_output_origins(tf, context)
            }
            _ => Vec::new(),
        },
        // A set operation merges its branches **positionally** — the result
        // column names come from the leftmost branch. A `Derived` trace reaches
        // here with `name` = an exposed (leftmost-branch) output name, so find
        // that name's position in the first branch and trace the like-positioned
        // output of *every* branch. A per-branch *name* match (the old shape)
        // would drop a branch whose output name differs and misattribute when a
        // later branch happens to reuse the name at a different position. The
        // positional fan-out matches the EXCLUDED trace in
        // `conflict_value_origins`, and `output_operands` flattens nested
        // set-ops so an N-way `A UNION B UNION C` traces all branches at once.
        LogicalPlan::SetOp(_) => {
            // Inline like the `Projection` arm above: a qualified reference
            // never claims an unaliased set-operation producer by name.
            if qualifier.is_some() {
                return Vec::new();
            }
            let operands = output_operands(op);
            let Some(i) = operands.first().and_then(|o| {
                output_slots(o.outputs)
                    .position(|(n, _)| n.is_some_and(|n| context.eq_column(n, name)))
            }) else {
                return Vec::new();
            };
            trace_nth_output(&operands, i, context)
        }
        LogicalPlan::With(w) => context.with_decls(&w.ctes, |context| {
            origins_into(&w.body, qualifier, name, context)
        }),
        // Like `SubqueryAlias`, a `CteRef` is a relation boundary: a qualified
        // trace only descends through the reference whose *exposed* name (its
        // alias, else the CTE name) the qualifier matches. Without this guard a
        // self-join of one CTE (`c x JOIN c y`) expands the body through *both*
        // references and duplicates the edge.
        LogicalPlan::CteRef(r) => {
            let exposed = r.alias.as_ref().unwrap_or(&r.name);
            if !qualifier.is_none_or(|q| context.eq_alias(q, exposed)) {
                return Vec::new();
            }
            // Expand the body once per reference. (No memo: a cache keyed on
            // (name, column) collided across same-named shadowing CTEs, and
            // keying it correctly buys little — distinct output columns hit
            // distinct slots, so only a *repeated identical* `cte.col` would
            // ever reuse one.) Active-set `None` (recursive self-reference)
            // becomes an empty `Vec` via `unwrap_or_default`.
            context
                .enter_cte(&r.name, |context, body| {
                    // A VALUES-backed CTE (`WITH v (a, b) AS (VALUES …)`)
                    // traces into its cells like the derived-table form.
                    if let Some(values) = values_node(body) {
                        values_named_origins(values, name, body, context)
                    } else {
                        origins_into(body, None, name, context)
                    }
                })
                .unwrap_or_default()
        }
        // A `Derived` reference resolves at a producer's named output (a
        // `Projection` / `Aggregate` expr), never at a raw `Scan` — a reference to
        // a base column is `Binding::Base` and returns directly, not via this
        // traversal. So a `Scan` reached here (e.g. the other side of a join
        // the qualified name doesn't own) contributes nothing.
        LogicalPlan::Scan(_) | LogicalPlan::Values(_) | LogicalPlan::Empty => Vec::new(),
        // DML/DDL roots are not column producers traced into here.
        LogicalPlan::Insert(_)
        | LogicalPlan::Update(_)
        | LogicalPlan::Delete(_)
        | LogicalPlan::Merge(_)
        | LogicalPlan::CreateTableAs(_)
        | LogicalPlan::CreateView(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => Vec::new(),
    }
}

/// The origins of a (sub)query's first output column (a scalar subquery's
/// value) — fanning across **every** set-operation branch (each branch's
/// position-0 output), not just the leftmost, since the branches merge
/// positionally.
fn subquery_output_origins<'a>(
    op: &'a LogicalPlan,
    output: usize,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    // The `output`-th column of every branch (0 for a scalar subquery; the i-th
    // for a tuple `SET (a, b) = (SELECT x, y)` assignment — a set-op body fans
    // the position over each branch). `output_operands` accumulates each peeled
    // `With`'s CTE declarations onto the operand and `trace_nth_output`
    // registers them — so a `CteRef` in the body (including the subquery's *own*
    // leading `WITH`, which `TraceContext::new` doesn't see) resolves.
    trace_nth_output(&output_operands(op), output, context)
}

/// Trace the `i`-th output column of every operand — the positional fan-out
/// shared by a set operation (its result column `i`), a scalar subquery
/// (column 0), and an `EXCLUDED.col` conflict reference. Each operand's
/// in-scope CTEs are registered for its trace ([`Operand::trace`]); a branch
/// shorter than `i` contributes nothing.
fn trace_nth_output<'a>(
    operands: &[Operand<'a>],
    i: usize,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    let mut out = Vec::new();
    for operand in operands {
        // A multi-alias tail output resolves to its head expression, so a
        // positional trace of `v` in `explode(arr) AS (k, v)` reaches `arr`.
        if let Some(expr) = resolved_output_expr(operand.outputs, i) {
            out.extend(operand.trace(context, |input, cx| origins_of_expr(expr, input, cx)));
        }
    }
    out
}

/// The origins of an ON CONFLICT DO UPDATE value. Like [`origins_of_expr`],
/// but an `EXCLUDED.col` reference (a `Derived` ref, qualified `excluded`) maps
/// to the INSERT source's like-positioned output column — a `VALUES` source
/// maps into its cells the same way. A source with nothing to inspect at all
/// (`DEFAULT VALUES`) yields no edge: the proposed value is untraceable,
/// like any constant. `positional` gates that mapping: the caller passes
/// `false` when the source projection kept an unexpanded wildcard — its
/// positions are then indeterminate, so an `EXCLUDED.col` yields no edge
/// rather than a mis-paired one (the same skip the INSERT relation pairing
/// applies).
pub(super) fn conflict_value_origins<'a>(
    value: &'a Expr,
    columns: &[ColumnWrite],
    source: &'a LogicalPlan,
    positional: bool,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match value {
        // A `Derived` ref here is `EXCLUDED.col` (the only synthetic relation in
        // a conflict scope). Map it to the source's `col`-positioned output —
        // fanning out to every set-operation branch, or into a `VALUES`
        // source's like-positioned cells (its rows *are* the proposed rows).
        Expr::Column(c) if matches!(c.binding, Binding::Derived) => {
            if !positional {
                return Vec::new();
            }
            let Some(i) = columns
                .iter()
                .position(|t| context.eq_column(&t.reference.name, &c.name))
            else {
                return Vec::new();
            };
            if let Some(values) = values_node(source) {
                return values_cell_origins(values, i, source, context);
            }
            // A source with nothing to inspect at all (`DEFAULT VALUES`):
            // the proposed value is untraceable, so the target column gets
            // no edge — like any constant.
            trace_nth_output(&output_operands(source), i, context)
        }
        // A non-EXCLUDED ref (a target column, MySQL `VALUES(col)` inner, …)
        // and the structural variants trace like any value.
        Expr::Column(_) => origins_of_expr(value, source, context),
        Expr::Call { args } => transform(
            args.iter()
                .flat_map(|e| conflict_value_origins(e, columns, source, positional, context)),
        ),
        Expr::Case {
            then, else_result, ..
        } => {
            let mut sources: Vec<_> = then
                .iter()
                .flat_map(|e| conflict_value_origins(e, columns, source, positional, context))
                .collect();
            if let Some(e) = else_result {
                sources.extend(conflict_value_origins(
                    e, columns, source, positional, context,
                ));
            }
            transform(sources)
        }
        Expr::Window { arg, .. } => transform(conflict_value_origins(
            arg, columns, source, positional, context,
        )),
        Expr::Subquery { plan, output } => {
            transform(subquery_output_origins(plan, *output, context))
        }
        Expr::Fanin(refs) => refs
            .iter()
            .filter_map(|c| column_read(c).map(|r| (r, ColumnLineageKind::Passthrough)))
            .collect(),
        // `a IN (subquery)`: the LHS flows as a value operand (see
        // `origins_of_expr`); the subquery side is a filter.
        Expr::InSubquery { expr, .. } => transform(conflict_value_origins(
            expr, columns, source, positional, context,
        )),
        // A conflict scope holds no derived relations besides `EXCLUDED`
        // (handled as a named `Derived` ref above), so an expanded slot can't
        // occur here — trace it like any value for completeness.
        Expr::DerivedSlot { .. } => origins_of_expr(value, source, context),
        Expr::Exists(_) | Expr::Filter(_) => Vec::new(),
    }
}

/// The origins of a table function's output — the arguments' origins,
/// composed as a [`Transformation`](ColumnLineageKind::Transformation)
/// (function granularity: see the `TableFunction` arm of [`origins_into`]).
/// The arguments were bound against the LATERAL-visible scope, so a base
/// reference (`UNNEST(t.arr)`) traces directly; a `Derived` reference to a
/// sibling derived table is out of this subtree's reach and drops (an
/// omission, not a misattribution — its physical read is still counted).
fn table_function_output_origins<'a>(
    tf: &'a super::logical_plan::TableFunction,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    let mut sources = Vec::new();
    for arg in &tf.args {
        sources.extend(origins_of_expr(arg, &tf.input, context));
    }
    transform(sources)
}

/// One output operand of a query: the projected columns and the input that
/// produces them, one per set-operation branch (a plain query has a single
/// operand). The `input` and the CTEs in scope at this operand (the `With`s
/// peeled above it) are private: a consumer that *traces* `input` reaches it
/// only through [`trace`](Operand::trace), which registers those CTEs first — so
/// a `CteRef` in the input resolves and no traversal can forget the
/// registration (the recurring "peel `With` for shape, drop its CTEs" bug). A
/// consumer that only needs the shape uses the public `outputs`.
pub(super) struct Operand<'a> {
    pub(super) outputs: &'a [NamedExpr],
    input: &'a LogicalPlan,
    ctes: Vec<&'a Cte>,
}

impl<'a> Operand<'a> {
    /// Trace this operand's input with its in-scope CTEs registered in
    /// `context` for the duration of `f`, unregistering after. The only path to
    /// the input, so the CTE registration can't be skipped.
    pub(super) fn trace<R>(
        &self,
        context: &mut TraceContext<'a>,
        f: impl FnOnce(&'a LogicalPlan, &mut TraceContext<'a>) -> R,
    ) -> R {
        context.with_cte_refs(&self.ctes, |context| f(self.input, context))
    }
}

/// A query's output operands (one per set-operation branch). Peels the clause
/// layers above the projection (GROUP BY / HAVING `Filter`, ORDER BY `Sort`)
/// and `With` — accumulating each peeled `With`'s CTEs into the operand so a
/// later [`Operand::trace`] can register them.
pub(super) fn output_operands(op: &LogicalPlan) -> Vec<Operand<'_>> {
    let mut out = Vec::new();
    collect_operands(op, &[], &mut out);
    out
}

/// Recursive worker for [`output_operands`]: collect each branch's operand,
/// carrying `ctes` — the CTEs from the `With`s peeled on the way down — into it.
fn collect_operands<'a>(op: &'a LogicalPlan, ctes: &[&'a Cte], out: &mut Vec<Operand<'a>>) {
    match op {
        LogicalPlan::Projection(p) => out.push(Operand {
            outputs: &p.exprs,
            input: &p.input,
            ctes: ctes.to_vec(),
        }),
        LogicalPlan::Sort(s) => collect_operands(&s.input, ctes, out),
        LogicalPlan::Filter(f) => collect_operands(&f.input, ctes, out),
        LogicalPlan::With(w) => {
            let mut inner: Vec<&Cte> = ctes.to_vec();
            inner.extend(w.ctes.iter());
            collect_operands(&w.body, &inner, out);
        }
        LogicalPlan::SetOp(so) => {
            collect_operands(&so.left, ctes, out);
            collect_operands(&so.right, ctes, out);
        }
        // An aliasing boundary renames the *relation*, not the columns — the
        // input's outputs are exposed unchanged (a pipe `|> AS u` sits right
        // on the statement's output path).
        LogicalPlan::SubqueryAlias(sa) => collect_operands(&sa.input, ctes, out),
        // A join on the output path is a pipe `|> JOIN` above the running
        // projection: the join's output keeps the left block's slots first
        // (the right side's columns follow it), so the left branch carries
        // the positional operands — a right-side slot has no operand and
        // stays edge-less, best-effort. (A join *below* a projection never
        // reaches here — the projection claims the walk first.)
        LogicalPlan::Join(jn) => collect_operands(&jn.left, ctes, out),
        // No projection at this level — a relation that doesn't carry a
        // SELECT list (a `Scan`, a join below a projection, a DML / DDL root,
        // …) yields no operands. Listed explicitly so a new operator that
        // *does* expose columns positionally forces an explicit handler.
        LogicalPlan::Scan(_)
        | LogicalPlan::Aggregate(_)
        | LogicalPlan::TableFunction(_)
        | LogicalPlan::CteRef(_)
        | LogicalPlan::Values(_)
        | LogicalPlan::Empty
        | LogicalPlan::Insert(_)
        | LogicalPlan::Update(_)
        | LogicalPlan::Delete(_)
        | LogicalPlan::Merge(_)
        | LogicalPlan::CreateTableAs(_)
        | LogicalPlan::CreateView(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => {}
    }
}

/// The number of output slots `op` exposes, when statically knowable: a
/// projection's slot count, a set operation's first branch, a join's two
/// sides summed, VALUES rows' width — through the transparent wrappers.
/// `None` when a side exposes no positional producer (a raw scan, an opaque
/// table function, a CTE reference), where a count would be a guess.
fn slot_width(op: &LogicalPlan) -> Option<usize> {
    match op {
        LogicalPlan::Projection(p) => Some(output_slots(&p.exprs).count()),
        LogicalPlan::SetOp(so) => slot_width(&so.left),
        LogicalPlan::SubqueryAlias(sa) => slot_width(&sa.input),
        LogicalPlan::Filter(f) => slot_width(&f.input),
        LogicalPlan::Sort(s) => slot_width(&s.input),
        LogicalPlan::Aggregate(a) => slot_width(&a.input),
        LogicalPlan::With(w) => slot_width(&w.body),
        LogicalPlan::Join(j) => Some(slot_width(&j.left)? + slot_width(&j.right)?),
        LogicalPlan::Values(v) => v.rows.first().map(Vec::len),
        LogicalPlan::Scan(_)
        | LogicalPlan::TableFunction(_)
        | LogicalPlan::CteRef(_)
        | LogicalPlan::Empty
        | LogicalPlan::Insert(_)
        | LogicalPlan::Update(_)
        | LogicalPlan::Delete(_)
        | LogicalPlan::Merge(_)
        | LogicalPlan::CreateTableAs(_)
        | LogicalPlan::CreateView(_)
        | LogicalPlan::AlterTable(_)
        | LogicalPlan::Drop(_) => None,
    }
}

/// Peel leading `With` nodes off `op`, pushing their CTE declarations into
/// `context` so a `CteRef` below resolves during the trace, and return the peeled
/// root. (`TraceContext::new` already does this for a query's leading `WITH`; a DML
/// source carries its own `WITH`, reached only here.) The push is not popped —
/// the context is per-statement scratch, discarded after the walk.
pub(super) fn enter_withs<'a>(
    op: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
) -> &'a LogicalPlan {
    let mut node = op;
    while let LogicalPlan::With(w) = node {
        w.ctes.iter().for_each(|c| context.ctes.push(c));
        node = &w.body;
    }
    node
}

// ===== helpers ===========================================================

fn transform(
    sources: impl IntoIterator<Item = (ColumnRead, ColumnLineageKind)>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    sources
        .into_iter()
        .map(|(r, _)| (r, ColumnLineageKind::Transformation))
        .collect()
}

/// The output **position** named `name` (case-folded), over the slot view —
/// so a fan's aliases each occupy their own position.
fn named_position(exprs: &[NamedExpr], name: &Ident, fold: CaseRule) -> Option<usize> {
    let target = fold.normalize(name);
    output_slots(exprs).position(|(n, _)| n.is_some_and(|n| fold.normalize(n) == target))
}
