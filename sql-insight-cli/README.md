# sql-insight-cli

A command-line interface built on top of the [sql-insight](https://github.com/takaebato/sql-insight/tree/master/sql-insight). It provides a set of commands that `sql-insight` supports.

[![Crates.io](https://img.shields.io/crates/v/sql-insight-cli.svg)](https://crates.io/crates/sql-insight-cli)
[![Rust](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml/badge.svg?branch=master)](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml)
[![codecov](https://codecov.io/gh/takaebato/sql-insight/graph/badge.svg?token=Z1KYAWA3HY)](https://codecov.io/gh/takaebato/sql-insight)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

## Features

- **SQL Formatting**: Format SQL queries to standardized form, improving readability and maintainability.
- **SQL Normalization**: Convert SQL queries into a normalized form, making them easier to analyze and process.
- **Table Extraction**: Extract tables referenced in SQL queries, clarifying the data sources involved.
- **CRUD Table Extraction**: Identify the create, read, update, and delete operations, along with the tables involved in each operation within SQL queries.

Additional Features:
 
- **File and Interactive Mode Support**: Process SQL queries directly from files or via an interactive CLI session.

## Installation

Install `sql-insight-cli` using Cargo:

```bash
cargo install sql-insight-cli
```

## Usage

`sql-insight-cli` supports the following commands. Commands can process input directly from the command line, from a file using the --file option, or interactively.

### General Options

- `--file <path>`: Read SQL queries from the specified file instead of command line arguments.
- interactive mode: Launch an interactive CLI session to input SQL queries. Enter this mode by running the command without a SQL argument nor --file option. To exit, type `exit`, `quit` or press `Ctrl + C`.

### Formatting SQL

Format SQL queries to a standardized style:

```bash
sql-insight format "SELECT *  \n FROM users         WHERE id = 1;"
```

This outputs:

```sql
SELECT * FROM users WHERE id = 1
```

### Normalizing SQL

Normalize SQL queries, abstracting values to placeholders:

```bash
sql-insight normalize "SELECT *  \n FROM users         WHERE id = 1;"
```

This outputs:

```sql
SELECT * FROM users WHERE id = ?
```

### Table Extraction

Identify tables involved in SQL queries:

```bash
sql-insight extract-tables "SELECT * FROM catalog.schema.users as users_alias"
```

This outputs:

```
catalog.schema.users AS users_alias
```

### CRUD Table Extraction

Extract and identify CRUD operations and involved tables:

```bash
sql-insight extract-crud "INSERT INTO users (name) SELECT name FROM employees"
```

This outputs:

```
Create: [users], Read: [employees], Update: [], Delete: []
```

## Supported SQL Dialects
`sql-insight-cli` leverages [sqlparser-rs](https://github.com/sqlparser-rs/sqlparser-rs) for parsing, supporting a wide range of SQL dialects. For a detailed list, please refer to the [sqlparser-rs documentation](https://docs.rs/sqlparser/latest/sqlparser/dialect/index.html#structs).

## Contributing
Contributions to `sql-insight-cli` are welcome! Whether it's adding new features, fixing bugs, or improving documentation, feel free to fork the repository and submit a pull request.

## License
`sql-insight-cli` is licensed under the [MIT License](https://github.com/takaebato/sql-insight/blob/master/sql-insight-cli/LICENSE.txt).
