//! The binder: lowers a `sqlparser` AST into the bound [`Operator`] tree,
//! resolving every column reference against the bind-time scope.
//!
//! Bricks ③–⑤: catalog-aware SELECT / FROM / JOIN / WHERE / projection,
//! GROUP BY / HAVING / ORDER BY, derived tables, CTEs, and set operations.
//! A table factor is matched against the catalog (right-anchored,
//! dialect-cased) into a canonical identity with its `Known` columns and
//! table-level [`ResolutionKind`], or `Open` (catalog-free / miss / ambiguous).
//! Column resolution ranks the in-scope relations: a Known-witness over an Open
//! suspect downgrades to `Inferred`, several owners give `Ambiguous`, none
//! `Unresolved`, and a derived / CTE relation that exposes the column gives
//! `Derived`. GROUP BY / HAVING / ORDER BY resolve against the FROM relations
//! together with the projection outputs (clause-alias visibility): an
//! introduced alias becomes `Derived` (dropped from reads), an identity
//! passthrough falls through to the real column. Their reads ride on a filter /
//! sort above the projection.
//!
//! Brick ⑥ adds the DML roots: INSERT (SELECT / VALUES source), UPDATE, and
//! DELETE. A DML target is in scope for resolving SET / WHERE but is the
//! **write target** named on the root, never a read scan — so the root's
//! `input` carries only the read relations and the predicate. The remaining
//! breadth (MERGE / DDL / RETURNING / ON CONFLICT / diagnostics) is later
//! bricks; unhandled constructs fall to [`Operator::Empty`].

use sqlparser::ast::{
    AlterTable as SqlAlterTable, AlterTableOperation, AssignmentTarget, CreateTable,
    CreateView as SqlCreateView, Cte as SqlCte, Delete as SqlDelete, Expr as SqlExpr, FromTable,
    Function, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, GroupByWithModifier,
    Ident, Insert as SqlInsert, JoinConstraint, JoinOperator, Merge as SqlMerge, MergeAction,
    MergeInsertKind, ObjectName, ObjectType, OnConflictAction, OnInsert, OrderBy, OrderByExpr,
    OrderByKind, PivotValueSource, Query, Select, SelectItem, SetExpr, Statement, TableAlias,
    TableFactor, TableObject, TableWithJoins, Update as SqlUpdate, UpdateTableFromKind,
    Values as SqlValues,
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
    Binder {
        catalog,
        casing,
        ctes: Vec::new(),
        outer: Vec::new(),
    }
    .bind_statement(statement)
}

/// A CTE in scope: its name and the output column names it exposes (so a
/// `FROM cte` reference resolves through them). The body lives once on the
/// owning `With` node; a reference is a lightweight `CteRef`.
#[derive(Clone)]
struct CteEnv {
    name: Ident,
    columns: Vec<Ident>,
}

// ===== bind-time scope (scratch, relation-grouped) =======================

/// The relations visible at a point in the bind. Scratch — never stored on
/// the [`Operator`] tree.
#[derive(Default)]
struct Scope {
    relations: Vec<Relation>,
    /// The enclosing SELECT's output columns, visible to its own GROUP BY /
    /// HAVING / ORDER BY (clause-alias visibility). Empty at FROM-level
    /// resolution (WHERE / projection).
    outputs: Vec<OutputCol>,
}

/// A projection output column, for clause-alias resolution. `identity` marks a
/// bare passthrough (`SELECT a`): a clause reference to it falls through to
/// the real column (a read); a non-identity (introduced) alias resolves to the
/// output itself (`Binding::Derived`, dropped from reads).
struct OutputCol {
    name: Option<Ident>,
    identity: bool,
}

/// A relation in scope: its use-site alias (if any) and where its columns
/// come from. Cloned onto the correlation stack so an inner subquery can
/// resolve against enclosing relations.
#[derive(Clone)]
struct Relation {
    alias: Option<Ident>,
    source: RelSource,
}

#[derive(Clone)]
enum RelSource {
    /// A real table: its canonical identity, catalog column knowledge, and
    /// table-level resolution.
    Table {
        table: TableReference,
        columns: Columns,
        resolution: ResolutionKind,
    },
    /// A derived table / CTE reference: a synthetic relation exposing the named
    /// output columns of an inner query. A reference through it is
    /// `Binding::Derived` — the origin traversal traces into the producing
    /// sub-plan (`SubqueryAlias` / `CteRef`).
    Derived { columns: Vec<Ident> },
    /// An opaque table function / PIVOT / … relation with dynamic columns. A
    /// bare name is **not** claimed by it (so it stays resolvable against real
    /// tables); a qualified ref through its alias is `Binding::Derived` — the
    /// origin traversal reaches the [`Operator::TableFunction`] node and emits
    /// the synthetic `alias.col` source (a lineage source, dropped from reads).
    TableFunction,
}

impl Relation {
    /// The name this relation answers to in a qualifier: its alias, else a
    /// real table's bare name. A derived relation answers only to its alias.
    fn exposed_name(&self) -> Option<&Ident> {
        self.alias.as_ref().or(match &self.source {
            RelSource::Table { table, .. } => Some(&table.name),
            RelSource::Derived { .. } | RelSource::TableFunction => None,
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
    /// CTEs in scope (declaration order, innermost `WITH` last).
    ctes: Vec<CteEnv>,
    /// Enclosing queries' relations (the correlation stack, outermost first)
    /// that an inner subquery's references fall through to.
    outer: Vec<Vec<Relation>>,
}

impl Binder<'_> {
    /// A child binder with a different CTE environment (sharing catalog /
    /// casing / correlation stack).
    fn with_ctes(&self, ctes: Vec<CteEnv>) -> Binder<'_> {
        Binder {
            catalog: self.catalog,
            casing: self.casing,
            ctes,
            outer: self.outer.clone(),
        }
    }

    /// A child binder with one more enclosing scope on the correlation stack
    /// (used when descending into a subquery in an expression / a LATERAL
    /// factor).
    fn with_outer(&self, relations: Vec<Relation>) -> Binder<'_> {
        let mut outer = self.outer.clone();
        outer.push(relations);
        Binder {
            catalog: self.catalog,
            casing: self.casing,
            ctes: self.ctes.clone(),
            outer,
        }
    }

    fn bind_statement(&self, statement: &Statement) -> Operator {
        match statement {
            Statement::Query(query) => self.bind_query(query).0,
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
            Statement::Truncate(truncate) => Operator::Drop(super::operator::Drop {
                targets: truncate
                    .table_names
                    .iter()
                    .filter_map(|t| TableReference::try_from_name(&t.name).ok())
                    .map(|written| self.table_match(&written).table)
                    .collect(),
            }),
            _ => Operator::Empty,
        }
    }

