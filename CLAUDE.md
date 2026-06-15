# CLAUDE.md

## Project

Rust workspace: `sql-insight` library + `sql-insight-cli`. SQL parsing is
built on `sqlparser-rs`; always work against its AST, never re-parse SQL
by hand.

## Commands

- Format: `cargo fmt`
- Test: `cargo test --all`
- Lint: `cargo clippy --all-targets -- -D warnings` (zero-warning policy)
- Docs: `RUSTDOCFLAGS=-Dwarnings cargo doc --document-private-items --no-deps --workspace --all-features`
  (matches CI; `--document-private-items` catches broken intra-doc
  links in private rustdoc that plain `cargo doc` silently skips)

## Architecture

- The private `resolver` module is the analysis engine: it binds a
  `Statement` into a **materialized, full-stack bound logical-plan
  tree** (`ir::Plan`) and walks that tree for the extraction surfaces.
  It is *not* an execution plan — nothing optimizes or runs SQL. (An
  earlier design built this as an incubating `plan` module beside a
  flat-buffer resolver; at parity it took over the `resolver` name, so
  the module is `resolver` but its output type is `Plan`.) Sub-modules:
  - `ir` — the persistent operator-tree types.
  - `binder` — `build_with_diagnostics(stmt, catalog, casing) ->
    (Option<Plan>, Vec<ColumnLevelDiagnostic>)`, the bind pass
    (AST → resolved `Plan`); plus the bind-time `Scope` / `Relation`
    scratch and the `ExprCollector` value/filter splitter.
  - `extract` — walk a `Plan` for `reads` / `writes` / `lineage`
    (column + table) and the legacy flat table list.
  - `operation` / `table_operation` — assemble the public
    `ColumnOperation` / `TableOperation` (and the flat list) from a
    `Plan`, using `classify_statement` for the verb and projecting
    column-level diagnostics down to the table level.
- `ir::Plan` variants: `Scan` (named real-table leaf, carrying
  `resolution: ResolutionKind` and `role: ScanRole {Read,Write}`),
  `OpaqueLeaf` (VALUES / table function / unmodelled leaf, no
  columns), `PassThrough` (join **and** every filter — output is the
  identity concat of inputs; `reads` are predicate columns; filter
  subqueries hang on `subqueries`, non-feeding), `Project` (the only
  column-defining producer — `outputs: Vec<BoundColumn>`; value
  subqueries on `subqueries`, feeding), `SetOp` (positional fan-in),
  `Write` (INSERT / UPDATE / MERGE / CTAS / CREATE VIEW / ALTER —
  `target` + `target_columns` + source `input` + `returning` +
  `conflict_updates`), `Delete` (multi-target `DeletePlan`), `Drop`
  (DROP / TRUNCATE relations), `With` + `CteRef` (shared-node CTE
  model — a CTE body is bound once at the `With` and referenced by a
  lightweight `CteRef`, so reads count once and an unreferenced CTE
  still counts).
- **Provenance is pre-collapsed bottom-up.** Each `BoundColumn`
  carries `provenance: Vec<ProvenanceSource>`, already resolved to
  real base columns, each with its composed `ColumnLineageKind`.
  A `ProvenanceSource` reached *through* a synthetic step (derived /
  CTE column, output alias, scalar subquery output) is marked
  `synthetic_origin`: it stays a lineage source (the collapsed base
  column) but is excluded from `reads` (its physical read was already
  counted at the inner producer). So extraction is a pure walk.
- The bind-time `Scope` (`Vec<Relation>` + `outputs` + `merge_columns`)
  is **scratch** — threaded bottom-up (`bind_* -> (Plan, Scope)`),
  never stored on the tree. A `Relation` is `{alias, source:
  RelationSource}` where source is `Table {table, columns:
  RelationColumns}` / `Derived {columns}` (synthetic) / `TableFunction`
  (opaque synthetic). Correlation reaches enclosing scopes via the
  binder's `outer_scopes` stack.
