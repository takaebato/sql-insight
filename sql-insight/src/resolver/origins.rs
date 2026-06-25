//! The column-origin traversal: tracing an expression's **value** to the base
//! columns it derives from, each with a composed [`ColumnLineageKind`]. This is
//! the `getColumnOrigins`-style core that [`super::lineage`] drives to build
//! the `source → target` edges.
//!
//! The value-vs-filter split falls out of this trace for free: filter-position
//! operands (CASE conditions, window partition / order keys, EXISTS / IN tests)
//! are *not* traced — they surface as reads, never as origins.

use sqlparser::ast::Ident;

use super::logical_plan::{Binding, BoundColumn, Cte, Expr, LogicalPlan, NamedExpr};
use super::reads::column_read;
use crate::casing::{CaseRule, IdentifierCasing};
use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableReference};

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
        // A scalar subquery's first output column flows as a transformation.
        Expr::Subquery(plan) => transform(scalar_subquery_origins(plan, context)),
        // A merge-column fan-in: each owning side is its own origin, traced
        // like a column ref — a real-table side is a `Passthrough` base read,
        // a derived / CTE side traces into its producing subquery (so a
        // `(SELECT id FROM s) d JOIN t USING (id)` yields both `s.id` and
        // `t.id`, not just one).
        Expr::Fanin(refs) => refs
            .iter()
            .flat_map(|c| origins_of_ref(c, input, context))
            .collect(),
        // Tests / suppressed operands contribute no value origin (reads only).
        Expr::Exists(_) | Expr::InSubquery { .. } | Expr::Filter(_) => Vec::new(),
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

/// Whether a (sub)plan's rows are synthesised by `VALUES` (peeling the clause
/// layers / a leading `With`): a column of such a relation has no base column
/// to collapse to, so a reference to it is a synthetic source.
fn values_backed(op: &LogicalPlan) -> bool {
    match op {
        LogicalPlan::Values(_) => true,
        // Only the clause-layer wrappers `Values` can sit beneath are peeled —
        // an ORDER BY / WHERE on a VALUES, and a leading WITH. Anything else
        // is a real relation (a Scan, a Projection rewriting the row shape, a
        // derived alias on top), and its columns *do* have a base to collapse
        // to. Listed explicitly so a new operator added between `Values` and
        // its clause layers needs an explicit decision here.
        LogicalPlan::Sort(s) => values_backed(&s.input),
        LogicalPlan::Filter(f) => values_backed(&f.input),
        LogicalPlan::With(w) => values_backed(&w.body),
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
        | LogicalPlan::Drop(_) => false,
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
        LogicalPlan::Projection(p) => match find_named(&p.exprs, name, context.casing.column) {
            Some(ne) => origins_of_expr(&ne.expr, &p.input, context),
            None => Vec::new(),
        },
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
            } else if values_backed(&sa.input) {
                // A `(VALUES …) AS t(x)` relation synthesises rows with no base
                // columns, so the exposed column is a synthetic source (t.x).
                vec![(
                    synthetic_source(&sa.alias, name),
                    ColumnLineageKind::Passthrough,
                )]
            } else {
                origins_into(&sa.input, None, name, context)
            }
        }
        // An opaque table function: its produced columns are dynamic, so a ref
        // through its alias is a synthetic lineage source (the alias as table)
        // — not collapsible to a base column.
        LogicalPlan::TableFunction(tf) => match &tf.alias {
            Some(alias) if qualifier.is_none_or(|q| context.eq_alias(q, alias)) => {
                vec![(
                    synthetic_source(alias, name),
                    ColumnLineageKind::Passthrough,
                )]
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
            let operands = output_operands(op);
            let Some(i) = operands.first().and_then(|&(outs, _)| {
                outs.iter()
                    .position(|ne| ne.name.as_ref().is_some_and(|n| context.eq_column(n, name)))
            }) else {
                return Vec::new();
            };
            operands
                .iter()
                .filter_map(|&(outs, input)| outs.get(i).map(|ne| (ne, input)))
                .flat_map(|(ne, input)| origins_of_expr(&ne.expr, input, context))
                .collect()
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
                    // A VALUES-backed CTE has no traceable base columns — the
                    // exposed column is a synthetic source (cte.col).
                    if values_backed(body) {
                        vec![(
                            synthetic_source(&r.name, name),
                            ColumnLineageKind::Passthrough,
                        )]
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
fn scalar_subquery_origins<'a>(
    op: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    // A leading `WITH` on the subquery must register its CTE declarations so a
    // `CteRef` in the body resolves during the trace: `output_operands` peels
    // the `With` for shape but doesn't push its CTEs (and `TraceContext::new` only
    // registers the statement's *top-level* leading WITH, not a nested
    // subquery's own). `with_decls` balances the push/pop.
    if let LogicalPlan::With(w) = op {
        return context.with_decls(&w.ctes, |context| scalar_subquery_origins(&w.body, context));
    }
    output_operands(op)
        .iter()
        .filter_map(|&(outputs, input)| outputs.first().map(|ne| (ne, input)))
        .flat_map(|(ne, input)| origins_of_expr(&ne.expr, input, context))
        .collect()
}

/// The origins of an ON CONFLICT DO UPDATE value. Like [`origins_of_expr`],
/// but an `EXCLUDED.col` reference (a `Derived` ref, qualified `excluded`) maps
/// to the INSERT source's like-positioned output column — or, when the source
/// has no inspectable projection (a `VALUES` source), to the `EXCLUDED.col`
/// pseudo-column itself (a synthetic lineage source, not a read).
pub(super) fn conflict_value_origins<'a>(
    value: &'a Expr,
    columns: &[Ident],
    source: &'a LogicalPlan,
    context: &mut TraceContext<'a>,
) -> Vec<(ColumnRead, ColumnLineageKind)> {
    match value {
        // A `Derived` ref here is `EXCLUDED.col` (the only synthetic relation in
        // a conflict scope). Map it to the source's `col`-positioned output —
        // fanning out to every set-operation branch. A source with no
        // inspectable projection (VALUES) keeps the `EXCLUDED.col` pseudo-source.
        Expr::Column(c) if matches!(c.binding, Binding::Derived) => {
            let Some(i) = columns.iter().position(|t| context.eq_column(t, &c.name)) else {
                return Vec::new();
            };
            let operands = output_operands(source);
            if operands.is_empty() {
                return vec![(excluded_source(c), ColumnLineageKind::Passthrough)];
            }
            let mut out = Vec::new();
            for (outputs, input) in operands {
                if let Some(ne) = outputs.get(i) {
                    out.extend(origins_of_expr(&ne.expr, input, context));
                }
            }
            out
        }
        // A non-EXCLUDED ref (a target column, MySQL `VALUES(col)` inner, …)
        // and the structural variants trace like any value.
        Expr::Column(_) => origins_of_expr(value, source, context),
        Expr::Call { args } => transform(
            args.iter()
                .flat_map(|e| conflict_value_origins(e, columns, source, context)),
        ),
        Expr::Case {
            then, else_result, ..
        } => {
            let mut sources: Vec<_> = then
                .iter()
                .flat_map(|e| conflict_value_origins(e, columns, source, context))
                .collect();
            if let Some(e) = else_result {
                sources.extend(conflict_value_origins(e, columns, source, context));
            }
            transform(sources)
        }
        Expr::Window { arg, .. } => {
            transform(conflict_value_origins(arg, columns, source, context))
        }
        Expr::Subquery(plan) => transform(scalar_subquery_origins(plan, context)),
        Expr::Fanin(refs) => refs
            .iter()
            .filter_map(|c| column_read(c).map(|r| (r, ColumnLineageKind::Passthrough)))
            .collect(),
        Expr::Exists(_) | Expr::InSubquery { .. } | Expr::Filter(_) => Vec::new(),
    }
}

/// The `EXCLUDED.col` pseudo-table lineage source (when the source can't be
/// collapsed through): the qualifier (`EXCLUDED`, original text) as the table.
fn excluded_source(c: &BoundColumn) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: c.qualifier.clone().map(|q| TableReference {
                catalog: None,
                schema: None,
                name: q,
            }),
            name: c.name.clone(),
        },
        resolution: ResolutionKind::Inferred,
    }
}

