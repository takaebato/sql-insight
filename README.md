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

## Usage

### Table-level Operation Extraction

Get the statement kind plus `reads` / `writes` / `lineage` in one call:

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

### Column-level Operation Extraction

Same surfaces, at column granularity. `reads` / `writes` are plain
occurrence lists of column references; `lineage` edges carry a kind
(`Passthrough` vs `Transformation`) describing how each source
reaches its target:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::extractor::extract_column_operations;

let dialect = GenericDialect {};
let result = extract_column_operations(
    &dialect,
    "INSERT INTO orders (id, total) SELECT id, SUM(amount) FROM staging GROUP BY id",
    None,
).unwrap();
let ops = result[0].as_ref().unwrap();
// id → id (Passthrough), amount → total (Transformation, via SUM).
assert_eq!(ops.lineage.len(), 2);
```

### Diagnostics

Non-fatal issues surface alongside the result. Each diagnostic carries
a `kind`, a human-readable `message`, and an optional source-location
`span`:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::diagnostic::ColumnLevelDiagnosticKind;
use sql_insight::extractor::extract_column_operations;

let dialect = GenericDialect {};
let result = extract_column_operations(&dialect, "SELECT * FROM users", None).unwrap();
let ops = result[0].as_ref().unwrap();
assert!(ops
    .diagnostics
    .iter()
    .any(|d| matches!(d.kind, ColumnLevelDiagnosticKind::WildcardSuppressed)));
```

### SQL Formatting

```rust
use sql_insight::sqlparser::dialect::GenericDialect;

let dialect = GenericDialect {};
let formatted = sql_insight::formatter::format(
    &dialect, "SELECT * \n from users   WHERE id = 1"
).unwrap();
assert_eq!(formatted, ["SELECT * FROM users WHERE id = 1"]);
```

`format_with_options` + `FormatterOptions::pretty` switches to
sqlparser's multi-line pretty-print.

### SQL Normalization

Substitute literals with placeholders so structurally identical
queries hash to the same shape:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;

let dialect = GenericDialect {};
let normalized = sql_insight::normalizer::normalize(
    &dialect, "SELECT * \n from users   WHERE id = 1"
).unwrap();
assert_eq!(normalized, ["SELECT * FROM users WHERE id = ?"]);
```

`normalize_with_options` adds three opt-in collapses:
`IN (1, 2, 3)` → `IN (...)`,
`VALUES (1, 2, 3), (4, 5, 6)` → `VALUES (...)`, and
`INSERT INTO t (c, b, a) VALUES (1, 2, 3)` → `INSERT INTO t (a, b, c) VALUES (...)`.

### Table Extraction (lightweight)

Flat list of table references touched by a statement:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::extractor::extract_tables;

let dialect = GenericDialect {};
let extractions = extract_tables(&dialect, "SELECT * FROM catalog.schema.users").unwrap();
println!("{:?}", extractions);
```

### CRUD Table Extraction

Bucket tables by create / read / update / delete role:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::extractor::extract_crud_tables;

let dialect = GenericDialect {};
let crud_tables = extract_crud_tables(&dialect, "INSERT INTO users (name) SELECT name FROM employees").unwrap();
println!("{:?}", crud_tables);
```

## Catalog

An optional [`Catalog`](https://docs.rs/sql-insight/latest/sql_insight/catalog/)
makes column resolution strict (column refs not in the
catalog-provided schema surface as `UnresolvedColumn`, INSERT
positional values pair with target columns). Every extractor works
catalog-free in best-effort mode.

## Limitations

See the
[Limitations](https://docs.rs/sql-insight/latest/sql_insight/#limitations)
section of the crate docs.

## Examples

Runnable examples under
[`sql-insight/examples/`](sql-insight/examples):

- [`table_operations.rs`](sql-insight/examples/table_operations.rs) —
  table-level `reads` / `writes` / `lineage` across a multi-statement
  batch, with `StatementKind`-based dispatch.
- [`column_operations.rs`](sql-insight/examples/column_operations.rs) —
  per-column reads and lineage classified by `ColumnLineageKind`
  (Passthrough vs Transformation) into `Relation` vs `QueryOutput`
  targets.
- [`with_catalog.rs`](sql-insight/examples/with_catalog.rs) — supplying
  a `Catalog` enables INSERT positional column pairing and surfaces
  `AmbiguousColumn` / `UnresolvedColumn` diagnostics.

Run with `cargo run --example <name> -p sql-insight`.

## Supported SQL Dialects

`sql-insight` supports a comprehensive range of SQL dialects through
[sqlparser-rs](https://github.com/sqlparser-rs/sqlparser-rs):

- Generic
- MySQL
- PostgreSQL
- Hive
- SQLite
- Snowflake
- Redshift
- Microsoft SQL Server
- ClickHouse
- BigQuery
- ANSI
- DuckDB
- Databricks
- Oracle

See the
[sqlparser-rs documentation](https://docs.rs/sqlparser/latest/sqlparser/dialect/index.html#structs)
for dialect-specific details.

## Contributing

Contributions to `sql-insight` are welcome! Whether it's adding new
features, fixing bugs, or improving documentation, feel free to fork
the repository and submit a pull request.

## License

MIT — see [LICENSE.txt](LICENSE.txt).
