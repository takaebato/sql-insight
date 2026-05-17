# CLAUDE.md

## Project

Rust workspace: `sql-insight` library + `sql-insight-cli`. SQL parsing is
built on `sqlparser-rs`; always work against its AST, never re-parse SQL
by hand.

## Commands

- Format: `cargo fmt`
- Test: `cargo test --all`
- Lint: `cargo clippy --all-targets -- -D warnings` (zero-warning policy)

## Architecture

- The `resolver` module walks a `Statement` once and produces a
  `Resolution`:
  - a scope arena of `Binding`s (`Table` / `Cte` / `DerivedTable` /
    `TableFunction`),
  - a buffer of `RawColumnRef`s captured at walk time with
    resolved-table + synthetic-vs-real + clause-kind metadata,
  - a buffer of `FlowEdge`s emitted directly during the walk.
  Two post-passes on `into_resolution` compose the flow graph
  end-to-end through CTE / derived intermediates and filter reads
  down to references whose walk-time owner was a real `Table`.
  Sub-modules are split by responsibility: `binding` (scope arena),
  `context` (`VisitContext`), `column_ref`, `projection`, `flow`,
  `composition`, `rename`; walker files (`expr` / `query` /
  `statement` / `table`) live as siblings and add `visit_*` methods
  via `impl Resolver` blocks.
- Pull-style design: `resolve_query` returns a `ResolvedQuery`
  carrying the body's `projections: Vec<ProjectionGroup>`. Callers
  (visit_insert / CTAS / scalar subqueries / etc.) decide what to do
  with them — pair with target columns, emit `QueryOutput` edges,
  bubble up through `SetExpr::Query`, etc.
- The resolver takes an optional `&dyn Catalog`. With a catalog,
  Table bindings come back with `Known` schemas and unqualified
  column resolution becomes strict (typos surface as `table: None`).
  Without a catalog the resolver is best-effort.
- Extractors consume the resolver's output:
  - `table_extractor` — flat list of `TableReference`s (legacy API).
  - `crud_table_extractor` — CRUD-bucketed tables (legacy API).
  - `operation_extractor` — `extract_table_operations` returns
    `StatementTableOperations { statement_kind, reads, writes,
    flows, diagnostics }` per parsed statement.
  - `column_operation_extractor` — `extract_column_operations`
    returns `StatementColumnOperations { statement_kind, reads,
    writes, flows, diagnostics }` at column granularity. Reads
    carry `kinds: Vec<ReadKind>`; flows carry `kind: ColumnFlowKind`.
- Per-statement output convention: extractors return
  `Vec<Result<X, Error>>` so one bad statement does not kill the
  rest.

## Vocabulary

- `StatementTableOperations` carries three parallel surfaces:
  - `reads: Vec<TableRead>` — every table the statement reads from.
  - `writes: Vec<TableWrite>` — every table the statement writes to.
  - `flows: Vec<TableFlow>` — directed `source → target` edges, only
    for statements that physically move data (INSERT / UPDATE /
    MERGE / CTAS / CREATE VIEW). A table that plays both roles
    (e.g. `DELETE t1 FROM t1`) appears in both `reads` and `writes`.
- `StatementColumnOperations` mirrors the same surfaces at column
  granularity:
  - `reads: Vec<ColumnRead>` — every column reference, with
    `kinds: Vec<ReadKind>` recording syntactic clause role
    (`Projection` / `Filter` / `GroupBy` / `Sort` / `Window`, plus a
    `Conditional` modifier for CASE-WHEN condition refs). References
    whose walk-time owning binding was synthetic (CTE / derived /
    table function) are dropped — only real-storage references and
    unresolved names surface.
  - `writes: Vec<ColumnWrite>` — INSERT column lists, UPDATE SET
    targets, CTAS / CREATE VIEW / ALTER VIEW columns, MERGE
    WHEN-clause writes.
  - `flows: Vec<ColumnFlow>` — `source → target` edges with
    `kind: ColumnFlowKind` (`Passthrough` / `Aggregation` /
    `Computed`). Sources flowing through CTE / derived intermediates
    are composed end-to-end; the composition is `Aggregation`-
    dominant. Targets: `QueryOutput { name, position }` for
    transient SELECT outputs, `Persisted(ColumnReference)` for
    writes into a real relation.
