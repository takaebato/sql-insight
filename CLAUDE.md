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

The design — the why, the shape, and the invariants — lives in
**`ARCHITECTURE.md`**. Read it before non-trivial resolver / extractor
work. Per-type contracts are in rustdoc (`cargo doc
--document-private-items`); don't restate them in prose.

Module map:

- `resolver` (private) — the analysis engine. `resolver.rs` is the facade
  (`build` + seven extraction free fns); `binder/` binds AST → a
  materialized `logical_plan::LogicalPlan` tree; `reads` / `origins` /
  `lineage` / `tables` walk it. `casing` / `catalog` feed it.
- `extractor` — thin public wrappers over the engine: `extract_tables` /
  `extract_crud_tables` / `extract_table_operations` /
  `extract_column_operations`, each with an `_with_options` twin taking
  `ExtractorOptions { catalog, casing }`. `StatementKind` /
  `classify_statement` live here.
- `reference` / `diagnostic` / `casing` / `catalog` / `error` — the public
  vocabulary (identities, diagnostics, the dialect casing policy, the
  schema registry).
- `normalizer` / `formatter` — independent AST-rewrite utilities.

## Invariants & gotchas

Things that bite if violated or forgotten (the why is in `ARCHITECTURE.md`):

- **Surfaces are a deterministic function of the SQL.** `reads` / `lineage`
  / `flat_tables` / `writes` are returned in **source order** (sorted by
  each reference's written `name.span`), not walk order — so changing a walk
  doesn't change output. Tests compare `reads` / `lineage` as **multisets**
  (`assert_unordered_eq!`, span-agnostic); the order is pinned by one
  dedicated test. `writes` stay order-exact.
- **`reads` is occurrence-based, by token** — each syntactic appearance of a
  base-column reference is one read (`SELECT a FROM t WHERE a>0` reads `t.a`
  twice). A post-projection clause (GROUP BY / HAVING / ORDER BY) naming an
  *identity* output (`GROUP BY a`, incl. the redundant `a AS a`) counts;
  naming an *introduced* alias (`ORDER BY x` for `a AS x`) does **not** (it
  binds `Derived`, dropped — the dependency is already at the projection).
  `lineage` is symmetric (captures it once either way); the asymmetry is
  only in the read count.
- **value vs filter is structural** — a value contributor is a `lineage`
  source, a filter-only column is in `reads` but not `lineage`. No tag.
- **A DML target is not a `Scan`** — it's named on the DML root, so it never
  lands in `reads`; it surfaces only via `writes` / `table_writes`.
- **Best-effort, never panic-on-input.** An unrepresentable construct (e.g.
  a >3-segment table name) is dropped, not `?`-bailed, and flagged with a
  diagnostic. Extractors return `Vec<Result<_, Error>>` per statement; a
  *parse* error fails the whole call.
- **Diagnostics are tool-side coverage gaps only** (unsupported statement,
  over-qualified name, suppressed wildcard, column-list-less INSERT) — never
  per-reference resolution status, which lives on `ColumnRead::resolution`.
- **Catalog-free never yields `Cataloged`** on the public surface, so
  `r.resolution == Cataloged` detects catalog-aware analysis.

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
  Use a layered helper convention — a base `assert_*` plus
  `_with_dialect` / `_with_catalog` variants that add one parameter each —
  so callsites stay terse and new parameters fall through cleanly.
- Tests double as behavior documentation: a reader should be able to
  learn what a given SQL construct produces by reading its test, so
  prefer concrete, minimal SQL with the full expected value spelled
  out over clever parameterization that hides the input/output pair.
  Per-construct "arm coverage" modules (one terse case per AST
  variant / statement kind) are encouraged — they both pin behavior
  and force a new test when an exhaustive `match` gains a variant.
  Adding tests is cheap and welcome; err on the side of more
  coverage rather than less.
