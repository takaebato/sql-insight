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

- `TableRole` (`Read` / `Write`) — the role a table plays in a statement.
- `TableUsage` (`Target` / `From` / `Projection` / `Predicate` / `Join` /
  `WriteValue`) — finer position-axis enrichment (mostly future).
- `StatementKind` — the verb of the statement; combined with `TableRole`
  recovers every table-granularity distinction.

## Conventions

- Keep changes small and scoped. Preserve public API compatibility unless an
  API change is intentional, and update doc comments when it changes.
- Default to writing no inline comments. Add one only when the *why* is
  non-obvious — a hidden constraint, a subtle invariant, or surprising
  behavior. Do not restate what the code does (good names already do that)
  and do not reference task or PR context. Keep them short; no multi-line
  comment blocks.
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