    /// `INSERT INTO target (columns) <source>`: the source query's plan is the
    /// read-carrying `input`, whose output columns pair positionally with the
    /// target columns for relation lineage. An explicit column list wins;
    /// otherwise the target's catalog columns fill in, truncated to the
    /// source's arity (so a column-less `INSERT … SELECT a, b` writes the
    /// target's first two columns). A `VALUES` source binds to
    /// [`Operator::Values`] (the rows are reads, but synthesise no traceable
    /// output, so there is no column lineage). RETURNING / ON CONFLICT / the
    /// MySQL `SET` form are later bricks.
    fn bind_insert(&self, insert: &SqlInsert) -> Operator {
        let name = match &insert.table {
            TableObject::TableName(name) => name,
            TableObject::TableFunction(function) => &function.name,
        };
        let Ok(written) = TableReference::try_from_name(name) else {
            return Operator::Empty;
        };
        let target = self.table_match(&written).table;
        // MySQL `INSERT INTO t SET col = expr, …`: the assignment form (no
        // VALUES / SELECT source) — each assignment is a value column named by
        // its target, like a single-row UPDATE.
        if insert.source.is_none() && !insert.assignments.is_empty() {
            return self.bind_insert_set(insert, target);
        }
        let (input, scope) = match &insert.source {
            Some(source) => self.bind_query(source),
            None => (Operator::Empty, Scope::default()),
        };
        let columns = if insert.columns.is_empty() {
            self.table_match(&target)
                .columns
                .into_iter()
                .take(scope.outputs.len())
                .collect()
        } else {
            insert.columns.clone()
        };
        // ON CONFLICT DO UPDATE / ON DUPLICATE KEY UPDATE: extra writes + their
        // `value → target.col` lineage, plus the optional `DO UPDATE … WHERE`
        // (filter reads).
        let (on_conflict, conflict_predicate) = match &insert.on {
            Some(on) => self.bind_conflict(on, &target, &columns),
            None => (Vec::new(), Vec::new()),
        };
        // RETURNING resolves against the target alone (the source query's
        // scope is already popped).
        let returning = self.bind_returning(&insert.returning, &self.target_scope(&target));
        Operator::Insert(super::operator::Insert {
            target,
            columns,
            input: Box::new(input),
            returning,
            on_conflict,
            conflict_predicate,
        })
    }

    /// MySQL `INSERT INTO t SET col = expr, …`: bind the assignment form like a
    /// single-row UPDATE — each assignment is a value column named by its target
    /// (resolved against the target's own columns), placed in a `Project` so the
    /// `value → target.col` lineage reuses the relation-lineage machinery.
    fn bind_insert_set(&self, insert: &SqlInsert, target: TableReference) -> Operator {
        let scope = self.target_scope(&target);
        let mut columns = Vec::new();
        let mut exprs = Vec::new();
        for assignment in &insert.assignments {
            for column in assignment_target_columns(&assignment.target) {
                exprs.push(NamedExpr {
                    name: Some(column.clone()),
                    expr: self.bind_expr(&assignment.value, &scope),
                });
                columns.push(column);
            }
        }
        let (on_conflict, conflict_predicate) = match &insert.on {
            Some(on) => self.bind_conflict(on, &target, &columns),
            None => (Vec::new(), Vec::new()),
        };
        let returning = self.bind_returning(&insert.returning, &scope);
        Operator::Insert(super::operator::Insert {
            target,
            columns,
            input: Box::new(Operator::Project(Project {
                input: Box::new(Operator::Empty),
                exprs,
            })),
            returning,
            on_conflict,
            conflict_predicate,
        })
    }

    /// Bind an INSERT's conflict action. PG / SQLite `ON CONFLICT DO UPDATE`
    /// puts the `EXCLUDED` pseudo-table in scope — a synthetic relation
    /// exposing the target columns, so `EXCLUDED.col` resolves to a `Derived`
    /// ref the traversal maps back to the source's like-positioned output.
    /// MySQL `ON DUPLICATE KEY UPDATE` has no EXCLUDED (its `VALUES(col)`
    /// self-references the target). Returns the conflict assignments (extra
    /// writes + lineage) and the optional `DO UPDATE … WHERE` (filter reads).
    fn bind_conflict(
        &self,
        on: &OnInsert,
        target: &TableReference,
        columns: &[Ident],
    ) -> (Vec<super::operator::Assignment>, Vec<Expr>) {
        let (scope, assignments, selection) = match on {
            OnInsert::DuplicateKeyUpdate(assignments) => {
                (self.target_scope(target), assignments.as_slice(), None)
            }
            OnInsert::OnConflict(on_conflict) => match &on_conflict.action {
                OnConflictAction::DoUpdate(do_update) => {
                    let mut scope = self.target_scope(target);
                    scope.relations.push(Relation {
                        alias: Some(Ident::new("excluded")),
                        source: RelSource::Derived {
                            columns: columns.to_vec(),
                        },
                    });
                    (
                        scope,
                        do_update.assignments.as_slice(),
                        do_update.selection.as_ref(),
                    )
                }
                OnConflictAction::DoNothing => return (Vec::new(), Vec::new()),
            },
            // `OnInsert` is non-exhaustive; an unmodelled action is a no-op.
            _ => return (Vec::new(), Vec::new()),
        };
        let bound = assignments
            .iter()
            .flat_map(|a| {
                assignment_target_columns(&a.target)
                    .into_iter()
                    .map(move |column| (column, &a.value))
            })
            .map(|(column, value)| super::operator::Assignment {
                target: column,
                value: self.bind_expr(value, &scope),
            })
            .collect();
        let predicate = selection
            .map(|s| self.bind_expr(s, &scope))
            .into_iter()
            .collect();
        (bound, predicate)
    }

    /// `UPDATE target SET col = expr [FROM src] WHERE pred`: the target is in
    /// scope for resolving SET / WHERE but is the **write target** (named on
    /// `Update.target`), not a read scan — so `input` carries only the read
    /// relations (the target-clause joins and the `FROM` relations) plus the
    /// WHERE predicate as a `Filter`. The SET assignments are the value path
    /// (each `RHS → target.col` for lineage / writes). RETURNING / the MySQL
    /// multi-table form's exotic shapes are later bricks.
    fn bind_update(&self, update: &SqlUpdate) -> Operator {
        let Some((target_relation, target)) = self.target_relation(&update.table.relation) else {
            return Operator::Empty;
        };
        let mut scope = scope_of(target_relation);
        let mut input = Operator::Empty;
        // Joins on the UPDATE target clause are read relations.
        for j in &update.table.joins {
            let (node, jscope) = self.bind_table_factor(&j.relation, &scope.relations);
            scope.relations.extend(jscope.relations);
            let on = join_on(&j.join_operator)
                .map(|e| self.bind_expr(e, &scope))
                .into_iter()
                .collect();
            input = join(input, node, on);
        }
        // FROM relations are reads (resolved against the target + joins so far).
        if let Some(from) = &update.from {
            let tables = match from {
                UpdateTableFromKind::BeforeSet(t) | UpdateTableFromKind::AfterSet(t) => t,
            };
            for twj in tables {
                let (node, fscope) = self.bind_table_with_joins(twj, &scope.relations);
                scope.relations.extend(fscope.relations);
                input = combine(input, node);
            }
        }
        // WHERE: a filter over the read input (picks which rows update; its
        // reads / subqueries do not feed the new value).
        if let Some(predicate) = &update.selection {
            input = Operator::Filter(Filter {
                input: Box::new(input),
                predicate: vec![self.bind_expr(predicate, &scope)],
            });
        }
        // SET assignments resolve against the target + FROM scope.
        let assignments = update
            .assignments
            .iter()
            .flat_map(|a| {
                assignment_target_columns(&a.target)
                    .into_iter()
                    .map(move |column| (column, &a.value))
            })
            .map(|(column, value)| super::operator::Assignment {
                target: column,
                value: self.bind_expr(value, &scope),
            })
            .collect();
        // RETURNING resolves against the statement scope (target + FROM).
        let returning = self.bind_returning(&update.returning, &scope);
        Operator::Update(super::operator::Update {
            target,
            assignments,
            input: Box::new(input),
            returning,
        })
    }