- The binder takes an optional `&dyn Catalog`. With a catalog, a
  matched table's columns are `Known` and column resolution is strict
  (typos → `table: None` / `ResolutionKind::Unresolved`; multi-hit →
  `Ambiguous`); catalog-free, columns are `Open` and resolved reads
  are `Inferred`.
- Identifier matching is dialect-aware (`crate::casing`). The
  extractor derives an `IdentifierCasing` from the `&dyn Dialect`
  (`IdentifierCasing::for_dialect`) and threads it into the binder;
  comparisons fold through `CaseFold::normalize`. The policy splits by
  class — `table` (catalog/schema/table), `table_alias` (aliases + CTE
  / derived / table-function names), `column` — each a `CaseFold`
  (`Upper` / `Lower` / `Insensitive` / `Sensitive`). Most dialects are
  homogeneous (PG=Lower, ANSI/Snowflake=Upper, DuckDB/SQLite=
  Insensitive); MySQL and BigQuery split (real tables `Sensitive`,
  columns/aliases `Insensitive`). Filesystem- / collation-dependent
  models (MySQL table names, SQL Server) resolve to a fixed safe
  default; a per-deployment override API is a future addition. Only
  matching folds — surfaced `TableReference` / `ColumnReference` keep
  the original identifier text.
- **Catalog canonicalization**: `table_match` rewrites a uniquely
  matched reference to its registered full `catalog.schema.name` path,
  so a bare `users` and an explicit `public.users` agree. Write
  targets canonicalize too (`canonical_target`). Without a catalog —
  or on a miss / ambiguous match — the reference stays as written, so
  `mydb.users` and `otherdb.users` stay distinct and a bare `users`
  does not merge into `mydb.users`. Column resolution is
  **right-anchored** (`qualifier_matches_table`): a partial qualifier
  like `users.col` matches `mydb.users`. DELETE-target merge identity
  is **exact** (`scope_target` / `table_identity_eq`): bare `t1`
  merges with FROM `t1` but not FROM `mydb.t1`.
- Extractors are thin wrappers around the plan engine:
  - `table_extractor` — flat list of `TableReference`s (legacy API).
  - `crud_table_extractor` — CRUD-bucketed tables (legacy API; a thin
    shim over `TableOperationExtractor` that buckets reads/writes).
  - `table_operation_extractor` — `extract_table_operations` returns
    `TableOperation { statement_kind, reads, writes,
    lineage, diagnostics }` per parsed statement.
  - `column_operation_extractor` — `extract_column_operations`
    returns `ColumnOperation { statement_kind, reads,
    writes, lineage, diagnostics }` at column granularity. `reads` /
    `writes` are plain occurrence lists; `lineage` edges carry
    `kind: ColumnLineageKind`.
- Per-statement output convention: extractors return
  `Vec<Result<X, Error>>` so one bad statement does not kill the
  rest. The plan engine is **best-effort** — an unrepresentable
  construct (e.g. a >3-segment table name) is dropped rather than
  hard-erroring, but flagged with a `TooManyTableQualifiers`
  diagnostic so the dropped relation stays observable.
- `reads` / `lineage` order is **non-contractual** (a tree-walk
  artifact); occurrence count is preserved and each reference carries
  its source span, so consumers sort by span for source order. Tests
  compare these surfaces as multisets. `writes` follow source order.

## Vocabulary

