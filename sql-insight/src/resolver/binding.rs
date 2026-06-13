//! `Binding` enum + the resolver-side `bind_*` constructors / lookup
//! helpers / diagnostic recording. The scope arena that holds these
//! bindings lives in [`super::scope`]; the FROM-position table-use
//! captures live in [`super::table_ref`].

use sqlparser::ast::{Ident, ObjectName, Statement};
use sqlparser::tokenizer::Span;

use crate::catalog::ColumnSchema;
use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::reference::TableReference;

use super::{CaseFold, IdentifierCasing, QueryBodyOutput, Resolver, ScopeId};

/// A normalized identifier key for binding lookup.
///
/// Two identifiers match iff their normalized forms are equal. The
/// normalization is supplied by a [`CaseFold`] chosen from the active
/// dialect's [`IdentifierCasing`] for the identifier's class (table /
/// table-alias / column) — so e.g. an unquoted name folds to lower
/// under PostgreSQL, to upper under Snowflake, and quoting matters
/// under both but not under MySQL / BigQuery / DuckDB. See
/// [`super::casing`] for the full per-dialect matrix.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct BindingKey(String);

impl BindingKey {
    /// Normalize `ident` under the given [`CaseFold`] for matching.
    /// The fold comes from the active dialect's [`IdentifierCasing`],
    /// picked per identifier class (table / table-alias / column) at
    /// the call site.
    pub(super) fn new(ident: &Ident, fold: CaseFold) -> Self {
        Self(fold.normalize(ident))
    }
}

/// Internal role a table binding carries within a statement. Surfaced
/// to the operation extractor via [`super::Resolution::read_tables`]
/// and [`super::Resolution::write_tables`]; the public API exposes
/// two separate lists instead of this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TableRole {
    Read,
    Write,
}

/// What's bound to a name in a [`super::Scope`] — a real Table or one
/// of the synthetic relations (CTE / derived subquery / table function)
/// that SQL exposes as a named row set.
///
/// Column-name info lives in the `output_columns` field, shaped per
/// binding kind:
/// - `Table::output_columns: Option<Vec<Ident>>` — naked catalog
///   column names (`Some` = catalog hit, `None` = miss / no catalog).
/// - `Cte` / `DerivedTable` `output_columns: Option<QueryBodyOutput>` —
///   one [`super::SetOperand`] per set-operation operand, each
///   carrying the SELECT body's [`super::OutputColumn`]s with full
///   lineage info (name, source_refs, kind). `None` covers recursive
///   CTE stubs, wrapper aliases (NestedJoin / Pivot / etc.), and
///   walk-failed bodies.
/// - `TableFunction` carries no column info — always unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Binding {
    // `table` is boxed because the variant otherwise dwarfs the others
    // (TableReference is ~300B) and inflates the entire enum's size.
    Table {
        /// The real underlying table. Alias-free so catalog lookup
        /// and cross-statement comparison behave intuitively (alias
        /// lives next door in `alias`).
        table: Box<TableReference>,
        /// Alias given at this use-site, if any. Kept separately so
        /// `TableReference` stays alias-free for catalog lookup and
        /// cross-statement comparison.
        alias: Option<Ident>,
        /// Catalog-derived column names. `Some` when the catalog has
        /// the table, `None` otherwise (no catalog or catalog miss).
        output_columns: Option<Vec<Ident>>,
        /// How this binding is used in the statement — Read, Write,
        /// or both (e.g. `DELETE t1 FROM t1`). Re-binding the same
        /// name merges roles rather than overwriting (see
        /// `Scope::bind`).
        roles: Vec<TableRole>,
    },
    Cte {
        /// The CTE's declared name (the `<name>` in `WITH <name> AS …`).
        /// Lookup keys derive from this via `BindingKey`.
        name: Ident,
        /// Body-walk output of the CTE: one [`super::SetOperand`] per
        /// set-operation operand, each carrying [`super::OutputColumn`]s
        /// with full lineage info (name, source refs, kind). `None` for
        /// recursive CTEs pre-bound under a stub (fixpoint-aware capture
        /// is deferred). Renamed via the CTE's column-alias list when
        /// one is given.
        output_columns: Option<QueryBodyOutput>,
        /// Arena id of the scope that holds the CTE body's bindings.
        /// Table-lineage collapse walks descendant scopes of this id
        /// to collect the real tables underneath the CTE — so a
        /// `FROM cte` use can resolve back to the body's `FROM s`.
        body_scope: ScopeId,
    },
    DerivedTable {
        /// Mandatory alias from `(SELECT …) AS d`. Unlike `Table::alias`,
        /// this is the only handle the outer query has on the derived
        /// relation.
        alias: Ident,
        /// Body-walk output. Same shape as `Cte::output_columns`,
        /// renamed via the alias's column list when one is given.
        /// `None` for wrapper aliases (`NestedJoin`, `Pivot`,
        /// `Unpivot`, `MatchRecognize`) whose body isn't a real
        /// subquery with its own projection.
        output_columns: Option<QueryBodyOutput>,
        /// Arena id of the scope holding the derived subquery body's
        /// bindings (`Some`) — or `None` for wrapper aliases whose
        /// inner tables are bound directly in the current scope and
        /// don't need collapse through this synthetic.
        body_scope: Option<ScopeId>,
    },
    TableFunction {
        /// Mandatory alias from `f(...) AS t`. Refs against the alias
        /// surface as synthetic-owned (filtered out of public reads).
        /// No column-info field — TableFunction column inference is
        /// not modelled (always unknown).
        alias: Ident,
    },
}