- `StatementKind` — the verb of the statement; combined with the
  `reads` / `writes` split recovers every granularity distinction.
- Internal-only `TableRole` (Read / Write) lives inside the resolver
  for binding metadata. It is not exposed via the public API —
  surface it through `reads` / `writes` instead.
- `TableReference` is identity-only (`catalog` / `schema` / `name`).
  Alias is a use-site decoration, not part of a table's identity,
  so `HashSet<TableReference>` dedup and cross-statement comparison
  behave intuitively. Resolver bindings carry alias as a separate
  field; the public API does not currently surface it.
- `ColumnReference` is identity-only too (`table: Option<TableReference>`,
  `name: Ident`). `table` is `Option` for cases where resolution
  fails (ambiguous, no candidate); the column name still surfaces.

## Design conventions

- Pull design: `resolve_query` collects facts (projections), callers
  decide edge construction. Avoid pushing state from caller into
  resolver via flag bags — instead expose helpers like
  `with_filter_clause` / `with_branch_scope` for scoped, lexical
  context.
- Walking-context state lives in `VisitContext` (`scope_kind` /
  `read_kind` / `in_case_condition`) — "in effect for the current
  visit", not "queued". Save / restore goes through `with_context`
  (and the focused `with_read_kind` / `with_branch_scope` /
  `with_filter_clause` / `with_case_condition` helpers) so the prior
  context is restored on scope exit. `resolve_query` resets the
  fields that don't propagate through a subquery boundary
  (`read_kind`, `in_case_condition`) but preserves `scope_kind` so
  predicate-ness flows transitively. For owning per-query buffers
  like `current_projections: Vec<…>`, `mem::replace` is used
  instead.
- Wildcards (`SELECT *`, `t.*`) are not expanded at the parser
  level — even with a catalog. The rigor cost (USING / NATURAL JOIN
  merge, EXCLUDE / REPLACE / RENAME clauses, CTE column rename,
  multi-segment qualifiers) is too high for a SQL-text-only library
  to handle correctly. Wildcards contribute nothing to `reads` /
  `flows`; consumers needing per-column lineage either supply
  resolved query plans or do their own expansion.
- Aggregate function classification combines spec-guaranteed
  structural markers (`FILTER (WHERE …)`, `WITHIN GROUP (…)`,
  `DISTINCT` in args — all aggregate-only per SQL standard) with a
  union name list of common aggregates across major dialects.
  Window-only functions are excluded.

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
- Keep `sqlparser-rs` AST `match` arms exhaustive in the resolver
  and extractors — wildcard arms silently hide newly added variants.
- Public enums that may grow new variants are `#[non_exhaustive]`
  so adding variants stays SemVer-minor (ReadKind / ColumnFlowKind /
  ColumnTarget / etc.).
- Use `Vec<Kind>` on classification fields where multi-role
  references are plausible (`ColumnRead.kinds`) — leaves room for
  features like USING / NATURAL JOIN merge without an API break.
- For unsupported SQL, accumulate diagnostics (`Diagnostic` /
  `OperationDiagnostic`) instead of `?`-bailing mid-walk. Reserve
  hard errors for genuinely unrecoverable conditions.
- Tests: compare whole values (`assert_eq!(ops.reads, vec![...])`)
  over field-by-field assertions. Use a layered helper convention
  — `extract` → `extract_with(dialect)` → `extract_with_catalog(
  dialect, catalog)` — so callsites stay terse and new parameters
  fall through cleanly.
