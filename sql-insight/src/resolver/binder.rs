//! The binder: lowers a `sqlparser` AST into the bound [`Plan`] IR,
//! resolving every column reference bottom-up.
//!
//! Resolution runs against a [`Scope`] threaded up through the bind (the
//! relations visible at the current node). The scope is bind-time
//! *scratch* — never stored on the [`Plan`], which keeps only resolved
//! provenance / reads. With a [`Catalog`] a relation's columns are
//! `Known` (resolution becomes strict — `Cataloged` hits, `Unresolved`
//! denials, narrowed candidates); catalog-free they are `Open` and
//! resolution is best-effort (`Inferred` / `Ambiguous`). Catalog matching
//! is right-anchored and dialect-cased (via [`crate::casing`]).

use sqlparser::ast::{
    AccessExpr, AlterTable, AlterTableOperation, Array, AssignmentTarget, ConnectByKind,
    CreateTable, CreateView, Cte, Delete, DictionaryField, Distinct, Expr, FromTable, Function,
    FunctionArg, FunctionArgExpr, FunctionArgumentClause, FunctionArgumentList, FunctionArguments,
    GroupByExpr, GroupByWithModifier, Ident, Insert, Join, JoinConstraint, JoinOperator,
    LimitClause, ListAggOnOverflow, Map, Merge, MergeAction, MergeInsertKind, NamedWindowExpr,
    ObjectName, ObjectType, OnConflictAction, OnInsert, OrderBy, OrderByExpr, OrderByKind,
    PipeOperator, Query, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Statement,
    Subscript, Table, TableAlias, TableFactor, TableWithJoins, TopQuantity, Update,
    UpdateTableFromKind, Values, WindowFrameBound, WindowSpec, WindowType,
};

use std::cell::RefCell;

use super::ir::{
    BoundColumn, CtePlan, CteRef, DeletePlan, PassThrough, Plan, Project, ProvenanceSource, Scan,
    ScanRole, SetOp, With, Write,
};
use crate::casing::{CaseFold, IdentifierCasing};
use crate::catalog::{Catalog, CatalogTable};
use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::extractor::ColumnLineageKind;
use crate::reference::{ColumnRead, ColumnReference, ResolutionKind, TableReference};
use sqlparser::tokenizer::Span;

/// Bind one statement into a [`Plan`] (or `None` for statement kinds not
/// modelled — queries and the data-moving DML / DDL are; other DDL and
/// session statements aren't), returning the column-level diagnostics the
/// bind accumulated (currently `WildcardSuppressed` for each suppressed
/// projection wildcard). The diagnostics buffer is shared across child
/// binders (CTE bodies, subqueries), so nested wildcards are reported too.
/// The top-level scope is discarded — callers consume the resolved tree.
pub(crate) fn build_with_diagnostics(
    statement: &Statement,
    catalog: Option<&Catalog>,
    casing: IdentifierCasing,
) -> (Option<Plan>, Vec<ColumnLevelDiagnostic>) {
    let diagnostics = RefCell::new(Vec::new());
    let binder = Binder {
        catalog,
        casing,
        ctes: Vec::new(),
        outer_scopes: Vec::new(),
        diagnostics: &diagnostics,
    };
    let plan = binder.bind_statement(statement);
    (plan, diagnostics.into_inner())
}

/// Bind-time resolution scope: the relations visible at a point in the
/// bind, plus (for the output-alias-visible clauses) the enclosing
/// SELECT's output columns. Scratch — never stored on the [`Plan`].
#[derive(Clone, Default)]
pub(crate) struct Scope {
    relations: Vec<Relation>,
    /// The enclosing SELECT's output columns, visible to its own
    /// GROUP BY / HAVING / ORDER BY (SQL alias visibility). Empty at
    /// FROM-level resolution (WHERE / projection / JOIN ON).
    outputs: Vec<BoundColumn>,
    /// `JOIN … USING (col)` merge columns in scope: an unqualified
    /// reference to one fans in to every relation that could own it
    /// (one source per side) instead of resolving ambiguously.
    merge_columns: Vec<Ident>,
}

impl Scope {
    fn empty() -> Self {
        Self {
            relations: Vec::new(),
            outputs: Vec::new(),
            merge_columns: Vec::new(),
        }
    }

    fn of(relation: Relation) -> Self {
        Self {
            relations: vec![relation],
            outputs: Vec::new(),
            merge_columns: Vec::new(),
        }
    }