    /// `DELETE`: the deletion targets, plus the consulted read relations and
    /// the predicate as the `input`. The FROM clause's role depends on the
    /// shape (mirroring the resolver):
    ///   `DELETE FROM t`                → FROM is the (write) target
    ///   `DELETE FROM t1, t2 USING src` → FROM are targets, USING are reads
    ///   `DELETE t1, t2 FROM src`       → FROM are reads, the list are targets
    /// A target is in scope for the predicate but never scanned (so it isn't a
    /// read). There are no column writes / lineage — rows go wholesale.
    fn bind_delete(&self, delete: &SqlDelete) -> Operator {
        let from_tables = match &delete.from {
            FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
        };
        let from_is_target = delete.tables.is_empty();
        let mut scope = Scope::default();
        let mut input = Operator::Empty;
        let mut targets = Vec::new();
        // USING relations are always reads. Bind first so an explicit target
        // alias can resolve against them.
        for twj in delete.using.iter().flatten() {
            let (node, uscope) = self.bind_table_with_joins(twj, &scope.relations);
            scope.relations.extend(uscope.relations);
            input = combine(input, node);
        }
        for twj in from_tables {
            if from_is_target {
                // The FROM relations are the deletion targets: in scope for the
                // predicate, but not read.
                let (_node, fscope) = self.bind_table_with_joins(twj, &scope.relations);
                targets.extend(self.twj_table_targets(twj));
                scope.relations.extend(fscope.relations);
            } else {
                let (node, fscope) = self.bind_table_with_joins(twj, &scope.relations);
                scope.relations.extend(fscope.relations);
                input = combine(input, node);
            }
        }
        // An explicit `DELETE t1, … FROM …` list names the targets (each may be
        // a FROM alias, resolved through the scope).
        for name in &delete.tables {
            if let Some(target) = self.resolve_delete_target(name, &scope) {
                targets.push(target);
            }
        }
        if let Some(predicate) = &delete.selection {
            input = Operator::Filter(Filter {
                input: Box::new(input),
                predicate: vec![self.bind_expr(predicate, &scope)],
            });
        }
        // RETURNING resolves against the FROM / USING scope (which holds the
        // target).
        let returning = self.bind_returning(&delete.returning, &scope);
        Operator::Delete(super::operator::Delete {
            targets,
            input: Box::new(input),
            returning,
        })
    }

    /// `MERGE INTO target USING source ON pred WHEN … THEN …`: the target
    /// (write, in scope but not scanned) and source (read) form the scope. The
    /// ON predicate and every per-clause / INSERT predicate are filter reads
    /// (non-feeding), folded onto `on`. Each WHEN action keeps its structure as
    /// a `MergeClause`: an UPDATE SET's `RHS → target.col` and an INSERT's
    /// `value → target.col` drive writes / lineage. A column-less INSERT fills
    /// from the catalog (empty without one — the values are then reads only).
    fn bind_merge(&self, merge: &SqlMerge) -> Operator {
        let Some((target_relation, target)) = self.target_relation(&merge.table) else {
            return Operator::Empty;
        };
        let mut scope = scope_of(target_relation);
        let (source, source_scope) = self.bind_table_factor(&merge.source, &scope.relations);
        scope.relations.extend(source_scope.relations);

        let mut on = vec![self.bind_expr(&merge.on, &scope)];
        let mut clauses = Vec::new();
        for clause in &merge.clauses {
            if let Some(predicate) = &clause.predicate {
                on.push(self.bind_expr(predicate, &scope));
            }
            match &clause.action {
                MergeAction::Insert(insert) => {
                    if let Some(predicate) = &insert.insert_predicate {
                        on.push(self.bind_expr(predicate, &scope));
                    }
                    if let MergeInsertKind::Values(values) = &insert.kind {
                        let explicit: Vec<Ident> = insert
                            .columns
                            .iter()
                            .filter_map(|n| n.0.last().and_then(|p| p.as_ident().cloned()))
                            .collect();
                        let columns = if explicit.is_empty() {
                            self.table_match(&target).columns
                        } else {
                            explicit
                        };
                        // A MERGE INSERT is a single VALUES row.
                        let row = values
                            .rows
                            .iter()
                            .flatten()
                            .map(|e| self.bind_expr(e, &scope))
                            .collect();
                        clauses.push(super::operator::MergeClause::Insert {
                            columns,
                            values: row,
                        });
                    }
                }
                MergeAction::Update(update) => {
                    for predicate in [&update.update_predicate, &update.delete_predicate]
                        .into_iter()
                        .flatten()
                    {
                        on.push(self.bind_expr(predicate, &scope));
                    }
                    let assignments = update
                        .assignments
                        .iter()
                        .flat_map(|a| {
                            assignment_target_columns(&a.target)
                                .into_iter()
                                .map(move |column| (column, &a.value))
                        })
                        .map(|(column, value)| super::operator::Assignment {
                            target: column,
                            value: self.bind_expr(value, &scope),
                        })
                        .collect();
                    clauses.push(super::operator::MergeClause::Update { assignments });
                }
                MergeAction::Delete { .. } => {
                    clauses.push(super::operator::MergeClause::Delete);
                }
            }
        }
        Operator::Merge(super::operator::Merge {
            target,
            source: Box::new(source),
            on,
            clauses,
        })
    }

    /// Resolve a DML target table factor into its scope relation (for
    /// resolving SET / WHERE against the target's columns) and its canonical
    /// write-target identity. Returns `None` for a non-table factor.
    fn target_relation(&self, factor: &TableFactor) -> Option<(Relation, TableReference)> {
        // Reuse the table-factor binder for catalog matching / column
        // knowledge, then discard the scan — the target is named on the DML
        // root, not read.
        let (_scan, scope) = self.bind_table_factor(factor, &[]);
        let relation = scope.relations.into_iter().next()?;
        match &relation.source {
            RelSource::Table { table, .. } => {
                let target = table.clone();
                Some((relation, target))
            }
            RelSource::Derived { .. } | RelSource::TableFunction => None,
        }
    }

    /// The plain-table deletion targets of a FROM `TableWithJoins` (its
    /// relation plus any joined relations), catalog-canonicalised.
    fn twj_table_targets(&self, twj: &TableWithJoins) -> Vec<TableReference> {
        std::iter::once(&twj.relation)
            .chain(twj.joins.iter().map(|join| &join.relation))
            .filter_map(|factor| TableReference::try_from(factor).ok())
            .map(|written| self.table_match(&written).table)
            .collect()
    }