pub(super) fn is_synthetic_binding(binding: &Binding) -> bool {
    matches!(
        binding,
        Binding::Cte { .. } | Binding::DerivedTable { .. } | Binding::TableFunction { .. }
    )
}

/// The scope-arena key for a binding's exposed name. The fold is
/// picked per binding kind: a non-aliased real table keys by its
/// `table` name (the [`IdentifierCasing::table`] class); an aliased
/// table and every synthetic relation (CTE / derived / table
/// function) key by their alias / name (the
/// [`IdentifierCasing::table_alias`] class).
pub(super) fn binding_alias_key(binding: &Binding, casing: IdentifierCasing) -> BindingKey {
    match binding {
        Binding::Table {
            table, alias: None, ..
        } => BindingKey::new(&table.name, casing.table),
        Binding::Table {
            alias: Some(alias), ..
        } => BindingKey::new(alias, casing.table_alias),
        Binding::Cte { name, .. } => BindingKey::new(name, casing.table_alias),
        Binding::DerivedTable { alias, .. } | Binding::TableFunction { alias, .. } => {
            BindingKey::new(alias, casing.table_alias)
        }
    }
}

pub(super) fn binding_could_contain_column(
    binding: &Binding,
    name: &Ident,
    column_fold: CaseFold,
) -> Option<TableReference> {
    match binding {
        // TableFunction columns are unmodeled, so any unqualified
        // column could plausibly come from one.
        Binding::TableFunction { .. } => Some(binding_table_ref(binding)),
        _ => names_could_contain(binding_column_names(binding).as_deref(), name, column_fold)
            .then(|| binding_table_ref(binding)),
    }
}

/// The `TableReference` that surfaces in `reads` / `lineage` when this
/// binding is the owner of a column ref. Real tables surface their
/// alias-free underlying `TableReference`; synthetic bindings (CTE /
/// derived / table function) surface a name-only synthetic ref so
/// lineage collapse can re-find the owning binding by name.
fn binding_table_ref(binding: &Binding) -> TableReference {
    match binding {
        Binding::Table { table, .. } => (**table).clone(),
        Binding::Cte { name, .. } => synthetic_table_ref(name),
        Binding::DerivedTable { alias, .. } | Binding::TableFunction { alias, .. } => {
            synthetic_table_ref(alias)
        }
    }
}

/// Known-membership: `true` iff the binding has a known column list
/// that declares the column. Distinguished from
/// `binding_could_contain_column`, which also returns `Some` for
/// bindings with unknown column lists. Used by diagnostic emit to
/// separate "definitely ambiguous" from "uncertain over unknown
/// columns".
pub(super) fn binding_confirms_column(
    binding: &Binding,
    name: &Ident,
    column_fold: CaseFold,
) -> bool {
    match binding_column_names(binding) {
        Some(cols) => cols
            .iter()
            .any(|c| BindingKey::new(c, column_fold) == BindingKey::new(name, column_fold)),
        None => false,
    }
}