    /// Concatenate the relations (and USING merge columns) of two scopes
    /// (a join / comma). Output columns aren't merged — they belong to a
    /// single SELECT.
    fn merge(mut self, mut other: Scope) -> Scope {
        self.relations.append(&mut other.relations);
        self.merge_columns.append(&mut other.merge_columns);
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
            RelationSource::Derived { .. } | RelationSource::TableFunction => None,
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
    /// A table function / `UNNEST` / `PIVOT` / `JSON_TABLE` output: a
    /// synthetic relation whose columns are opaque (dynamically produced).
    /// A reference qualified by its alias resolves *to it* but contributes
    /// nothing (the produced columns aren't stored), so it neither surfaces
    /// as a read nor as a lineage source — its inputs are read separately
    /// from the function's argument expressions.
    TableFunction,
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
/// once at its `WITH` declaration. This is the bind-time *resolution*
/// entry — just the name and the exposed `outputs` (already
/// collapsed like a derived table). The body sub-plan itself lives on the
/// [`With`] node (so it is walked once regardless of reference count); a
/// FROM reference resolves through these `outputs` and emits a lightweight
/// [`CteRef`], never a clone of the body.
#[derive(Clone)]
struct CteRelation {
    name: Ident,
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
    /// Column-level diagnostics accumulated during the bind, shared across
    /// child binders (CTE bodies / subqueries) so nested ones surface too.
    diagnostics: &'a RefCell<Vec<ColumnLevelDiagnostic>>,
}

/// Accumulator for walking one expression. References split by position:
/// `sources` are value references (they flow to the output → lineage),
/// `filter_reads` are predicate references (they only influence which rows
/// / values are produced → reads but not lineage). Sub-plans of nested
/// subqueries split the same way: `value_subplans` sit in value position
/// (their output feeds the enclosing value → lineage), `filter_subplans`
/// sit in a predicate (`EXISTS` / `IN` / a `CASE` condition → reads only).
/// `is_suppressed` marks the current position as a filter, mirroring the
/// resolver's `suppress_lineage`, and routes a subquery to the right list.
#[derive(Default)]
struct ExprCollector {
    sources: Vec<ProvenanceSource>,
    filter_reads: Vec<ColumnRead>,
    value_subplans: Vec<Plan>,
    filter_subplans: Vec<Plan>,
    is_suppressed: bool,
}

/// One value expression bound into an output column, with its
/// position-split side effects. `value_subplans` feed lineage (a scalar
/// subquery whose result flows to the column), while `filter_reads` /
/// `filter_subplans` are predicate-only (a `CASE` condition, an `EXISTS`
/// test) — reads that don't feed, destined for a non-feeding position.
struct BoundValue {
    column: BoundColumn,
    value_subplans: Vec<Plan>,
    filter_reads: Vec<ColumnRead>,
    filter_subplans: Vec<Plan>,
}

impl ExprCollector {
    /// A value position (a projection item / SET / VALUES RHS): references
    /// flow as lineage sources unless a sub-expression suppresses them.
    fn value() -> Self {
        Self::default()
    }

    /// A filter position (WHERE / ON / clause predicate / DML predicate):
    /// the whole expression is a predicate, so every reference is a read.
    fn filter() -> Self {
        Self {
            is_suppressed: true,
            ..Self::default()
        }
    }

    /// Run `f` with the position forced to a filter (a predicate
    /// sub-expression — a `CASE` condition, an `EXISTS` test, a sort /
    /// partition key), restoring the prior position afterward.
    fn suppressed(&mut self, f: impl FnOnce(&mut Self)) {
        let prev = self.is_suppressed;
        self.is_suppressed = true;
        f(self);
        self.is_suppressed = prev;
    }

    /// Drain a filter-context collector (WHERE / ON / clause / arg / pipe):
    /// its reads plus *all* its sub-plans. In a filter position no sub-plan
    /// feeds lineage, so value- and filter-position sub-plans merge into one
    /// non-feeding list (a filter collector never collects value sub-plans
    /// anyway, since `is_suppressed` is never cleared).
    fn into_filter_parts(self) -> (Vec<ColumnRead>, Vec<Plan>) {
        let mut subplans = self.value_subplans;
        subplans.extend(self.filter_subplans);
        (self.filter_reads, subplans)
    }
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
            Statement::AlterView {
                name,
                columns,
                query,
                ..
            } => self.bind_alter_view(name, columns, query),
            Statement::AlterTable(alter) => self.bind_alter_table(alter),
            Statement::Drop {
                object_type,
                names,
                table,
                ..
            } => self.bind_drop(object_type, names, table.as_ref()),
            Statement::Truncate(truncate) => Some(Plan::Drop(
                truncate
                    .table_names
                    .iter()
                    .filter_map(|t| TableReference::try_from(&t.name).ok())
                    .map(|t| self.canonical_target(t))
                    .collect(),
            )),
            // `CREATE VIRTUAL TABLE t USING module(…)`: the new table is a
            // write target with no inspectable source (classified
            // `CreateTable`, like a plain `CREATE TABLE`).
            Statement::CreateVirtualTable { name, .. } => {
                let target = self.canonical_target(TableReference::try_from_name(name).ok()?);
                Some(Plan::Write(Write {
                    target,
                    target_columns: Vec::new(),
                    input: Box::new(Plan::OpaqueLeaf),
                    returning: Vec::new(),
                    conflict_updates: Vec::new(),
                }))
            }
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
        let target = self.canonical_target(target);
        let (input, source_scope) = match &insert.source {
            Some(source) => self.bind_query(source),
            // MySQL `INSERT INTO t SET a = expr, …`: no VALUES / SELECT
            // source; the assignment right-hand sides are reads against the
            // target, folded onto the input so their tables surface.
            None if !insert.assignments.is_empty() => {
                let scope = self.target_scope(&target);
                let mut reads = Vec::new();
                let mut subplans = Vec::new();
                for assignment in &insert.assignments {
                    let (r, s) = self.expr_reads(&assignment.value, &scope);
                    reads.extend(r);
                    subplans.extend(s);
                }
                (
                    wrap_reads(Plan::OpaqueLeaf, reads, subplans),
                    Scope::empty(),
                )
            }
            None => (Plan::OpaqueLeaf, Scope::empty()),
        };
        // An explicit column list wins; otherwise pair the source against
        // the target's catalog schema, up to the source's arity (so a
        // column-less `INSERT INTO t SELECT a, b` writes / traces `t`'s
        // first two columns). No catalog → no inferred columns.
        let target_columns = if insert.columns.is_empty() {
            self.catalog_columns(&target)
                .into_iter()
                .take(source_scope.outputs.len())
                .collect()
        } else {
            insert.columns.clone()
        };
        // ON CONFLICT DO UPDATE / ON DUPLICATE KEY UPDATE: a conflict-time
        // mini-UPDATE on the target. Its reads / sub-plans fold onto the
        // input; its assignments drive extra writes + lineage. EXCLUDED
        // collapses to the source only for a query source — a VALUES source
        // exposes no projection, so EXCLUDED stays opaque (a synthetic
        // self-reference), matching the resolver.
        let excluded_source = if source_has_projection(insert) {
            source_scope.clone()
        } else {
            Scope::empty()
        };
        let (conflict_updates, conflict_reads, conflict_subplans) = match &insert.on {
            Some(on) => self.bind_conflict(on, &target, &target_columns, &excluded_source),
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let input = wrap_reads(input, conflict_reads, conflict_subplans);
        // RETURNING references resolve against the target alone (the source
        // query's scope is already popped).
        let returning = self.bind_returning(&insert.returning, &self.target_scope(&target));
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(input),
            returning,
            conflict_updates,
        }))
    }

    /// `UPDATE target SET col = expr [FROM src] WHERE pred`: the target
    /// (write) plus any FROM relations (read) form the scope. The SET
    /// assignments become a `Project` whose outputs are named by their
    /// target columns (so the lineage `RHS → target` pairing falls out of
    /// the same output-column machinery as INSERT / CTAS); the WHERE
    /// predicate is a filter `PassThrough` below it.
    fn bind_update(&self, update: &Update) -> Option<Plan> {
        let (target_plan, mut scope) = self.bind_table_with_joins(&update.table, &Scope::empty());
        // The UPDATE target is in scope for resolving SET / WHERE, but it's
        // a write (reported via `Write.target`), not a table read.
        let mut inputs = vec![into_write_target(target_plan)];
        if let Some(from) = &update.from {
            let tables = match from {
                UpdateTableFromKind::BeforeSet(tables) | UpdateTableFromKind::AfterSet(tables) => {
                    tables
                }
            };
            for twj in tables {
                let (plan, from_scope) = self.bind_table_with_joins(twj, &scope);
                inputs.push(plan);
                scope = scope.merge(from_scope);
            }
        }
        // The WHERE predicate's reads / sub-plans are filter position (they
        // pick which rows update, they don't feed the new value); only a SET
        // RHS value sub-plan feeds the target.
        let (mut reads, mut filter_subqueries) = update
            .selection
            .as_ref()
            .map(|s| self.expr_reads(s, &scope))
            .unwrap_or_default();
        let mut value_subqueries = Vec::new();
        let mut outputs = Vec::new();
        let mut target_columns = Vec::new();
        for assignment in &update.assignments {
            for column in assignment_target_columns(&assignment.target) {
                let bound = self.bind_value_column(Some(column.clone()), &assignment.value, &scope);
                outputs.push(bound.column);
                value_subqueries.extend(bound.value_subplans);
                reads.extend(bound.filter_reads);
                filter_subqueries.extend(bound.filter_subplans);
                target_columns.push(column);
            }
        }
        let target = self.canonical_target(TableReference::try_from(&update.table.relation).ok()?);
        // RETURNING resolves against the statement scope (target + FROM).
        let returning = self.bind_returning(&update.returning, &scope);
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(Plan::Project(Project {
                input: Box::new(wrap_inputs(inputs, reads, filter_subqueries)),
                outputs,
                subqueries: value_subqueries,
            })),
            returning,
            conflict_updates: Vec::new(),
        }))
    }

    /// `DELETE`: bind every consulted relation as a scan with the right
    /// role and collect the deletion targets. The FROM clause's role
    /// depends on the statement shape (mirroring the resolver):
    ///   `DELETE FROM t`                → FROM is the (write) target
    ///   `DELETE FROM t1, t2 USING src` → FROM are targets, USING are reads
    ///   `DELETE t1, t2 FROM src`       → FROM are reads, the list are targets
    /// An explicit `DELETE alias FROM …` list resolves each name (possibly
    /// a FROM alias) to its real table through the FROM scope. There are no
    /// column writes / lineage — rows go wholesale; only `RETURNING`
    /// projects the deleted rows.
    fn bind_delete(&self, delete: &Delete) -> Option<Plan> {
        let from_tables = match &delete.from {
            FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
        };
        // With no explicit `DELETE t, …` list the FROM relations are the
        // deletion targets (write role: in scope for the predicate /
        // RETURNING but not a read); with a list the FROM relations are all
        // reads and the list names the targets.
        let from_is_target = delete.tables.is_empty();
        let mut inputs = Vec::new();
        let mut scope = Scope::empty();
        let mut targets = Vec::new();
        // USING relations are always reads. Bind them first so an explicit
        // target alias can resolve against them too (alias-defining clause).
        for twj in delete.using.iter().flatten() {
            let (plan, twj_scope) = self.bind_table_with_joins(twj, &scope);
            inputs.push(plan);
            scope = scope.merge(twj_scope);
        }
        for twj in from_tables {
            if from_is_target {
                // The FROM relations are the deletion targets. A target may
                // be an alias into a USING relation already bound above
                // (`DELETE FROM t_alias USING real AS t_alias`) — or just the
                // same name as a USING relation — so resolve it through the
                // scope and don't re-bind. Otherwise it's a fresh target
                // table, bound write-role (in scope for the predicate /
                // RETURNING, but not a read).
                let resolved = TableReference::try_from(&twj.relation)
                    .ok()
                    .and_then(|written| self.scope_target(&written, &scope));
                if let Some(target) = resolved {
                    targets.push(target);
                } else {
                    let (plan, twj_scope) = self.bind_table_with_joins(twj, &scope);
                    targets.extend(self.twj_table_targets(twj));
                    inputs.push(into_write_target(plan));
                    scope = scope.merge(twj_scope);
                }
            } else {
                let (plan, twj_scope) = self.bind_table_with_joins(twj, &scope);
                inputs.push(plan);
                scope = scope.merge(twj_scope);
            }
        }
        for name in &delete.tables {
            if let Some(target) = self.resolve_delete_target(name, &scope) {
                targets.push(target);
            }
        }
        let (reads, subqueries) = delete
            .selection
            .as_ref()
            .map(|s| self.expr_reads(s, &scope))
            .unwrap_or_default();
        // RETURNING resolves against the FROM / USING scope (which holds the
        // target).
        let returning = self.bind_returning(&delete.returning, &scope);
        let input = wrap_inputs(inputs, reads, subqueries);
        Some(Plan::Delete(DeletePlan {
            input: Box::new(input),
            targets,
            returning,
        }))
    }

    /// The plain-table targets of a FROM `TableWithJoins` (its relation plus
    /// any joined relations), catalog-canonicalized. Used when the FROM
    /// clause *is* the deletion target (`DELETE FROM t1, t2 …`). Derived /
    /// table-function factors yield no target.
    fn twj_table_targets(&self, twj: &TableWithJoins) -> Vec<TableReference> {
        std::iter::once(&twj.relation)
            .chain(twj.joins.iter().map(|join| &join.relation))
            .filter_map(|factor| TableReference::try_from(factor).ok())
            .map(|target| self.canonical_target(target))
            .collect()
    }

    /// Resolve an explicit `DELETE` target name to its real table: a
    /// single-segment name may be a FROM alias (or the bare name of an
    /// in-scope relation), so consult the scope first; otherwise
    /// canonicalize it as written.
    fn resolve_delete_target(&self, name: &ObjectName, scope: &Scope) -> Option<TableReference> {
        let written = TableReference::try_from_name(name).ok()?;
        Some(
            self.scope_target(&written, scope)
                .unwrap_or_else(|| self.canonical_target(written)),
        )
    }

    /// If a `written` DELETE-target name matches an in-scope real-table
    /// relation by **merge identity**, return that relation's real table.
    /// An aliased relation matches a single-segment name against its alias;
    /// a non-aliased relation matches its full `catalog.schema.name` path
    /// exactly — so a bare `t1` merges with FROM `t1` but **not** with FROM
    /// `mydb.t1` (we assume no default schema), matching the resolver.
    fn scope_target(&self, written: &TableReference, scope: &Scope) -> Option<TableReference> {
        let canonical = self.canonical_target(written.clone());
        scope
            .relations
            .iter()
            .find_map(|relation| match &relation.source {
                RelationSource::Table { table, .. } => {
                    let matches = match &relation.alias {
                        Some(alias) => {
                            written.schema.is_none()
                                && written.catalog.is_none()
                                && self.ident_eq(alias, &written.name)
                        }
                        None => self.table_identity_eq(&canonical, table),
                    };
                    matches.then(|| table.clone())
                }
                _ => None,
            })
    }

    /// Exact (not right-anchored) identity match of two table references
    /// under the dialect's table casing — every present segment must agree
    /// and a missing segment matches only a missing one.
    fn table_identity_eq(&self, a: &TableReference, b: &TableReference) -> bool {
        let fold = self.casing.table;
        let seg_eq = |x: Option<&Ident>, y: Option<&Ident>| match (x, y) {
            (Some(p), Some(q)) => fold.normalize(p) == fold.normalize(q),
            (None, None) => true,
            _ => false,
        };
        fold.normalize(&a.name) == fold.normalize(&b.name)
            && seg_eq(a.schema.as_ref(), b.schema.as_ref())
            && seg_eq(a.catalog.as_ref(), b.catalog.as_ref())
    }

    /// `MERGE INTO target USING source ON pred WHEN … THEN …`: target
    /// (write) and source (read) form the scope. ON and the per-clause
    /// predicates are filter reads; each WHEN action's value expressions
    /// become `Project` outputs named by their written column (UPDATE SET
    /// target / INSERT column), so the `value → target` lineage pairing
    /// reuses the output-column machinery.
    fn bind_merge(&self, merge: &Merge) -> Option<Plan> {
        let (target_plan, target_scope) = self.bind_table_factor(&merge.table, &Scope::empty());
        let (source_plan, source_scope) =
            self.bind_table_factor(&merge.source, &target_scope.clone());
        let scope = target_scope.merge(source_scope);
        // ON / WHEN predicates are filter position (non-feeding); only a WHEN
        // action's value expression feeds the target via a `Project` output.
        let (mut reads, mut filter_subqueries) = self.expr_reads(&merge.on, &scope);
        let mut value_subqueries = Vec::new();
        let mut outputs = Vec::new();
        let mut target_columns = Vec::new();
        for clause in &merge.clauses {
            if let Some(predicate) = &clause.predicate {
                let (r, s) = self.expr_reads(predicate, &scope);
                reads.extend(r);
                filter_subqueries.extend(s);
            }
            match &clause.action {
                MergeAction::Insert(insert) => {
                    if let Some(predicate) = &insert.insert_predicate {
                        let (r, s) = self.expr_reads(predicate, &scope);
                        reads.extend(r);
                        filter_subqueries.extend(s);
                    }
                    if let MergeInsertKind::Values(values) = &insert.kind {
                        let columns: Vec<Ident> = insert
                            .columns
                            .iter()
                            .filter_map(object_name_last_ident)
                            .collect();
                        if columns.is_empty() {
                            // No explicit column list: the inserted values
                            // are still reads. (Pairing them with the
                            // catalog schema for writes / lineage is a later
                            // brick — the resolver leaves those empty too.)
                            for expr in values.rows.iter().flatten() {
                                let (r, s) = self.expr_reads(expr, &scope);
                                reads.extend(r);
                                filter_subqueries.extend(s);
                            }
                        } else {
                            for row in &values.rows {
                                for (column, expr) in columns.iter().zip(row) {
                                    let bound =
                                        self.bind_value_column(Some(column.clone()), expr, &scope);
                                    outputs.push(bound.column);
                                    value_subqueries.extend(bound.value_subplans);
                                    reads.extend(bound.filter_reads);
                                    filter_subqueries.extend(bound.filter_subplans);
                                    target_columns.push(column.clone());
                                }
                            }
                        }
                    }
                }
                MergeAction::Update(update) => {
                    for assignment in &update.assignments {
                        for column in assignment_target_columns(&assignment.target) {
                            let bound = self.bind_value_column(
                                Some(column.clone()),
                                &assignment.value,
                                &scope,
                            );
                            outputs.push(bound.column);
                            value_subqueries.extend(bound.value_subplans);
                            reads.extend(bound.filter_reads);
                            filter_subqueries.extend(bound.filter_subplans);
                            target_columns.push(column);
                        }
                    }
                    for predicate in [&update.update_predicate, &update.delete_predicate]
                        .into_iter()
                        .flatten()
                    {
                        let (r, s) = self.expr_reads(predicate, &scope);
                        reads.extend(r);
                        filter_subqueries.extend(s);
                    }
                }
                // DELETE moves no column values.
                MergeAction::Delete { .. } => {}
            }
        }
        let target = self.canonical_target(TableReference::try_from(&merge.table).ok()?);
        // The MERGE target is in scope for ON / WHEN resolution but is a
        // write, not a read; the source relation is a read. Predicate
        // sub-plans ride the non-feeding PassThrough.
        let source = wrap_inputs(
            vec![into_write_target(target_plan), source_plan],
            reads,
            filter_subqueries,
        );
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(Plan::Project(Project {
                input: Box::new(source),
                outputs,
                subqueries: value_subqueries,
            })),
            returning: Vec::new(),
            conflict_updates: Vec::new(),
        }))
    }

    /// `CREATE TABLE dst AS <query>` (CTAS): the source query's reads,
    /// paired with the new table's columns (explicit defs win, else the
    /// source output names). A plain `CREATE TABLE t (cols)` (no query) is a
    /// write target with no columns / reads / lineage — its column
    /// definitions aren't writes — so it binds to a target-only `Write`.
    fn bind_create_table(&self, create: &CreateTable) -> Option<Plan> {
        let target = self.canonical_target(TableReference::try_from(&create.name).ok()?);
        let Some(query) = create.query.as_ref() else {
            return Some(Plan::Write(Write {
                target,
                target_columns: Vec::new(),
                input: Box::new(Plan::OpaqueLeaf),
                returning: Vec::new(),
                conflict_updates: Vec::new(),
            }));
        };
        let (input, scope) = self.bind_query(query);
        let target_columns = if create.columns.is_empty() {
            output_names(&scope.outputs)
        } else {
            create.columns.iter().map(|c| c.name.clone()).collect()
        };
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(input),
            returning: Vec::new(),
            conflict_updates: Vec::new(),
        }))
    }

    /// `CREATE VIEW v AS <query>`: like CTAS — the query's reads paired
    /// with the view's columns (explicit list wins, else source names).
    fn bind_create_view(&self, create: &CreateView) -> Option<Plan> {
        let (input, scope) = self.bind_query(&create.query);
        let target = self.canonical_target(TableReference::try_from(&create.name).ok()?);
        let target_columns = if create.columns.is_empty() {
            output_names(&scope.outputs)
        } else {
            create.columns.iter().map(|c| c.name.clone()).collect()
        };
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(input),
            returning: Vec::new(),
            conflict_updates: Vec::new(),
        }))
    }

    /// `ALTER VIEW v AS <query>`: treated like CREATE VIEW — the
    /// replacement query's reads paired with the view's columns (explicit
    /// list wins, else the source output names).
    fn bind_alter_view(&self, name: &ObjectName, columns: &[Ident], query: &Query) -> Option<Plan> {
        let (input, scope) = self.bind_query(query);
        let target = self.canonical_target(TableReference::try_from(name).ok()?);
        let target_columns = if columns.is_empty() {
            output_names(&scope.outputs)
        } else {
            columns.to_vec()
        };
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(input),
            returning: Vec::new(),
            conflict_updates: Vec::new(),
        }))
    }

    /// `ALTER TABLE t <ops>`: the altered table is a write target; each
    /// column-naming operation contributes its column(s) as writes (RENAME
    /// / CHANGE surface both the old and new names). Schema-level ops
    /// (constraints, partitions, RENAME TABLE) name no columns. No reads or
    /// lineage — ALTER restructures, it doesn't move row data.
    fn bind_alter_table(&self, alter: &AlterTable) -> Option<Plan> {
        let target = self.canonical_target(TableReference::try_from(&alter.name).ok()?);
        let target_columns = alter
            .operations
            .iter()
            .flat_map(alter_table_op_target_columns)
            .collect();
        Some(Plan::Write(Write {
            target,
            target_columns,
            input: Box::new(Plan::OpaqueLeaf),
            returning: Vec::new(),
            conflict_updates: Vec::new(),
        }))
    }

    /// `DROP TABLE/VIEW/MATERIALIZED VIEW a, b`: the dropped relations are
    /// write targets. Other object types (index / schema / …) name no
    /// relations and classify as unsupported, so they don't reach here.
    fn bind_drop(
        &self,
        object_type: &ObjectType,
        names: &[ObjectName],
        table: Option<&ObjectName>,
    ) -> Option<Plan> {
        if !matches!(
            object_type,
            ObjectType::Table | ObjectType::View | ObjectType::MaterializedView
        ) {
            return None;
        }
        let targets = names
            .iter()
            .chain(table)
            .filter_map(|name| TableReference::try_from(name).ok())
            .map(|target| self.canonical_target(target))
            .collect();
        Some(Plan::Drop(targets))
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
        // The bodies declared by *this* clause, kept to hang on the `With`
        // node so each is walked exactly once (inherited outer CTEs hang on
        // their own outer `With`, so they aren't re-attached here).
        let mut declared = Vec::new();
        for cte in &with.cte_tables {
            let (relation, plan) = if with.recursive {
                self.bind_recursive_cte(cte, &env)
            } else {
                let (plan, scope) = self.with_ctes(env.clone()).bind_query(&cte.query);
                let mut outputs = scope.outputs;
                apply_column_aliases(&mut outputs, &cte.alias);
                (
                    CteRelation {
                        name: cte.alias.name.clone(),
                        outputs,
                    },
                    plan,
                )
            };
            declared.push(CtePlan {
                name: relation.name.clone(),
                plan,
            });
            env.push(relation);
        }
        let (body, scope) = self.with_ctes(env).bind_query_body(query);
        (
            Plan::With(With {
                ctes: declared,
                body: Box::new(body),
            }),
            scope,
        )
    }

    /// Bind a `RECURSIVE` CTE: bind the anchor (the body set-operation's
    /// non-recursive left branch) to learn the output shape, register the
    /// CTE name with those columns so the recursive branch's
    /// self-reference resolves, then bind the full body. Self-reference
    /// resolution sees the anchor's columns (shallow — matching the
    /// resolver's deferred recursive collapse). A `RECURSIVE` CTE whose
    /// body isn't a set operation degenerates to a plain CTE.
    fn bind_recursive_cte(&self, cte: &Cte, env: &[CteRelation]) -> (CteRelation, Plan) {
        let name = cte.alias.name.clone();
        let SetExpr::SetOperation { left, .. } = cte.query.body.as_ref() else {
            // A `RECURSIVE` CTE whose body isn't a set operation has no
            // separate anchor to learn columns from, but its name is still
            // in scope inside its own body — register it (with no known
            // columns) so a self-reference resolves to a `CteRef`, not a
            // phantom real table read.
            let mut provisional = env.to_vec();
            provisional.push(CteRelation {
                name: name.clone(),
                outputs: Vec::new(),
            });
            let (plan, scope) = self.with_ctes(provisional).bind_query(&cte.query);
            let mut outputs = scope.outputs;
            apply_column_aliases(&mut outputs, &cte.alias);
            return (CteRelation { name, outputs }, plan);
        };
        let mut anchor_outputs = self.with_ctes(env.to_vec()).bind_set_expr(left).1.outputs;
        apply_column_aliases(&mut anchor_outputs, &cte.alias);
        // Provisional registration: the recursive branch sees the CTE name
        // resolving to the anchor's columns (resolution consults only the
        // outputs; the self-reference binds to a `CteRef`, never a body).
        let mut provisional = env.to_vec();
        provisional.push(CteRelation {
            name: name.clone(),
            outputs: anchor_outputs,
        });
        let (plan, scope) = self.with_ctes(provisional).bind_query(&cte.query);
        let mut outputs = scope.outputs;
        apply_column_aliases(&mut outputs, &cte.alias);
        (CteRelation { name, outputs }, plan)
    }

    /// A child binder sharing this one's catalog / casing, with the given
    /// CTE environment and correlation stack.
    fn child(&self, ctes: Vec<CteRelation>, outer_scopes: Vec<Vec<Relation>>) -> Binder<'_> {
        Binder {
            catalog: self.catalog,
            casing: self.casing,
            ctes,
            outer_scopes,
            diagnostics: self.diagnostics,
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

    /// Bind a query's body and its trailing ORDER BY / LIMIT (the WITH
    /// clause is already in scope via `self.ctes`).
    fn bind_query_body(&self, query: &Query) -> (Plan, Scope) {
        let (body, mut scope) = self.bind_set_expr(&query.body);
        // Pipe operators (`|> WHERE`, `|> SELECT`, …) transform the body in
        // sequence. Fold the chain on top of `node`, evolving the output
        // scope: an output-producing operator (SELECT / EXTEND / AGGREGATE
        // / SET) layers a `Project` so its value expressions feed
        // `QueryOutput` lineage, while a filter operator (WHERE / ORDER BY
        // / LIMIT / JOIN / …) adds reads. Pipe expressions resolve against
        // the body's relations plus the running outputs (relations stay in
        // scope across the chain — loose pipe scoping).
        let mut node = body;
        if !query.pipe_operators.is_empty() {
            let mut pipe_scope = match &node {
                // A set-op body exposes no single relation scope for refs.
                Plan::SetOp(_) => Scope {
                    relations: Vec::new(),
                    outputs: scope.outputs.clone(),
                    merge_columns: Vec::new(),
                },
                _ => scope.clone(),
            };
            for op in &query.pipe_operators {
                node = self.bind_pipe_operator(op, node, &mut pipe_scope);
            }
            scope = pipe_scope;
        }
        // A trailing ORDER BY / LIMIT sits above the (possibly piped) body
        // and sees its output aliases — but only for a single relation.
        // Over a set operation there's no single relation to resolve
        // against, so a reference is unresolved (an empty scope makes that
        // fall out).
        let clause_scope = match &node {
            Plan::SetOp(_) => Scope::empty(),
            _ => scope.clone(),
        };
        let mut reads = Vec::new();
        let mut subqueries = Vec::new();
        if let Some(order_by) = &query.order_by {
            let (r, s) = self.order_by_reads(order_by, &clause_scope);
            reads.extend(r);
            subqueries.extend(s);
        }
        // LIMIT / OFFSET / LIMIT BY are row-count bounds — filter reads.
        if let Some(limit) = &query.limit_clause {
            let (r, s) = self.limit_reads(limit, &clause_scope);
            reads.extend(r);
            subqueries.extend(s);
        }
        // ClickHouse `SETTINGS key = expr`: a value may hold a subquery.
        if let Some(settings) = &query.settings {
            let mut c = ExprCollector::filter();
            for setting in settings {
                self.collect_expr(&setting.value, &clause_scope, &mut c);
            }
            let (r, s) = c.into_filter_parts();
            reads.extend(r);
            subqueries.extend(s);
        }
        (wrap_reads(node, reads, subqueries), scope)
    }

    /// Bind one pipe operator (`|> …`) on top of `input`, returning the new
    /// plan node and updating `scope.outputs` when the operator reshapes the
    /// output. An **output-producing** operator (`SELECT` / `EXTEND` /
    /// `AGGREGATE` / `SET`) layers a [`Project`] whose value expressions feed
    /// `QueryOutput` lineage; a **filter** operator (`WHERE` / `ORDER BY` /
    /// `LIMIT` / `CALL` / `JOIN` / …) wraps a non-feeding read `PassThrough`.
    /// The match is exhaustive so new pipe operators are reviewed here.
    fn bind_pipe_operator(&self, op: &PipeOperator, input: Plan, scope: &mut Scope) -> Plan {
        match op {
            // `|> SELECT exprs`: replace the output with these columns.
            PipeOperator::Select { exprs } => {
                let bound = exprs
                    .iter()
                    .filter_map(|i| self.bind_output_column(i, scope));
                let (node, outputs) = self.pipe_project(input, Vec::new(), bound);
                scope.outputs = outputs;
                node
            }
            // `|> EXTEND exprs`: append columns to the running output.
            PipeOperator::Extend { exprs } => {
                let base = scope.outputs.clone();
                let bound = exprs
                    .iter()
                    .filter_map(|i| self.bind_output_column(i, scope));
                let (node, outputs) = self.pipe_project(input, base, bound);
                scope.outputs = outputs;
                node
            }
            // `|> SET col = expr`: each assignment is a value column named by
            // its target, replacing a same-named output (or appended).
            PipeOperator::Set { assignments } => {
                let bound: Vec<BoundValue> = assignments
                    .iter()
                    .flat_map(|a| {
                        assignment_target_columns(&a.target)
                            .into_iter()
                            .map(move |col| (col, &a.value))
                    })
                    .map(|(col, value)| self.bind_value_column(Some(col), value, scope))
                    .collect();
                let (node, outputs) = self.pipe_set_project(input, scope.outputs.clone(), bound);
                scope.outputs = outputs;
                node
            }
            // `|> AGGREGATE aggs GROUP BY keys`: the output is the aggregate
            // expressions plus the grouping keys (both value position).
            PipeOperator::Aggregate {
                full_table_exprs,
                group_by_expr,
            } => {
                let bound = full_table_exprs.iter().chain(group_by_expr).map(|e| {
                    let name = e
                        .expr
                        .alias
                        .clone()
                        .or_else(|| inferred_output_name(&e.expr.expr));
                    self.bind_value_column(name, &e.expr.expr, scope)
                });
                let (node, outputs) = self.pipe_project(input, Vec::new(), bound);
                scope.outputs = outputs;
                node
            }
            // Filter operators below: reads only, output unchanged.
            PipeOperator::Limit { expr, offset } => self.pipe_filter(input, |c| {
                self.collect_expr(expr, scope, c);
                if let Some(offset) = offset {
                    self.collect_expr(offset, scope, c);
                }
            }),
            PipeOperator::Where { expr } => {
                self.pipe_filter(input, |c| self.collect_expr(expr, scope, c))
            }
            PipeOperator::OrderBy { exprs } => self.pipe_filter(input, |c| {
                for order_by in exprs {
                    self.collect_order_by_expr(order_by, scope, c);
                }
            }),
            PipeOperator::Call { function, .. } => {
                self.pipe_filter(input, |c| self.collect_function(function, scope, c))
            }
            PipeOperator::Pivot {
                aggregate_functions,
                value_source,
                ..
            } => self.pipe_filter(input, |c| {
                for expr in aggregate_functions {
                    self.collect_expr(&expr.expr, scope, c);
                }
                self.collect_pivot_value_source(value_source, scope, c);
            }),
            PipeOperator::Union { queries, .. }
            | PipeOperator::Intersect { queries, .. }
            | PipeOperator::Except { queries, .. } => self.pipe_filter(input, |c| {
                for query in queries {
                    self.collect_subquery(query, scope, c);
                }
            }),
            // `|> JOIN t ON …` introduces another relation: bind it as a
            // read scan (a non-feeding sub-plan so its table surfaces) and
            // collect the ON predicate's reads.
            PipeOperator::Join(join) => self.pipe_filter(input, |c| {
                let (plan, _) = self.bind_table_factor(&join.relation, scope);
                c.filter_subplans.push(plan);
                if let Some(JoinConstraint::On(expr)) = join_constraint(join) {
                    self.collect_expr(expr, scope, c);
                }
            }),
            // No inspectable column expressions (or a later refinement):
            // a sampling clause, a rename / drop / unpivot.
            PipeOperator::TableSample { .. }
            | PipeOperator::Drop { .. }
            | PipeOperator::As { .. }
            | PipeOperator::Rename { .. }
            | PipeOperator::Unpivot { .. } => input,
        }
    }

    /// Wrap `input` in a non-feeding read `PassThrough` carrying whatever a
    /// filter pipe operator collected (reads + predicate sub-plans).
    fn pipe_filter(&self, input: Plan, collect: impl FnOnce(&mut ExprCollector)) -> Plan {
        let mut c = ExprCollector::filter();
        collect(&mut c);
        let (reads, subplans) = c.into_filter_parts();
        wrap_reads(input, reads, subplans)
    }

    /// Build the [`Project`] for an output-producing pipe operator: append
    /// the `bound` value columns to `base` (empty when the operator replaces
    /// the output, the running outputs when it extends them). Value
    /// sub-plans feed lineage; filter reads / sub-plans ride a non-feeding
    /// `PassThrough` below the projection.
    fn pipe_project(
        &self,
        input: Plan,
        mut outputs: Vec<BoundColumn>,
        bound: impl Iterator<Item = BoundValue>,
    ) -> (Plan, Vec<BoundColumn>) {
        let mut value_subqueries = Vec::new();
        let mut filter_reads = Vec::new();
        let mut filter_subqueries = Vec::new();
        for b in bound {
            outputs.push(b.column);
            value_subqueries.extend(b.value_subplans);
            filter_reads.extend(b.filter_reads);
            filter_subqueries.extend(b.filter_subplans);
        }
        let project = Plan::Project(Project {
            input: Box::new(wrap_reads(input, filter_reads, filter_subqueries)),
            outputs: outputs.clone(),
            subqueries: value_subqueries,
        });
        (project, outputs)
    }

    /// Like [`pipe_project`](Self::pipe_project) but for `|> SET`: each bound
    /// column replaces a same-named output in `base` (or is appended), so a
    /// `SET` after a `SELECT` rewrites that column in place rather than
    /// duplicating it.
    fn pipe_set_project(
        &self,
        input: Plan,
        mut outputs: Vec<BoundColumn>,
        bound: Vec<BoundValue>,
    ) -> (Plan, Vec<BoundColumn>) {
        let mut value_subqueries = Vec::new();
        let mut filter_reads = Vec::new();
        let mut filter_subqueries = Vec::new();
        for b in bound {
            value_subqueries.extend(b.value_subplans);
            filter_reads.extend(b.filter_reads);
            filter_subqueries.extend(b.filter_subplans);
            let slot = b.column.name.as_ref().and_then(|name| {
                outputs
                    .iter_mut()
                    .find(|o| o.name.as_ref().is_some_and(|n| n.value == name.value))
            });
            match slot {
                Some(existing) => *existing = b.column,
                None => outputs.push(b.column),
            }
        }
        let project = Plan::Project(Project {
            input: Box::new(wrap_reads(input, filter_reads, filter_subqueries)),
            outputs: outputs.clone(),
            subqueries: value_subqueries,
        });
        (project, outputs)
    }

    /// Filter-position reads from a `LIMIT` / `OFFSET` / `LIMIT BY` clause
    /// (row-count bounds — never value sources).
    fn limit_reads(&self, limit: &LimitClause, scope: &Scope) -> (Vec<ColumnRead>, Vec<Plan>) {
        let mut c = ExprCollector::filter();
        match limit {
            LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            } => {
                for expr in limit.iter().chain(limit_by) {
                    self.collect_expr(expr, scope, &mut c);
                }
                if let Some(offset) = offset {
                    self.collect_expr(&offset.value, scope, &mut c);
                }
            }
            LimitClause::OffsetCommaLimit { offset, limit } => {
                self.collect_expr(offset, scope, &mut c);
                self.collect_expr(limit, scope, &mut c);
            }
        }
        c.into_filter_parts()
    }

    /// Bind a query body's set expression: a leaf `SELECT`, a
    /// parenthesized inner query, a set operation (`UNION` / `INTERSECT` /
    /// `EXCEPT`), or a DML body. A set operation fans its operands into a
    /// [`SetOp`] and merges their outputs positionally — each result
    /// column unions the branches' provenance (so a derived / CTE over a
    /// `UNION` traces to every branch's base columns), taking its name
    /// from the left branch. The set-operation kind itself doesn't change
    /// lineage, so it's dropped. A leading statement-level `WITH` parses as
    /// a `Query` whose body is the DML (`WITH … INSERT/UPDATE …`), so those
    /// bodies bind through here to their `Write`-rooted tree.
    fn bind_set_expr(&self, set_expr: &SetExpr) -> (Plan, Scope) {
        match set_expr {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(inner) => self.bind_query(inner),
            // `WITH … INSERT/UPDATE/DELETE/MERGE …`: the DML statement is
            // the query body. Bind it to its `Write` tree; it exposes no
            // output scope to an enclosing query.
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => (
                self.bind_statement(statement).unwrap_or(Plan::OpaqueLeaf),
                Scope::empty(),
            ),
            // `VALUES (…), (…)`: a literal row set. Each column position is
            // an output whose provenance unions that position's row
            // expressions, so a `(VALUES …) AS v(x)` exposes resolvable
            // columns (literals collapse to nothing; a row expression
            // referencing an outer relation surfaces it).
            SetExpr::Values(values) => self.bind_values(values),
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
                        merge_columns: Vec::new(),
                    },
                )
            }
            // `TABLE foo` / `TABLE schema.foo`: a whole-table query body
            // (e.g. the source of `CREATE TABLE t AS TABLE foo`). Bind it as
            // a read scan so the table surfaces.
            SetExpr::Table(table) => match table_set_expr_ref(table) {
                Some(written) => self.bind_named_table(&written, None),
                None => (Plan::OpaqueLeaf, Scope::empty()),
            },
        }
    }

    /// Bind a `VALUES (…), (…)` row set into a column-defining `Project`:
    /// one opaque output per column position (no provenance — the produced
    /// row is synthesized, so a reference to it collapses to a synthetic
    /// self-source, never to a row expression). The row expressions
    /// themselves are reads, resolved against the empty current scope and
    /// falling through to the correlation stack — a `(VALUES (t.a)) AS v`
    /// reads the enclosing / sibling `t.a` like a derived subquery's body.
    fn bind_values(&self, values: &Values) -> (Plan, Scope) {
        let width = values.rows.iter().map(Vec::len).max().unwrap_or(0);
        let outputs: Vec<BoundColumn> = (0..width)
            .map(|_| BoundColumn {
                name: None,
                provenance: Vec::new(),
            })
            .collect();
        let mut reads = Vec::new();
        let mut subplans = Vec::new();
        for expr in values.rows.iter().flatten() {
            let (r, s) = self.expr_reads(expr, &Scope::empty());
            reads.extend(r);
            subplans.extend(s);
        }
        let plan = Plan::Project(Project {
            input: Box::new(wrap_reads(Plan::OpaqueLeaf, reads, subplans)),
            outputs: outputs.clone(),
            subqueries: Vec::new(),
        });
        (
            plan,
            Scope {
                relations: Vec::new(),
                outputs,
                merge_columns: Vec::new(),
            },
        )
    }

    /// Filter-position reads from a SELECT's auxiliary clauses, resolved
    /// against the FROM scope: `DISTINCT ON` keys, `TOP n`, Hive `LATERAL
    /// VIEW` generators, `PREWHERE`, `QUALIFY`, `CONNECT BY` / `START WITH`,
    /// `CLUSTER BY` / `DISTRIBUTE BY`, and named `WINDOW` specs. None feed
    /// values — they are all reads, never lineage sources.
    fn select_clause_reads(&self, select: &Select, scope: &Scope) -> (Vec<ColumnRead>, Vec<Plan>) {
        let mut c = ExprCollector::filter();
        if let Some(Distinct::On(exprs)) = &select.distinct {
            for expr in exprs {
                self.collect_expr(expr, scope, &mut c);
            }
        }
        if let Some(top) = &select.top {
            if let Some(TopQuantity::Expr(expr)) = &top.quantity {
                self.collect_expr(expr, scope, &mut c);
            }
        }
        for lateral_view in &select.lateral_views {
            self.collect_expr(&lateral_view.lateral_view, scope, &mut c);
        }
        if let Some(expr) = &select.prewhere {
            self.collect_expr(expr, scope, &mut c);
        }
        if let Some(expr) = &select.qualify {
            self.collect_expr(expr, scope, &mut c);
        }
        for connect_by in &select.connect_by {
            match connect_by {
                ConnectByKind::ConnectBy { relationships, .. } => {
                    for expr in relationships {
                        self.collect_expr(expr, scope, &mut c);
                    }
                }
                ConnectByKind::StartWith { condition, .. } => {
                    self.collect_expr(condition, scope, &mut c)
                }
            }
        }
        for expr in select.cluster_by.iter().chain(&select.distribute_by) {
            self.collect_expr(expr, scope, &mut c);
        }
        for window in &select.named_window {
            if let NamedWindowExpr::WindowSpec(spec) = &window.1 {
                self.collect_window_spec(spec, scope, &mut c);
            }
        }
        c.into_filter_parts()
    }

    fn bind_select(&self, select: &Select) -> (Plan, Scope) {
        let (from, from_scope) = self.bind_from(&select.from);
        // The WHERE-family clauses wrap the FROM in a filter PassThrough:
        // they resolve against the FROM scope only (Project is above, so no
        // output aliases are visible — the clause-phase rule, structurally).
        let (mut reads, mut subqueries) = select
            .selection
            .as_ref()
            .map(|predicate| self.expr_reads(predicate, &from_scope))
            .unwrap_or_default();
        let (clause_reads, clause_subqueries) = self.select_clause_reads(select, &from_scope);
        reads.extend(clause_reads);
        subqueries.extend(clause_subqueries);
        let input = wrap_reads(from, reads, subqueries);
        // PassThrough is identity, so the projection resolves against the
        // FROM scope either way. A projection scalar subquery's sub-plan is
        // kept on the Project (walked for its tables / reads); its output
        // already folded into the owning column's provenance.
        let mut outputs = Vec::new();
        let mut projection_subqueries = Vec::new();
        // Predicate references / sub-plans inside a projection expression (a
        // `CASE` condition, an `EXISTS` test) are reads, not value sources —
        // carry them on a non-feeding PassThrough below the Project so they
        // surface as reads without feeding lineage.
        let mut projection_filter_reads = Vec::new();
        let mut projection_filter_subqueries = Vec::new();
        for item in &select.projection {
            if let Some(bound) = self.bind_output_column(item, &from_scope) {
                outputs.push(bound.column);
                projection_subqueries.extend(bound.value_subplans);
                projection_filter_reads.extend(bound.filter_reads);
                projection_filter_subqueries.extend(bound.filter_subplans);
            }
        }
        let project = Plan::Project(Project {
            input: Box::new(wrap_reads(
                input,
                projection_filter_reads,
                projection_filter_subqueries,
            )),
            outputs: outputs.clone(),
            subqueries: projection_subqueries,
        });
        // GROUP BY / HAVING / SORT BY see the output aliases (clause
        // phase): resolve against the FROM relations *plus* the outputs,
        // keeping the USING merge columns so they fan in there too.
        let clause_scope = Scope {
            relations: from_scope.relations,
            outputs,
            merge_columns: from_scope.merge_columns,
        };
        let (mut clause_reads, mut clause_subqueries) =
            self.group_by_reads(&select.group_by, &clause_scope);
        if let Some(having) = &select.having {
            let (reads, subqueries) = self.expr_reads(having, &clause_scope);
            clause_reads.extend(reads);
            clause_subqueries.extend(subqueries);
        }
        for sort in &select.sort_by {
            let (reads, subqueries) = self.expr_reads(&sort.expr, &clause_scope);
            clause_reads.extend(reads);
            clause_subqueries.extend(subqueries);
        }
        let body = wrap_reads(project, clause_reads, clause_subqueries);
        // `SELECT … INTO t`: the query also creates / writes table `t`
        // (MsSql / Postgres). Wrap the projection as the write source so `t`
        // surfaces as a write target (its columns feed it positionally).
        let plan = match &select.into {
            Some(into) => match TableReference::try_from_name(&into.name) {
                Ok(target) => Plan::Write(Write {
                    target: self.canonical_target(target),
                    target_columns: Vec::new(),
                    input: Box::new(body),
                    returning: Vec::new(),
                    conflict_updates: Vec::new(),
                }),
                Err(_) => body,
            },
            None => body,
        };
        // A trailing top-level ORDER BY also resolves against this scope,
        // so hand it back to `bind_query`.
        (plan, clause_scope)
    }

    fn bind_from(&self, items: &[TableWithJoins]) -> (Plan, Scope) {
        // Bind each comma-separated FROM item in order, accumulating the
        // scope so a later item (a LATERAL table function / derived table)
        // can resolve references to an earlier sibling.
        let mut scope = Scope::empty();
        let mut inputs = Vec::with_capacity(items.len());
        for twj in items {
            let (node, node_scope) = self.bind_table_with_joins(twj, &scope);
            inputs.push(node);
            scope = scope.merge(node_scope);
        }
        match inputs.len() {
            // `SELECT 1` (no FROM) — an empty opaque source.
            0 => (Plan::OpaqueLeaf, Scope::empty()),
            1 => (inputs.pop().unwrap(), scope),
            // Comma join: a PassThrough with no predicate.
            _ => (
                Plan::PassThrough(PassThrough {
                    inputs,
                    reads: Vec::new(),
                    subqueries: Vec::new(),
                }),
                scope,
            ),
        }
    }

    fn bind_table_with_joins(&self, twj: &TableWithJoins, siblings: &Scope) -> (Plan, Scope) {
        let (mut node, mut scope) = self.bind_table_factor(&twj.relation, siblings);
        for join in &twj.joins {
            // A joined relation sees the preceding FROM siblings plus the
            // relations bound so far in this join chain (LATERAL).
            let joined_siblings = siblings.clone().merge(scope.clone());
            let (right, right_scope) = self.bind_table_factor(&join.relation, &joined_siblings);
            // The ON predicate sees both sides; resolve its reads against
            // the combined scope, which is also this PassThrough's output.
            let mut combined = scope.merge(right_scope);
            let (reads, subqueries) = match join_constraint(join) {
                Some(JoinConstraint::On(expr)) => self.expr_reads(expr, &combined),
                // USING (col, …) records merge columns: a later unqualified
                // reference fans in to every side that could own one. The
                // join itself contributes no reads (only references do).
                // NATURAL is not expanded (needs both schemas).
                Some(JoinConstraint::Using(columns)) => {
                    combined
                        .merge_columns
                        .extend(columns.iter().filter_map(object_name_last_ident));
                    (Vec::new(), Vec::new())
                }
                _ => (Vec::new(), Vec::new()),
            };
            node = Plan::PassThrough(PassThrough {
                inputs: vec![node, right],
                reads,
                subqueries,
            });
            scope = combined;
        }
        (node, scope)
    }

    /// Bind a bare named table reference into a read `Scan` plus a
    /// single-relation scope. A unique catalog hit canonicalizes the
    /// identity, supplies the columns, and is `Cataloged`; an ambiguous hit
    /// stays as written and `Ambiguous`; a miss / no-catalog stays as
    /// written, open, and `Inferred`.
    fn bind_named_table(&self, written: &TableReference, alias: Option<Ident>) -> (Plan, Scope) {
        let TableMatch {
            table,
            resolution,
            columns,
        } = self.table_match(written);
        let columns = if columns.is_empty() {
            RelationColumns::Open
        } else {
            RelationColumns::Known(columns)
        };
        let relation = Relation {
            alias,
            source: RelationSource::Table {
                table: table.clone(),
                columns,
            },
        };
        let scan = Plan::Scan(Scan {
            table,
            resolution,
            role: ScanRole::Read,
        });
        (scan, Scope::of(relation))
    }

    fn bind_table_factor(&self, factor: &TableFactor, siblings: &Scope) -> (Plan, Scope) {
        match factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                let Ok(written) = TableReference::try_from_name(name) else {
                    return (Plan::OpaqueLeaf, Scope::empty());
                };
                // A parameterised table reference `foo(args)` carries
                // argument expressions (read against the surrounding scope).
                let arg_reads = args.as_ref().map(|args| {
                    let mut c = ExprCollector::filter();
                    for arg in &args.args {
                        self.collect_function_arg(arg, siblings, &mut c);
                    }
                    c
                });
                let alias = alias.as_ref().map(|a| a.name.clone());
                // A bare name matching an in-scope CTE resolves to that
                // CTE's synthetic relation — its pre-collapsed outputs,
                // exposed via the scope exactly like a derived table. The
                // plan node is a lightweight `CteRef` (not a clone of the
                // body): the body is walked once at its `With` declaration,
                // so references neither double-count nor lose its reads.
                // Qualified names are never CTEs.
                if written.schema.is_none() && written.catalog.is_none() {
                    if let Some(cte) = self.lookup_cte(&written.name) {
                        let name = cte.name.clone();
                        let relation = Relation {
                            alias: alias.or_else(|| Some(cte.name.clone())),
                            source: RelationSource::Derived {
                                columns: cte.outputs.clone(),
                            },
                        };
                        return (Plan::CteRef(CteRef { name }), Scope::of(relation));
                    }
                }
                let (scan, scope) = self.bind_named_table(&written, alias);
                // Table-function args (rare on a named table) read against
                // the surrounding scope; embed them so they surface.
                let plan = match arg_reads {
                    Some(c) => {
                        let (reads, subplans) = c.into_filter_parts();
                        wrap_reads(scan, reads, subplans)
                    }
                    None => scan,
                };
                (plan, scope)
            }
            // A derived table `(<subquery>) AS d`: bind the subquery and
            // expose its output columns as a synthetic relation. Those
            // outputs already carry collapsed provenance, so an outer
            // reference through `d` surfaces the inner real columns —
            // collapse falls out of construction. The subquery's plan is
            // this factor's plan (an input to the enclosing operators). The
            // preceding FROM siblings are visible to a LATERAL subquery (the
            // `lateral` flag is not enforced, matching the resolver).
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let (plan, sub_scope) = self
                    .with_outer_scope(siblings.relations.clone())
                    .bind_query(subquery);
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
            // A parenthesized join `(a JOIN b ...)`: the inner tables bind
            // directly into the current scope (so refs to them resolve and
            // their ON reads surface); the wrapper alias exposes nothing.
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => self.bind_table_with_joins(table_with_joins, siblings),
            // PIVOT / UNPIVOT / MATCH_RECOGNIZE wrap an inner table whose
            // columns the clause expressions read; the produced relation is
            // opaque (dynamic columns).
            TableFactor::Pivot {
                table,
                aggregate_functions,
                value_column,
                value_source,
                default_on_null,
                alias,
                ..
            } => {
                let (inner, inner_scope) = self.bind_table_factor(table, siblings);
                let mut c = ExprCollector::filter();
                for agg in aggregate_functions {
                    self.collect_expr(&agg.expr, &inner_scope, &mut c);
                }
                for expr in value_column {
                    self.collect_expr(expr, &inner_scope, &mut c);
                }
                self.collect_pivot_value_source(value_source, &inner_scope, &mut c);
                if let Some(expr) = default_on_null {
                    self.collect_expr(expr, &inner_scope, &mut c);
                }
                self.opaque_relation(inner, alias.as_ref(), c)
            }
            TableFactor::Unpivot {
                table,
                value,
                columns,
                alias,
                ..
            } => {
                let (inner, inner_scope) = self.bind_table_factor(table, siblings);
                let mut c = ExprCollector::filter();
                self.collect_expr(value, &inner_scope, &mut c);
                for col in columns {
                    self.collect_expr(&col.expr, &inner_scope, &mut c);
                }
                self.opaque_relation(inner, alias.as_ref(), c)
            }
            TableFactor::MatchRecognize {
                table,
                partition_by,
                order_by,
                measures,
                symbols,
                alias,
                ..
            } => {
                let (inner, inner_scope) = self.bind_table_factor(table, siblings);
                let mut c = ExprCollector::filter();
                for expr in partition_by {
                    self.collect_expr(expr, &inner_scope, &mut c);
                }
                for ob in order_by {
                    self.collect_order_by_expr(ob, &inner_scope, &mut c);
                }
                for measure in measures {
                    self.collect_expr(&measure.expr, &inner_scope, &mut c);
                }
                for symbol in symbols {
                    self.collect_expr(&symbol.definition, &inner_scope, &mut c);
                }
                self.opaque_relation(inner, alias.as_ref(), c)
            }
            // Table functions / UNNEST / JSON_TABLE / XML / semantic views:
            // an opaque relation whose argument expressions read against the
            // surrounding (LATERAL-visible) scope.
            TableFactor::TableFunction { expr, alias } => {
                let mut c = ExprCollector::filter();
                self.collect_expr(expr, siblings, &mut c);
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
            TableFactor::Function { args, alias, .. } => {
                let mut c = ExprCollector::filter();
                for arg in args {
                    self.collect_function_arg(arg, siblings, &mut c);
                }
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
            TableFactor::UNNEST {
                array_exprs, alias, ..
            } => {
                let mut c = ExprCollector::filter();
                for expr in array_exprs {
                    self.collect_expr(expr, siblings, &mut c);
                }
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
            TableFactor::JsonTable {
                json_expr, alias, ..
            }
            | TableFactor::OpenJsonTable {
                json_expr, alias, ..
            } => {
                let mut c = ExprCollector::filter();
                self.collect_expr(json_expr, siblings, &mut c);
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
            TableFactor::XmlTable {
                row_expression,
                passing,
                alias,
                ..
            } => {
                let mut c = ExprCollector::filter();
                self.collect_expr(row_expression, siblings, &mut c);
                for argument in &passing.arguments {
                    self.collect_expr(&argument.expr, siblings, &mut c);
                }
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
            TableFactor::SemanticView {
                dimensions,
                metrics,
                facts,
                where_clause,
                alias,
                ..
            } => {
                let mut c = ExprCollector::filter();
                for expr in dimensions.iter().chain(metrics).chain(facts) {
                    self.collect_expr(expr, siblings, &mut c);
                }
                if let Some(expr) = where_clause {
                    self.collect_expr(expr, siblings, &mut c);
                }
                self.opaque_relation(Plan::OpaqueLeaf, alias.as_ref(), c)
            }
        }
    }

    /// Wrap an opaque relation's `base` plan with the reads collected from
    /// its argument expressions, exposing the result as a synthetic
    /// [`TableFunction`](RelationSource::TableFunction) relation (its
    /// produced columns are dynamic, so a reference through its alias yields
    /// nothing).
    fn opaque_relation(
        &self,
        base: Plan,
        alias: Option<&TableAlias>,
        collector: ExprCollector,
    ) -> (Plan, Scope) {
        let (reads, subplans) = collector.into_filter_parts();
        let plan = wrap_reads(base, reads, subplans);
        let scope = alias.map_or_else(Scope::empty, |alias| {
            Scope::of(Relation {
                alias: Some(alias.name.clone()),
                source: RelationSource::TableFunction,
            })
        });
        (plan, scope)
    }

    fn collect_pivot_value_source(
        &self,
        value_source: &sqlparser::ast::PivotValueSource,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        use sqlparser::ast::PivotValueSource;
        match value_source {
            PivotValueSource::List(values) => {
                for value in values {
                    self.collect_expr(&value.expr, scope, c);
                }
            }
            PivotValueSource::Any(order_by) => {
                for ob in order_by {
                    self.collect_order_by_expr(ob, scope, c);
                }
            }
            PivotValueSource::Subquery(query) => self.collect_subquery(query, scope, c),
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

    /// Bind one projection item into a [`BoundValue`] (its output column plus
    /// the position-split sub-plans / filter reads it contributes), or `None`
    /// for a wildcard, which isn't expanded. A `(expr).*` qualified wildcard
    /// still reads its base expression (projected as one `Transformation`
    /// output) even though the produced columns are suppressed.
    fn bind_output_column(&self, item: &SelectItem, scope: &Scope) -> Option<BoundValue> {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias.clone())),
            // A wildcard isn't expanded (the rigor cost is too high for a
            // SQL-text-only library); record it so consumers know this
            // projection's column lineage is incomplete, and skip it.
            SelectItem::Wildcard(options) => {
                self.record_wildcard_suppressed("wildcard `*`", options.wildcard_token.0.span);
                return None;
            }
            SelectItem::QualifiedWildcard(kind, options) => {
                let description = match kind {
                    SelectItemQualifiedWildcardKind::Expr(_) => {
                        "qualified wildcard `(expr).*`".to_string()
                    }
                    SelectItemQualifiedWildcardKind::ObjectName(name) => {
                        format!("qualified wildcard `{name}.*`")
                    }
                };
                self.record_wildcard_suppressed(&description, options.wildcard_token.0.span);
                // `(expr).*` (Snowflake) still projects its base expression
                // as one output — a structural field access, so the value
                // flows as a `Transformation` even though the produced
                // columns aren't enumerated. `alias.*` (an ObjectName) has
                // no inspectable base expression.
                if let SelectItemQualifiedWildcardKind::Expr(expr) = kind {
                    let mut bound = self.bind_value_column(None, expr, scope);
                    for source in &mut bound.column.provenance {
                        source.kind = ColumnLineageKind::Transformation;
                    }
                    return Some(bound);
                }
                return None;
            }
        };
        let name = alias.or_else(|| inferred_output_name(expr));
        Some(self.bind_value_column(name, expr, scope))
    }

    /// Record a `WildcardSuppressed` diagnostic for a projection wildcard
    /// (`*` / `t.*` / `(expr).*`) left unexpanded, carrying the wildcard
    /// token's location (a zero-line span is treated as unknown).
    fn record_wildcard_suppressed(&self, description: &str, span: Span) {
        let span = (span.start.line != 0).then_some(span);
        let suffix = match span {
            Some(s) => format!(" at L{}:C{}", s.start.line, s.start.column),
            None => String::new(),
        };
        self.diagnostics.borrow_mut().push(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::WildcardSuppressed,
            message: format!(
                "{description}{suffix} left unexpanded — column lineage will be incomplete for this projection"
            ),
            span,
        });
    }

    /// Bind a `RETURNING` clause's projected columns against `scope`. Each
    /// is a value-position projection (like a SELECT list) over the written
    /// relation, so it contributes target reads and a `QueryOutput` lineage
    /// edge. A wildcard records `WildcardSuppressed` and is skipped.
    fn bind_returning(
        &self,
        returning: &Option<Vec<SelectItem>>,
        scope: &Scope,
    ) -> Vec<BoundColumn> {
        let Some(items) = returning else {
            return Vec::new();
        };
        items
            .iter()
            // Sub-plans / filter reads of a RETURNING expression (rare —
            // a subquery or CASE in RETURNING) aren't modelled yet.
            .filter_map(|item| {
                self.bind_output_column(item, scope)
                    .map(|bound| bound.column)
            })
            .collect()
    }

    /// Bind an `INSERT`'s conflict action (`ON CONFLICT DO UPDATE SET …`
    /// or MySQL `ON DUPLICATE KEY UPDATE …`) into a mini-UPDATE on the
    /// target: the SET assignments become bound columns (named by their
    /// target column, carrying the RHS provenance) for extra writes +
    /// `Relation` lineage, and the optional `DO UPDATE … WHERE` is a filter
    /// read. Returns the assignments plus the conflict's reads / sub-plans
    /// (to fold onto the write's input). For Postgres / SQLite the
    /// `EXCLUDED` pseudo-table is in scope (its columns are the INSERT
    /// source's outputs renamed to the target columns, so `EXCLUDED.col`
    /// collapses back to the source); MySQL's `VALUES(col)` self-references
    /// the target instead, so no `EXCLUDED` is bound.
    fn bind_conflict(
        &self,
        on: &OnInsert,
        target: &TableReference,
        target_columns: &[Ident],
        source_scope: &Scope,
    ) -> (Vec<BoundColumn>, Vec<ColumnRead>, Vec<Plan>) {
        let mut updates = Vec::new();
        let mut reads = Vec::new();
        let mut subplans = Vec::new();
        let (assignments, scope, selection) = match on {
            OnInsert::DuplicateKeyUpdate(assignments) => {
                (assignments.as_slice(), self.target_scope(target), None)
            }
            OnInsert::OnConflict(on_conflict) => match &on_conflict.action {
                OnConflictAction::DoUpdate(do_update) => {
                    let scope = self
                        .target_scope(target)
                        .merge(self.excluded_scope(target_columns, source_scope));
                    (
                        do_update.assignments.as_slice(),
                        scope,
                        do_update.selection.as_ref(),
                    )
                }
                // DO NOTHING moves no data.
                OnConflictAction::DoNothing => return (updates, reads, subplans),
            },
            // `OnInsert` is non-exhaustive; an unmodelled action is a no-op.
            _ => return (updates, reads, subplans),
        };
        for assignment in assignments {
            for column in assignment_target_columns(&assignment.target) {
                let bound = self.bind_value_column(Some(column), &assignment.value, &scope);
                updates.push(bound.column);
                // The conflict action's lineage rides the `conflict_updates`
                // provenance, not these sub-plans, so both value- and
                // filter-position sub-plans land on the non-feeding input.
                subplans.extend(bound.value_subplans);
                subplans.extend(bound.filter_subplans);
                reads.extend(bound.filter_reads);
            }
        }
        if let Some(selection) = selection {
            let (r, s) = self.expr_reads(selection, &scope);
            reads.extend(r);
            subplans.extend(s);
        }
        (updates, reads, subplans)
    }

    /// The `EXCLUDED` pseudo-table's resolution scope: a synthetic relation
    /// whose columns are the INSERT source's output columns renamed
    /// positionally to the target columns (so `EXCLUDED.col` collapses to
    /// whatever feeds that position of the source). A source with no
    /// inspectable outputs (`VALUES`) yields an opaque table-function-like
    /// relation, so `EXCLUDED.col` stays a synthetic self-reference.
    fn excluded_scope(&self, target_columns: &[Ident], source_scope: &Scope) -> Scope {
        let columns: Vec<BoundColumn> = source_scope
            .outputs
            .iter()
            .enumerate()
            .map(|(i, column)| BoundColumn {
                name: target_columns
                    .get(i)
                    .cloned()
                    .or_else(|| column.name.clone()),
                provenance: column.provenance.clone(),
            })
            .collect();
        let source = if columns.is_empty() {
            RelationSource::TableFunction
        } else {
            RelationSource::Derived { columns }
        };
        Scope::of(Relation {
            alias: Some(Ident::new("EXCLUDED")),
            source,
        })
    }

    /// The target table's catalog column names (unquoted), for filling in
    /// the target columns of a column-less `INSERT` — `INSERT INTO t SELECT
    /// …` pairs the source positionally with `t`'s schema. Empty without a
    /// unique catalog hit (column-less INSERT then writes / pairs nothing).
    fn catalog_columns(&self, target: &TableReference) -> Vec<Ident> {
        self.table_match(target)
            .columns
            .iter()
            .map(|column| Ident::new(&column.value))
            .collect()
    }

    /// A resolution scope holding just the write target (for `INSERT …
    /// RETURNING`, whose references resolve against the target alone — the
    /// source query's scope has already been popped).
    fn target_scope(&self, target: &TableReference) -> Scope {
        let TableMatch { table, columns, .. } = self.table_match(target);
        let columns = if columns.is_empty() {
            RelationColumns::Open
        } else {
            RelationColumns::Known(columns)
        };
        Scope::of(Relation {
            alias: None,
            source: RelationSource::Table { table, columns },
        })
    }

    /// Bind a value-producing expression (a projection item or a SET /
    /// VALUES assignment RHS) into a named output column, split by position
    /// (see [`BoundValue`]). The column's provenance is the value's lineage
    /// sources (direct refs + any nested value subquery's *output*); each
    /// source's composed kind folds in this expression's own kind (a bare
    /// column ref is `Passthrough`, anything else `Transformation`). Filter
    /// position inside the expression (a `CASE` condition, an `EXISTS` test)
    /// yields `filter_reads` / `filter_subplans` — reads that don't feed
    /// this value, so the caller routes them to a non-feeding position.
    fn bind_value_column(&self, name: Option<Ident>, expr: &Expr, scope: &Scope) -> BoundValue {
        let outer = expr_kind(expr);
        let mut collector = ExprCollector::value();
        self.collect_expr(expr, scope, &mut collector);
        let provenance = collector
            .sources
            .into_iter()
            .map(|source| ProvenanceSource {
                kind: combine_kind(source.kind, outer),
                read: source.read,
                synthetic_origin: source.synthetic_origin,
            })
            .collect();
        BoundValue {
            column: BoundColumn { name, provenance },
            value_subplans: collector.value_subplans,
            filter_reads: collector.filter_reads,
            filter_subplans: collector.filter_subplans,
        }
    }

    /// The plain column reads of an expression (filter position — WHERE /
    /// ON / clause predicates / DML predicates) plus the sub-plans of any
    /// subqueries it contains. The whole expression is a predicate, so it
    /// collects in suppressed mode: every direct reference is a read, never
    /// a lineage source. Synthetic-origin references (through a derived /
    /// CTE relation, an output alias, or a nested subquery's output) are
    /// dropped — their physical reads are counted by walking the sub-plans.
    fn expr_reads(&self, expr: &Expr, scope: &Scope) -> (Vec<ColumnRead>, Vec<Plan>) {
        let mut collector = ExprCollector::filter();
        self.collect_expr(expr, scope, &mut collector);
        collector.into_filter_parts()
    }

    /// Walk an expression, routing each column reference to the collector by
    /// position: a **value** reference (one whose value flows to the output)
    /// becomes a lineage `source`; a **filter** reference (a predicate that
    /// only influences *which* rows / values are produced — a `CASE`
    /// condition, an `EXISTS` / `IN` / `ANY` / `ALL` test, a window
    /// PARTITION / ORDER key, an aggregate `FILTER`) becomes a `filter_read`
    /// instead. The split mirrors the resolver's `suppress_lineage`. Nested
    /// subqueries are kept whole as `subplans` (walked for their own tables
    /// / reads); a scalar subquery's *output* additionally folds in as a
    /// synthetic-origin value source (unless it sits in a filter position).
    /// The match is exhaustive so new `Expr` variants are reviewed here.
    fn collect_expr(&self, expr: &Expr, scope: &Scope, c: &mut ExprCollector) {
        match expr {
            Expr::Identifier(id) => self.emit_ref(std::slice::from_ref(id), scope, c),
            Expr::CompoundIdentifier(ids) => self.emit_ref(ids, scope, c),
            // Both operands flow / filter with the surrounding position.
            Expr::BinaryOp { left, right, .. }
            | Expr::IsDistinctFrom(left, right)
            | Expr::IsNotDistinctFrom(left, right) => {
                self.collect_expr(left, scope, c);
                self.collect_expr(right, scope, c);
            }
            // ANY / ALL: the LHS keeps the surrounding position; the RHS is
            // a shape test (its rows don't flow as values) → suppressed.
            Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
                self.collect_expr(left, scope, c);
                c.suppressed(|c| self.collect_expr(right, scope, c));
            }
            Expr::UnaryOp { expr, .. }
            | Expr::Nested(expr)
            | Expr::OuterJoin(expr)
            | Expr::Prior(expr)
            | Expr::IsFalse(expr)
            | Expr::IsNotFalse(expr)
            | Expr::IsTrue(expr)
            | Expr::IsNotTrue(expr)
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr)
            | Expr::IsUnknown(expr)
            | Expr::IsNotUnknown(expr)
            | Expr::Cast { expr, .. }
            | Expr::IsNormalized { expr, .. }
            | Expr::Extract { expr, .. }
            | Expr::Ceil { expr, .. }
            | Expr::Floor { expr, .. }
            | Expr::Collate { expr, .. }
            | Expr::Prefixed { value: expr, .. }
            | Expr::Named { expr, .. } => self.collect_expr(expr, scope, c),
            Expr::CompoundFieldAccess { root, access_chain } => {
                self.collect_expr(root, scope, c);
                for access in access_chain {
                    self.collect_access(access, scope, c);
                }
            }
            Expr::JsonAccess { value, .. } => self.collect_expr(value, scope, c),
            Expr::InList { expr, list, .. } => {
                self.collect_expr(expr, scope, c);
                for item in list {
                    self.collect_expr(item, scope, c);
                }
            }
            Expr::InUnnest {
                expr, array_expr, ..
            } => {
                self.collect_expr(expr, scope, c);
                self.collect_expr(array_expr, scope, c);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.collect_expr(expr, scope, c);
                self.collect_expr(low, scope, c);
                self.collect_expr(high, scope, c);
            }
            Expr::Like { expr, pattern, .. }
            | Expr::ILike { expr, pattern, .. }
            | Expr::SimilarTo { expr, pattern, .. }
            | Expr::RLike { expr, pattern, .. } => {
                self.collect_expr(expr, scope, c);
                self.collect_expr(pattern, scope, c);
            }
            Expr::Convert { expr, styles, .. } => {
                self.collect_expr(expr, scope, c);
                for style in styles {
                    self.collect_expr(style, scope, c);
                }
            }
            Expr::AtTimeZone {
                timestamp,
                time_zone,
            } => {
                self.collect_expr(timestamp, scope, c);
                self.collect_expr(time_zone, scope, c);
            }
            Expr::Position { expr, r#in } => {
                self.collect_expr(expr, scope, c);
                self.collect_expr(r#in, scope, c);
            }
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                self.collect_expr(expr, scope, c);
                for sub in [substring_from, substring_for].into_iter().flatten() {
                    self.collect_expr(sub, scope, c);
                }
            }
            Expr::Trim {
                expr,
                trim_what,
                trim_characters,
                ..
            } => {
                self.collect_expr(expr, scope, c);
                if let Some(trim_what) = trim_what {
                    self.collect_expr(trim_what, scope, c);
                }
                if let Some(exprs) = trim_characters {
                    for sub in exprs {
                        self.collect_expr(sub, scope, c);
                    }
                }
            }
            Expr::Overlay {
                expr,
                overlay_what,
                overlay_from,
                overlay_for,
            } => {
                self.collect_expr(expr, scope, c);
                self.collect_expr(overlay_what, scope, c);
                self.collect_expr(overlay_from, scope, c);
                if let Some(overlay_for) = overlay_for {
                    self.collect_expr(overlay_for, scope, c);
                }
            }
            // CASE: the operand and the WHEN conditions are predicates (they
            // select which result is produced) → suppressed; the THEN / ELSE
            // results are the values that flow → keep the surrounding position.
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                if let Some(operand) = operand {
                    c.suppressed(|c| self.collect_expr(operand, scope, c));
                }
                for condition in conditions {
                    c.suppressed(|c| self.collect_expr(&condition.condition, scope, c));
                    self.collect_expr(&condition.result, scope, c);
                }
                if let Some(else_result) = else_result {
                    self.collect_expr(else_result, scope, c);
                }
            }
            Expr::Rollup(sets) | Expr::Cube(sets) | Expr::GroupingSets(sets) => {
                for set in sets {
                    for expr in set {
                        self.collect_expr(expr, scope, c);
                    }
                }
            }
            Expr::Tuple(exprs) | Expr::Struct { values: exprs, .. } => {
                for expr in exprs {
                    self.collect_expr(expr, scope, c);
                }
            }
            Expr::Function(function) => self.collect_function(function, scope, c),
            Expr::Dictionary(fields) => {
                for field in fields {
                    self.collect_dictionary_field(field, scope, c);
                }
            }
            Expr::Map(map) => self.collect_map(map, scope, c),
            Expr::Array(array) => self.collect_array(array, scope, c),
            Expr::Interval(interval) => self.collect_expr(&interval.value, scope, c),
            Expr::Lambda(lambda) => self.collect_expr(&lambda.body, scope, c),
            Expr::MemberOf(member_of) => {
                self.collect_expr(&member_of.value, scope, c);
                self.collect_expr(&member_of.array, scope, c);
            }
            // A scalar subquery's value flows out → keep the surrounding
            // position; EXISTS is a boolean test → suppressed.
            Expr::Subquery(query) => self.collect_subquery(query, scope, c),
            Expr::Exists {
                subquery: query, ..
            } => c.suppressed(|c| self.collect_subquery(query, scope, c)),
            Expr::InSubquery { expr, subquery, .. } => {
                self.collect_expr(expr, scope, c);
                c.suppressed(|c| self.collect_subquery(subquery, scope, c));
            }
            Expr::Value(_)
            | Expr::TypedString(_)
            | Expr::MatchAgainst { .. }
            | Expr::Wildcard(_)
            | Expr::QualifiedWildcard(_, _) => {}
        }
    }

    /// Emit one column reference into the collector: a value position adds
    /// the resolved lineage sources; a filter position keeps only the
    /// non-synthetic reads (synthetic-origin references are counted at the
    /// inner producer by walking its sub-plan).
    fn emit_ref(&self, parts: &[Ident], scope: &Scope, c: &mut ExprCollector) {
        let resolved = self.resolve_ref(parts, scope);
        if c.is_suppressed {
            c.filter_reads.extend(
                resolved
                    .into_iter()
                    .filter(|source| !source.synthetic_origin)
                    .map(|source| source.read),
            );
        } else {
            c.sources.extend(resolved);
        }
    }

    fn collect_function(&self, function: &Function, scope: &Scope, c: &mut ExprCollector) {
        self.collect_function_arguments(&function.parameters, scope, c);
        self.collect_function_arguments(&function.args, scope, c);
        // Aggregate `FILTER (WHERE …)` is a row-selection predicate — its
        // refs don't flow as values.
        if let Some(filter) = &function.filter {
            c.suppressed(|c| self.collect_expr(filter, scope, c));
        }
        for order_by in &function.within_group {
            self.collect_order_by_expr(order_by, scope, c);
        }
        if let Some(WindowType::WindowSpec(spec)) = &function.over {
            self.collect_window_spec(spec, scope, c);
        }
    }

    fn collect_function_arguments(
        &self,
        arguments: &FunctionArguments,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        match arguments {
            FunctionArguments::None => {}
            FunctionArguments::Subquery(query) => self.collect_subquery(query, scope, c),
            FunctionArguments::List(list) => self.collect_function_argument_list(list, scope, c),
        }
    }

    fn collect_function_argument_list(
        &self,
        list: &FunctionArgumentList,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        for arg in &list.args {
            self.collect_function_arg(arg, scope, c);
        }
        for clause in &list.clauses {
            match clause {
                FunctionArgumentClause::OrderBy(order_by) => {
                    for order_by in order_by {
                        self.collect_order_by_expr(order_by, scope, c);
                    }
                }
                // Row-count / predicate bounds inside aggregate args.
                FunctionArgumentClause::Limit(expr) => {
                    c.suppressed(|c| self.collect_expr(expr, scope, c))
                }
                FunctionArgumentClause::Having(bound) => {
                    c.suppressed(|c| self.collect_expr(&bound.1, scope, c))
                }
                FunctionArgumentClause::OnOverflow(ListAggOnOverflow::Truncate {
                    filler: Some(filler),
                    ..
                }) => self.collect_expr(filler, scope, c),
                FunctionArgumentClause::OnOverflow(_)
                | FunctionArgumentClause::IgnoreOrRespectNulls(_)
                | FunctionArgumentClause::Separator(_)
                | FunctionArgumentClause::JsonNullClause(_)
                | FunctionArgumentClause::JsonReturningClause(_) => {}
            }
        }
    }

    fn collect_function_arg(&self, arg: &FunctionArg, scope: &Scope, c: &mut ExprCollector) {
        match arg {
            FunctionArg::Named { arg, .. } | FunctionArg::Unnamed(arg) => {
                if let FunctionArgExpr::Expr(expr) = arg {
                    self.collect_expr(expr, scope, c);
                }
            }
            FunctionArg::ExprNamed { name, arg, .. } => {
                self.collect_expr(name, scope, c);
                if let FunctionArgExpr::Expr(expr) = arg {
                    self.collect_expr(expr, scope, c);
                }
            }
        }
    }

    fn collect_access(&self, access: &AccessExpr, scope: &Scope, c: &mut ExprCollector) {
        match access {
            AccessExpr::Dot(expr) => self.collect_expr(expr, scope, c),
            AccessExpr::Subscript(subscript) => match subscript {
                Subscript::Index { index } => self.collect_expr(index, scope, c),
                Subscript::Slice {
                    lower_bound,
                    upper_bound,
                    stride,
                } => {
                    for expr in [lower_bound, upper_bound, stride].into_iter().flatten() {
                        self.collect_expr(expr, scope, c);
                    }
                }
            },
        }
    }

    fn collect_dictionary_field(
        &self,
        field: &DictionaryField,
        scope: &Scope,
        c: &mut ExprCollector,
    ) {
        self.collect_expr(&field.value, scope, c);
    }

    fn collect_map(&self, map: &Map, scope: &Scope, c: &mut ExprCollector) {
        for entry in &map.entries {
            self.collect_expr(&entry.key, scope, c);
            self.collect_expr(&entry.value, scope, c);
        }
    }

    fn collect_array(&self, array: &Array, scope: &Scope, c: &mut ExprCollector) {
        for expr in &array.elem {
            self.collect_expr(expr, scope, c);
        }
    }

    /// An ORDER BY expression in value-bearing position (window / WITHIN
    /// GROUP / aggregate ORDER BY): sort keys never flow as values, so the
    /// key and any WITH FILL bounds are suppressed.
    fn collect_order_by_expr(&self, order_by: &OrderByExpr, scope: &Scope, c: &mut ExprCollector) {
        c.suppressed(|c| {
            self.collect_expr(&order_by.expr, scope, c);
            if let Some(with_fill) = &order_by.with_fill {
                for expr in [&with_fill.from, &with_fill.to, &with_fill.step]
                    .into_iter()
                    .flatten()
                {
                    self.collect_expr(expr, scope, c);
                }
            }
        });
    }

    /// A window `OVER (…)` spec: PARTITION BY keys, ORDER BY keys, and frame
    /// bounds are all row-positioning, not value sources → suppressed.
    fn collect_window_spec(&self, spec: &WindowSpec, scope: &Scope, c: &mut ExprCollector) {
        c.suppressed(|c| {
            for expr in &spec.partition_by {
                self.collect_expr(expr, scope, c);
            }
        });
        for order_by in &spec.order_by {
            self.collect_order_by_expr(order_by, scope, c);
        }
        if let Some(frame) = &spec.window_frame {
            let mut bounds = vec![&frame.start_bound];
            bounds.extend(frame.end_bound.as_ref());
            for bound in bounds {
                if let WindowFrameBound::Preceding(Some(expr))
                | WindowFrameBound::Following(Some(expr)) = bound
                {
                    c.suppressed(|c| self.collect_expr(expr, scope, c));
                }
            }
        }
    }

    /// Bind a subquery nested in an expression (with the containing
    /// `scope`'s relations pushed onto the correlation stack, so a
    /// correlated reference resolves outward). The bound sub-plan is kept
    /// whole in `subplans` (so its tables / reads surface by walking it). In
    /// value position its output columns fold into `sources` as
    /// synthetic-origin lineage sources of the enclosing value; in a filter
    /// position (`EXISTS`, `IN`, an aggregate `FILTER`) the output is a test
    /// result, not a value, so only the sub-plan is kept.
    fn collect_subquery(&self, query: &Query, scope: &Scope, c: &mut ExprCollector) {
        let binder = self.with_outer_scope(scope.relations.clone());
        let (plan, _) = binder.bind_query(query);
        if c.is_suppressed {
            // A predicate subquery (`EXISTS` / `IN` / a `CASE` condition):
            // its output doesn't feed the enclosing value, so it's a
            // non-feeding sub-plan (its tables / reads still surface).
            c.filter_subplans.push(plan);
        } else {
            // A value-position subquery (a scalar `(SELECT …)`): its output
            // folds into the enclosing value as a synthetic-origin source,
            // and the sub-plan feeds lineage.
            c.sources.extend(
                output_sources(&plan)
                    .into_iter()
                    .map(|source| ProvenanceSource {
                        synthetic_origin: true,
                        ..source
                    }),
            );
            c.value_subplans.push(plan);
        }
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
            // An *introduced* output alias (computed expr or rename) named
            // here is a reference to that output value, not a stored
            // column — return its sources marked synthetic-origin so they
            // stay lineage sources but aren't double-counted as reads (the
            // physical reads are already at the projection). An *identity*
            // alias (`GROUP BY a` for a bare `a`) falls through to normal
            // resolution, so it resolves to the real column with *this*
            // reference's own span and counts as its own occurrence.
            if let Some(output) = scope
                .outputs
                .iter()
                .find(|c| self.name_matches(c.name.as_ref(), column))
            {
                if !self.is_identity_output(output) {
                    return output
                        .provenance
                        .iter()
                        .map(|source| ProvenanceSource {
                            synthetic_origin: true,
                            ..source.clone()
                        })
                        .collect();
                }
            }
            // USING merge column: fan in to every relation that could own
            // it — one source per side — instead of resolving to one /
            // ambiguous. A catalog narrows the fan-in to declaring
            // relations (Cataloged); catalog-free it reaches every joined
            // relation (Inferred).
            if self.list_has(&scope.merge_columns, column) {
                let sources: Vec<ProvenanceSource> = scope
                    .relations
                    .iter()
                    .filter_map(|rel| self.unqualified_candidate(rel, column))
                    .flat_map(|candidate| candidate.provenance)
                    .collect();
                if !sources.is_empty() {
                    return sources;
                }
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

    /// Whether an output column is a bare-column passthrough under its own
    /// name (its single source's column name matches the output name).
    fn is_identity_output(&self, output: &BoundColumn) -> bool {
        let Some(name) = &output.name else {
            return false;
        };
        matches!(
            output.provenance.as_slice(),
            [source] if self.name_matches(Some(&source.read.reference.name), name)
        )
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
            // The qualifier is every segment but the column. Decode it into
            // a `TableReference` for right-anchored matching; `None` when it
            // overshoots `catalog.schema.name` depth (a 5+-part ref), which
            // no real table can match.
            let qualifier_parts = &parts[..parts.len() - 1];
            let qualifier_ref = TableReference::try_from_parts(qualifier_parts);
            relations
                .iter()
                .filter_map(|rel| {
                    self.qualified_candidate(rel, qualifier_parts, qualifier_ref.as_ref(), column)
                })
                .collect()
        };
        (!candidates.is_empty()).then(|| self.pick(candidates, column))
    }

    /// Reads (and any subquery sub-plans) contributed by GROUP BY (plain
    /// keys + ROLLUP / CUBE / GROUPING SETS members + a `GROUPING SETS`
    /// modifier), resolved against `scope` (which carries the output
    /// aliases).
    fn group_by_reads(
        &self,
        group_by: &GroupByExpr,
        scope: &Scope,
    ) -> (Vec<ColumnRead>, Vec<Plan>) {
        let mut reads = Vec::new();
        let mut subqueries = Vec::new();
        if let GroupByExpr::Expressions(exprs, modifiers) = group_by {
            let members =
                exprs
                    .iter()
                    .chain(modifiers.iter().filter_map(|modifier| match modifier {
                        GroupByWithModifier::GroupingSets(expr) => Some(expr),
                        _ => None,
                    }));
            for expr in members {
                let (r, s) = self.expr_reads(expr, scope);
                reads.extend(r);
                subqueries.extend(s);
            }
        }
        (reads, subqueries)
    }

    /// Reads (and any subquery sub-plans) contributed by an ORDER BY,
    /// resolved against `scope`.
    fn order_by_reads(&self, order_by: &OrderBy, scope: &Scope) -> (Vec<ColumnRead>, Vec<Plan>) {
        let OrderByKind::Expressions(exprs) = &order_by.kind else {
            return (Vec::new(), Vec::new());
        };
        let mut reads = Vec::new();
        let mut subqueries = Vec::new();
        for expr in exprs {
            let (r, s) = self.expr_reads(&expr.expr, scope);
            reads.extend(r);
            subqueries.extend(s);
        }
        (reads, subqueries)
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
            RelationSource::Derived { columns } => self.derived_candidate(rel, columns, column),
            // A table function's columns are opaque; a bare name is not
            // claimed by it (so it stays resolvable against real tables).
            RelationSource::TableFunction => None,
        }
    }

    /// A candidate owner of a qualified reference. The qualifier matches a
    /// **non-aliased real table** by right-anchored path
    /// (`catalog.schema.name`, omitted segments wildcard, table casing) and
    /// anything with a single exposed name (an aliased table / derived /
    /// CTE / table function) by a single-segment alias match (table-alias
    /// casing). A qualifier that overshoots the path depth (a 5+-part ref,
    /// `qualifier_ref` is `None`) matches no real table. When the qualifier
    /// pins a `Known` table that doesn't list the column it still resolves
    /// (`Inferred`); a derived relation that doesn't expose it contributes
    /// nothing.
    fn qualified_candidate(
        &self,
        rel: &Relation,
        qualifier_parts: &[Ident],
        qualifier_ref: Option<&TableReference>,
        column: &Ident,
    ) -> Option<Candidate> {
        let qualifier_ok = match &rel.source {
            RelationSource::Table { table, .. } if rel.alias.is_none() => {
                qualifier_ref.is_some_and(|q| self.qualifier_matches_table(q, table))
            }
            _ => rel
                .exposed_name()
                .is_some_and(|name| matches!(qualifier_parts, [only] if self.ident_eq(only, name))),
        };
        if !qualifier_ok {
            return None;
        }
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
            RelationSource::Derived { columns } => self.derived_candidate(rel, columns, column),
            // A reference qualified by a table function's alias resolves to
            // it: the produced column isn't stored, so it's marked
            // synthetic-origin — excluded from `reads`, but still a lineage
            // source (the value flows out of the function), exactly as the
            // resolver surfaces a synthetic table-function reference.
            RelationSource::TableFunction => rel.exposed_name().map(|name| {
                let table = TableReference {
                    catalog: None,
                    schema: None,
                    name: name.clone(),
                };
                Candidate {
                    provenance: vec![ProvenanceSource {
                        synthetic_origin: true,
                        ..passthrough(read(&table, column, ResolutionKind::Inferred))
                    }],
                    confirmed: true,
                    synthetic: true,
                }
            }),
        }
    }

    /// A derived relation is a candidate iff it exposes an output column
    /// named `column`; its (already collapsed) provenance is the
    /// candidate's. Synthetic — the witness tiebreaker keeps it verbatim.
    /// Every source is marked `synthetic_origin`: this reference goes
    /// *through* the derived / CTE relation, so its physical read was
    /// already counted at the relation's own body (it stays a lineage
    /// source, but not a `reads` occurrence). A column with no underlying
    /// source (a `VALUES` literal / `SELECT` constant) falls back to a
    /// synthetic self-reference (`alias.column`), so it is still a lineage
    /// source rather than vanishing.
    fn derived_candidate(
        &self,
        rel: &Relation,
        columns: &[BoundColumn],
        column: &Ident,
    ) -> Option<Candidate> {
        let exposed = columns
            .iter()
            .find(|c| self.name_matches(c.name.as_ref(), column))?;
        let provenance = if exposed.provenance.is_empty() {
            rel.exposed_name()
                .map(|name| {
                    let table = TableReference {
                        catalog: None,
                        schema: None,
                        name: name.clone(),
                    };
                    ProvenanceSource {
                        synthetic_origin: true,
                        ..passthrough(read(&table, column, ResolutionKind::Inferred))
                    }
                })
                .into_iter()
                .collect()
        } else {
            exposed
                .provenance
                .iter()
                .map(|source| ProvenanceSource {
                    synthetic_origin: true,
                    ..source.clone()
                })
                .collect()
        };
        Some(Candidate {
            provenance,
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

    /// Right-anchored match of a decoded qualifier against a real table's
    /// `catalog.schema.name`, under the dialect's *table* casing: the name
    /// must match, and each present qualifier segment must match its
    /// counterpart (an omitted segment is a wildcard, so a bare `users`
    /// matches `mydb.users` but `otherdb.users` does not).
    fn qualifier_matches_table(&self, qualifier: &TableReference, table: &TableReference) -> bool {
        let fold = self.casing.table;
        let eq = |a: &Ident, b: &Ident| fold.normalize(a) == fold.normalize(b);
        let opt_eq = |a: Option<&Ident>, b: Option<&Ident>| match (a, b) {
            (Some(x), Some(y)) => eq(x, y),
            _ => true,
        };
        eq(&qualifier.name, &table.name)
            && opt_eq(qualifier.schema.as_ref(), table.schema.as_ref())
            && opt_eq(qualifier.catalog.as_ref(), table.catalog.as_ref())
    }

    /// Canonicalize a write target to its registered full path when the
    /// catalog uniquely identifies it, so write surfaces agree with the
    /// (canonicalized) read side. An ambiguous / missing / catalog-free
    /// target — including a freshly created CTAS / view name — stays as
    /// written (`table_match` returns the written ref in those cases).
    fn canonical_target(&self, target: TableReference) -> TableReference {
        self.table_match(&target).table
    }

    /// Match a query table reference against the catalog by right-anchored,
    /// dialect-cased comparison (after default-fill). A unique hit yields
    /// the registered table's canonical identity, its column names, and
    /// `Cataloged`; several hits yield the written ref and `Ambiguous`; no
    /// catalog or no hit yields the written ref and `Inferred`. Mirrors the
    /// resolver's `catalog_match` but keeps the resolution kind so the
    /// `Scan`'s table-level resolution survives (the resolver collapsed
    /// ambiguous / miss to "open").
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
}

/// The outcome of matching a written table reference against the catalog —
/// one shape for every case, so a `Scan` gets its identity, table-level
/// [`ResolutionKind`], and (when known) column list from a single match.
struct TableMatch {
    /// Canonical identity for a unique hit; the written reference for an
    /// ambiguous / missing / catalog-free match.
    table: TableReference,
    /// `Cataloged` (unique hit), `Ambiguous` (several), or `Inferred`
    /// (no hit / no catalog).
    resolution: ResolutionKind,
    /// The registered column names for a unique hit that declared them;
    /// empty otherwise (schema-known-columns-unknown, or no hit).
    columns: Vec<Ident>,
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
fn wrap_inputs(mut inputs: Vec<Plan>, reads: Vec<ColumnRead>, subqueries: Vec<Plan>) -> Plan {
    if reads.is_empty() && subqueries.is_empty() && inputs.len() == 1 {
        inputs.pop().unwrap()
    } else {
        Plan::PassThrough(PassThrough {
            inputs,
            reads,
            subqueries,
        })
    }
}

/// Whether an `INSERT`'s source query exposes a per-column projection
/// (a `SELECT` / set-operation / nested query) rather than a `VALUES`
/// row set. Drives whether `EXCLUDED.col` collapses to the source: a
/// `VALUES` source has no projection, so `EXCLUDED` stays opaque.
fn source_has_projection(insert: &Insert) -> bool {
    insert
        .source
        .as_ref()
        .is_some_and(|query| !matches!(query.body.as_ref(), SetExpr::Values(_)))
}

/// The column names an `ALTER TABLE` operation writes to. Column-naming
/// ops (ADD / DROP / MODIFY / ALTER COLUMN) name one column; RENAME /
/// CHANGE name both the old and new (both ends of the rename are useful
/// to a lineage consumer). Schema-level ops (constraints, partitions,
/// RENAME TABLE, …) name no columns.
fn alter_table_op_target_columns(op: &AlterTableOperation) -> Vec<Ident> {
    match op {
        AlterTableOperation::AddColumn { column_def, .. } => vec![column_def.name.clone()],
        AlterTableOperation::DropColumn { column_names, .. } => column_names.clone(),
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => vec![old_column_name.clone(), new_column_name.clone()],
        AlterTableOperation::ChangeColumn {
            old_name, new_name, ..
        } if old_name != new_name => vec![old_name.clone(), new_name.clone()],
        AlterTableOperation::ChangeColumn { old_name, .. } => vec![old_name.clone()],
        AlterTableOperation::ModifyColumn { col_name, .. } => vec![col_name.clone()],
        AlterTableOperation::AlterColumn { column_name, .. } => vec![column_name.clone()],
        _ => Vec::new(),
    }
}

/// Re-tag a write target's leaf `Scan` as [`ScanRole::Write`] so
/// table-level read extraction skips it (it is reported via
/// `Write.target`). The target is still kept in the tree so its columns
/// stay in scope for resolving SET / WHERE / ON. A non-`Scan` target
/// (e.g. a joined UPDATE target) is left unchanged — that shape is a
/// later brick.
fn into_write_target(plan: Plan) -> Plan {
    match plan {
        Plan::Scan(scan) => Plan::Scan(Scan {
            role: ScanRole::Write,
            ..scan
        }),
        // A joined target (`UPDATE t1 JOIN t2 …`): only the target relation
        // (the leftmost leaf) is the write; the joined partners stay reads.
        Plan::PassThrough(mut pt) => {
            if let Some(first) = pt.inputs.first_mut() {
                let taken = std::mem::replace(first, Plan::OpaqueLeaf);
                *first = into_write_target(taken);
            }
            Plan::PassThrough(pt)
        }
        other => other,
    }
}

/// Wrap `plan` in a filter `PassThrough` carrying `reads` and the
/// `subqueries` of those predicates, or return it unchanged when there's
/// nothing to carry.
fn wrap_reads(plan: Plan, reads: Vec<ColumnRead>, subqueries: Vec<Plan>) -> Plan {
    if reads.is_empty() && subqueries.is_empty() {
        plan
    } else {
        Plan::PassThrough(PassThrough {
            inputs: vec![plan],
            reads,
            subqueries,
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

/// The lineage sources a bound plan exposes through its output columns —
/// a nested subquery's output provenance, used as the lineage sources of
/// the enclosing value (its internal filter reads are collected
/// separately).
fn output_sources(plan: &Plan) -> Vec<ProvenanceSource> {
    super::extract::output_operands(plan)
        .iter()
        .flat_map(|operand| operand.iter())
        .flat_map(|column| column.provenance.iter().cloned())
        .collect()
}

/// The last (rightmost) identifier of a possibly-qualified name — a
/// write-target column's bare name.
fn object_name_last_ident(name: &ObjectName) -> Option<Ident> {
    name.0.last().and_then(|part| part.as_ident().cloned())
}

/// The table reference of a `TABLE [schema.]name` set-expression body
/// (`SetExpr::Table`), whose parts are plain strings rather than an
/// `ObjectName`. `None` when no table name is present.
fn table_set_expr_ref(table: &Table) -> Option<TableReference> {
    let name = table.table_name.as_ref()?;
    let mut parts = Vec::new();
    if let Some(schema) = &table.schema_name {
        parts.push(Ident::new(schema));
    }
    parts.push(Ident::new(name));
    TableReference::try_from_parts(&parts)
}

/// The column(s) an assignment writes: a single `col = …` or a tuple
/// `(a, b) = …`, each reduced to its bare name.
fn assignment_target_columns(target: &AssignmentTarget) -> Vec<Ident> {
    match target {
        // A SET target is `col` up to `catalog.schema.table.col` (≤ 4
        // segments); a deeper qualifier overshoots and is skipped.
        AssignmentTarget::ColumnName(name) if name.0.len() <= 4 => {
            object_name_last_ident(name).into_iter().collect()
        }
        // Tuple targets `(a, b) = …` aren't column-paired (skipped, like
        // the resolver), and a too-deep `ColumnName` overshoots.
        AssignmentTarget::ColumnName(_) | AssignmentTarget::Tuple(_) => Vec::new(),
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

/// The lineage kind an expression contributes to its direct sources: a
/// bare column reference forwards its value (`Passthrough`); anything
/// else derives a new value (`Transformation`).
fn expr_kind(expr: &Expr) -> ColumnLineageKind {
    if matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
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
/// containing expression's kind folds in later. A direct physical
/// reference, so not synthetic.
fn passthrough(read: ColumnRead) -> ProvenanceSource {
    ProvenanceSource {
        read,
        kind: ColumnLineageKind::Passthrough,
        synthetic_origin: false,
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
        build_with_diagnostics(&statements[0], None, casing)
            .0
            .expect("supported statement")
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
            resolution: ResolutionKind::Inferred,
            role: ScanRole::Read,
        })
    }

    /// A write-target leaf `Scan` (role = Write): kept in the tree for
    /// resolution scope but skipped by table-level read extraction.
    fn write_scan(name: &str) -> Plan {
        Plan::Scan(Scan {
            table: tref(name),
            resolution: ResolutionKind::Inferred,
            role: ScanRole::Write,
        })
    }

    fn project(input: Plan, outputs: Vec<BoundColumn>) -> Plan {
        Plan::Project(Project {
            input: Box::new(input),
            outputs,
            subqueries: Vec::new(),
        })
    }

    fn pass(inputs: Vec<Plan>, reads: Vec<ColumnRead>) -> Plan {
        Plan::PassThrough(PassThrough {
            inputs,
            reads,
            subqueries: Vec::new(),
        })
    }

    /// A `WITH` node: named CTE bodies wrapping a body plan.
    fn with(ctes: Vec<(&str, Plan)>, body: Plan) -> Plan {
        Plan::With(With {
            ctes: ctes
                .into_iter()
                .map(|(name, plan)| CtePlan {
                    name: Ident::new(name),
                    plan,
                })
                .collect(),
            body: Box::new(body),
        })
    }

    /// A lightweight FROM reference to an in-scope CTE.
    fn cteref(name: &str) -> Plan {
        Plan::CteRef(CteRef {
            name: Ident::new(name),
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
                    synthetic_origin: false,
                })
                .collect(),
        }
    }

    /// A passthrough output whose sources are reached through a synthetic
    /// (derived / CTE) relation — the collapse references that are lineage
    /// sources but excluded from `reads`.
    fn synthetic_col(name: &str, reads: Vec<ColumnRead>) -> BoundColumn {
        BoundColumn {
            name: Some(Ident::new(name)),
            provenance: reads
                .into_iter()
                .map(|read| ProvenanceSource {
                    read,
                    kind: ColumnLineageKind::Passthrough,
                    synthetic_origin: true,
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
            project(
                scan("t"),
                vec![
                    passthrough_col("a", vec![inferred("t", "a")]),
                    passthrough_col("b", vec![inferred("t", "b")]),
                ],
            )
        );
    }

    #[test]
    fn join_on_and_where_become_passthrough_reads() {
        // FROM x JOIN y ON … is one PassThrough (join); WHERE wraps it in
        // another. The projection's qualified `x.a` resolves to x.
        assert_eq!(
            bind_one("SELECT x.a FROM x JOIN y ON x.id = y.id WHERE y.b > 0"),
            project(
                pass(
                    vec![pass(
                        vec![scan("x"), scan("y")],
                        vec![inferred("x", "id"), inferred("y", "id")],
                    )],
                    vec![inferred("y", "b")],
                ),
                vec![passthrough_col("a", vec![inferred("x", "a")])],
            )
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
            project(
                project(
                    scan("t"),
                    vec![passthrough_col("x", vec![inferred("t", "a")])]
                ),
                // The outer `d.x` reaches `t.a` through the derived relation,
                // so it's a synthetic-origin source (a lineage source, not a
                // physical read — that read is the inner Project's).
                vec![synthetic_col("x", vec![inferred("t", "a")])],
            )
        );
    }

    #[test]
    fn cte_reference_resolves_to_inner_columns() {
        // A WITH-bound CTE is a synthetic relation: the body's `id`
        // resolves through it to the real `t.id`, same as a derived table.
        // The CTE body lives once on the `With` node; the FROM reference is
        // a lightweight `CteRef`, not a clone of the body.
        assert_eq!(
            bind_one("WITH c AS (SELECT id FROM t) SELECT id FROM c"),
            with(
                vec![(
                    "c",
                    project(
                        scan("t"),
                        vec![passthrough_col("id", vec![inferred("t", "id")])]
                    )
                )],
                project(
                    cteref("c"),
                    // Referenced through the CTE relation → synthetic-origin.
                    vec![synthetic_col("id", vec![inferred("t", "id")])],
                ),
            )
        );
    }

    #[test]
    fn cte_referenced_twice_keeps_one_shared_body() {
        // Two references to the same CTE share its single body on the `With`
        // node (each FROM item is a `CteRef`), so the body's `t.a` is walked
        // exactly once — not duplicated per reference.
        assert_eq!(
            bind_one("WITH c AS (SELECT a FROM t) SELECT c1.a FROM c c1 JOIN c c2 ON c1.a = c2.a"),
            with(
                vec![(
                    "c",
                    project(
                        scan("t"),
                        vec![passthrough_col("a", vec![inferred("t", "a")])]
                    )
                )],
                project(
                    // JOIN of two `CteRef`s; the ON predicate `c1.a = c2.a`
                    // resolves through the CTE relations (synthetic-origin),
                    // so it contributes no physical reads here.
                    pass(vec![cteref("c"), cteref("c")], vec![]),
                    vec![synthetic_col("a", vec![inferred("t", "a")])],
                ),
            )
        );
    }

    #[test]
    fn unreferenced_cte_body_is_still_present() {
        // An unreferenced CTE's body still hangs on the `With` node (so its
        // reads surface), while the body plan reads an unrelated table.
        assert_eq!(
            bind_one("WITH c AS (SELECT a FROM t) SELECT b FROM other"),
            with(
                vec![(
                    "c",
                    project(
                        scan("t"),
                        vec![passthrough_col("a", vec![inferred("t", "a")])]
                    )
                )],
                project(
                    scan("other"),
                    vec![passthrough_col("b", vec![inferred("other", "b")])],
                ),
            )
        );
    }

    #[test]
    fn chained_ctes_resolve_through_the_chain() {
        // `b`'s body reads CTE `a`, and the outer body reads `b`. B
        // resolves the outer `id` end-to-end to the real `t.id` — an
        // improvement over the resolver (whose flat scope yields
        // Ambiguous), so this is pinned here rather than in the
        // differential-parity corpus.
        let Plan::With(with) =
            bind_one("WITH a AS (SELECT id FROM t), b AS (SELECT id FROM a) SELECT id FROM b")
        else {
            panic!("expected With");
        };
        let Plan::Project(body) = with.body.as_ref() else {
            panic!("expected body Project");
        };
        assert_eq!(
            body.outputs,
            vec![synthetic_col("id", vec![inferred("t", "id")])]
        );
    }

    #[test]
    fn subquery_in_where_is_kept_as_a_sub_plan() {
        // `b IN (SELECT id FROM u)`: the outer `b` is a direct filter read;
        // the subquery is kept whole as a sub-plan on the WHERE PassThrough
        // (walked for its `u.id`), not folded into the reads.
        let Plan::Project(outer) = bind_one("SELECT a FROM t WHERE b IN (SELECT id FROM u)") else {
            panic!("expected Project");
        };
        let Plan::PassThrough(where_pt) = outer.input.as_ref() else {
            panic!("expected WHERE PassThrough");
        };
        assert_eq!(where_pt.reads, vec![inferred("t", "b")]);
        assert_eq!(
            where_pt.subqueries,
            vec![project(
                scan("u"),
                vec![passthrough_col("id", vec![inferred("u", "id")])]
            )]
        );
    }

    #[test]
    fn correlated_subquery_resolves_outward() {
        // The EXISTS subquery is a sub-plan on the WHERE PassThrough; inside
        // it, `t.a` finds no `t` in the subquery's own scope `[u]`, so it
        // falls through the correlation stack to the outer `t`.
        let Plan::Project(project) =
            bind_one("SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.x = t.a)")
        else {
            panic!("expected Project");
        };
        let Plan::PassThrough(where_pt) = project.input.as_ref() else {
            panic!("expected WHERE PassThrough");
        };
        // The subquery's own WHERE resolves `u.x` locally and `t.a` outward.
        let [Plan::Project(subquery)] = where_pt.subqueries.as_slice() else {
            panic!("expected one subquery sub-plan");
        };
        let Plan::PassThrough(sub_where) = subquery.input.as_ref() else {
            panic!("expected the subquery's WHERE PassThrough");
        };
        assert_eq!(
            sub_where.reads,
            vec![inferred("u", "x"), inferred("t", "a")]
        );
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
            vec![synthetic_col(
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
        let Plan::With(with) = bind_one(
            "WITH RECURSIVE c AS (SELECT id FROM t UNION ALL SELECT id FROM c) SELECT id FROM c",
        ) else {
            panic!("expected With");
        };
        let Plan::Project(body) = with.body.as_ref() else {
            panic!("expected body Project");
        };
        assert_eq!(
            body.outputs,
            vec![synthetic_col(
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
                input: Box::new(project(
                    scan("source"),
                    vec![
                        passthrough_col("x", vec![inferred("source", "x")]),
                        passthrough_col("y", vec![inferred("source", "y")]),
                    ],
                )),
                returning: vec![],
                conflict_updates: vec![],
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
                input: Box::new(project(
                    pass(vec![write_scan("t")], vec![inferred("t", "d")]),
                    vec![transform_col(
                        "c",
                        vec![inferred("t", "a"), inferred("t", "b")]
                    )],
                )),
                returning: vec![],
                conflict_updates: vec![],
            })
        );
    }

    #[test]
    fn delete_reads_predicate_and_writes_no_columns() {
        // DELETE removes whole rows: the predicate is a read, but there are
        // no column writes. The target is a write-role scan in the input
        // (in scope, not a read) and surfaces via `targets`.
        assert_eq!(
            bind_one("DELETE FROM t WHERE d > 0"),
            Plan::Delete(DeletePlan {
                input: Box::new(pass(vec![write_scan("t")], vec![inferred("t", "d")])),
                targets: vec![tref("t")],
                returning: vec![],
            })
        );
    }

    #[test]
    fn using_merge_column_fans_in() {
        // `JOIN y USING (a)` makes the unqualified `a` fan in to both
        // sides (one Inferred source each), not resolve to an ambiguous
        // single column.
        let Plan::Project(project) = bind_one("SELECT a FROM x JOIN y USING (a)") else {
            panic!("expected Project");
        };
        assert_eq!(
            project.outputs,
            vec![passthrough_col(
                "a",
                vec![inferred("x", "a"), inferred("y", "a")]
            )]
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