/// A synthetic single-segment lineage source `table.name` (`Inferred`) — for a
/// table-function column, whose produced value flows out but has no base
/// column to collapse to.
fn synthetic_source(table: &Ident, name: &Ident) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: Some(TableReference {
                catalog: None,
                schema: None,
                name: table.clone(),
            }),
            name: name.clone(),
        },
        resolution: ResolutionKind::Inferred,
    }
}

/// A query's output operands: one `(output columns, producing input)` per
/// set-operation branch (a plain query has a single operand). Peels the clause
/// layers above the projection (GROUP BY / HAVING `Filter`, ORDER BY `Sort`)
/// and `With`.
pub(super) fn output_operands(op: &LogicalPlan) -> Vec<(&[NamedExpr], &LogicalPlan)> {
    match op {
        LogicalPlan::Projection(p) => vec![(&p.exprs, &p.input)],
        LogicalPlan::Sort(s) => output_operands(&s.input),
        LogicalPlan::Filter(f) => output_operands(&f.input),
        LogicalPlan::With(w) => output_operands(&w.body),
        LogicalPlan::SetOp(so) => {
            let mut operands = output_operands(&so.left);
            operands.extend(output_operands(&so.right));
            operands
        }
        // No projection at this level — a relation that doesn't carry a
        // SELECT list (a `Scan`, a join below a projection, a DML / DDL root,
        // …) yields no operands. Listed explicitly so a new operator that
        // *does* expose columns positionally forces an explicit handler.
        LogicalPlan::Scan(_)
        | LogicalPlan::Join(_)
        | LogicalPlan::Aggregate(_)
        | LogicalPlan::SubqueryAlias(_)
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
        | LogicalPlan::Drop(_) => Vec::new(),
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

fn find_named<'a>(exprs: &'a [NamedExpr], name: &Ident, fold: CaseRule) -> Option<&'a NamedExpr> {
    let target = fold.normalize(name);
    exprs.iter().find(|ne| {
        ne.name
            .as_ref()
            .is_some_and(|n| fold.normalize(n) == target)
    })
}
