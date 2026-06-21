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
  tree** (`logical_plan::LogicalPlan`) and walks that tree for the
  extraction surfaces. It is *not* an execution plan — nothing
  optimizes or runs SQL. `resolver.rs` is the **facade**: `build` plus
  the seven free-function surfaces (`reads` / `table_reads` / `writes`
  / `table_writes` / `column_lineage` / `table_lineage` /
  `flat_tables`), each a thin entry delegating to a concern submodule
  (so `LogicalPlan` stays plain data — extraction is a pass *over* the
  tree, not a method *on* it). Sub-modules:
  - `logical_plan` — the bound operator-tree types (the `LogicalPlan`
    enum + its node structs) and the shared tree-navigation helpers
    (`own_exprs` / `children` / `own_expr_subplans` / `peel_with` /
    `idents_eq`) every extraction walker uses.
  - `binder` — `build_with_diagnostics(stmt, catalog, casing) ->
    (LogicalPlan, Vec<ColumnLevelDiagnostic>)`, the bind pass
    (AST → resolved `LogicalPlan`). Split by concern into
    `binder/{statement, query, expr, resolve}` (each an `impl Binder`
    block) over the shared `binder/scope` model (`Scope` / `Relation` /
    `OutputCol` / `CteDecl`); the root holds the `Binder` context and
    the plan- / AST-construction helpers.
  - `reads` / `origins` / `lineage` / `tables` — the extraction walkers
    backing the facade. `origins` is the `getColumnOrigins`-style value
    trace that `lineage` builds on; `tables` covers the write surfaces
    and the flat table list. `classify_statement` (in `extractor`)
    supplies the verb; column-level diagnostics project down to the
    table level via `ColumnLevelDiagnostic::to_table_level`.
- `LogicalPlan` is a **standard relational-algebra tree** — recognizable
  to anyone who knows logical plans. Query operators: `Scan` (named
  real-table leaf, carrying `columns: Columns` and `resolution:
  ResolutionKind`), `Filter` (WHERE / HAVING / any non-feeding predicate
  reads), `Join`, `Aggregate` (GROUP BY keys), `Projection` (the only
  column-defining producer — `exprs: Vec<NamedExpr>`), `Sort` (ORDER BY
  keys), `SetOp` (positional fan-in), `SubqueryAlias` (a named derived
  table), `TableFunction` (opaque dynamic-column leaf), `Values`,
  `With` + `Cte` + `CteRef` (shared-node CTE model — a CTE body is bound
  once on the `With` and referenced by a lightweight `CteRef`, so reads
  count once and an unreferenced CTE still counts), `Empty`; plus
  distinct DML / DDL roots `Insert` / `Update` / `Delete` / `Merge` /
  `CreateTableAs` / `CreateView` / `AlterTable` / `Drop`. The clause
  stack is canonical: `Scan → Filter(WHERE) → Aggregate(GROUP BY) →
  Filter(HAVING) → Projection → Sort(ORDER BY)`.
- **Lineage is traced at walk time, not pre-collapsed.** A resolved
  column reference on the tree is a `BoundColumn { qualifier, name,
  binding }`; `binding: Binding` is `Base { table, resolution }` /
  `Derived` / `Unresolved` / `Ambiguous`. The `origins` walk collapses
  a `Derived` reference to its real base columns *lazily*, tracing into
  the producing `Projection` / `CteRef` / `SubqueryAlias` and composing
  the `ColumnLineageKind` end-to-end; `reads` drops `Derived` (its
  physical read was counted at the inner producer). So extraction is a
  pure walk of a clean tree — no provenance is baked onto the nodes.
- The bind-time `Scope` (`relations: Vec<Relation>` + `query_outputs` +
  `merge_columns`) is **scratch** — threaded bottom-up (`bind_* ->
  (LogicalPlan, Scope)`), never stored on the tree, with its operations
  as `Scope` methods (`single` / `from_relations` / `absorb` /
  `add_merge_columns` / `with_query_outputs` / `exposed_columns`). A
  `Relation` is an enum: `Table { alias, table, columns }` / `Derived
  { alias, columns }` (synthetic) / `TableFunction { alias }` (opaque
  synthetic). Correlation reaches enclosing scopes via the binder's
  `outer` stack; CTEs in scope live on the binder's `ctes`
  (`Vec<CteDecl>`).
