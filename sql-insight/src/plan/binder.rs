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
    AssignmentTarget, CreateTable, CreateView, Cte, Delete, Expr, FromTable, Function, FunctionArg,
    FunctionArgExpr, FunctionArguments, GroupByExpr, GroupByWithModifier, Ident, Insert, Join,
    JoinConstraint, JoinOperator, Merge, MergeAction, MergeInsertKind, ObjectName, OrderBy,
    OrderByKind, Query, Select, SelectItem, SetExpr, Statement, TableAlias, TableFactor,
    TableWithJoins, Update, UpdateTableFromKind,
};

use super::ir::{
    BoundColumn, OpaqueLeaf, PassThrough, Plan, Project, ProvenanceSource, Scan, SetOp, Write,
};
use crate::catalog::{Catalog, CatalogTable};
use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableReference};
use crate::resolver::{CaseFold, IdentifierCasing};

/// Bind one statement into a [`Plan`], or `None` for statement kinds not
/// modelled (queries and the data-moving DML / DDL are; other DDL and
/// session statements aren't). The top-level scope is discarded —
/// callers consume the resolved tree, not the scope.
pub(crate) fn build(
    statement: &Statement,
    catalog: Option<&Catalog>,
    casing: IdentifierCasing,
) -> Option<Plan> {
    let binder = Binder {
        catalog,
        casing,
        ctes: Vec::new(),
        outer_scopes: Vec::new(),
    };
    binder.bind_statement(statement)
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
/// columns come from. Cloned into the binder's outer-scope stack so a
/// correlated subquery can resolve against enclosing relations.
#[derive(Clone)]
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
#[derive(Clone)]
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
#[derive(Clone)]
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
    provenance: Vec<ProvenanceSource>,
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

/// A common table expression in scope: a named synthetic relation. Bound
/// once at its `WITH` declaration; references in FROM clone its `plan`
/// into the tree and expose its `outputs` (so unreferenced CTEs
/// contribute nothing, and the body is collapsed by construction like a
/// derived table).
#[derive(Clone)]
struct CteRelation {
    name: Ident,
    plan: Plan,
    outputs: Vec<BoundColumn>,
}

/// Carries the bind-time context: the optional catalog, the dialect
/// casing, the common table expressions in scope (accumulated in
/// declaration order, innermost `WITH` last), and the enclosing queries'
/// relations (the correlation stack, outermost first) that an inner
/// subquery's references fall through to.
struct Binder<'a> {
    catalog: Option<&'a Catalog>,
    casing: IdentifierCasing,
    ctes: Vec<CteRelation>,
    outer_scopes: Vec<Vec<Relation>>,
}

