# sql-insight

Operation extraction for SQL, built on
[sqlparser-rs](https://github.com/sqlparser-rs/sqlparser-rs). Turn a
SQL string into structured facts about what the statement does —
which tables and columns it reads, which it writes, and how data
moves from sources to targets — alongside utilities for formatting
and normalization.

[![Crates.io](https://img.shields.io/crates/v/sql-insight.svg)](https://crates.io/crates/sql-insight)
[![Docs.rs](https://docs.rs/sql-insight/badge.svg)](https://docs.rs/sql-insight)
[![Rust](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml/badge.svg?branch=master)](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml)
[![codecov](https://codecov.io/gh/takaebato/sql-insight/graph/badge.svg?token=Z1KYAWA3HY)](https://codecov.io/gh/takaebato/sql-insight)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

## Features

- **Table-level Operation Extraction**: `reads` / `writes` / `flows`
  surfaces with statement-kind classification per parsed statement.
- **Column-level Operation Extraction**: the same three surfaces at
  column granularity, with clause-role (`Projection` / `Filter` /
  `GroupBy` / `Sort` / `Window`) and flow-kind (`Passthrough` /
  `Aggregation` / `Computed`) metadata. Column flows form a
  source → target graph suitable for lineage-style analyses.
- **Optional Catalog**: supply a schema provider to make resolution
  strict — catch typos as unresolved references, pair INSERT
  positional values with target columns. Every extractor still
  works catalog-free in best-effort mode.
- **Diagnostics**: non-fatal issues (unsupported statements,
  suppressed wildcards, ambiguous / unresolved columns) surface
  alongside the result with optional source-location spans, rather
  than failing the whole call.
- **Table Extraction / CRUD Table Extraction**: flat or
  CRUD-bucketed table sets — lightweight extraction when the
  operation graph isn't needed.
- **SQL Formatting & Normalization**: pretty-print or normalize
  queries (placeholder-substitute literals) for hashing and
  comparison.

## Installation

Add `sql_insight` to your `Cargo.toml` file:

```toml
[dependencies]
sql-insight = { version = "0.2.0" }
```

## Usage

### Table-level Operation Extraction

Get the statement kind plus `reads` / `writes` / `flows` in one call:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::{extract_table_operations, StatementKind};

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
assert_eq!(ops.flows.len(), 1);   // staging → orders
```

### Column-level Operation Extraction

Same surfaces, at column granularity. Reads carry the clause role
they appeared in; flows carry the flow kind through which they reach
the target:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::extract_column_operations;

let dialect = GenericDialect {};
let result = extract_column_operations(
    &dialect,
    "INSERT INTO orders (id, total) SELECT id, SUM(amount) FROM staging GROUP BY id",
    None,
).unwrap();
let ops = result[0].as_ref().unwrap();
// One flow per target column: id → id (Passthrough), amount → total (Aggregation).
assert_eq!(ops.flows.len(), 2);
```

### Diagnostics

Non-fatal issues surface alongside the result. Each diagnostic carries
a `kind`, a human-readable `message`, and an optional source-location
`span`:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::{extract_column_operations, DiagnosticKind};

let dialect = GenericDialect {};
let result = extract_column_operations(&dialect, "SELECT * FROM users", None).unwrap();
let ops = result[0].as_ref().unwrap();
assert!(ops
    .diagnostics
    .iter()
    .any(|d| matches!(d.kind, DiagnosticKind::WildcardSuppressed)));
```

### SQL Formatting

```rust
use sql_insight::sqlparser::dialect::GenericDialect;

let dialect = GenericDialect {};
let formatted_sql = sql_insight::format(&dialect, "SELECT * \n from users   WHERE id = 1").unwrap();
assert_eq!(formatted_sql, ["SELECT * FROM users WHERE id = 1"]);
```

### SQL Normalization

Substitute literals with placeholders so structurally identical
queries hash to the same shape:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;

let dialect = GenericDialect {};
let normalized_sql = sql_insight::normalize(&dialect, "SELECT * \n from users   WHERE id = 1").unwrap();
assert_eq!(normalized_sql, ["SELECT * FROM users WHERE id = ?"]);
```

### Table Extraction (lightweight)

Flat list of table references touched by a statement:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;

let dialect = GenericDialect {};
let extractions = sql_insight::extract_tables(&dialect, "SELECT * FROM catalog.schema.users").unwrap();
println!("{:?}", extractions);
```

### CRUD Table Extraction

Bucket tables by create / read / update / delete role:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;

let dialect = GenericDialect {};
let crud_tables = sql_insight::extract_crud_tables(&dialect, "INSERT INTO users (name) SELECT name FROM employees").unwrap();
println!("{:?}", crud_tables);
```

## Limitations and Behavior Notes

A few intentional non-supports and behavior nuances that shape what
you can rely on:

- **Wildcards (`SELECT *`, `t.*`) are not expanded** — they contribute
  nothing to `reads` / `flows` and surface as a `WildcardSuppressed`
  diagnostic.
- **TableFunction schemas stay `Unknown`** (`UNNEST`, `JSON_TABLE`,
  etc.) — catalog enrichment doesn't reach them yet.
- **Recursive CTE bodies** are pre-bound under a stub; flow
  composition through them is deferred.
- **Aggregate detection** uses a built-in name list across major
  dialects plus structural markers — dialect-specific UDAFs may be
  misclassified.
- **Catalog is optional**, and its presence shapes resolver
  strictness: with a catalog, ambiguous / unresolved column
  diagnostics fire; without, they are suppressed (every `Unknown`
  schema could contain anything).
- **No type checking** — the catalog is an enrichment input, not a
  validator.

See the
[Limitations](https://docs.rs/sql-insight/latest/sql_insight/#limitations)
and
[Behavior notes](https://docs.rs/sql-insight/latest/sql_insight/#behavior-notes)
sections of the crate docs for the full set.

## Supported SQL Dialects

`sql-insight` supports a comprehensive range of SQL dialects through [sqlparser-rs](https://github.com/sqlparser-rs/sqlparser-rs). For details on supported dialects, please refer to the [sqlparser-rs documentation](https://docs.rs/sqlparser/latest/sqlparser/dialect/index.html#structs).

## Contributing

Contributions to `sql-insight` are welcome! Whether it's adding new features, fixing bugs, or improving documentation, feel free to fork the repository and submit a pull request.

## License

`sql-insight` is distributed under the [MIT license](https://github.com/takaebato/sql-insight/blob/master/LICENSE.txt).