- The binder takes an optional `&Catalog`. With a catalog, a matched
  table's columns are `Columns::Cataloged` and column resolution is
  strict (typos → `table: None` / `ResolutionKind::Unresolved`;
  multi-hit → `Ambiguous`); catalog-free, columns are `Columns::Unknown`
  and resolved reads are `Inferred`.
- Identifier matching is dialect-aware (`crate::casing`). The
  extractor derives an `IdentifierCasing` from the `&dyn Dialect`
  (`IdentifierCasing::for_dialect`) and threads it into the binder —
  bundled with the dialect's surface quote char (`canonical_quote`) as
  the internal `IdentifierStyle { casing, quote }` (casing is a
  user-overridable matching policy via `ExtractorOptions::with_casing`;
  quote is always dialect-derived, a surface concern). Comparisons fold
  through `CaseRule::normalize` (quote-aware: it keys on quoted-vs-unquoted
  and the fold, never the quote *char*). The policy splits by class —
  `table` (catalog/schema/table), `table_alias` (aliases + CTE / derived /
  table-function names), `column` — each a `CaseRule` (`Upper` / `Lower` /
  `Insensitive` / `Sensitive`). Most dialects are homogeneous (PG=Lower,
  ANSI/Snowflake/Oracle=Upper, DuckDB/SQLite/Hive/Databricks/Redshift=
  Insensitive, ClickHouse=Sensitive); MySQL and BigQuery split (real
  tables `Sensitive`, columns/aliases `Insensitive`). Filesystem- /
  collation-dependent models (MySQL table names, SQL Server) and
  config-dependent ones (Redshift `enable_case_sensitive_identifier`,
  Snowflake `QUOTED_IDENTIFIERS_IGNORE_CASE`) resolve to a fixed safe
  default; a per-deployment override API is a future addition. Only
  matching folds — a *written* (non-canonical) `TableReference` /
  `ColumnReference` keeps the original identifier text.
- **Catalog canonicalization**: `table_match` rewrites a uniquely
  matched reference to its registered full `catalog.schema.name` path,
  so a bare `users` and an explicit `public.users` agree. The canonical
  identity is **case-exact**, so it surfaces **quoted** with the dialect's
  `canonical_quote` (`canonical_ref`) — else a later qualifier fold under
  an upper-folding dialect would re-case it and fail to match its own
  relation. Write targets canonicalize too (same `table_match` path).
  Without a catalog, the reference stays exactly as written. On a miss /
  ambiguous match it surfaces *default-normalized* (`surface_with_defaults`):
  omitted prefix segments filled from the catalog's `default_schema` /
  `default_catalog` as **unquoted** (foldable) idents — an unconfirmed
  search-path qualifier, so it still fold-matches a plain column
  qualifier — while written segments stay verbatim. With no configured
  defaults this equals the written ref, so `mydb.users` and `otherdb.users`
  stay distinct and a bare `users` does not merge into `mydb.users`.
  Column resolution is **right-anchored** (`qualifier_matches_table`): a
  partial qualifier like `users.col` matches `mydb.users`. DELETE-target
  merge identity is **exact** (`scope_target` / `table_identity_eq`): bare
  `t1` merges with FROM `t1` but not FROM `mydb.t1`.
- **Two-level identity equality.** `TableReference` / `ColumnReference`
  derive *structural* `Eq` / `Hash` (case- and quote-sensitive) — the right
  dedup for catalog-backed analysis (matched refs are canonicalized) and
  direct cross-statement comparison. For **catalog-free** dedup, where one
  table appears under fold-equivalent spellings (`users` vs `USERS`),
  `identity_key(&IdentifierCasing)` / `same_table` / `same_column` fold by a
  dialect's casing. The key is an **opaque** `TableIdentityKey` /
  `ColumnIdentityKey` (`Eq` + `Hash`, no readable value) so the fold output
  never surfaces — it's identity (every present segment significant, not
  the resolver's right-anchored wildcard matching).