- `TableOperation` carries three parallel surfaces:
  - `reads: Vec<TableRead>` — every table the statement reads from
    (occurrence-based; a table read more than once appears more than
    once). Each `TableRead` pairs a `TableReference` identity with the
    catalog-match `ResolutionKind` (Cataloged for a unique registered
    hit, Ambiguous for several, Inferred for a miss / catalog-less).
    Unlike `ColumnRead`, the reference is always present (table names
    are written out), so `Unresolved` never arises at table
    granularity.
  - `writes: Vec<TableReference>` — every table the statement writes
    to. Bare `TableReference` — write targets are trivially resolved
    by construction.
  - `lineage: Vec<TableLineageEdge>` — directed `source → target`
    edges, only for statements that physically move data (INSERT /
    UPDATE / MERGE / CTAS / CREATE VIEW). `source` is a `TableRead`
    (same shape as `reads`'s entries); `target` stays a bare
    `TableReference`. A table that plays both roles (e.g. `DELETE t1
    FROM t1`) appears in both `reads` and `writes`.
- `ColumnOperation` mirrors the same surfaces at column
  granularity:
  - `reads: Vec<ColumnRead>` — every column reference, as a plain
    occurrence list with no clause tag. Each `ColumnRead` pairs a
    `ColumnReference` identity with a `ResolutionKind` (Cataloged /
    Inferred / Ambiguous / Unresolved). References whose walk-time
    owning binding was synthetic (CTE / derived / table function)
    are dropped — only real-storage references and unresolved names
    surface.
  - `writes: Vec<ColumnReference>` — INSERT column lists, UPDATE SET
    targets, CTAS / CREATE VIEW / ALTER VIEW columns, MERGE
    WHEN-clause writes. Write targets come straight from SQL syntax
    so they don't carry a resolution kind (trivially resolved by
    construction).
  - `lineage: Vec<ColumnLineageEdge>` — `source → target` edges with
    `kind: ColumnLineageKind` (`Passthrough` / `Transformation`).
    `source` is a `ColumnRead` (same shape as `reads`'s entries);
    sources flowing through CTE / derived intermediates are composed
    end-to-end and inherit the inner real-table ref's confidence.
    Composition yields `Transformation` if any step transforms.
    Targets: `QueryOutput { name, position }` for transient SELECT
    outputs, `Relation(ColumnReference)` for writes into a named
    relation (table or view).
- The value-vs-filter distinction is structural, not a tag: a value
  contributor is a `lineage` source; a filter-only column is in
  `reads` but not `lineage`.
- `StatementKind` — the verb of the statement; combined with the
  `reads` / `writes` split recovers every granularity distinction.
- Internal-only `ScanRole` (Read / Write) lives on `ir::Scan` to mark
  a write-target scan (kept in scope for resolution, skipped from
  reads). It is not exposed via the public API — surface it through
  `reads` / `writes` instead.
- `TableReference` is identity-only (`catalog` / `schema` / `name`).
  Alias is a use-site decoration, not part of a table's identity,
  so `HashSet<TableReference>` dedup and cross-statement comparison
  behave intuitively. The binder's `Scope` relations carry alias as a
  separate field; the public API does not currently surface it.
  Per-occurrence metadata lives on the read-side wrapper
  `TableRead { reference, resolution }` (mirrors `ColumnRead`), so
  `TableReference` stays a pure identity. Write-side surfaces
  (`writes`, `TableLineageEdge::target`) stay bare `TableReference`.
- `ColumnReference` is identity-only too (`table: Option<TableReference>`,
  `name: Ident`). `table` is `Option` for cases where resolution
  fails (ambiguous, no candidate); the column name still surfaces.
  Per-occurrence metadata (`ResolutionKind`, future per-occurrence
  fields) lives on `ColumnRead { reference, resolution }`
  on the read side, so `ColumnReference` stays a pure identity for
  dedup / cross-statement comparison. Write-side surfaces stay bare
  `ColumnReference` since writes are trivially resolved by construction.
- `ResolutionKind` (`Cataloged` / `Inferred` / `Ambiguous` /
  `Unresolved`) records *how* a `ColumnRead` / `TableRead` resolved,
  not the SQL's
  correctness. `Cataloged` means a `Known` schema (catalog or CTE /
  derived body) positively confirmed the reference; `Inferred` means
  the binder adopted a candidate without firm evidence (catalog-less
  mode, qualifier-only resolution, or Known-witness-over-Open-suspects
  tiebreaker). `Ambiguous` / `Unresolved` are the two failure modes —
  both come with `table: None` on a `ColumnRead` (`Unresolved` is
  columns-only). At table granularity the reference is always present,
  so a `TableRead` can be `Ambiguous` (the catalog matched several
  registrations) but never `Unresolved`. Invariant:
  catalog-less mode never produces `ResolutionKind::Cataloged` on the
  public surface (synthetic-origin sources are dropped from `reads`),
  so detecting catalog-aware analysis is as simple as
  `r.resolution == Cataloged` on any surviving read.

## Design conventions

- **Materialize, then walk.** The binder builds a complete `Plan`
  tree (bind = AST → tree, one pass); extraction is a pure walk of the
  clean tree. Keep the plan a *complete* representation: where a bind
  shortcut would throw info away (folding a subquery, dropping an
  unreferenced CTE, discarding a role), raise plan granularity so it's
  carried in the tree, not routed around it via side channels.
- **Bind returns `(Plan, Scope)`** bottom-up; the `Scope` is scratch
  (current frame = the subtree's output relations + introduced
  outputs; enclosing frames via `outer_scopes` for correlation). Don't
  push caller state into the binder via flag bags — use scoped helpers.
- **Value vs filter** is decided at bind time by `ExprCollector`
  (`sources` / `value_subplans` feed lineage; `filter_reads` /
  `filter_subplans` don't), with `suppressed(|c| …)` forcing filter
  position inside a `CASE` condition / `EXISTS` / `IN` / `ANY` / `ALL`
  / window key. The split is then **structural** in the tree: value
  subqueries sit on `Project.subqueries` (feeding), filter subqueries
  on a non-feeding `PassThrough` — so the extraction walk needs no tag.
- Wildcards (`SELECT *`, `t.*`) are not expanded at the parser
  level — even with a catalog. The rigor cost (USING / NATURAL JOIN
  merge, EXCLUDE / REPLACE / RENAME clauses, CTE column rename,
  multi-segment qualifiers) is too high for a SQL-text-only library
  to handle correctly. Wildcards contribute nothing to `reads` /
  `lineage`; consumers needing per-column source → target lineage
  either supply resolved query plans or do their own expansion.
- Projection-alias visibility in GROUP BY / HAVING / ORDER BY is
  **structural**: the clause ordering is a logical-plan operator stack,
  so the WHERE `PassThrough` sits *below* the `Project` (no output
  aliases visible) while GROUP BY / HAVING / ORDER BY resolve against a
  scope that includes the `Project`'s `outputs`. A bare ref there
  naming an **introduced** alias (computed expr or renamed column, e.g.
  `total` in `SELECT a+b AS total … ORDER BY total`) resolves to that
  output column's pre-collapsed provenance, marked `synthetic_origin`,
  so it stays a lineage source but drops from `reads` (no phantom
  `t.total`; the real dependency is already at the projection). An
  *identity* passthrough (`SELECT a … GROUP BY a`) falls through to
  normal resolution, so the common case still surfaces `a`. Qualified
  refs (`t.total`) are never treated as aliases. Dialect-specific
  alias-vs-column precedence (ORDER BY favours alias, GROUP BY the input
  column) is not modelled.
- `JOIN … USING (col)` merge columns **fan in**: a `USING (a)` join
  folds both sides' `a` into one COALESCE-style logical column with no
  single owner, so an unqualified ref to `a` resolves to *every* joined
  relation that could own it — one read / lineage source per side, not
  an ambiguous `table: None`. Mechanism: the binder records the USING
  names on `Scope::merge_columns`; `resolve_ref`, for an unqualified
  merge-column ref, fans in to each scope relation that could own it —
  a catalog narrows the fan-in to declaring relations (`Cataloged`),
  catalog-free reaches every joined relation (`Inferred`). Qualified
  `t.a` keeps its single owner. NATURAL JOIN is **not** expanded (its
  merge set is every same-named column of both sides — needs both
  schemas, same reason wildcards aren't expanded). Known limit: for a
  3+-relation scope the catalog-free fan-in includes every relation,
  not just the two USING operands.

## Code conventions

- Keep changes small and scoped. Preserve public API compatibility
  unless an API change is intentional, and update doc comments when
  it changes.
- **Public items deserve rustdoc** (`///` on items, `//!` on
  modules / crates). State purpose, contract, edge cases, and
  include examples where useful — rustdoc is the published API
  surface and shows up in `cargo doc`, docs.rs, and IDE hovers.
  Length is fine when it earns it.
- **Inline `//` comments**: keep them concise and well-structured.
  Add a short example when it clarifies.
- Prefer private modules; export through explicit re-exports in
  `lib.rs`.
- Avoid `bool` or ambiguous `Option` parameters in new public APIs.
  Prefer enums, named methods, or small option structs.
- Avoid growing large modules. Split before a file becomes
  unscannable.
- Keep `sqlparser-rs` AST `match` arms exhaustive in the binder
  and extractors — wildcard arms silently hide newly added variants.
  Likewise keep the `match plan { … }` arms over `ir::Plan` exhaustive
  in `extract`.
- Public enums are **exhaustive (no `#[non_exhaustive]`) while pre-1.0**
  (`StatementKind` / `ColumnLineageKind` / `ColumnTarget` /
  `ResolutionKind` / `TableLevelDiagnosticKind` /
  `ColumnLevelDiagnosticKind`). Adding a variant is therefore a
  breaking change on purpose — pre-1.0 that rides a `0.x` bump and
  forces consumers to re-acknowledge the new case rather than
  silently hitting a wildcard arm. Add `#[non_exhaustive]` at the
  1.0 freeze (removing it later is non-breaking; adding it is
  breaking, so the 1.0 boundary is the place). Keep internal
  `match`es exhaustive regardless.
- Diagnostics are reserved for **tool-side coverage gaps**, not
  per-reference resolution outcomes. `TableLevelDiagnostic` carries
  `UnsupportedStatement` and `TooManyTableQualifiers` (an
  over-qualified, >`catalog.schema.name`, table name that's dropped);
  `ColumnLevelDiagnostic` is the superset, adding `WildcardSuppressed`.
  The binder produces the column-level set; table-level surfaces
  project it down via `ColumnLevelDiagnostic::to_table_level`
  (exhaustive match, so a new column kind forces a table-level
  decision — `WildcardSuppressed` maps to `None`). Per-occurrence
  resolution status (ambiguous / unresolved column refs) lives on
  `ColumnRead::resolution`, not in a parallel diagnostic stream.
- For unsupported SQL, accumulate diagnostics instead of `?`-bailing
  mid-walk. Reserve hard errors for genuinely unrecoverable
  conditions.
- Tests: compare whole values over field-by-field assertions, but
  treat `reads` / `lineage` as **multisets** (order is non-contractual)
  — use the `assert_unordered_eq!` helper; `writes` stay order-exact.
  Use a layered helper convention — `extract` → `extract_with(dialect)`
  → `extract_with_catalog(dialect, catalog)` — so callsites stay terse
  and new parameters fall through cleanly.
- Tests double as behavior documentation: a reader should be able to
  learn what a given SQL construct produces by reading its test, so
  prefer concrete, minimal SQL with the full expected value spelled
  out over clever parameterization that hides the input/output pair.
  Per-construct "arm coverage" modules (one terse case per AST
  variant / statement kind) are encouraged — they both pin behavior
  and force a new test when an exhaustive `match` gains a variant.
  Adding tests is cheap and welcome; err on the side of more
  coverage rather than less.