/// Cross-binding column-name lookup: the single entry point that
/// abstracts over `Binding::Table`'s catalog list and
/// `Cte`/`DerivedTable`'s body-derived column names. `TableFunction`
/// always returns `None`.
pub(super) fn binding_column_names(binding: &Binding) -> Option<Vec<Ident>> {
    match binding {
        Binding::Table { output_columns, .. } => output_columns.clone(),
        Binding::Cte { output_columns, .. } | Binding::DerivedTable { output_columns, .. } => {
            output_columns.as_ref()?.column_names()
        }
        Binding::TableFunction { .. } => None,
    }
}

fn names_could_contain(names: Option<&[Ident]>, name: &Ident, column_fold: CaseFold) -> bool {
    match names {
        None => true, // unknown columns — anything could match
        Some(cols) => cols
            .iter()
            .any(|c| BindingKey::new(c, column_fold) == BindingKey::new(name, column_fold)),
    }
}

/// Right-anchored qualifier match for a *qualified* column reference.
///
/// The column qualifier (decoded into a [`TableReference`] via
/// [`TableReference::try_from_parts`]) matches a binding's underlying
/// `table` when the `name` segments are equal and each of `schema` /
/// `catalog` is either equal or absent on at least one side (absent =
/// wildcard). This is the ANSI "partial qualifier" rule:
///
/// - `users.col` and `mydb.users.col` both name `FROM mydb.users` —
///   the binding fills the missing schema (`users.col`), or the ref's
///   extra schema is simply left unverified (`mydb.users.col`).
/// - `otherdb.users.col` does *not* match `FROM mydb.users` — schema
///   is present on both sides and differs, a contradiction a catalog
///   can't fix.
///
/// Comparison folds case via [`BindingKey`] under the `table` fold
/// (catalog / schema / table all share the [`IdentifierCasing::table`]
/// class). Callers handle aliased and synthetic bindings separately
/// (those expose only their single alias / name, never their
/// underlying path).
pub(super) fn qualifier_matches_table(
    qualifier: &TableReference,
    table: &TableReference,
    table_fold: CaseFold,
) -> bool {
    ident_key_eq(&qualifier.name, &table.name, table_fold)
        && opt_ident_key_eq(qualifier.schema.as_ref(), table.schema.as_ref(), table_fold)
        && opt_ident_key_eq(
            qualifier.catalog.as_ref(),
            table.catalog.as_ref(),
            table_fold,
        )
}

fn ident_key_eq(a: &Ident, b: &Ident, fold: CaseFold) -> bool {
    BindingKey::new(a, fold) == BindingKey::new(b, fold)
}

/// Equal, or absent on at least one side (absent = right-anchored
/// wildcard). Only `(Some(x), Some(y))` with `x != y` fails;
/// `(Some, None)` and `(None, Some)` both match.
fn opt_ident_key_eq(a: Option<&Ident>, b: Option<&Ident>, fold: CaseFold) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => ident_key_eq(x, y, fold),
        _ => true,
    }
}

pub(super) fn synthetic_table_ref(name: &Ident) -> TableReference {
    TableReference {
        catalog: None,
        schema: None,
        name: name.clone(),
    }
}

/// Convert a raw sqlparser `Span` to the `Option<Span>` shape stored on
/// `ColumnLevelDiagnostic`: an empty span (sqlparser convention: `line == 0`) is
/// flattened to `None` so consumers can distinguish "no source location"
/// from "location at (0, 0)".
pub(super) fn normalize_span(span: Span) -> Option<Span> {
    (span.start.line != 0).then_some(span)
}

/// Format an `Option<Span>` as ` at L<line>:C<col>` for inclusion in
/// diagnostic messages, or an empty string when no location is known.
pub(super) fn span_suffix(span: Option<Span>) -> String {
    match span {
        Some(s) => format!(" at L{}:C{}", s.start.line, s.start.column),
        None => String::new(),
    }
}

impl<'a> Resolver<'a> {
    pub(super) fn is_cte_reference(&self, relation: &ObjectName) -> bool {
        matches!(
            self.resolve_unqualified_relation(relation),
            Some(Binding::Cte { .. })
        )
    }