- Extractors are thin wrappers around the plan engine:
  - `table_extractor` — flat list of `TableReference`s.
  - `crud_table_extractor` — CRUD-bucketed tables (a thin shim over
    `TableOperationExtractor` that buckets reads/writes).
  - `table_operation_extractor` — `extract_table_operations` returns
    `TableOperation { statement_kind, reads, writes,
    lineage, diagnostics }` per parsed statement.
  - `column_operation_extractor` — `extract_column_operations`
    returns `ColumnOperation { statement_kind, reads,
    writes, lineage, diagnostics }` at column granularity. `reads` /
    `writes` are plain occurrence lists; `lineage` edges carry
    `kind: ColumnLineageKind`.
- Each extractor exposes the **`normalizer`-style pair**: a convenience
  `extract_*(dialect, sql)` (dialect defaults — no catalog, dialect-derived
  casing) and `extract_*_with_options(dialect, sql, options)` taking a
  shared `ExtractorOptions { catalog, casing }` (in `extractor.rs`,
  builder: `new` / `with_catalog` / `with_casing`). Catalog and casing are
  *inputs* in the options, not positional args; all four extractors accept
  a catalog this way (the flat / CRUD lists canonicalize matched tables
  too). `casing: Option<IdentifierCasing>` is `None` = derive from dialect
  (`casing_for`). `CaseRule` / `IdentifierCasing` are re-exported at the
  crate root; `ExtractorOptions` lives in `extractor`.
- Per-statement output convention: extractors return
  `Vec<Result<X, Error>>` so one bad statement does not kill the
  rest. The plan engine is **best-effort** — an unrepresentable
  construct (e.g. a >3-segment table name) is dropped rather than
  hard-erroring, but flagged with a `TooManyTableQualifiers`
  diagnostic so the dropped relation stays observable.
- `reads` / `lineage` are returned in **source order** — the facade
  re-sorts the walkers' (walk-order) output by each reference's
  written-token span (`reference.name.span`), so the surfaces are a
  deterministic function of the SQL, not the internal walk; references
  sharing a token (USING fan-in) keep a stable relative order, and a
  catalog-filled prefix segment (no source token) has an empty span, so
  the **name** segment is the sort key. Occurrence count is preserved.
  Tests still compare these surfaces as multisets (span-agnostic; the order
  is pinned by a dedicated test). `writes` follow source order too.
