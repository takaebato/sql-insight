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

Six entry points, organized by module:

- [`extractor::extract_tables`](https://docs.rs/sql-insight/latest/sql_insight/extractor/fn.extract_tables.html) —
  flat list of `TableReference`s per statement.
- [`extractor::extract_crud_tables`](https://docs.rs/sql-insight/latest/sql_insight/extractor/fn.extract_crud_tables.html) —
  tables bucketed by CRUD verb.
- [`extractor::extract_table_operations`](https://docs.rs/sql-insight/latest/sql_insight/extractor/fn.extract_table_operations.html) —
  per-statement `reads` / `writes` / `lineage` at table granularity.
- [`extractor::extract_column_operations`](https://docs.rs/sql-insight/latest/sql_insight/extractor/fn.extract_column_operations.html) —
  same surfaces at column granularity, with `Passthrough` / `Transformation` kinds.
- [`formatter::format`](https://docs.rs/sql-insight/latest/sql_insight/formatter/fn.format.html) /
  `format_with_options` — re-emit SQL via sqlparser's `Display`
  (multi-line pretty-print via `FormatterOptions::pretty`).
- [`normalizer::normalize`](https://docs.rs/sql-insight/latest/sql_insight/normalizer/fn.normalize.html) /
  `normalize_with_options` — substitute literals with placeholders
  (`1` → `?`) so structurally identical queries hash to the same shape.
  Three opt-in collapses tighten the equivalence further:
  `IN (1, 2, 3)` → `IN (...)`,
  `VALUES (1, 2, 3), (4, 5, 6)` → `VALUES (...)`, and
  `INSERT INTO t (c, b, a) VALUES (1, 2, 3)` → `INSERT INTO t (a, b, c) VALUES (...)`.

An optional [`Catalog`](https://docs.rs/sql-insight/latest/sql_insight/catalog/)
makes column resolution strict (typos surface as
`UnresolvedColumn`, INSERT positional values pair with target
columns). Every extractor works catalog-free in best-effort mode.

## Limitations

See the
[Limitations](https://docs.rs/sql-insight/latest/sql_insight/#limitations)
section of the crate docs.

## Examples & docs

- Runnable examples in
  [`sql-insight/examples/`](sql-insight/examples) — table-level
  operations, column-level operations, and the catalog-on path.
- Full API and design notes on
  [docs.rs](https://docs.rs/sql-insight/).

## License

MIT — see [LICENSE.txt](LICENSE.txt).