    /// Resolve an explicit `DELETE` target name to its real table: a
    /// single-segment name may be a FROM alias (or the bare name of an
    /// in-scope relation), so consult the scope first; otherwise canonicalise
    /// it as written.
    fn resolve_delete_target(&self, name: &ObjectName, scope: &Scope) -> Option<TableReference> {
        let written = TableReference::try_from_name(name).ok()?;
        Some(
            self.scope_target(&written, scope)
                .unwrap_or_else(|| self.table_match(&written).table),
        )
    }

    /// If a `written` DELETE-target name matches an in-scope real-table
    /// relation by **merge identity**, return that relation's real table. An
    /// aliased relation matches a single-segment name against its alias; a
    /// non-aliased relation matches its full `catalog.schema.name` path exactly
    /// (so a bare `t1` merges with FROM `t1` but not FROM `mydb.t1`).
    fn scope_target(&self, written: &TableReference, scope: &Scope) -> Option<TableReference> {
        let canonical = self.table_match(written).table;
        scope
            .relations
            .iter()
            .find_map(|relation| match &relation.source {
                RelSource::Table { table, .. } => {
                    let matches = match &relation.alias {
                        Some(alias) => {
                            written.schema.is_none()
                                && written.catalog.is_none()
                                && self.eq(self.casing.table_alias, alias, &written.name)
                        }
                        None => self.table_identity_eq(&canonical, table),
                    };
                    matches.then(|| table.clone())
                }
                RelSource::Derived { .. } | RelSource::TableFunction => None,
            })
    }

    /// Exact (not right-anchored) identity match of two table references under
    /// the dialect's table casing — every present segment must agree and a
    /// missing segment matches only a missing one.
    fn table_identity_eq(&self, a: &TableReference, b: &TableReference) -> bool {
        let fold = self.casing.table;
        let seg_eq = |x: Option<&Ident>, y: Option<&Ident>| match (x, y) {
            (Some(p), Some(q)) => self.eq(fold, p, q),
            (None, None) => true,
            _ => false,
        };
        self.eq(fold, &a.name, &b.name)
            && seg_eq(a.schema.as_ref(), b.schema.as_ref())
            && seg_eq(a.catalog.as_ref(), b.catalog.as_ref())
    }

    /// A resolution scope holding just a write target (for `INSERT … RETURNING`,
    /// whose references resolve against the target alone — the source query's
    /// scope is already popped).
    fn target_scope(&self, target: &TableReference) -> Scope {
        let m = self.table_match(target);
        let columns = if m.columns.is_empty() {
            Columns::Open
        } else {
            Columns::Known(m.columns)
        };
        scope_of(Relation {
            alias: None,
            source: RelSource::Table {
                table: m.table,
                columns,
                resolution: m.resolution,
            },
        })
    }

    /// Bind a `RETURNING` clause's projected columns against `scope` — a value
    /// projection over the written relation, like a SELECT list, so each item
    /// contributes target reads and a `QueryOutput` lineage edge. A wildcard is
    /// suppressed (its diagnostic is a later brick).
    fn bind_returning(&self, returning: &Option<Vec<SelectItem>>, scope: &Scope) -> Vec<NamedExpr> {
        returning
            .iter()
            .flatten()
            .filter_map(|item| self.bind_select_item(item, scope))
            .collect()
    }

    /// `CREATE TABLE dst AS <query>` (CTAS): the source query's reads, paired
    /// with the new table's columns (explicit defs win, else the source output
    /// names). A plain `CREATE TABLE t (cols)` (no query) is a target-only
    /// create — its column definitions aren't writes — so it binds with no
    /// columns / input.
    fn bind_create_table(&self, create: &CreateTable) -> Operator {
        let Ok(written) = TableReference::try_from_name(&create.name) else {
            return Operator::Empty;
        };
        let target = self.table_match(&written).table;
        let Some(query) = create.query.as_ref() else {
            return Operator::CreateTableAs(super::operator::CreateTableAs {
                target,
                columns: Vec::new(),
                input: Box::new(Operator::Empty),
            });
        };
        let (input, scope) = self.bind_query(query);
        let columns = if create.columns.is_empty() {
            exposed_columns(&scope.outputs, None)
        } else {
            create.columns.iter().map(|c| c.name.clone()).collect()
        };
        Operator::CreateTableAs(super::operator::CreateTableAs {
            target,
            columns,
            input: Box::new(input),
        })
    }

    /// `CREATE VIEW v AS <query>`: like CTAS — the query's reads paired with
    /// the view's columns (explicit list wins, else source names).
    fn bind_create_view(&self, create: &SqlCreateView) -> Operator {
        let Ok(written) = TableReference::try_from_name(&create.name) else {
            return Operator::Empty;
        };
        let target = self.table_match(&written).table;
        let (input, scope) = self.bind_query(&create.query);
        let columns = if create.columns.is_empty() {
            exposed_columns(&scope.outputs, None)
        } else {
            create.columns.iter().map(|c| c.name.clone()).collect()
        };
        Operator::CreateView(super::operator::CreateView {
            target,
            columns,
            input: Box::new(input),
        })
    }

    /// `ALTER VIEW v AS <query>`: a view replacement — bound like
    /// [`bind_create_view`](Self::bind_create_view).
    fn bind_alter_view(&self, name: &ObjectName, columns: &[Ident], query: &Query) -> Operator {
        let Ok(written) = TableReference::try_from_name(name) else {
            return Operator::Empty;
        };
        let target = self.table_match(&written).table;
        let (input, scope) = self.bind_query(query);
        let columns = if columns.is_empty() {
            exposed_columns(&scope.outputs, None)
        } else {
            columns.to_vec()
        };
        Operator::CreateView(super::operator::CreateView {
            target,
            columns,
            input: Box::new(input),
        })
    }

    /// `ALTER TABLE t <ops>`: the altered table is a write target; each
    /// column-naming operation contributes its column(s) as writes (RENAME /
    /// CHANGE surface both names). Schema-level ops name no columns. No reads
    /// or lineage — ALTER restructures, it doesn't move row data.
    fn bind_alter_table(&self, alter: &SqlAlterTable) -> Operator {
        let Ok(written) = TableReference::try_from_name(&alter.name) else {
            return Operator::Empty;
        };
        let target = self.table_match(&written).table;
        let columns = alter
            .operations
            .iter()
            .flat_map(alter_table_op_target_columns)
            .collect();
        Operator::AlterTable(super::operator::AlterTable { target, columns })
    }

    /// `DROP TABLE/VIEW/MATERIALIZED VIEW a, b`: the dropped relations are
    /// write targets. Other object types (index / schema / …) name no
    /// relations — unbound (`Operator::Empty`).
    fn bind_drop(
        &self,
        object_type: &ObjectType,
        names: &[ObjectName],
        table: Option<&ObjectName>,
    ) -> Operator {
        if !matches!(
            object_type,
            ObjectType::Table | ObjectType::View | ObjectType::MaterializedView
        ) {
            return Operator::Empty;
        }
        let targets = names
            .iter()
            .chain(table)
            .filter_map(|name| TableReference::try_from_name(name).ok())
            .map(|written| self.table_match(&written).table)
            .collect();
        Operator::Drop(super::operator::Drop { targets })
    }