- `reads` is **occurrence-based, by token** — each *syntactic* appearance
  of a base-column reference is one read, not each *physical* read. The
  unit is "the column name appeared as a token here", not "the engine
  re-read the table": `SELECT a FROM t WHERE a > 0` reads `t.a` twice
  (projection + WHERE), and `SELECT a FROM t GROUP BY a` twice (projection
  + GROUP BY). A **post-projection clause** (GROUP BY / HAVING / ORDER BY)
  is the subtle case, because output aliases are visible there: a token
  naming a **base column** (an *identity* output — `a`, or the redundant
  `a AS a`) counts as another occurrence, but a token naming only an
  **introduced output alias** (`ORDER BY x` for `a AS x`, or `ORDER BY s`
  for `a + b AS s`) is a reference to the projection *output*, not a base
  occurrence — it binds `Binding::Derived` and is **not** a read (the
  dependency was already counted at the projection). So `GROUP BY a`
  (counts) and `ORDER BY x` (doesn't) are deliberately **asymmetric** in
  `reads`. `lineage` is **symmetric** — each captures the `a -> output`
  dependency once at the projection — so dependency comprehension is
  unaffected; the asymmetry lives only in the read occurrence count. (See
  the projection-alias visibility convention below for the mechanism.)

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
- A DML target is **not a `Scan`** in the tree — it's named on the DML
  root (`Insert.target` / `Update.target` / `Merge.target` / …), and the
  root's `input` carries only the *read* relations. So a write target
  never lands in `reads` by construction (there is no scan-role flag to
  skip); it surfaces only through `writes` / `table_writes`. Its columns
  are still bound — for resolving SET / WHERE / RETURNING — via a scratch
  target `Scope` that is then discarded.
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
  correctness. `Cataloged` means a known column list (a catalog table, or
  a CTE / derived body) positively confirmed the reference; `Inferred` means
  the binder adopted a candidate without firm evidence (catalog-less
  mode, qualifier-only resolution, or the
  `Cataloged`-witness-over-`Unknown`-suspect tiebreaker). `Ambiguous` / `Unresolved` are the two failure modes —
  both come with `table: None` on a `ColumnRead` (`Unresolved` is
  columns-only). At table granularity the reference is always present,
  so a `TableRead` can be `Ambiguous` (the catalog matched several
  registrations) but never `Unresolved`. Invariant:
  catalog-less mode never produces `ResolutionKind::Cataloged` on the
  public surface (synthetic-origin sources are dropped from `reads`),
  so detecting catalog-aware analysis is as simple as
  `r.resolution == Cataloged` on any surviving read.

## Design conventions

- **Materialize, then walk.** The binder builds a complete `LogicalPlan`
  tree (bind = AST → tree, one pass); extraction is a pure walk of the
  clean tree. Keep the plan a *complete* representation: where a bind
  shortcut would throw info away (folding a subquery, dropping an
  unreferenced CTE, discarding a role), raise plan granularity so it's
  carried in the tree, not routed around it via side channels.
  - **Why a *materialized* tree** (rather than fusing bind and extraction
    into one AST pass): the tree exists for the **breadth**, not for the
    lineage collapse. It is walked by seven independent extraction
    surfaces (`reads` / `table_reads` / `writes` / `table_writes` /
    `column_lineage` / `table_lineage` / `flat_tables`), each with its own
    traversal rules; a complete tree lets each be a pure, separate walk
    instead of seven rule-sets interleaved into the bind. It also keeps
    the value/filter split **structural** (positional in the tree, not a
    clause tag threaded through bind — the side channel the design
    rejects) and models a CTE as a **shared node** (bound once, referenced
    by a lightweight `CteRef` the walk resolves to its body on demand —
    without the tree you'd rebuild a CTE registry and re-traverse, i.e.
    reinvent part of the tree). For column-lineage collapse *alone* a
    fused bind+collapse pass would suffice; the tree is the price of that
    breadth, paid once and discarded (it is not an execution plan, nothing
    optimizes or reuses it).
- **Bind returns `(LogicalPlan, Scope)`** bottom-up; the `Scope` is
  scratch (current frame = the subtree's relations + introduced
  `query_outputs`; enclosing frames via the binder's `outer` stack for
  correlation). Don't push caller state into the binder via flag bags —
  spawn a child binder (`with_ctes` / `with_outer`) and thread the
  `Scope`.
- **Value vs filter** is decided at bind time and made **structural** in
  the `Expr` it lowers to: a value operand becomes `Expr::Call` /
  `Expr::Window` `arg` / a scalar `Expr::Subquery` (the `origins` trace
  follows these), while a filter operand is bucketed into `Expr::Filter`
  (the `suppress` helper) or is a construct's filter slot (a `CASE`
  `when`, a window `partition` / `order` key, `Expr::Exists` /
  `Expr::InSubquery`). The clause split is structural too — WHERE /
  HAVING are `Filter` nodes, the SELECT list is the `Projection`. So the
  extraction walk needs no clause tag: `reads` walks every column,
  `origins` follows only the value operands.
- Wildcards (`SELECT *`, `t.*`) are not expanded at the parser
  level — even with a catalog. The rigor cost (USING / NATURAL JOIN
  merge, EXCLUDE / REPLACE / RENAME clauses, CTE column rename,
  multi-segment qualifiers) is too high for a SQL-text-only library
  to handle correctly. Wildcards contribute nothing to `reads` /
  `lineage`; consumers needing per-column source → target lineage
  either supply resolved query plans or do their own expansion.
