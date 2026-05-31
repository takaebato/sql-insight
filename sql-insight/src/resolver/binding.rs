//! `Binding` enum + the resolver-side `bind_*` constructors / lookup
//! helpers / diagnostic recording. The scope arena that holds these
//! bindings lives in [`super::scope`]; the FROM-position table-use
//! captures live in [`super::table_ref`].

use sqlparser::ast::{Ident, ObjectName, Statement};
use sqlparser::tokenizer::Span;

use crate::catalog::ColumnSchema;
use crate::diagnostic::{ColumnLevelDiagnostic, ColumnLevelDiagnosticKind};
use crate::reference::TableReference;

use super::{BodyOutput, Resolver, ScopeId};

/// A normalized identifier key for binding lookup.
///
/// Two identifiers match iff their normalized forms are equal. The
/// rule: fold an unquoted name to lowercase, keep a quoted name exact.
/// So `"id"` and unquoted `id` are the same column, while `"ID"` and
/// `id` are not.
///
/// This is one fixed rule, applied uniformly — it is *not* varied by
/// dialect, nor by table-vs-column. Real dialects do diverge there
/// (e.g. MySQL / BigQuery / SQLite treat quoting as mere escaping and
/// keep quoted names case-insensitive; BigQuery columns are
/// case-insensitive but its tables are case-sensitive; ClickHouse is
/// fully case-sensitive). Modelling each faithfully would need a
/// per-dialect identifier-resolution strategy, which is deferred — the
/// fixed rule here is a deliberate common-denominator approximation:
///
/// - **Unquoted → lowercase** makes unquoted matching case-insensitive,
///   which every supported dialect except ClickHouse does. (ClickHouse
///   is over-matched — sound, just imprecise.) The fold *direction*
///   only affects the quoted/unquoted edge; lowercase follows the
///   popular majority (PG / MySQL / SQLite / BigQuery / Redshift / Spark)
///   over the uppercase minority (ANSI / Oracle / Snowflake).
/// - **Quoted → exact** follows the ANSI / PostgreSQL family, where
///   quoting makes an identifier case-sensitive. The MySQL / BigQuery /
///   SQLite family instead treat quoting as escaping, so this is
///   stricter than they are for quoted names — accepted, since quoted
///   identifiers are rare in practice.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct BindingKey(String);

impl BindingKey {
    pub(super) fn from_ident(ident: &Ident) -> Self {
        Self(if ident.quote_style.is_some() {
            ident.value.clone()
        } else {
            ident.value.to_ascii_lowercase()
        })
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
/// - `Cte` / `DerivedTable` `output_columns: Option<BodyOutput>` —
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
        output_columns: Option<BodyOutput>,
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
        output_columns: Option<BodyOutput>,
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

pub(super) fn binding_alias_key(binding: &Binding) -> BindingKey {
    match binding {
        Binding::Table { table, alias, .. } => {
            BindingKey::from_ident(alias.as_ref().unwrap_or(&table.name))
        }
        Binding::Cte { name, .. } => BindingKey::from_ident(name),
        Binding::DerivedTable { alias, .. } | Binding::TableFunction { alias, .. } => {
            BindingKey::from_ident(alias)
        }
    }
}

pub(super) fn binding_could_contain_column(
    binding: &Binding,
    name: &Ident,
) -> Option<TableReference> {
    match binding {
        // TableFunction columns are unmodeled, so any unqualified
        // column could plausibly come from one.
        Binding::TableFunction { .. } => Some(binding_table_ref(binding)),
        _ => names_could_contain(binding_column_names(binding).as_deref(), name)
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
pub(super) fn binding_confirms_column(binding: &Binding, name: &Ident) -> bool {
    match binding_column_names(binding) {
        Some(cols) => cols
            .iter()
            .any(|c| BindingKey::from_ident(c) == BindingKey::from_ident(name)),
        None => false,
    }
}

/// `true` iff the binding has a known column list. Used to gate
/// `UnresolvedColumn` diagnostics — without at least one binding
/// with known columns in scope, the resolver can't claim a column is
/// missing.
pub(super) fn binding_has_known_columns(binding: &Binding) -> bool {
    binding_column_names(binding).is_some()
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

fn names_could_contain(names: Option<&[Ident]>, name: &Ident) -> bool {
    match names {
        None => true, // unknown columns — anything could match
        Some(cols) => cols
            .iter()
            .any(|c| BindingKey::from_ident(c) == BindingKey::from_ident(name)),
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
        let binding_name = alias.clone().unwrap_or_else(|| table.name.clone());
        let output_columns = self.lookup_catalog_columns(&table);
        if role == TableRole::Read {
            // Read-position FROM/JOIN — emit a RawTableRef so table-lineage
            // collapse sees this as a real source. Write targets feed
            // `write_tables` separately and don't drive collapse.
            self.record_real_table_ref(table.clone());
        }
        self.bind_current(
            binding_name,
            Binding::Table {
                table: Box::new(table),
                alias,
                output_columns,
                roles: vec![role],
            },
        );
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
    pub(super) fn cte_output_columns(&self, cte_name: &ObjectName) -> Option<BodyOutput> {
        match self.resolve_unqualified_relation(cte_name) {
            Some(Binding::Cte { output_columns, .. }) => output_columns.clone(),
            _ => None,
        }
    }

    pub(super) fn bind_cte(
        &mut self,
        name: Ident,
        output_columns: Option<BodyOutput>,
        body_scope: ScopeId,
    ) {
        self.bind_current(
            name.clone(),
            Binding::Cte {
                name,
                output_columns,
                body_scope,
            },
        );
    }

    pub(super) fn bind_derived_table(
        &mut self,
        alias: Ident,
        output_columns: Option<BodyOutput>,
        body_scope: Option<ScopeId>,
    ) {
        self.bind_current(
            alias.clone(),
            Binding::DerivedTable {
                alias,
                output_columns,
                body_scope,
            },
        );
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
        self.bind_current(alias.clone(), Binding::TableFunction { alias });
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