    pub(super) fn bind_real_table(
        &mut self,
        table: TableReference,
        alias: Option<Ident>,
        role: TableRole,
    ) {
        let output_columns = self.lookup_catalog_columns(&table);
        if role == TableRole::Read {
            // Read-position FROM/JOIN — emit a CapturedTableRef so table-lineage
            // collapse sees this as a real source. Write targets feed
            // `write_tables` separately and don't drive collapse.
            self.capture_real_table_ref(table.clone());
        }
        self.bind_current(Binding::Table {
            table: Box::new(table),
            alias,
            output_columns,
            roles: vec![role],
        });
    }

    /// Query the optional catalog for a table's columns.
    /// `TableReference` is already alias-free, so it is a valid
    /// catalog key as-is. Returns `None` when no catalog is supplied
    /// or the catalog has no entry for the table.
    fn lookup_catalog_columns(&self, table: &TableReference) -> Option<Vec<Ident>> {
        let catalog = self.catalog?;
        let cols = catalog.columns(table)?;
        Some(
            cols.into_iter()
                .map(|ColumnSchema { name }| Ident::new(name))
                .collect(),
        )
    }

    /// Resolve the effective target column list for INSERT-style
    /// positional pairing: explicit list wins when non-empty,
    /// otherwise the catalog-provided schema if known. Returns an
    /// empty `Vec` when neither path yields names — the caller then
    /// emits no Relation edges (matches the no-catalog
    /// column-list-less INSERT behavior).
    pub(super) fn effective_target_columns(
        &self,
        explicit: &[Ident],
        target: &TableReference,
    ) -> Vec<Ident> {
        if !explicit.is_empty() {
            return explicit.to_vec();
        }
        self.lookup_catalog_columns(target).unwrap_or_default()
    }

    /// Look up an in-scope CTE's `output_columns`, for re-binding
    /// under an alias (`FROM cte AS c`). Returns `None` when the
    /// reference is multi-segment, not bound, or not a Cte binding —
    /// the caller (alias-bound Cte construction) treats that as "no
    /// collapse through this alias", matching recursive-CTE behavior.
    pub(super) fn cte_output_columns(&self, cte_name: &ObjectName) -> Option<QueryBodyOutput> {
        match self.resolve_unqualified_relation(cte_name) {
            Some(Binding::Cte { output_columns, .. }) => output_columns.clone(),
            _ => None,
        }
    }

    pub(super) fn bind_cte(
        &mut self,
        name: Ident,
        output_columns: Option<QueryBodyOutput>,
        body_scope: ScopeId,
    ) {
        self.bind_current(Binding::Cte {
            name,
            output_columns,
            body_scope,
        });
    }

    pub(super) fn bind_derived_table(
        &mut self,
        alias: Ident,
        output_columns: Option<QueryBodyOutput>,
        body_scope: Option<ScopeId>,
    ) {
        self.bind_current(Binding::DerivedTable {
            alias,
            output_columns,
            body_scope,
        });
    }

    /// Look up the body scope for a CTE name. Returns `None` if the
    /// name does not resolve to a `Cte` binding — same fall-through
    /// semantics as [`Self::cte_output_columns`].
    pub(super) fn cte_body_scope(&self, cte_name: &ObjectName) -> Option<ScopeId> {
        match self.resolve_unqualified_relation(cte_name) {
            Some(Binding::Cte { body_scope, .. }) => Some(*body_scope),
            _ => None,
        }
    }

    pub(super) fn bind_table_function(&mut self, alias: Ident) {
        self.bind_current(Binding::TableFunction { alias });
    }

    pub(super) fn record_diagnostic(&mut self, diagnostic: ColumnLevelDiagnostic) {
        self.resolution.diagnostics.push(diagnostic);
    }

    pub(super) fn record_unsupported_statement(&mut self, statement: &Statement) {
        self.record_diagnostic(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::UnsupportedStatement,
            message: format!("Unsupported statement while inspecting SQL: {}", statement),
            span: None,
        });
    }

    pub(super) fn record_wildcard_suppressed(&mut self, description: &str, span: Span) {
        let span = normalize_span(span);
        self.record_diagnostic(ColumnLevelDiagnostic {
            kind: ColumnLevelDiagnosticKind::WildcardSuppressed,
            message: format!(
                "{}{} left unexpanded — column lineage will be incomplete for this projection",
                description,
                span_suffix(span),
            ),
            span,
        });
    }
}
