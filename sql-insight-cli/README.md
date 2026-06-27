# sql-insight-cli

A command-line interface to [sql-insight](https://github.com/takaebato/sql-insight/tree/master/sql-insight) — format, normalize, and extract tables / operations / lineage from SQL.

[![Crates.io](https://img.shields.io/crates/v/sql-insight-cli.svg)](https://crates.io/crates/sql-insight-cli)
[![Rust](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml/badge.svg?branch=master)](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml)
[![codecov](https://codecov.io/gh/takaebato/sql-insight/graph/badge.svg?token=Z1KYAWA3HY)](https://codecov.io/gh/takaebato/sql-insight)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

## Installation

```bash
cargo install sql-insight-cli
```

## Commands

- `format` — pretty-print SQL to a standardized layout (comments are not
  preserved — the parser does not retain them in the AST).
- `normalize` — abstract literals to placeholders (`--unify-in-list` /
  `--unify-values` collapse repetitive shapes).
- `extract crud` — tables bucketed by Create / Read / Update / Delete.
- `extract table-ops` — table-level reads / writes / lineage per statement.
- `extract column-ops` — the same at column granularity, with lineage kinds.

## Input

Every command takes the SQL one of four ways (mutually exclusive):

- an inline argument — `sql-insight format "SELECT 1"`
- `--file <path>` — read it from a file
- piped stdin — `echo "SELECT 1" | sql-insight format`
- `--interactive` / `-i` — a REPL with line editing and history (↑/↓,
  Ctrl-R); enter statements terminated by `;`, exit with `exit` / `quit` /
  Ctrl-D (Ctrl-C clears the in-progress statement)

With no input on an interactive terminal, the command errors rather than
guessing.

## Examples

```bash
$ sql-insight format "SELECT   *   FROM users   WHERE id = 1;"
SELECT * FROM users WHERE id = 1

$ sql-insight normalize "SELECT * FROM users WHERE id = 1"
SELECT * FROM users WHERE id = ?

$ sql-insight extract crud "INSERT INTO users (name) SELECT name FROM employees"
Create: [users], Read: [employees], Update: [], Delete: []

$ sql-insight extract column-ops "INSERT INTO users (name) SELECT LOWER(name) FROM employees"
[1] Insert
  reads:   employees.name
  writes:  users.name
  lineage: employees.name -> users.name [transform]
```

## Options (extract commands)

- `--dialect <name>` — parse under a specific dialect (default `generic`).
- `--format <text|json>` — `json` emits one array of per-statement results.
- `--ddl-file <path>` — a DDL file (`CREATE TABLE`s) to resolve against;
  enables catalog-aware analysis (canonicalized identities, strict columns).
- `--default-schema` / `--default-catalog` — search-path-style fill for bare
  query references.
- `--casing <upper|lower|insensitive|sensitive>` (and per-class
  `--casing-table` / `--casing-table-alias` / `--casing-column`) — override
  the dialect's identifier casing.

## Supported SQL Dialects

Via [sqlparser-rs](https://github.com/sqlparser-rs/sqlparser-rs): Generic,
MySQL, PostgreSQL, Hive, SQLite, Snowflake, Redshift, Microsoft SQL Server,
ClickHouse, BigQuery, ANSI, DuckDB, Databricks, Oracle. See the
[sqlparser-rs docs](https://docs.rs/sqlparser/latest/sqlparser/dialect/index.html#structs)
for details.

## Contributing

Contributions to `sql-insight` are welcome! Whether it's adding new
features, fixing bugs, or improving documentation, feel free to fork
the repository and submit a pull request.

## License

MIT — see [LICENSE.txt](https://github.com/takaebato/sql-insight/blob/master/sql-insight-cli/LICENSE.txt).
