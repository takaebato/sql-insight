# sql-insight

A utility for SQL query analysis, formatting, and transformation. Built on
[sqlparser-rs](https://github.com/sqlparser-rs/sqlparser-rs), it works
across every SQL dialect sqlparser-rs supports.

[![Crates.io](https://img.shields.io/crates/v/sql-insight.svg)](https://crates.io/crates/sql-insight)
[![Docs.rs](https://docs.rs/sql-insight/badge.svg)](https://docs.rs/sql-insight)
[![Rust](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml/badge.svg?branch=master)](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml)
[![codecov](https://codecov.io/gh/takaebato/sql-insight/graph/badge.svg?token=Z1KYAWA3HY)](https://codecov.io/gh/takaebato/sql-insight)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

## Features

- **Table-level Operation Extraction**: identify which tables a
  statement reads, which it writes, and the lineage between sources
  and targets. Classifies the statement by verb (Insert / Update /
  Merge / …).
- **Column-level Operation Extraction**: the same at column granularity
  — track lineage from individual source columns to target columns,
  distinguishing pure forwarding from value-changing expressions.
- **Optional Catalog**: pass column schemas to tighten column
  resolution and pair INSERT values with target columns by position.
  Best-effort without one.
- **Table Extraction / CRUD Table Extraction**: flat or CRUD-bucketed
  table list, for when you just need to know which tables a statement
  touches.
- **SQL Formatting**: emit a query in a consistent layout (single-line
  by default, multi-line pretty-print on demand).
- **SQL Normalization**: collapse structurally identical queries to
  the same string (placeholder-substitute literals, optionally
  collapse repetitive shapes), useful for query fingerprinting and
  deduplication.

## Install

```toml
[dependencies]
sql-insight = "0.2.0"
```

## Usage

### Table-level Operation Extraction

Get the statement kind plus three surfaces — `reads` (tables read),
`writes` (tables written), and `lineage` (source → target edges, only
for statements that physically move data) — in one call:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::extractor::{extract_table_operations, StatementKind};

let dialect = GenericDialect {};
let result = extract_table_operations(
    &dialect,
    "INSERT INTO t1 (a) SELECT a FROM t2",
    None,
).unwrap();
let ops = result[0].as_ref().unwrap();
assert_eq!(ops.statement_kind, StatementKind::Insert);
assert_eq!(ops.reads.len(), 1);   // t2
assert_eq!(ops.writes.len(), 1);  // t1
assert_eq!(ops.lineage.len(), 1); // t2 → t1
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
    "INSERT INTO t1 (a, b) SELECT a, LOWER(b) FROM t2",
    None,
).unwrap();
let ops = result[0].as_ref().unwrap();
// a → a (Passthrough), b → b (Transformation, via LOWER).
assert_eq!(ops.lineage.len(), 2);
```

### Table Extraction (lightweight)

Flat list of table references touched by a statement:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::extractor::extract_tables;

let dialect = GenericDialect {};
let extractions = extract_tables(&dialect, "SELECT * FROM catalog.schema.t1").unwrap();
let extraction = extractions[0].as_ref().unwrap();
assert_eq!(extraction.tables.len(), 1);
assert_eq!(extraction.tables[0].to_string(), "catalog.schema.t1");
```

### CRUD Table Extraction

Bucket tables by create / read / update / delete role:

```rust
use sql_insight::sqlparser::dialect::GenericDialect;
use sql_insight::extractor::extract_crud_tables;

let dialect = GenericDialect {};
let result = extract_crud_tables(&dialect, "INSERT INTO t1 (a) SELECT a FROM t2").unwrap();
let crud = result[0].as_ref().unwrap();
assert_eq!(crud.create_tables.len(), 1); // t1
assert_eq!(crud.read_tables.len(), 1);   // t2
assert!(crud.update_tables.is_empty());
assert!(crud.delete_tables.is_empty());
```

### SQL Formatting

```rust
use sql_insight::sqlparser::dialect::GenericDialect;

let dialect = GenericDialect {};
let formatted = sql_insight::formatter::format(
    &dialect, "SELECT * \n from t1   WHERE a = 1"
).unwrap();
assert_eq!(formatted, ["SELECT * FROM t1 WHERE a = 1"]);
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
    &dialect, "SELECT * \n from t1   WHERE a = 1"
).unwrap();
assert_eq!(normalized, ["SELECT * FROM t1 WHERE a = ?"]);
```

`normalize_with_options` adds three opt-in collapses:
`IN (1, 2, 3)` → `IN (...)`,
`VALUES (1, 2, 3), (4, 5, 6)` → `VALUES (...)`, and
`INSERT INTO t (c, b, a) VALUES (1, 2, 3)` → `INSERT INTO t (a, b, c) VALUES (...)`.

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

See [`sql-insight/examples/`](sql-insight/examples) for runnable
samples covering table-level operations, column-level lineage, and the
catalog path. Run with `cargo run --example <name> -p sql-insight`.

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
