# sql-insight

A utility for SQL query analysis, formatting, and transformation. Built on
[sqlparser-rs](https://github.com/sqlparser-rs/sqlparser-rs), it works
across every SQL dialect sqlparser-rs supports.

[![Crates.io](https://img.shields.io/crates/v/sql-insight.svg)](https://crates.io/crates/sql-insight)
[![Docs.rs](https://docs.rs/sql-insight/badge.svg)](https://docs.rs/sql-insight)
[![Rust](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml/badge.svg?branch=master)](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml)
[![codecov](https://codecov.io/gh/takaebato/sql-insight/graph/badge.svg?token=Z1KYAWA3HY)](https://codecov.io/gh/takaebato/sql-insight)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

## Install

```toml
[dependencies]
sql-insight = "0.2.0"
```

## Quick start

`reads` / `writes` / `lineage` from a single call:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::extractor::{extract_table_operations, StatementKind};

let dialect = GenericDialect {};
let result = extract_table_operations(
    &dialect,
    "INSERT INTO orders (id) SELECT id FROM staging",
    None,
).unwrap();
let ops = result[0].as_ref().unwrap();
assert_eq!(ops.statement_kind, StatementKind::Insert);
assert_eq!(ops.reads.len(), 1);   // staging
assert_eq!(ops.writes.len(), 1);  // orders
assert_eq!(ops.lineage.len(), 1); // staging → orders
```

## API

Four extractors at increasing granularity (all in
[`sql_insight::extractor`](https://docs.rs/sql-insight/latest/sql_insight/extractor/)):

| Function | Output |
|---|---|
| `extract_tables` | flat list of `TableReference`s |
| `extract_crud_tables` | tables bucketed by CRUD verb |
| `extract_table_operations` | per-statement `reads` / `writes` / `lineage` at table granularity |
| `extract_column_operations` | same surfaces at column granularity, with `Passthrough` / `Transformation` kinds |

Plus two SQL utilities:

- [`formatter::format`](https://docs.rs/sql-insight/latest/sql_insight/formatter/fn.format.html) /
  `format_with_options` — re-emit SQL via sqlparser's `Display`
  (multi-line pretty-print via `FormatterOptions::pretty`).
- [`normalizer::normalize`](https://docs.rs/sql-insight/latest/sql_insight/normalizer/fn.normalize.html) /
  `normalize_with_options` — placeholder-substitute literals
  (`1` → `?`) plus three opt-in collapses (`IN (1,2,3)` → `IN (...)`, etc.).

Every extractor returns `Vec<Result<X, Error>>` — one entry per
parsed statement, so a bad statement in a batch doesn't kill the
rest. Non-fatal issues surface in each result's `diagnostics` field
(unsupported statements, suppressed wildcards, ambiguous /
unresolved columns).

An optional [`Catalog`](https://docs.rs/sql-insight/latest/sql_insight/catalog/)
makes column resolution strict (typos surface as
`UnresolvedColumn`, INSERT positional values pair with target
columns). Every extractor works catalog-free in best-effort mode.

## Limitations

Full list in the
[crate docs](https://docs.rs/sql-insight/latest/sql_insight/#limitations).
Highlights:

- **Wildcards (`SELECT *`, `t.*`) are not expanded** — they surface
  as `WildcardSuppressed` and contribute nothing to `reads` /
  `lineage`.
- **Recursive CTE column lineage is deferred.** Table-level lineage
  traces the anchor branch's real tables; a column edge from a
  recursive CTE surfaces with the CTE binding as the source.
- **No type checking.** The catalog is an enrichment input, not a
  validator.

## Examples & docs

- Runnable examples in
  [`sql-insight/examples/`](sql-insight/examples) — table-level
  operations, column-level operations, and the catalog-on path.
- Full API and design notes on
  [docs.rs](https://docs.rs/sql-insight/).

## License

MIT — see [LICENSE.txt](LICENSE.txt).