impl Binder<'_> {
    /// Bind a statement into a [`Plan`], or `None` for kinds not modelled
    /// yet. A query is bound directly; the data-moving statements
    /// (INSERT / UPDATE / DELETE / MERGE / CTAS / CREATE VIEW) produce a
    /// [`Write`]-rooted tree whose `input` carries every read (the source
    /// query plus any SET / predicate / VALUES reads).
    fn bind_statement(&self, statement: &Statement) -> Option<Plan> {
        match statement {
            Statement::Query(query) => Some(self.bind_query(query).0),
            Statement::Insert(insert) => self.bind_insert(insert),
            Statement::Update(update) => self.bind_update(update),
            Statement::Delete(delete) => self.bind_delete(delete),
            Statement::Merge(merge) => self.bind_merge(merge),
            Statement::CreateTable(create) => self.bind_create_table(create),
            Statement::CreateView(create) => self.bind_create_view(create),
            // Other DDL / session statements aren't data operations — not
            // bound (the wildcard mirrors `build`'s "unsupported → None").
            _ => None,
        }
    }

    /// `INSERT INTO target (cols) <source>`: the source query's plan is
    /// the read-carrying input; the target columns are the write targets.
    /// A `VALUES` source binds to an opaque leaf (no column reads).
    fn bind_insert(&self, insert: &Insert) -> Option<Plan> {
        let (target, _alias) = TableReference::from_insert_with_alias(insert).ok()?;
        let target_columns = insert.columns.clone();
        let input = match &insert.source {
            Some(source) => self.bind_query(source).0,
            None => Plan::OpaqueLeaf(OpaqueLeaf { alias: None }),
        };
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(input),
        }))
    }

    /// `UPDATE target SET col = expr [FROM src] WHERE pred`: the target
    /// (write) plus any FROM relations (read) form the scope. The SET
    /// assignments become a `Project` whose outputs are named by their
    /// target columns (so the lineage `RHS → target` pairing falls out of
    /// the same output-column machinery as INSERT / CTAS); the WHERE
    /// predicate is a filter `PassThrough` below it.
    fn bind_update(&self, update: &Update) -> Option<Plan> {
        let (target_plan, mut scope) = self.bind_table_with_joins(&update.table);
        let mut inputs = vec![target_plan];
        if let Some(from) = &update.from {
            let tables = match from {
                UpdateTableFromKind::BeforeSet(tables) | UpdateTableFromKind::AfterSet(tables) => {
                    tables
                }
            };
            for twj in tables {
                let (plan, from_scope) = self.bind_table_with_joins(twj);
                inputs.push(plan);
                scope = scope.merge(from_scope);
            }
        }
        let where_reads = update
            .selection
            .as_ref()
            .map(|s| self.expr_reads(s, &scope))
            .unwrap_or_default();
        let source = wrap_inputs(inputs, where_reads);
        let mut outputs = Vec::new();
        let mut target_columns = Vec::new();
        for assignment in &update.assignments {
            for column in assignment_target_columns(&assignment.target) {
                outputs.push(self.bind_value_column(
                    Some(column.clone()),
                    &assignment.value,
                    &scope,
                ));
                target_columns.push(column);
            }
        }
        let target = TableReference::try_from(&update.table.relation).ok()?;
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(Plan::Project(Project {
                input: Box::new(source),
                outputs,
            })),
        }))
    }

    /// `DELETE FROM target [USING src] WHERE pred`: the predicate's reads
    /// against the FROM (and USING) relations; no column writes (the rows
    /// are removed wholesale), so the [`Write`]'s `target_columns` is
    /// empty — the target still surfaces for table-level analysis.
    fn bind_delete(&self, delete: &Delete) -> Option<Plan> {
        let from_tables = match &delete.from {
            FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
        };
        let mut inputs = Vec::new();
        let mut scope = Scope::empty();
        for twj in from_tables.iter().chain(delete.using.iter().flatten()) {
            let (plan, twj_scope) = self.bind_table_with_joins(twj);
            inputs.push(plan);
            scope = scope.merge(twj_scope);
        }
        let reads = delete
            .selection
            .as_ref()
            .map(|s| self.expr_reads(s, &scope))
            .unwrap_or_default();
        let input = wrap_inputs(inputs, reads);
        // The deleted relation is the explicit `DELETE t` list when
        // present, else the FROM target.
        let target = if let Some(name) = delete.tables.first() {
            TableReference::try_from_name(name).ok()
        } else {
            from_tables
                .first()
                .and_then(|twj| TableReference::try_from(&twj.relation).ok())
        };
        Some(match target {
            Some(target) => Plan::Write(Write {
                target,
                target_columns: Vec::new(),
                input: Box::new(input),
            }),
            None => input,
        })
    }

    /// `MERGE INTO target USING source ON pred WHEN … THEN …`: target
    /// (write) and source (read) form the scope. ON and the per-clause
    /// predicates are filter reads; each WHEN action's value expressions
    /// become `Project` outputs named by their written column (UPDATE SET
    /// target / INSERT column), so the `value → target` lineage pairing
    /// reuses the output-column machinery.
    fn bind_merge(&self, merge: &Merge) -> Option<Plan> {
        let (target_plan, target_scope) = self.bind_table_factor(&merge.table);
        let (source_plan, source_scope) = self.bind_table_factor(&merge.source);
        let scope = target_scope.merge(source_scope);
        let mut reads = self.expr_reads(&merge.on, &scope);
        let mut outputs = Vec::new();
        let mut target_columns = Vec::new();
        for clause in &merge.clauses {
            if let Some(predicate) = &clause.predicate {
                reads.extend(self.expr_reads(predicate, &scope));
            }
            match &clause.action {
                MergeAction::Insert(insert) => {
                    if let Some(predicate) = &insert.insert_predicate {
                        reads.extend(self.expr_reads(predicate, &scope));
                    }
                    if let MergeInsertKind::Values(values) = &insert.kind {
                        let columns: Vec<Ident> = insert
                            .columns
                            .iter()
                            .filter_map(object_name_last_ident)
                            .collect();
                        for row in &values.rows {
                            for (column, expr) in columns.iter().zip(row) {
                                outputs.push(self.bind_value_column(
                                    Some(column.clone()),
                                    expr,
                                    &scope,
                                ));
                                target_columns.push(column.clone());
                            }
                        }
                    }
                }
                MergeAction::Update(update) => {
                    for assignment in &update.assignments {
                        for column in assignment_target_columns(&assignment.target) {
                            outputs.push(self.bind_value_column(
                                Some(column.clone()),
                                &assignment.value,
                                &scope,
                            ));
                            target_columns.push(column);
                        }
                    }
                    for predicate in [&update.update_predicate, &update.delete_predicate]
                        .into_iter()
                        .flatten()
                    {
                        reads.extend(self.expr_reads(predicate, &scope));
                    }
                }
                // DELETE moves no column values.
                MergeAction::Delete { .. } => {}
            }
        }
        let target = TableReference::try_from(&merge.table).ok()?;
        let source = wrap_inputs(vec![target_plan, source_plan], reads);
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(Plan::Project(Project {
                input: Box::new(source),
                outputs,
            })),
        }))
    }

    /// `CREATE TABLE dst AS <query>` (CTAS): the source query's reads,
    /// paired with the new table's columns (explicit defs win, else the
    /// source output names). A non-CTAS `CREATE TABLE` (no query) isn't a
    /// data operation — not bound.
    fn bind_create_table(&self, create: &CreateTable) -> Option<Plan> {
        let query = create.query.as_ref()?;
        let (input, scope) = self.bind_query(query);
        let target = TableReference::try_from(&create.name).ok()?;
        let target_columns = if create.columns.is_empty() {
            output_names(&scope.outputs)
        } else {
            create.columns.iter().map(|c| c.name.clone()).collect()
        };
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(input),
        }))
    }

    /// `CREATE VIEW v AS <query>`: like CTAS — the query's reads paired
    /// with the view's columns (explicit list wins, else source names).
    fn bind_create_view(&self, create: &CreateView) -> Option<Plan> {
        let (input, scope) = self.bind_query(&create.query);
        let target = TableReference::try_from(&create.name).ok()?;
        let target_columns = if create.columns.is_empty() {
            output_names(&scope.outputs)
        } else {
            create.columns.iter().map(|c| c.name.clone()).collect()
        };
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(input),
        }))
    }

    /// Bind a query, returning the plan node and its output scope. A
    /// leading `WITH` is peeled first: each CTE binds in declaration order
    /// into an environment the later CTEs and the body resolve against. A
    /// `RECURSIVE` CTE also sees itself (via its anchor's columns) while
    /// its recursive branch binds.
    fn bind_query(&self, query: &Query) -> (Plan, Scope) {
        let Some(with) = &query.with else {
            return self.bind_query_body(query);
        };
        let mut env = self.ctes.clone();
        for cte in &with.cte_tables {
            let cte_relation = if with.recursive {
                self.bind_recursive_cte(cte, &env)
            } else {
                let (plan, scope) = self.with_ctes(env.clone()).bind_query(&cte.query);
                let mut outputs = scope.outputs;
                apply_column_aliases(&mut outputs, &cte.alias);
                CteRelation {
                    name: cte.alias.name.clone(),
                    plan,
                    outputs,
                }
            };
            env.push(cte_relation);
        }
        self.with_ctes(env).bind_query_body(query)
    }

    /// Bind a `RECURSIVE` CTE: bind the anchor (the body set-operation's
    /// non-recursive left branch) to learn the output shape, register the
    /// CTE name with those columns so the recursive branch's
    /// self-reference resolves, then bind the full body. Self-reference
    /// resolution sees the anchor's columns (shallow — matching the
    /// resolver's deferred recursive collapse). A `RECURSIVE` CTE whose
    /// body isn't a set operation degenerates to a plain CTE.
    fn bind_recursive_cte(&self, cte: &Cte, env: &[CteRelation]) -> CteRelation {
        let name = cte.alias.name.clone();
        let SetExpr::SetOperation { left, .. } = cte.query.body.as_ref() else {
            let (plan, scope) = self.with_ctes(env.to_vec()).bind_query(&cte.query);
            let mut outputs = scope.outputs;
            apply_column_aliases(&mut outputs, &cte.alias);
            return CteRelation {
                name,
                plan,
                outputs,
            };
        };
        let mut anchor_outputs = self.with_ctes(env.to_vec()).bind_set_expr(left).1.outputs;
        apply_column_aliases(&mut anchor_outputs, &cte.alias);
        // Provisional registration: the recursive branch sees the CTE name
        // resolving to the anchor's columns (the stub plan is never walked
        // — only the outputs are consulted during resolution).
        let mut provisional = env.to_vec();
        provisional.push(CteRelation {
            name: name.clone(),
            plan: Plan::OpaqueLeaf(OpaqueLeaf { alias: None }),
            outputs: anchor_outputs,
        });
        let (plan, scope) = self.with_ctes(provisional).bind_query(&cte.query);
        let mut outputs = scope.outputs;
        apply_column_aliases(&mut outputs, &cte.alias);
        CteRelation {
            name,
            plan,
            outputs,
        }
    }

    /// A child binder sharing this one's catalog / casing, with the given
    /// CTE environment and correlation stack.
    fn child(&self, ctes: Vec<CteRelation>, outer_scopes: Vec<Vec<Relation>>) -> Binder<'_> {
        Binder {
            catalog: self.catalog,
            casing: self.casing,
            ctes,
            outer_scopes,
        }
    }

    /// A child binder with a different CTE environment (extending scope
    /// across a `WITH`); the correlation stack carries over unchanged.
    fn with_ctes(&self, ctes: Vec<CteRelation>) -> Binder<'_> {
        self.child(ctes, self.outer_scopes.clone())
    }

    /// A child binder with one more enclosing scope on the correlation
    /// stack (used when descending into a subquery in an expression).
    fn with_outer_scope(&self, relations: Vec<Relation>) -> Binder<'_> {
        let mut outer_scopes = self.outer_scopes.clone();
        outer_scopes.push(relations);
        self.child(self.ctes.clone(), outer_scopes)
    }

    /// Bind a query's body and trailing ORDER BY (the WITH clause is
    /// already in scope via `self.ctes`).
    fn bind_query_body(&self, query: &Query) -> (Plan, Scope) {
        let (body, scope) = self.bind_set_expr(&query.body);
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

    /// Bind a query body's set expression: a leaf `SELECT`, a
    /// parenthesized inner query, or a set operation (`UNION` /
    /// `INTERSECT` / `EXCEPT`). A set operation fans its operands into a
    /// [`SetOp`] and merges their outputs positionally — each result
    /// column unions the branches' provenance (so a derived / CTE over a
    /// `UNION` traces to every branch's base columns), taking its name
    /// from the left branch. The set-operation kind itself doesn't change
    /// lineage, so it's dropped.
    fn bind_set_expr(&self, set_expr: &SetExpr) -> (Plan, Scope) {
        match set_expr {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(inner) => self.bind_query(inner),
            SetExpr::SetOperation { left, right, .. } => {
                let (left_plan, left_scope) = self.bind_set_expr(left);
                let (right_plan, right_scope) = self.bind_set_expr(right);
                let outputs = merge_set_outputs(left_scope.outputs, right_scope.outputs);
                (
                    Plan::SetOp(SetOp {
                        operands: vec![left_plan, right_plan],
                    }),
                    Scope {
                        relations: Vec::new(),
                        outputs,
                    },
                )
            }
            // VALUES / table-valued bodies: no inspectable columns yet.
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
                reads: self.expr_reads(predicate, &from_scope),
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
            clause_reads.extend(self.expr_reads(having, &clause_scope));
        }
        for sort in &select.sort_by {
            clause_reads.extend(self.expr_reads(&sort.expr, &clause_scope));
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
                Some(JoinConstraint::On(expr)) => self.expr_reads(expr, &combined),
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
            TableFactor::Table { name, alias, .. } => {
                let Ok(written) = TableReference::try_from_name(name) else {
                    return (Plan::OpaqueLeaf(OpaqueLeaf { alias: None }), Scope::empty());
                };
                let alias = alias.as_ref().map(|a| a.name.clone());
                // A bare name matching an in-scope CTE resolves to that
                // CTE's synthetic relation — its plan (cloned into the
                // tree) plus its pre-collapsed outputs — exactly like a
                // derived table. Qualified names are never CTEs.
                if written.schema.is_none() && written.catalog.is_none() {
                    if let Some(cte) = self.lookup_cte(&written.name) {
                        let relation = Relation {
                            alias: alias.or_else(|| Some(cte.name.clone())),
                            source: RelationSource::Derived {
                                columns: cte.outputs.clone(),
                            },
                        };
                        return (cte.plan.clone(), Scope::of(relation));
                    }
                }
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
                let mut columns = sub_scope.outputs;
                if let Some(alias) = alias {
                    apply_column_aliases(&mut columns, alias);
                }
                let relation = Relation {
                    alias: alias.as_ref().map(|a| a.name.clone()),
                    source: RelationSource::Derived { columns },
                };
                (plan, Scope::of(relation))
            }
            // Table functions / pivots / VALUES etc. are later bricks;
            // they expose no inspectable columns yet.
            _ => (Plan::OpaqueLeaf(OpaqueLeaf { alias: None }), Scope::empty()),
        }
    }

    /// Find an in-scope CTE by name (innermost `WITH` shadows outer),
    /// matched with table-alias casing.
    fn lookup_cte(&self, name: &Ident) -> Option<&CteRelation> {
        self.ctes
            .iter()
            .rev()
            .find(|c| self.ident_eq(&c.name, name))
    }

    /// Resolve one SELECT-list item against `scope`. Wildcards are
    /// skipped for now (`None`).
    fn bind_output_column(&self, item: &SelectItem, scope: &Scope) -> Option<BoundColumn> {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.clone())),
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => return None,
        };
        let name = alias.or_else(|| inferred_output_name(expr));
        Some(self.bind_value_column(name, expr, scope))
    }

    /// Bind a value-producing expression (a projection item or a SET /
    /// VALUES assignment RHS) into a named output column. Each source's
    /// composed kind folds in this expression's own kind: a bare column
    /// ref is `Passthrough`, anything else `Transformation` — so a
    /// transforming step anywhere along the chain wins.
    fn bind_value_column(&self, name: Option<Ident>, expr: &Expr, scope: &Scope) -> BoundColumn {
        let outer = expr_kind(expr);
        let provenance = self
            .expr_provenance(expr, scope)
            .into_iter()
            .map(|source| ProvenanceSource {
                kind: combine_kind(source.kind, outer),
                read: source.read,
            })
            .collect();
        BoundColumn { name, provenance }
    }

    /// Every lineage source an expression contributes against `scope`:
    /// its direct column references (resolved to their pre-collapsed
    /// provenance) plus any nested subquery's reads. Used for value
    /// positions (a column's provenance); the filter-only callers strip
    /// the kind via [`Self::expr_reads`].
    fn expr_provenance(&self, expr: &Expr, scope: &Scope) -> Vec<ProvenanceSource> {
        let mut out = Vec::new();
        self.collect_expr_provenance(expr, scope, &mut out);
        out
    }

    /// The plain column reads of an expression (filter position — WHERE /
    /// ON / clause predicates): the provenance with its lineage kind
    /// dropped.
    fn expr_reads(&self, expr: &Expr, scope: &Scope) -> Vec<ColumnRead> {
        self.expr_provenance(expr, scope)
            .into_iter()
            .map(|source| source.read)
            .collect()
    }

    fn collect_expr_provenance(&self, expr: &Expr, scope: &Scope, out: &mut Vec<ProvenanceSource>) {
        match expr {
            Expr::Identifier(id) => out.extend(self.resolve_ref(std::slice::from_ref(id), scope)),
            Expr::CompoundIdentifier(ids) => out.extend(self.resolve_ref(ids, scope)),
            Expr::BinaryOp { left, right, .. } => {
                self.collect_expr_provenance(left, scope, out);
                self.collect_expr_provenance(right, scope, out);
            }
            Expr::UnaryOp { expr, .. } | Expr::Nested(expr) | Expr::Cast { expr, .. } => {
                self.collect_expr_provenance(expr, scope, out)
            }
            Expr::Function(function) => self.collect_function_provenance(function, scope, out),
            // GROUP BY ROLLUP / CUBE / GROUPING SETS — each grouping set is
            // a list of expressions.
            Expr::Rollup(sets) | Expr::Cube(sets) | Expr::GroupingSets(sets) => {
                for set in sets {
                    for expr in set {
                        self.collect_expr_provenance(expr, scope, out);
                    }
                }
            }
            // Subqueries: bind against the enclosing scope (correlation)
            // and fold in their reads as transformation sources.
            Expr::Subquery(query)
            | Expr::Exists {
                subquery: query, ..
            } => out.extend(self.subquery_sources(query, scope)),
            Expr::InSubquery { expr, subquery, .. } => {
                self.collect_expr_provenance(expr, scope, out);
                out.extend(self.subquery_sources(subquery, scope));
            }
            _ => {}
        }
    }

    fn collect_function_provenance(
        &self,
        function: &Function,
        scope: &Scope,
        out: &mut Vec<ProvenanceSource>,
    ) {
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
                    self.collect_expr_provenance(expr, scope, out);
                }
            }
        }
    }

    /// The lineage sources of a subquery nested in an expression: bind it
    /// as its own plan (with the containing `scope`'s relations pushed
    /// onto the correlation stack, so a correlated reference resolves
    /// outward) and walk it. A subquery derives its value, so its sources
    /// are tagged `Transformation`.
    fn subquery_sources(&self, query: &Query, scope: &Scope) -> Vec<ProvenanceSource> {
        let binder = self.with_outer_scope(scope.relations.clone());
        let (plan, _) = binder.bind_query(query);
        super::extract::extract_reads(&plan)
            .into_iter()
            .map(|read| ProvenanceSource {
                read,
                kind: ColumnLineageKind::Transformation,
            })
            .collect()
    }

    /// Resolve a single column reference to its pre-collapsed provenance.
    /// Mirrors the resolver: unqualified scans candidate relations
    /// (Known-witness over Open suspects); qualified matches the
    /// qualifier first. Catalog-free everything is `Open` → `Inferred` /
    /// `Ambiguous`. A name no relation in the current scope can own falls
    /// through to the enclosing scopes (correlation), innermost-first,
    /// before giving up as `Unresolved`.
    fn resolve_ref(&self, parts: &[Ident], scope: &Scope) -> Vec<ProvenanceSource> {
        let Some(column) = parts.last() else {
            return Vec::new();
        };
        // Output-alias visibility (GROUP BY / HAVING / ORDER BY): a bare
        // name matching an enclosing output column is a reference to that
        // output — return its provenance so an introduced alias resolves
        // to its real sources instead of a phantom stored column. Empty at
        // FROM-level (no outputs). Only the current scope's outputs are
        // visible; correlation reaches enclosing *relations*, not aliases.
        if parts.len() == 1 {
            if let Some(output) = scope
                .outputs
                .iter()
                .find(|c| self.name_matches(c.name.as_ref(), column))
            {
                return output.provenance.clone();
            }
        }
        if let Some(sources) = self.resolve_in_relations(parts, &scope.relations, column) {
            return sources;
        }
        for outer in self.outer_scopes.iter().rev() {
            if let Some(sources) = self.resolve_in_relations(parts, outer, column) {
                return sources;
            }
        }
        vec![passthrough(unresolved(column))]
    }

    /// Resolve `parts` against one set of relations, returning `None` when
    /// none of them could own the column (so the caller can fall through
    /// to an enclosing scope) and `Some(sources)` once at least one is a
    /// candidate — even if that resolves `Ambiguous` (a name owned within
    /// this scope doesn't escape it).
    fn resolve_in_relations(
        &self,
        parts: &[Ident],
        relations: &[Relation],
        column: &Ident,
    ) -> Option<Vec<ProvenanceSource>> {
        let candidates: Vec<Candidate> = if parts.len() == 1 {
            relations
                .iter()
                .filter_map(|rel| self.unqualified_candidate(rel, column))
                .collect()
        } else {
            let qualifier = &parts[parts.len() - 2];
            relations
                .iter()
                .filter(|rel| {
                    rel.exposed_name()
                        .is_some_and(|n| self.ident_eq(n, qualifier))
                })
                .filter_map(|rel| self.qualified_candidate(rel, column))
                .collect()
        };
        (!candidates.is_empty()).then(|| self.pick(candidates, column))
    }

    /// Reads contributed by GROUP BY (plain keys + ROLLUP / CUBE /
    /// GROUPING SETS members + a `GROUPING SETS` modifier), resolved
    /// against `scope` (which carries the output aliases).
    fn group_by_reads(&self, group_by: &GroupByExpr, scope: &Scope) -> Vec<ColumnRead> {
        let mut reads = Vec::new();
        if let GroupByExpr::Expressions(exprs, modifiers) = group_by {
            for expr in exprs {
                reads.extend(self.expr_reads(expr, scope));
            }
            for modifier in modifiers {
                if let GroupByWithModifier::GroupingSets(expr) = modifier {
                    reads.extend(self.expr_reads(expr, scope));
                }
            }
        }
        reads
    }

    /// Reads contributed by an ORDER BY, resolved against `scope`.
    fn order_by_reads(&self, order_by: &OrderBy, scope: &Scope) -> Vec<ColumnRead> {
        let OrderByKind::Expressions(exprs) = &order_by.kind else {
            return Vec::new();
        };
        let mut reads = Vec::new();
        for expr in exprs {
            reads.extend(self.expr_reads(&expr.expr, scope));
        }
        reads
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
                provenance: vec![passthrough(read(table, column, ResolutionKind::Cataloged))],
                confirmed: true,
                synthetic: false,
            }),
            RelationSource::Table {
                table,
                columns: RelationColumns::Open,
            } => Some(Candidate {
                provenance: vec![passthrough(read(table, column, ResolutionKind::Inferred))],
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
                    provenance: vec![passthrough(read(
                        table,
                        column,
                        if confirmed {
                            ResolutionKind::Cataloged
                        } else {
                            ResolutionKind::Inferred
                        },
                    ))],
                    confirmed,
                    synthetic: false,
                })
            }
            RelationSource::Table {
                table,
                columns: RelationColumns::Open,
            } => Some(Candidate {
                provenance: vec![passthrough(read(table, column, ResolutionKind::Inferred))],
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
    fn pick(&self, candidates: Vec<Candidate>, column: &Ident) -> Vec<ProvenanceSource> {
        if candidates.is_empty() {
            return vec![passthrough(unresolved(column))];
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
            _ => vec![passthrough(ambiguous(column))],
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

/// Combine source inputs under an optional filter: a lone input with no
/// reads passes through unwrapped; otherwise a `PassThrough` joins the
/// inputs and carries the filter `reads`. Used to build a DML statement's
/// scanned-and-filtered source below its SET / VALUES `Project`.
fn wrap_inputs(mut inputs: Vec<Plan>, reads: Vec<ColumnRead>) -> Plan {
    if reads.is_empty() && inputs.len() == 1 {
        inputs.pop().unwrap()
    } else {
        Plan::PassThrough(PassThrough { inputs, reads })
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

/// Merge two set-operation branches' output columns positionally: each
/// result column unions both branches' (kind-carrying) provenance and
/// takes its name from the left branch. Extra columns on either side
/// (mismatched arity) are dropped — a set operation requires equal arity,
/// and any dropped branch's reads still surface from its own sub-plan.
fn merge_set_outputs(left: Vec<BoundColumn>, right: Vec<BoundColumn>) -> Vec<BoundColumn> {
    left.into_iter()
        .zip(right)
        .map(|(left, right)| {
            let mut provenance = left.provenance;
            provenance.extend(right.provenance);
            BoundColumn {
                name: left.name,
                provenance,
            }
        })
        .collect()
}

/// Apply a CTE / derived table's explicit column list (`AS c(x, y)`),
/// renaming the body's output columns positionally. Surplus outputs keep
/// their inferred names; surplus alias names have nothing to bind to.
fn apply_column_aliases(outputs: &mut [BoundColumn], alias: &TableAlias) {
    for (output, column) in outputs.iter_mut().zip(&alias.columns) {
        output.name = Some(column.name.clone());
    }
}

/// The named output columns of a bound body — the target column list a
/// CTAS / CREATE VIEW pairs its source against when no explicit columns
/// are given. Anonymous (un-nameable) outputs are dropped.
fn output_names(outputs: &[BoundColumn]) -> Vec<Ident> {
    outputs.iter().filter_map(|c| c.name.clone()).collect()
}

/// The last (rightmost) identifier of a possibly-qualified name — a
/// write-target column's bare name.
fn object_name_last_ident(name: &ObjectName) -> Option<Ident> {
    name.0.last().and_then(|part| part.as_ident().cloned())
}

/// The column(s) an assignment writes: a single `col = …` or a tuple
/// `(a, b) = …`, each reduced to its bare name.
fn assignment_target_columns(target: &AssignmentTarget) -> Vec<Ident> {
    match target {
        AssignmentTarget::ColumnName(name) => object_name_last_ident(name).into_iter().collect(),
        AssignmentTarget::Tuple(names) => names.iter().filter_map(object_name_last_ident).collect(),
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

/// The lineage kind an expression contributes to its direct sources: a
/// bare column reference forwards its value (`Passthrough`); anything
/// else derives a new value (`Transformation`).
fn expr_kind(expr: &Expr) -> ColumnLineageKind {
    if is_single_column(expr) {
        ColumnLineageKind::Passthrough
    } else {
        ColumnLineageKind::Transformation
    }
}

/// Compose two lineage kinds along a chain: `Transformation` wins if
/// either step transforms (so a passthrough of a transformed value is a
/// transformation), else `Passthrough`.
fn combine_kind(inner: ColumnLineageKind, outer: ColumnLineageKind) -> ColumnLineageKind {
    if inner == ColumnLineageKind::Transformation || outer == ColumnLineageKind::Transformation {
        ColumnLineageKind::Transformation
    } else {
        ColumnLineageKind::Passthrough
    }
}

/// Wrap a read as a `Passthrough` provenance source — a base column or an
/// unresolved / ambiguous placeholder forwards its value by default; the
/// containing expression's kind folds in later.
fn passthrough(read: ColumnRead) -> ProvenanceSource {
    ProvenanceSource {
        read,
        kind: ColumnLineageKind::Passthrough,
    }
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
fn downgrade_to_inferred(provenance: Vec<ProvenanceSource>) -> Vec<ProvenanceSource> {
    provenance
        .into_iter()
        .map(|mut source| {
            source.read.resolution = ResolutionKind::Inferred;
            source
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

    fn passthrough_col(name: &str, reads: Vec<ColumnRead>) -> BoundColumn {
        BoundColumn {
            name: Some(Ident::new(name)),
            provenance: reads.into_iter().map(passthrough).collect(),
        }
    }

    fn transform_col(name: &str, reads: Vec<ColumnRead>) -> BoundColumn {
        BoundColumn {
            name: Some(Ident::new(name)),
            provenance: reads
                .into_iter()
                .map(|read| ProvenanceSource {
                    read,
                    kind: ColumnLineageKind::Transformation,
                })
                .collect(),
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
    fn cte_reference_resolves_to_inner_columns() {
        // A WITH-bound CTE referenced in FROM is a synthetic relation: the
        // body's `id` resolves through it to the real `t.id`, same as a
        // derived table. The CTE's plan is cloned in below the body Project.
        assert_eq!(
            bind_one("WITH c AS (SELECT id FROM t) SELECT id FROM c"),
            Plan::Project(Project {
                input: Box::new(Plan::Project(Project {
                    input: Box::new(scan("t")),
                    outputs: vec![passthrough_col("id", vec![inferred("t", "id")])],
                })),
                outputs: vec![passthrough_col("id", vec![inferred("t", "id")])],
            })
        );
    }

    #[test]
    fn chained_ctes_resolve_through_the_chain() {
        // `b`'s body reads CTE `a`, and the outer body reads `b`. B
        // resolves the outer `id` end-to-end to the real `t.id` — an
        // improvement over the resolver (whose flat scope yields
        // Ambiguous), so this is pinned here rather than in the
        // differential-parity corpus.
        let Plan::Project(project) =
            bind_one("WITH a AS (SELECT id FROM t), b AS (SELECT id FROM a) SELECT id FROM b")
        else {
            panic!("expected Project");
        };
        assert_eq!(
            project.outputs,
            vec![passthrough_col("id", vec![inferred("t", "id")])]
        );
    }

    #[test]
    fn subquery_in_where_folds_in_its_reads() {
        // `b IN (SELECT id FROM u)` contributes both the outer `b` and the
        // subquery's `u.id` as filter reads on the WHERE PassThrough.
        let Plan::Project(project) = bind_one("SELECT a FROM t WHERE b IN (SELECT id FROM u)")
        else {
            panic!("expected Project");
        };
        let Plan::PassThrough(where_pt) = project.input.as_ref() else {
            panic!("expected WHERE PassThrough");
        };
        assert_eq!(
            where_pt.reads,
            vec![inferred("t", "b"), inferred("u", "id")]
        );
    }

    #[test]
    fn correlated_subquery_resolves_outward() {
        // The inner `t.a` finds no `t` in the subquery's own scope `[u]`,
        // so it falls through the correlation stack to the outer `t`.
        let Plan::Project(project) =
            bind_one("SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.x = t.a)")
        else {
            panic!("expected Project");
        };
        let Plan::PassThrough(where_pt) = project.input.as_ref() else {
            panic!("expected WHERE PassThrough");
        };
        assert_eq!(where_pt.reads, vec![inferred("u", "x"), inferred("t", "a")]);
    }

    #[test]
    fn union_merges_branch_provenance() {
        // A derived table over `UNION` exposes one output `x` whose
        // provenance unions both branches' base columns.
        let Plan::Project(project) =
            bind_one("SELECT x FROM (SELECT a AS x FROM t UNION SELECT b AS x FROM u) d")
        else {
            panic!("expected Project");
        };
        assert_eq!(
            project.outputs,
            vec![passthrough_col(
                "x",
                vec![inferred("t", "a"), inferred("u", "b")]
            )]
        );
    }

    #[test]
    fn recursive_cte_self_reference_traces_the_anchor() {
        // The recursive branch's `FROM c` resolves to the anchor's `id`
        // (→ `t.id`); the body's `id` then unions both branches, both
        // tracing to the same real column.
        let Plan::Project(project) = bind_one(
            "WITH RECURSIVE c AS (SELECT id FROM t UNION ALL SELECT id FROM c) SELECT id FROM c",
        ) else {
            panic!("expected Project");
        };
        assert_eq!(
            project.outputs,
            vec![passthrough_col(
                "id",
                vec![inferred("t", "id"), inferred("t", "id")]
            )]
        );
    }

    #[test]
    fn insert_select_writes_target_over_source_reads() {
        // The source SELECT is the read-carrying input; the column list is
        // the write target. No reads come from the target.
        assert_eq!(
            bind_one("INSERT INTO target (a, b) SELECT x, y FROM source"),
            Plan::Write(Write {
                target: tref("target"),
                target_columns: vec![Ident::new("a"), Ident::new("b")],
                input: Box::new(Plan::Project(Project {
                    input: Box::new(scan("source")),
                    outputs: vec![
                        passthrough_col("x", vec![inferred("source", "x")]),
                        passthrough_col("y", vec![inferred("source", "y")]),
                    ],
                })),
            })
        );
    }

    #[test]
    fn update_reads_set_rhs_and_predicate() {
        // The SET assignment is a Project output named by its target `c`,
        // whose provenance (the transforming `a + b`) is the lineage
        // source; the WHERE predicate is a filter PassThrough below it.
        assert_eq!(
            bind_one("UPDATE t SET c = a + b WHERE d > 0"),
            Plan::Write(Write {
                target: tref("t"),
                target_columns: vec![Ident::new("c")],
                input: Box::new(Plan::Project(Project {
                    input: Box::new(Plan::PassThrough(PassThrough {
                        inputs: vec![scan("t")],
                        reads: vec![inferred("t", "d")],
                    })),
                    outputs: vec![transform_col(
                        "c",
                        vec![inferred("t", "a"), inferred("t", "b")]
                    )],
                })),
            })
        );
    }

    #[test]
    fn delete_reads_predicate_and_writes_no_columns() {
        // DELETE removes whole rows: the predicate is a read, but there
        // are no column writes (the target still surfaces for table-level).
        assert_eq!(
            bind_one("DELETE FROM t WHERE d > 0"),
            Plan::Write(Write {
                target: tref("t"),
                target_columns: vec![],
                input: Box::new(Plan::PassThrough(PassThrough {
                    inputs: vec![scan("t")],
                    reads: vec![inferred("t", "d")],
                })),
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
                provenance: vec![passthrough(ambiguous(&Ident::new("a")))],
            }]
        );
    }
}
