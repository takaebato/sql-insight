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

Design (the why, shape, and rationale) lives in **`ARCHITECTURE.md`** —
read it before non-trivial resolver / extractor work. Per-type contracts
are in rustdoc; don't restate them in prose. Module map:

- `resolver` (private) — the engine: `resolver.rs` facade (`build` + seven
  extraction fns), `binder/` (AST → `logical_plan::LogicalPlan` tree),
  `reads` / `origins` / `lineage` / `tables` walkers.
- `extractor` — public wrappers (`extract_tables` / `_crud_tables` /
  `_table_operations` / `_column_operations`, each with an `_with_options`
  twin) + `StatementKind`.
- `reference` / `diagnostic` / `casing` / `catalog` / `error` — public
  vocabulary. `normalizer` / `formatter` — standalone AST utilities.

## Invariants & gotchas

Rules that bite if forgotten (the why is in `ARCHITECTURE.md`):

- **Surfaces are source-ordered** (by written `name.span`), not walk-order,
  so changing a walk can't change output. Tests compare `reads` / `lineage`
  as multisets (`assert_unordered_eq!`); `writes` stay order-exact.
- **`reads` is occurrence-based, by token** — each syntactic appearance
  counts. Post-projection (GROUP BY / HAVING / ORDER BY): an *identity*
  output (`GROUP BY a`) counts, an *introduced* alias (`ORDER BY x` for
  `a AS x`) doesn't (binds `Derived`). `lineage` is symmetric.
- **value vs filter is structural** — value → `lineage` source; filter-only
  → in `reads` but not `lineage`. No tag.
- **Reads = source/sink** — a scanned relation (FROM / JOIN / subquery) always
  reads (a source: its rows feed/filter, even `SELECT COUNT(*) FROM t`); a DML
  write target (the sink, named on the root, not a scan) reads only when its
  *own* data is referenced (`UPDATE t SET a=a+1`, `DELETE … WHERE t.flag`, an
  upsert) — a constant `UPDATE t SET a=1` / plain INSERT stays write-only. A
  target that is *also* scanned as a source (`INSERT INTO t SELECT * FROM t`)
  reads through that scan.
- **A multi-table UPDATE writes per SET-target** — `UPDATE t1 JOIN t2 SET
  t2.col = …` writes (and lineage-targets) `t2`, the relation its qualifier
  resolves to, not the root (`Assignment.target` carries the resolved table).
- **Best-effort** — an unrepresentable construct is dropped + flagged, not
  `?`-bailed; per-statement `Vec<Result<_, Error>>` (a *parse* error fails
  the whole call).
- **Diagnostics are tool-side coverage gaps only**, never per-reference
  resolution status (that's `ColumnRead::resolution`).
- **Catalog-free never yields `Cataloged`** — so `r.resolution == Cataloged`
  detects catalog-aware analysis.

## Code conventions

- Keep changes small and scoped. Preserve public API compatibility unless
  the change is intentional; update doc comments with the code.
- Prefer private modules + explicit `lib.rs` re-exports. Split a module
  before it gets unscannable. Avoid `bool` / ambiguous `Option` params in
  new public APIs — prefer enums or small option structs.
- **Keep `match` arms exhaustive** (sqlparser AST in the binder/extractors,
  `LogicalPlan` in the walkers) — wildcard arms hide new variants.
- **Public enums stay exhaustive (no `#[non_exhaustive]`) pre-1.0**, so a
  new variant is a deliberate breaking change consumers must acknowledge;
  add `#[non_exhaustive]` at the 1.0 freeze. Internal `match`es exhaustive
  regardless.
- **Accumulate diagnostics, don't `?`-bail** mid-walk on unsupported SQL;
  reserve hard errors for unrecoverable conditions.
- **Docs**: public items get rustdoc (purpose / contract / edge cases /
  examples). Each fact lives once at its most specific level (module
  orients + links; detail on the type/field) — duplicated docs drift.
  Prefer structured bullets over prose; keep docs true to the code (watch
  for rename / behaviour drift that `cargo doc` can't catch); cut
  redundancy but keep the why and worked examples (doctests are contract).
- **Tests** double as behavior docs: prefer concrete, minimal SQL with the
  full expected value spelled out over clever parameterization. Compare
  whole values; layer helpers (`assert_*` + `_with_dialect` / `_with_catalog`
  variants). Per-construct "arm coverage" modules (one case per AST variant)
  are encouraged — they pin behavior and force a test when a `match` grows.
  Err toward more coverage.

## Pull requests

- **PR titles follow [Conventional Commits](https://www.conventionalcommits.org/)**,
  CI-enforced via `.github/workflows/pr-title.yaml`. Squash-merge uses the title
  as the commit subject, which release-plz reads to compute per-crate version
  bumps and changelogs — so `fix:` / `feat:` / `feat!:` (breaking) on the title
  is what ships. Allowed types live in that workflow.
- **Breaking-change changelog notes live in the PR description**, between the
  `**▼ changelog ▼**` / `**▲ changelog ▲**` markers (uncomment the block in
  `.github/pull_request_template.md`; `!` on the title flags it breaking). Label
  with `**sql-insight:**` / `**sql-insight-cli:**` and write only what applies.
  release-plz slices that range into each crate's CHANGELOG "Breaking Changes"
  section; it can't route per crate, so both labels land in both changelogs. Keep
  notes short — what changed plus the replacement; link rustdoc for usage.