    /// Bind a query, returning the operator and its output scope (relations +
    /// outputs). A leading `WITH` is peeled first: each CTE binds in
    /// declaration order into an environment the later CTEs and the body
    /// resolve against; the bodies are owned by a `With` node, references are
    /// `CteRef`s.
    fn bind_query(&self, query: &Query) -> (Operator, Scope) {
        let Some(with) = &query.with else {
            return self.bind_query_body(query);
        };
        let mut env = self.ctes.clone();
        let mut declared = Vec::new();
        for cte in &with.cte_tables {
            let (entry, plan) = self.bind_cte(cte, &env, with.recursive);
            declared.push(super::operator::Cte {
                name: entry.name.clone(),
                plan,
            });
            env.push(entry);
        }
        let (body, scope) = self.with_ctes(env).bind_query_body(query);
        (
            Operator::With(super::operator::With {
                ctes: declared,
                body: Box::new(body),
            }),
            scope,
        )
    }

    /// Bind one declared CTE against the environment of the earlier CTEs,
    /// returning its scope entry (name + exposed columns) and its bound body.
    /// A `RECURSIVE` CTE registers its name provisionally first so the body's
    /// self-reference resolves: a set-operation body learns its column shape
    /// from the anchor (the left branch); any other body registers with no
    /// known columns (so a self-reference is still a `CteRef`, not a phantom).
    fn bind_cte(&self, cte: &SqlCte, env: &[CteEnv], recursive: bool) -> (CteEnv, Operator) {
        let name = cte.alias.name.clone();
        let inner_env = if recursive {
            let provisional = match cte.query.body.as_ref() {
                SetExpr::SetOperation { left, .. } => exposed_columns(
                    &self.with_ctes(env.to_vec()).bind_set_expr(left).1.outputs,
                    Some(&cte.alias),
                ),
                _ => Vec::new(),
            };
            let mut e = env.to_vec();
            e.push(CteEnv {
                name: name.clone(),
                columns: provisional,
            });
            e
        } else {
            env.to_vec()
        };
        let (mut plan, scope) = self.with_ctes(inner_env).bind_query(&cte.query);
        let columns = exposed_columns(&scope.outputs, Some(&cte.alias));
        // An explicit `c (x, y)` column list renames the body's output columns
        // so a reference through the CTE traces to them.
        rename_outputs(&mut plan, &alias_column_names(&cte.alias));
        (CteEnv { name, columns }, plan)
    }

    /// Bind a query's body and trailing ORDER BY (the `WITH` is already in
    /// scope via `self.ctes`).
    fn bind_query_body(&self, query: &Query) -> (Operator, Scope) {
        let (mut op, scope) = self.bind_set_expr(&query.body);
        if let Some(order_by) = &query.order_by {
            let keys = self.order_by_keys(order_by, &scope);
            if !keys.is_empty() {
                op = sort(op, keys);
            }
        }
        (op, scope)
    }

    fn bind_set_expr(&self, body: &SetExpr) -> (Operator, Scope) {
        match body {
            SetExpr::Select(select) => self.bind_select(select),
            SetExpr::Query(query) => self.bind_query(query),
            SetExpr::Values(values) => self.bind_values(values),
            // `WITH … INSERT/UPDATE/DELETE/MERGE …`: the DML statement is the
            // query body (the parser wraps a CTE-prefixed DML this way). Bind it
            // to its DML root; it exposes no output scope to an enclosing query.
            SetExpr::Insert(statement)
            | SetExpr::Update(statement)
            | SetExpr::Delete(statement)
            | SetExpr::Merge(statement) => (self.bind_statement(statement), Scope::default()),
            // A set operation: result columns are the left operand's (names
            // from the left, positional merge).
            SetExpr::SetOperation { left, right, .. } => {
                let (l, scope) = self.bind_set_expr(left);
                let (r, _) = self.bind_set_expr(right);
                (
                    Operator::SetOp(super::operator::SetOp {
                        left: Box::new(l),
                        right: Box::new(r),
                    }),
                    scope,
                )
            }
            _ => (Operator::Empty, Scope::default()),
        }
    }

