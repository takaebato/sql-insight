# sql-insight

A toolkit for SQL query analysis, formatting, and transformation.
Leveraging the comprehensive parsing capabilities of [sqlparser-rs](https://github.com/sqlparser-rs/sqlparser-rs), it can handle various SQL dialects.

[![Crates.io](https://img.shields.io/crates/v/sql-insight.svg)](https://crates.io/crates/sql-insight)
[![Docs.rs](https://docs.rs/sql-insight/badge.svg)](https://docs.rs/sql-insight)
[![Rust](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml/badge.svg?branch=master)](https://github.com/takaebato/sql-insight/actions/workflows/rust.yaml)
[![codecov](https://codecov.io/gh/takaebato/sql-insight/graph/badge.svg?token=Z1KYAWA3HY)](https://codecov.io/gh/takaebato/sql-insight)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

## Features

- **SQL Formatting**: Format SQL queries to standardized form, improving readability and maintainability.
- **SQL Normalization**: Convert SQL queries into a normalized form, making them easier to analyze and process.
- **Table Extraction**: Extract tables referenced in SQL queries, clarifying the data sources involved.
- **CRUD Table Extraction**: Identify the create, read, update, and delete operations, along with the tables involved in each operation within SQL queries.

## Installation

Add `sql_insight` to your `Cargo.toml` file:

```toml
[dependencies]
sql-insight = { version = "0.1.0" }
```

## Usage

### SQL Formatting

Format SQL queries according to different dialects:

```rust
use sqlparser::dialect::GenericDialect;

let dialect = GenericDialect {};
let formatted_sql = sql_insight::format(&dialect, "SELECT * \n from users   WHERE id = 1").unwrap();
println!("{}", formatted_sql);
```

This outputs:

```sql
SELECT * FROM users WHERE id = 1
```

### SQL Normalization

Normalize SQL queries to abstract away literals:

```rust
use sqlparser::dialect::GenericDialect;

let dialect = GenericDialect {};
let normalized_sql = sql_insight::normalize(&dialect, "SELECT * \n from users   WHERE id = 1").unwrap();
println!("{}", normalized_sql);
```

This outputs:

```sql
SELECT * FROM users WHERE id = ?
```


### Table Extraction

Extract table references from SQL queries:

```rust
use sqlparser::dialect::GenericDialect;

let dialect = GenericDialect {};
let tables = sql_insight::extract_tables(&dialect, "SELECT * FROM catalog.schema.`users` as users_alias").unwrap();
println!("{:?}", tables);
```

This outputs:

```
[Ok(Tables([TableReference { catalog: Some(Ident { value: "catalog", quote_style: None }), schema: Some(Ident { value: "schema", quote_style: None }), name: Ident { value: "users", quote_style: Some('`') }, alias: Some(Ident { value: "users_alias", quote_style: None }) }]))]
```

### CRUD Table Extraction

Identify CRUD operations and the tables involved in each operation within SQL queries:

```rust
use sqlparser::dialect::GenericDialect;

let dialect = GenericDialect {};
let crud_tables = sql_insight::extract_crud_tables(&dialect, "INSERT INTO users (name) SELECT name FROM employees").unwrap();
println!("{:?}", crud_tables);
```

This outputs:

```
[Ok(CrudTables { create_tables: [TableReference { catalog: None, schema: None, name: Ident { value: "users", quote_style: None }, alias: None }], read_tables: [TableReference { catalog: None, schema: None, name: Ident { value: "employees", quote_style: None }, alias: None }], update_tables: [], delete_tables: [] })]
```

## Supported SQL Dialects

`sql-insight` supports a comprehensive range of SQL dialects through [sqlparser-rs](https://github.com/sqlparser-rs/sqlparser-rs). For details on supported dialects, please refer to the documentation.

## Contributing

Contributions to `sql-insight` are welcome! Whether it's adding new features, fixing bugs, or improving documentation, feel free to fork the repository and submit a pull request.

## License

`sql-insight` is distributed under the [MIT license](https://github.com/takaebato/sql-insight/blob/master/LICENSE.txt).
