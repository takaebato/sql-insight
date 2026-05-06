# AGENTS.md

## Scope

This file applies to the entire repository.

## Project

This is a Rust workspace with the `sql-insight` library and `sql-insight-cli`.
SQL parsing is based on `sqlparser-rs`; prefer working with its AST instead of
ad hoc SQL string parsing.

## Commands

- Format: `cargo fmt`
- Test: `cargo test`
- Lint: `cargo clippy --all-targets -- -D warnings`

After Rust code changes, run `cargo fmt`. Prefer focused tests first; run the
workspace test suite when shared extractor behavior or public API changes.

## Development Notes

- Keep changes small and scoped to the requested behavior.
- Preserve public API compatibility unless an API change is intentional.
- Update docs when public API or documented behavior changes.
- Prefer private modules and explicitly exported public crate API.
- Avoid boolean or ambiguous `Option` parameters in new public APIs. Prefer
  enums, named methods, or small option structs when they make call sites
  clearer.
- Avoid growing large modules. Prefer adding focused modules when new behavior
  would make a central file harder to scan.
- Add focused tests for extractor behavior changes.
- In tests, prefer comparing whole values over asserting fields one by one.
- For relation binding and table extraction, keep `sqlparser-rs` AST enum
  matches exhaustive where practical. Avoid broad wildcard arms when they would
  hide newly added AST variants.
- For unsupported SQL in table extraction, prefer reporting diagnostics over
  failing the whole extraction unless strict behavior is explicitly required.