- Projection-alias visibility in GROUP BY / HAVING / ORDER BY is
  **structural**: the clause ordering is a logical-plan operator stack,
  so the WHERE `Filter` sits *below* the `Projection` (no output aliases
  visible) while GROUP BY / HAVING / ORDER BY resolve against a scope that
  includes the `Projection`'s outputs. A bare ref there naming an
  **introduced** alias (computed expr or renamed column, e.g. `total` in
  `SELECT a+b AS total … ORDER BY total`) binds `Binding::Derived` and the
  origin traversal traces it through the output column to its base origins,
  so it stays a lineage source but drops from `reads` (no phantom
  `t.total`; the real dependency is already at the projection — this is the
  occurrence-based `reads` policy above). An *identity* passthrough
  (`SELECT a … GROUP BY a`) falls through to normal resolution, so the
  common case still surfaces `a`. **Identity is name-equality, not alias
  presence**: an output is identity iff it is a bare column whose output
  name equals that column's own name, so the redundant `a AS a` is identity
  too (keeps the `GROUP BY a` read), while `a AS x` is not. Qualified refs
  (`t.total`) are never treated as aliases. Dialect-specific alias-vs-column
  precedence (ORDER BY favours alias, GROUP BY the input column) is not
  modelled.
- `JOIN … USING (col)` merge columns **fan in**: a `USING (a)` join
  folds both sides' `a` into one COALESCE-style logical column with no
  single owner, so an unqualified ref to `a` resolves to *every* joined
  relation that could own it — one read / lineage source per side, not
  an ambiguous `table: None`. Mechanism: the binder records the USING
  names on `Scope::merge_columns`; `merge_fanin`, for an unqualified
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
- **Comment style** (applies to `///` / `//!` / `//`):
  - **Each fact lives once, at its most specific level.** A module / crate
    doc orients and links; per-construct detail belongs on the type /
    field. Don't restate a field's contract in the module doc, or a
    dialect→rule mapping in both a variant doc and the function that owns
    it — duplicated docs drift (one side gets updated, the other rots).
  - **Prefer structure over prose.** A bold lead-in (`**Foo.** …`) or a
    hierarchical bullet list scans better and dedups better than a long
    paragraph. Restructure, don't pad.
  - **Keep docs true to the code.** Watch for drift: a renamed
    symbol mentioned in prose, a behaviour that changed, a stale "we don't
    yet …". `cargo doc` / `clippy` can't catch wrong-but-valid prose — a
    human read is the only guard.
  - **Trim redundancy, keep the load-bearing.** Cut what restates code or
    repeats another doc; keep purpose, contract, edge cases, the *why*,
    and worked examples (doctests are contract — never cut).
- Prefer private modules; export through explicit re-exports in
  `lib.rs`.
- Avoid `bool` or ambiguous `Option` parameters in new public APIs.
  Prefer enums, named methods, or small option structs.
- Avoid growing large modules. Split before a file becomes
  unscannable.
- Keep `sqlparser-rs` AST `match` arms exhaustive in the binder
  and extractors — wildcard arms silently hide newly added variants.
  Likewise keep the `match` arms over `LogicalPlan` exhaustive in the
  extraction walkers (`reads` / `origins` / `lineage` / `tables`).
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
  `ColumnLevelDiagnostic` is the superset, adding `WildcardSuppressed`
  and `InsertColumnsUnresolved` (a column-list-less INSERT / MERGE-INSERT
  whose target columns can't be filled without a catalog). The binder
  produces the column-level set; table-level surfaces project it down
  via `ColumnLevelDiagnostic::to_table_level` (exhaustive match, so a new
  column kind forces a table-level decision — `WildcardSuppressed` and
  `InsertColumnsUnresolved` both map to `None`, being column-only gaps).
  Per-occurrence resolution status (ambiguous / unresolved column refs)
  lives on `ColumnRead::resolution`, not in a parallel diagnostic stream.
- For unsupported SQL, accumulate diagnostics instead of `?`-bailing
  mid-walk. Reserve hard errors for genuinely unrecoverable
  conditions.
- Tests: compare whole values over field-by-field assertions, but
  treat `reads` / `lineage` as **multisets** (span-agnostic; the source
  order they're returned in is pinned by a dedicated test) — use the
  `assert_unordered_eq!` helper; `writes` stay order-exact.
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