    /// Bind a `VALUES (…), (…)` row set into [`Operator::Values`]: one
    /// anonymous output per column position (synthesised rows have no base
    /// columns, so a reference to a `(VALUES …) AS v(x)` column is `Derived`
    /// and traces to nothing — there is no column lineage). The row
    /// expressions are reads, resolved against the empty current scope and
    /// falling through to the correlation stack (a `(VALUES (t.a)) AS v` reads
    /// the enclosing / sibling `t.a` like a derived subquery's body).
    fn bind_values(&self, values: &SqlValues) -> (Operator, Scope) {
        let width = values.rows.iter().map(Vec::len).max().unwrap_or(0);
        let rows: Vec<Vec<Expr>> = values
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|expr| self.bind_expr(expr, &Scope::default()))
                    .collect()
            })
            .collect();
        let outputs = (0..width)
            .map(|_| OutputCol {
                name: None,
                identity: false,
            })
            .collect();
        (
            Operator::Values(super::operator::Values { rows }),
            Scope {
                relations: Vec::new(),
                outputs,
            },
        )
    }

    /// Bind a SELECT, returning the operator and its clause scope (FROM
    /// relations + projection outputs) for a trailing ORDER BY to resolve
    /// against.
    fn bind_select(&self, select: &Select) -> (Operator, Scope) {
        let (from, scope) = self.bind_from(&select.from);
        // WHERE: a filter over the FROM (no output aliases visible — the
        // clause-phase rule, structural).
        let mut node = match &select.selection {
            Some(predicate) => Operator::Filter(Filter {
                input: Box::new(from),
                predicate: vec![self.bind_expr(predicate, &scope)],
            }),
            None => from,
        };
        // SELECT: the projection resolves against the FROM scope.
        let exprs: Vec<NamedExpr> = select
            .projection
            .iter()
            .filter_map(|item| self.bind_select_item(item, &scope))
            .collect();
        let outputs = self.output_cols(&exprs);
        node = Operator::Project(Project {
            input: Box::new(node),
            exprs,
        });
        // GROUP BY / HAVING / SORT BY see the output aliases (clause phase):
        // resolve against the FROM relations *plus* the outputs. Their columns
        // are reads, not lineage origins, so they ride on a filter / sort
        // above the projection (which the origin traversal peels through).
        let clause_scope = Scope {
            relations: scope.relations,
            outputs,
        };
        let mut clause_reads = self.group_by_keys(&select.group_by, &clause_scope);
        if let Some(having) = &select.having {
            clause_reads.push(self.bind_expr(having, &clause_scope));
        }
        if !clause_reads.is_empty() {
            node = Operator::Filter(Filter {
                input: Box::new(node),
                predicate: clause_reads,
            });
        }
        let sort_keys = self.order_by_expr_keys(&select.sort_by, &clause_scope);
        if !sort_keys.is_empty() {
            node = sort(node, sort_keys);
        }
        (node, clause_scope)
    }

    fn bind_from(&self, items: &[TableWithJoins]) -> (Operator, Scope) {
        let mut iter = items.iter();
        let Some(first) = iter.next() else {
            return (Operator::Empty, Scope::default());
        };
        let (mut node, mut scope) = self.bind_table_with_joins(first, &[]);
        // Comma-separated FROM items are a cross join; a later item sees the
        // earlier ones only if it is LATERAL.
        for twj in iter {
            let (right, right_scope) = self.bind_table_with_joins(twj, &scope.relations);
            scope.relations.extend(right_scope.relations);
            node = join(node, right, Vec::new());
        }
        (node, scope)
    }

    /// `left` are the FROM siblings to this item's left, visible to a LATERAL
    /// factor (and to a joined factor, after the preceding join inputs).
    fn bind_table_with_joins(&self, twj: &TableWithJoins, left: &[Relation]) -> (Operator, Scope) {
        let (mut node, mut scope) = self.bind_table_factor(&twj.relation, left);
        for j in &twj.joins {
            // A joined LATERAL factor sees the left siblings plus the join
            // inputs accumulated so far.
            let visible: Vec<Relation> = left.iter().chain(&scope.relations).cloned().collect();
            let (right, right_scope) = self.bind_table_factor(&j.relation, &visible);
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

    fn bind_table_factor(&self, factor: &TableFactor, left: &[Relation]) -> (Operator, Scope) {
        match factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                let Ok(written) = TableReference::try_from_name(name) else {
                    return (Operator::Empty, Scope::default());
                };
                let alias_name = alias.as_ref().map(|a| a.name.clone());
                // A bare name matching an in-scope CTE resolves to a `CteRef`
                // (the body lives once on the owning `With`) exposing the CTE's
                // output columns as a synthetic relation.
                if written.schema.is_none() && written.catalog.is_none() {
                    if let Some(cte) = self
                        .ctes
                        .iter()
                        .rev()
                        .find(|c| self.eq(self.casing.table_alias, &c.name, &written.name))
                    {
                        let relation = Relation {
                            alias: alias_name.or_else(|| Some(cte.name.clone())),
                            source: RelSource::Derived {
                                columns: cte.columns.clone(),
                            },
                        };
                        return (
                            Operator::CteRef(super::operator::CteRef {
                                name: cte.name.clone(),
                            }),
                            scope_of(relation),
                        );
                    }
                }
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
                    alias: alias_name,
                    source: RelSource::Table {
                        table: m.table,
                        columns,
                        resolution: m.resolution,
                    },
                };
                // A parameterised table reference `foo(args)`: the argument
                // expressions read against the surrounding (sibling) scope —
                // attach them as a non-feeding filter over the scan.
                let node = match args {
                    Some(args) => Operator::Filter(Filter {
                        input: Box::new(scan),
                        predicate: self.bind_function_arg_list(&args.args, &sibling_scope(left)),
                    }),
                    None => scan,
                };
                (node, scope_of(relation))
            }
            // A derived table `(<subquery>) AS d`: bind the subquery, expose
            // its output columns as a synthetic relation under the alias. A
            // LATERAL derived table sees the left siblings (pushed onto the
            // correlation stack); a non-lateral one does not.
            TableFactor::Derived {
                lateral,
                subquery,
                alias,
                ..
            } => {
                let (mut op, sub_scope) = if *lateral {
                    self.with_outer(left.to_vec()).bind_query(subquery)
                } else {
                    self.bind_query(subquery)
                };
                let columns = exposed_columns(&sub_scope.outputs, alias.as_ref());
                let relation = Relation {
                    alias: alias.as_ref().map(|a| a.name.clone()),
                    source: RelSource::Derived { columns },
                };
                let node = match alias {
                    Some(a) => {
                        rename_outputs(&mut op, &alias_column_names(a));
                        Operator::SubqueryAlias(super::operator::SubqueryAlias {
                            alias: a.name.clone(),
                            input: Box::new(op),
                        })
                    }
                    None => op,
                };
                (node, scope_of(relation))
            }
            // A parenthesized join `(a JOIN b …)`: the inner tables bind
            // directly into the current scope (their refs resolve, their ON
            // reads surface); the wrapper exposes nothing of its own.
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => self.bind_table_with_joins(table_with_joins, left),
            // --- opaque table-producing factors (dynamic columns) ---
            // Bare table functions / UNNEST / JSON_TABLE / XML / semantic views:
            // the argument expressions read against the surrounding
            // (LATERAL-visible) `left` scope; no inner table feeds.
            TableFactor::TableFunction { expr, alias } => {
                let args = vec![self.bind_expr(expr, &sibling_scope(left))];
                self.opaque(Operator::Empty, args, alias.as_ref())
            }
            TableFactor::Function { args, alias, .. } => {
                let bound = self.bind_function_arg_list(args, &sibling_scope(left));
                self.opaque(Operator::Empty, bound, alias.as_ref())
            }
            TableFactor::UNNEST {
                array_exprs, alias, ..
            } => {
                let args = self.bind_exprs(array_exprs, &sibling_scope(left));
                self.opaque(Operator::Empty, args, alias.as_ref())
            }
            TableFactor::JsonTable {
                json_expr, alias, ..
            }
            | TableFactor::OpenJsonTable {
                json_expr, alias, ..
            } => {
                let args = vec![self.bind_expr(json_expr, &sibling_scope(left))];
                self.opaque(Operator::Empty, args, alias.as_ref())
            }
            TableFactor::XmlTable {
                row_expression,
                passing,
                alias,
                ..
            } => {
                let scope = sibling_scope(left);
                let mut args = vec![self.bind_expr(row_expression, &scope)];
                args.extend(
                    passing
                        .arguments
                        .iter()
                        .map(|a| self.bind_expr(&a.expr, &scope)),
                );
                self.opaque(Operator::Empty, args, alias.as_ref())
            }
            TableFactor::SemanticView {
                dimensions,
                metrics,
                facts,
                where_clause,
                alias,
                ..
            } => {
                let scope = sibling_scope(left);
                let mut args = self.bind_exprs(dimensions, &scope);
                args.extend(self.bind_exprs(metrics, &scope));
                args.extend(self.bind_exprs(facts, &scope));
                args.extend(where_clause.iter().map(|e| self.bind_expr(e, &scope)));
                self.opaque(Operator::Empty, args, alias.as_ref())
            }
            // PIVOT / UNPIVOT / MATCH_RECOGNIZE wrap an inner table whose
            // columns the clause expressions read; the produced relation is
            // opaque. The inner table feeds (it's a real source).
            TableFactor::Pivot {
                table,
                aggregate_functions,
                value_column,
                value_source,
                default_on_null,
                alias,
                ..
            } => {
                let (inner, inner_scope) = self.bind_table_factor(table, left);
                let mut args = aggregate_functions
                    .iter()
                    .map(|a| self.bind_expr(&a.expr, &inner_scope))
                    .collect::<Vec<_>>();
                args.extend(self.bind_exprs(value_column, &inner_scope));
                args.extend(self.pivot_value_source_exprs(value_source, &inner_scope));
                args.extend(
                    default_on_null
                        .iter()
                        .map(|e| self.bind_expr(e, &inner_scope)),
                );
                self.opaque(inner, args, alias.as_ref())
            }
            TableFactor::Unpivot {
                table,
                value,
                columns,
                alias,
                ..
            } => {
                let (inner, inner_scope) = self.bind_table_factor(table, left);
                let mut args = vec![self.bind_expr(value, &inner_scope)];
                args.extend(
                    columns
                        .iter()
                        .map(|c| self.bind_expr(&c.expr, &inner_scope)),
                );
                self.opaque(inner, args, alias.as_ref())
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
                let (inner, inner_scope) = self.bind_table_factor(table, left);
                let mut args = self.bind_exprs(partition_by, &inner_scope);
                args.extend(
                    order_by
                        .iter()
                        .map(|o| self.bind_expr(&o.expr, &inner_scope)),
                );
                args.extend(
                    measures
                        .iter()
                        .map(|m| self.bind_expr(&m.expr, &inner_scope)),
                );
                args.extend(
                    symbols
                        .iter()
                        .map(|s| self.bind_expr(&s.definition, &inner_scope)),
                );
                self.opaque(inner, args, alias.as_ref())
            }
        }
    }

    /// Assemble an opaque table-producing factor: an [`Operator::TableFunction`]
    /// node carrying the (already-bound) argument reads over `input` (the wrapped
    /// inner table, or [`Operator::Empty`] for a bare function), exposed as a
    /// synthetic [`RelSource::TableFunction`] relation under the alias.
    fn opaque(
        &self,
        input: Operator,
        args: Vec<Expr>,
        alias: Option<&TableAlias>,
    ) -> (Operator, Scope) {
        let alias_name = alias.map(|a| a.name.clone());
        let node = Operator::TableFunction(super::operator::TableFunction {
            alias: alias_name.clone(),
            input: Box::new(input),
            args,
        });
        let scope = match alias_name {
            Some(name) => scope_of(Relation {
                alias: Some(name),
                source: RelSource::TableFunction,
            }),
            None => Scope::default(),
        };
        (node, scope)
    }

    /// The value expressions of a PIVOT value source (`IN (list)` / `ANY ORDER
    /// BY …` / a subquery). The subquery's reads come from binding it.
    fn pivot_value_source_exprs(&self, source: &PivotValueSource, scope: &Scope) -> Vec<Expr> {
        match source {
            PivotValueSource::List(values) => values
                .iter()
                .map(|v| self.bind_expr(&v.expr, scope))
                .collect(),
            PivotValueSource::Any(order_by) => order_by
                .iter()
                .map(|o| self.bind_expr(&o.expr, scope))
                .collect(),
            PivotValueSource::Subquery(query) => {
                vec![Expr::Subquery(Box::new(self.bind_subquery(query, scope)))]
            }
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
            // A scalar subquery (value position): its output flows into the
            // enclosing value.
            SqlExpr::Subquery(query) => Expr::Subquery(Box::new(self.bind_subquery(query, scope))),
            // A test (filter position): its columns are reads, never an origin.
            SqlExpr::Exists { subquery, .. } => {
                Expr::Exists(Box::new(self.bind_subquery(subquery, scope)))
            }
            SqlExpr::InSubquery { expr, subquery, .. } => Expr::InSubquery {
                expr: Box::new(self.bind_expr(expr, scope)),
                subquery: Box::new(self.bind_subquery(subquery, scope)),
            },
            // Literals and not-yet-modelled forms contribute no column refs.
            _ => Expr::Call { args: Vec::new() },
        }
    }

    /// Bind a subquery nested in an expression: its references resolve against
    /// its own FROM plus the containing scope's relations (pushed onto the
    /// correlation stack), so a correlated reference reaches outward.
    fn bind_subquery(&self, query: &Query, scope: &Scope) -> Operator {
        self.with_outer(scope.relations.clone()).bind_query(query).0
    }

    fn bind_function_args(&self, function: &Function, scope: &Scope) -> Vec<Expr> {
        match &function.args {
            FunctionArguments::List(list) => self.bind_function_arg_list(&list.args, scope),
            _ => Vec::new(),
        }
    }

    /// Bind a function-argument list's value expressions (dropping `*` and
    /// other non-expression args). Shared by scalar functions and table
    /// functions (`FROM f(args)`).
    fn bind_function_arg_list(&self, args: &[FunctionArg], scope: &Scope) -> Vec<Expr> {
        args.iter()
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

    /// Bind several expressions against `scope`.
    fn bind_exprs(&self, exprs: &[SqlExpr], scope: &Scope) -> Vec<Expr> {
        exprs.iter().map(|e| self.bind_expr(e, scope)).collect()
    }

    // ===== clauses (GROUP BY / HAVING / ORDER BY) ========================

    /// The GROUP BY key expressions (plain + GROUPING SETS members), resolved
    /// against the clause scope. These are reads, not lineage origins.
    fn group_by_keys(&self, group_by: &GroupByExpr, scope: &Scope) -> Vec<Expr> {
        let mut keys = Vec::new();
        if let GroupByExpr::Expressions(exprs, modifiers) = group_by {
            let members = exprs.iter().chain(modifiers.iter().filter_map(|m| match m {
                GroupByWithModifier::GroupingSets(expr) => Some(expr),
                _ => None,
            }));
            for expr in members {
                keys.push(self.bind_expr(expr, scope));
            }
        }
        keys
    }

    /// The ORDER BY key expressions (a trailing `query.order_by`).
    fn order_by_keys(&self, order_by: &OrderBy, scope: &Scope) -> Vec<Expr> {
        let OrderByKind::Expressions(exprs) = &order_by.kind else {
            return Vec::new();
        };
        self.order_by_expr_keys(exprs, scope)
    }

    /// Bind a list of order-by expressions (`query.order_by` members or a
    /// `SELECT … SORT BY` list) as clause reads.
    fn order_by_expr_keys(&self, exprs: &[OrderByExpr], scope: &Scope) -> Vec<Expr> {
        exprs
            .iter()
            .map(|e| self.bind_expr(&e.expr, scope))
            .collect()
    }

    /// Summarise the projection outputs for clause-alias resolution. An output
    /// is *identity* iff it is a bare column under its own name.
    fn output_cols(&self, exprs: &[NamedExpr]) -> Vec<OutputCol> {
        exprs
            .iter()
            .map(|ne| {
                let identity = match &ne.expr {
                    Expr::Column(c) => ne
                        .name
                        .as_ref()
                        .is_some_and(|n| self.eq(self.casing.column, n, &c.name)),
                    _ => false,
                };
                OutputCol {
                    name: ne.name.clone(),
                    identity,
                }
            })
            .collect()
    }

    // ===== column resolution =============================================

    /// Resolve a dotted reference (`parts`) against the scope. Unqualified
    /// ranks every relation; qualified matches the qualifier first. Collapsed
    /// by [`pick`](Self::pick).
    fn resolve(&self, parts: &[Ident], scope: &Scope) -> ColRef {
        let name = parts.last().expect("a reference has at least one segment");
        // Clause-alias visibility (GROUP BY / HAVING / ORDER BY): a bare ref
        // naming an *introduced* output alias resolves to that output
        // (`Derived`, dropped from reads — the real dependency is at the
        // projection). An *identity* passthrough falls through to the real
        // column. (Empty `outputs` at FROM-level, so this is a no-op there.)
        if parts.len() == 1 {
            if let Some(out) = scope.outputs.iter().find(|o| {
                o.name
                    .as_ref()
                    .is_some_and(|n| self.eq(self.casing.column, n, name))
            }) {
                if !out.identity {
                    return ColRef {
                        qualifier: None,
                        name: name.clone(),
                        binding: Binding::Derived,
                    };
                }
            }
        }
        // Resolve against the current scope, then fall through the enclosing
        // scopes (correlation), innermost first, before giving up.
        let binding = self
            .resolve_in(parts, &scope.relations)
            .or_else(|| {
                self.outer
                    .iter()
                    .rev()
                    .find_map(|relations| self.resolve_in(parts, relations))
            })
            .unwrap_or(Binding::Unresolved);
        ColRef {
            qualifier: (parts.len() >= 2).then(|| parts[parts.len() - 2].clone()),
            name: name.clone(),
            binding,
        }
    }

    /// Resolve `parts` against one set of relations, returning `None` when none
    /// could own the column (so the caller falls through to an enclosing
    /// scope) and `Some(binding)` once at least one is a candidate (even if
    /// that collapses to `Ambiguous` — a name owned here doesn't escape).
    fn resolve_in(&self, parts: &[Ident], relations: &[Relation]) -> Option<Binding> {
        let name = parts.last()?;
        let candidates: Vec<Candidate> = if parts.len() == 1 {
            relations
                .iter()
                .filter_map(|rel| self.unqualified_candidate(rel, name))
                .collect()
        } else {
            let qualifier_parts = &parts[..parts.len() - 1];
            let qualifier_ref = TableReference::try_from_parts(qualifier_parts);
            relations
                .iter()
                .filter_map(|rel| {
                    self.qualified_candidate(rel, qualifier_parts, qualifier_ref.as_ref(), name)
                })
                .collect()
        };
        (!candidates.is_empty()).then(|| self.pick(candidates))
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
            // A derived relation owns the column iff it exposes it (confirmed
            // witness, like a Known table); the origin traversal collapses it.
            RelSource::Derived { columns } => self.list_has(columns, name).then_some(Candidate {
                binding: Binding::Derived,
                confirmed: true,
            }),
            // A table function's columns are opaque — a bare name is not
            // claimed by it (stays resolvable against real tables).
            RelSource::TableFunction => None,
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
            RelSource::Derived { columns } => self.list_has(columns, name).then_some(Candidate {
                binding: Binding::Derived,
                confirmed: true,
            }),
            // A ref qualified by a table function's alias resolves to it: a
            // `Derived` binding the traversal turns into the synthetic
            // `alias.col` lineage source (dropped from reads).
            RelSource::TableFunction => Some(Candidate {
                binding: Binding::Derived,
                confirmed: true,
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

/// Cross-join `right` onto `left`, but if `left` is the empty placeholder
/// just take `right` (so a single read relation isn't wrapped in a join with
/// nothing). Used to accumulate a DML statement's read relations.
fn combine(left: Operator, right: Operator) -> Operator {
    if matches!(left, Operator::Empty) {
        right
    } else {
        join(left, right, Vec::new())
    }
}

/// The column(s) a SET assignment writes: a `col` up to
/// `catalog.schema.table.col` (≤ 4 segments) contributes its last identifier;
/// a tuple target `(a, b) = …` or a deeper qualifier contributes nothing
/// (not column-paired).
fn assignment_target_columns(target: &AssignmentTarget) -> Vec<Ident> {
    match target {
        AssignmentTarget::ColumnName(name) if name.0.len() <= 4 => name
            .0
            .last()
            .and_then(|p| p.as_ident().cloned())
            .into_iter()
            .collect(),
        AssignmentTarget::ColumnName(_) | AssignmentTarget::Tuple(_) => Vec::new(),
    }
}

/// The column name(s) an `ALTER TABLE` operation writes to. Column-naming ops
/// (ADD / DROP / RENAME / CHANGE / MODIFY / ALTER COLUMN) name their column(s);
/// RENAME / CHANGE surface both old and new names. Schema-level ops
/// (constraints, partitions, RENAME TABLE) name no columns.
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

fn sort(input: Operator, keys: Vec<Expr>) -> Operator {
    Operator::Sort(super::operator::Sort {
        input: Box::new(input),
        keys,
    })
}

fn scope_of(relation: Relation) -> Scope {
    Scope {
        relations: vec![relation],
        ..Scope::default()
    }
}

/// A resolution scope over the FROM siblings to a factor's left (the
/// LATERAL-visible relations a table function's arguments read against).
fn sibling_scope(left: &[Relation]) -> Scope {
    Scope {
        relations: left.to_vec(),
        ..Scope::default()
    }
}

/// The names of a table alias's explicit column list (`AS d(x, y)`), empty if
/// none.
fn alias_column_names(alias: &TableAlias) -> Vec<Ident> {
    alias.columns.iter().map(|c| c.name.clone()).collect()
}

/// Rename a (sub)plan's output columns positionally to `names` (an explicit
/// `AS d(x, y)` column list). A no-op when `names` is empty. Descends through
/// the clause layers / `With` to the producing `Project` / `Aggregate`; a
/// `SetOp` renames both operands.
fn rename_outputs(op: &mut Operator, names: &[Ident]) {
    if names.is_empty() {
        return;
    }
    match op {
        Operator::Project(p) => {
            for (ne, n) in p.exprs.iter_mut().zip(names) {
                ne.name = Some(n.clone());
            }
        }
        Operator::Aggregate(a) => {
            for (ne, n) in a.group_by.iter_mut().chain(&mut a.aggregates).zip(names) {
                ne.name = Some(n.clone());
            }
        }
        Operator::Sort(s) => rename_outputs(&mut s.input, names),
        Operator::Filter(f) => rename_outputs(&mut f.input, names),
        Operator::With(w) => rename_outputs(&mut w.body, names),
        Operator::SetOp(so) => {
            rename_outputs(&mut so.left, names);
            rename_outputs(&mut so.right, names);
        }
        _ => {}
    }
}

/// The output column names a derived table / CTE exposes: an explicit column
/// alias list (`AS d(x, y)`) renames positionally; otherwise each output keeps
/// its own inferred name (anonymous outputs with no alias are unnameable, so
/// dropped — they can't be referenced).
fn exposed_columns(outputs: &[OutputCol], alias: Option<&TableAlias>) -> Vec<Ident> {
    let alias_columns: Vec<&Ident> = alias
        .map(|a| a.columns.iter().map(|c| &c.name).collect())
        .unwrap_or_default();
    outputs
        .iter()
        .enumerate()
        .filter_map(|(i, o)| {
            alias_columns
                .get(i)
                .map(|n| (*n).clone())
                .or_else(|| o.name.clone())
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
