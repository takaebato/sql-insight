# CLAUDE.md

## Project

Rust workspace: `sql-insight` library + `sql-insight-cli`. SQL parsing is built
on `sqlparser-rs`; always work against its AST, never re-parse SQL by hand.

## Commands

- Format: `cargo fmt`
- Test: `cargo test --all`
- Lint: `cargo clippy --all-targets -- -D warnings` (zero-warning policy)

## Architecture

- `resolver/relation_resolver.rs` walks a `Statement` and builds a scope
  arena of `RelationBinding`s (`Table` / `Cte` / `DerivedTable` /
  `TableFunction`). It accepts an optional `&dyn Catalog` for relation-level
  enrichment but does not touch columns; column resolution belongs to a
  future, separate visitor.
- Extractors consume the resolver's output:
  - `table_extractor` — flat list of `TableReference`s (legacy API).
  - `crud_table_extractor` — CRUD-bucketed tables (legacy API).
  - `operation_extractor` — `extract_table_operations` returns
    `StatementTableOperations { statement_kind, table_operations, table_flows,
    diagnostics }` per parsed statement. `extract_column_operations` and an
    `extract_operations` façade are planned for Phase 5.
- Per-statement output convention: extractors return
  `Vec<Result<X, Error>>` so one bad statement does not kill the rest.

## Vocabulary

- `StatementTableOperations` carries three parallel surfaces:
  - `reads: Vec<TableRead>` — every table the statement reads from.
  - `writes: Vec<TableWrite>` — every table the statement writes to.
  - `flows: Vec<TableFlow>` — directed `source → target` edges, only for
    statements that physically move data (INSERT / UPDATE / MERGE / CTAS
    / CREATE VIEW). A table that plays both roles (e.g. `DELETE t1 FROM
    t1`) appears in both `reads` and `writes`.
- `StatementKind` — the verb of the statement; combined with the
  `reads` / `writes` split recovers every table-granularity distinction.
- Internal-only `TableRole` (Read / Write) lives inside the resolver
  for binding metadata. It is not exposed via the public API — surface
  it through `reads` / `writes` instead.
- `TableReference` is identity-only (`catalog` / `schema` / `name`).
  Alias is a use-site decoration, not part of a table's identity, so
  `HashSet<TableReference>` dedup and cross-statement comparison
  behave intuitively. Resolver bindings carry alias as a separate
  field; the public API does not currently surface it.

## Conventions

- Keep changes small and scoped. Preserve public API compatibility unless an
  API change is intentional, and update doc comments when it changes.
- **Public items deserve rustdoc** (`///` on items, `//!` on
  modules / crates). State purpose, contract, edge cases, and include
  examples where useful — rustdoc is the published API surface and shows
  up in `cargo doc`, docs.rs, and IDE hovers. Length is fine when it
  earns it.
- **Inline `//` comments**: keep them concise and well-structured. Add
  a short example when it clarifies.
- Prefer private modules; export through explicit re-exports in `lib.rs`.
- Avoid `bool` or ambiguous `Option` parameters in new public APIs. Prefer
  enums, named methods, or small option structs.
- Avoid growing large modules. Split before a file becomes unscannable.
- Keep `sqlparser-rs` AST `match` arms exhaustive in the resolver and
  extractors — wildcard arms silently hide newly added variants.
- For unsupported SQL, accumulate diagnostics (`Diagnostic` /
  `OperationDiagnostic`) instead of `?`-bailing mid-walk. Reserve hard
  errors for genuinely unrecoverable conditions.
- Tests: compare whole values (`assert_eq!(ops.table_operations, vec![...])`)
  over field-by-field assertions. Use a layered helper convention —
  `extract` → `extract_with(dialect)` → `extract_with_catalog(dialect,
  catalog)` — so callsites stay terse and new parameters fall through
  cleanly.
