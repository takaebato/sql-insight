#[cfg(test)]
mod integration {
    use assert_cmd::Command;
    use predicates::prelude::*;
    use std::io::Write;
    use std::process;
    use tempfile::NamedTempFile;

    fn sql_insight_cmd() -> Command {
        Command::cargo_bin("sql-insight").unwrap()
    }

    mod format {
        use super::*;

        #[test]
        fn test_format() {
            sql_insight_cmd()
                .arg("format")
                .arg("select  *  \n  from  t1; INSERT INTO t2 ( a )   VALUES  \n (1);")
                .assert()
                .success()
                .stdout("SELECT * FROM t1\nINSERT INTO t2 (a) VALUES (1)\n")
                .stderr("");
        }

        #[test]
        fn test_format_with_dialect() {
            sql_insight_cmd()
                .arg("format")
                .arg("--dialect")
                .arg("mysql")
                .arg("select  *  \n  from  t1; INSERT INTO t2 ( a )   VALUES  \n (1);")
                .assert()
                .success()
                .stdout("SELECT * FROM t1\nINSERT INTO t2 (a) VALUES (1)\n")
                .stderr("");
        }

        #[test]
        fn test_format_from_file() {
            let mut temp_file = NamedTempFile::new().unwrap();
            temp_file
                .write_all(b"select  *  \n  from  t1; INSERT INTO t2 ( a )   VALUES  \n (1);")
                .unwrap();
            sql_insight_cmd()
                .arg("format")
                .arg("--file")
                .arg(temp_file.path())
                .assert()
                .success()
                .stdout("SELECT * FROM t1\nINSERT INTO t2 (a) VALUES (1)\n")
                .stderr("");
        }

        #[test]
        fn test_format_from_stdin() {
            // No explicit source + piped stdin → read the SQL from stdin
            // (not the old surprise of dropping into interactive mode).
            sql_insight_cmd()
                .arg("format")
                .write_stdin("select * from t1")
                .assert()
                .success()
                .stdout("SELECT * FROM t1\n")
                .stderr("");
        }

        #[test]
        fn test_source_flags_are_mutually_exclusive() {
            // --interactive cannot combine with an inline SQL argument.
            sql_insight_cmd()
                .arg("format")
                .arg("-i")
                .arg("SELECT 1")
                .assert()
                .failure()
                .stderr(predicate::str::contains("cannot be used with"));
        }
    }

    mod normalize {
        use super::*;

        #[test]
        fn test_normalize() {
            sql_insight_cmd()
                .arg("normalize")
                .arg("select * from t1 where a = 1 and b in (2, 3); insert into t2 (a) values (4);")
                .assert()
                .success()
                .stdout(
                    "SELECT * FROM t1 WHERE a = ? AND b IN (?, ?)\nINSERT INTO t2 (a) VALUES (?)\n",
                )
                .stderr("");
        }

        #[test]
        fn test_normalize_with_unify_in_list_option() {
            sql_insight_cmd()
                .arg("normalize")
                .arg("--unify-in-list")
                .arg("select * from t1 where a = 1 and b in (2, 3); insert into t2 (a) values (4);")
                .assert()
                .success()
                .stdout(
                    "SELECT * FROM t1 WHERE a = ? AND b IN (...)\nINSERT INTO t2 (a) VALUES (?)\n",
                )
                .stderr("");
        }

        #[test]
        fn test_normalize_with_unify_values_option() {
            sql_insight_cmd()
                .arg("normalize")
                .arg("--unify-values")
                .arg("select * from t1 where a = 1 and b in (2, 3); insert into t2 (a) values (4), (5), (6);")
                .assert()
                .success()
                .stdout(
                    "SELECT * FROM t1 WHERE a = ? AND b IN (?, ?)\nINSERT INTO t2 (a) VALUES (...)\n",
                )
                .stderr("");
        }

        #[test]
        fn test_normalize_with_all_options() {
            sql_insight_cmd()
                .arg("normalize")
                .arg("--unify-in-list")
                .arg("--unify-values")
                .arg("select * from t1 where a = 1 and b in (2, 3); insert into t2 (a) values (4), (5), (6);")
                .assert()
                .success()
                .stdout(
                    "SELECT * FROM t1 WHERE a = ? AND b IN (...)\nINSERT INTO t2 (a) VALUES (...)\n",
                )
                .stderr("");
        }

        #[test]
        fn test_normalize_with_dialect() {
            sql_insight_cmd()
                .arg("normalize")
                .arg("--dialect")
                .arg("mysql")
                .arg("select * from t1 where a = 1 and b in (2, 3); insert into t2 (a) values (4);")
                .assert()
                .success()
                .stdout(
                    "SELECT * FROM t1 WHERE a = ? AND b IN (?, ?)\nINSERT INTO t2 (a) VALUES (?)\n",
                )
                .stderr("");
        }

        #[test]
        fn test_normalize_from_file() {
            let mut temp_file = NamedTempFile::new().unwrap();
            temp_file
                .write_all(
                    b"select * from t1 where a = 1 and b in (2, 3); insert into t2 (a) values (4);",
                )
                .unwrap();
            sql_insight_cmd()
                .arg("normalize")
                .arg("--file")
                .arg(temp_file.path())
                .assert()
                .success()
                .stdout(
                    "SELECT * FROM t1 WHERE a = ? AND b IN (?, ?)\nINSERT INTO t2 (a) VALUES (?)\n",
                )
                .stderr("");
        }
    }

    mod extract_crud_tables {
        use super::*;

        #[test]
        fn test_extract_crud_tables() {
            sql_insight_cmd()
                .arg("extract").arg("crud")
                .arg("select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
                .assert()
                .success()
                .stdout("Create: [], Read: [t1, t2], Update: [], Delete: []\nCreate: [t1], Read: [t2], Update: [], Delete: []\n")
                .stderr("");
        }

        #[test]
        fn test_extract_crud_tables_with_dialect() {
            sql_insight_cmd()
                .arg("extract").arg("crud")
                .arg("--dialect")
                .arg("mysql")
                .arg("select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
                .assert()
                .success()
                .stdout("Create: [], Read: [t1, t2], Update: [], Delete: []\nCreate: [t1], Read: [t2], Update: [], Delete: []\n")
                .stderr("");
        }

        #[test]
        fn test_extract_crud_tables_with_cte() {
            sql_insight_cmd()
                .arg("extract")
                .arg("crud")
                .arg("with t2 as (select id from t1) select * from t2;")
                .assert()
                .success()
                .stdout("Create: [], Read: [t1], Update: [], Delete: []\n")
                .stderr("");
        }

        #[test]
        fn test_extract_crud_tables_from_file() {
            let mut temp_file = NamedTempFile::new().unwrap();
            temp_file
                .write_all(b"select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
                .unwrap();
            sql_insight_cmd()
                .arg("extract").arg("crud")
                .arg("--file")
                .arg(temp_file.path())
                .assert()
                .success()
                .stdout("Create: [], Read: [t1, t2], Update: [], Delete: []\nCreate: [t1], Read: [t2], Update: [], Delete: []\n")
                .stderr("");
        }
    }

    mod extract_tables {
        use super::*;

        #[test]
        fn test_extract_tables() {
            sql_insight_cmd()
                .arg("extract").arg("tables")
                .arg("select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
                .assert()
                .success()
                .stdout("t1, t2\nt1, t2\n")
                .stderr("");
        }

        #[test]
        fn test_extract_tables_with_full_identifiers_and_alis() {
            sql_insight_cmd()
                .arg("extract").arg("tables")
                .arg("select * from catalog.schema.t1 as t1 inner join catalog.schema.t2 as t2 using(id); \
                      insert into catalog.schema.t1 (a) select b from catalog.schema.t2;")
                .assert()
                .success()
                .stdout("catalog.schema.t1, catalog.schema.t2\ncatalog.schema.t1, catalog.schema.t2\n")
                .stderr("");
        }

        #[test]
        fn test_extract_tables_with_cte() {
            sql_insight_cmd()
                .arg("extract")
                .arg("tables")
                .arg("with t2 as (select id from t1) select * from t2;")
                .assert()
                .success()
                .stdout("t1\n")
                .stderr("");
        }

        #[test]
        fn test_extract_tables_with_dialect() {
            sql_insight_cmd()
                .arg("extract").arg("tables")
                .arg("--dialect")
                .arg("mysql")
                .arg("select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
                .assert()
                .success()
                .stdout("t1, t2\nt1, t2\n")
                .stderr("");
        }

        #[test]
        fn test_extract_tables_from_file() {
            let mut temp_file = NamedTempFile::new().unwrap();
            temp_file
                .write_all(b"select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
                .unwrap();
            sql_insight_cmd()
                .arg("extract")
                .arg("tables")
                .arg("--file")
                .arg(temp_file.path())
                .assert()
                .success()
                .stdout("t1, t2\nt1, t2\n")
                .stderr("");
        }
    }

    mod extract_table_ops {
        use super::*;

        #[test]
        fn test_extract_table_ops() {
            sql_insight_cmd()
                .arg("extract")
                .arg("table-ops")
                .arg("INSERT INTO orders SELECT id, amount FROM staging")
                .assert()
                .success()
                .stdout(
                    "[1] Insert\n  reads:   staging\n  writes:  orders\n  lineage: staging -> orders\n",
                )
                .stderr("");
        }

        #[test]
        fn test_extract_table_ops_select_reads_only() {
            // A SELECT has reads but no writes / lineage — empty surfaces
            // are omitted from the block.
            sql_insight_cmd()
                .arg("extract")
                .arg("table-ops")
                .arg("SELECT a FROM t1 JOIN t2 ON t1.id = t2.id")
                .assert()
                .success()
                .stdout("[1] Select\n  reads:   t1, t2\n")
                .stderr("");
        }
    }

    mod extract_column_ops {
        use super::*;

        #[test]
        fn test_extract_column_ops() {
            // Transformations carry a `[transform]` marker; passthroughs don't.
            sql_insight_cmd()
                .arg("extract")
                .arg("column-ops")
                .arg("INSERT INTO orders (id, total) SELECT id, a + b AS total FROM staging")
                .assert()
                .success()
                .stdout(
                    "[1] Insert\n  \
                     reads:   staging.id, staging.a, staging.b\n  \
                     writes:  orders.id, orders.total\n  \
                     lineage: staging.id -> orders.id\n           \
                     staging.a -> orders.total [transform]\n           \
                     staging.b -> orders.total [transform]\n",
                )
                .stderr("");
        }

        #[test]
        fn test_extract_column_ops_ambiguous_marker() {
            // Unqualified `a` is ambiguous between t1 / t2 → `(ambiguous)`;
            // the qualified `t1.a` / `t2.a` are catalog-free Inferred (unmarked).
            sql_insight_cmd()
                .arg("extract")
                .arg("column-ops")
                .arg("SELECT a FROM t1 JOIN t2 ON t1.a = t2.a")
                .assert()
                .success()
                .stdout(
                    "[1] Select\n  reads:   a (ambiguous), t1.a, t2.a\n  lineage: a (ambiguous) -> a\n",
                )
                .stderr("");
        }
    }

    mod extract_options {
        use super::*;

        #[test]
        fn test_extract_with_ddl_file() {
            // A `--ddl-file` makes resolution catalog-aware. The unqualified
            // `CREATE TABLE users` registers schema-less, so the read is
            // `(cataloged)` and surfaces bare `users.name` (no fabricated schema).
            let mut schema = NamedTempFile::new().unwrap();
            schema
                .write_all(b"CREATE TABLE users (id INT, name TEXT);")
                .unwrap();
            sql_insight_cmd()
                .arg("extract")
                .arg("column-ops")
                .arg("--ddl-file")
                .arg(schema.path())
                .arg("SELECT name FROM users")
                .assert()
                .success()
                .stdout(
                    "[1] Select\n  reads:   \"users\".name (cataloged)\n  lineage: \"users\".name (cataloged) -> name\n",
                )
                .stderr("");
        }

        #[test]
        fn test_extract_with_default_schema() {
            // An explicit --default-schema fills the bare query ref before
            // matching; the unqualified DDL table (schema-less) still matches.
            let mut schema = NamedTempFile::new().unwrap();
            schema.write_all(b"CREATE TABLE users (id INT);").unwrap();
            sql_insight_cmd()
                .arg("extract")
                .arg("column-ops")
                .arg("--ddl-file")
                .arg(schema.path())
                .arg("--default-schema")
                .arg("app")
                .arg("SELECT id FROM users")
                .assert()
                .success()
                .stdout("[1] Select\n  reads:   \"users\".id (cataloged)\n  lineage: \"users\".id (cataloged) -> id\n")
                .stderr("");
        }

        #[test]
        fn test_extract_with_default_schema_only() {
            // --default-schema without a --ddl-file: no tables to confirm, but
            // the declared default still qualifies the bare ref — `users`
            // surfaces as `public.users` (Inferred, so unmarked).
            sql_insight_cmd()
                .arg("extract")
                .arg("column-ops")
                .arg("--default-schema")
                .arg("public")
                .arg("SELECT id FROM users")
                .assert()
                .success()
                .stdout(
                    "[1] Select\n  reads:   public.users.id\n  lineage: public.users.id -> id\n",
                )
                .stderr("");
        }

        #[test]
        fn test_extract_with_casing_sensitive() {
            // Case-sensitive override: lowercase `t2` no longer binds the
            // uppercase CTE `T2`, so it surfaces as a distinct table.
            sql_insight_cmd()
                .arg("extract")
                .arg("tables")
                .arg("--casing")
                .arg("sensitive")
                .arg("WITH T2 AS (SELECT id FROM t1) SELECT * FROM t2")
                .assert()
                .success()
                .stdout("t1, t2\n")
                .stderr("");
        }
    }

    mod extract_json {
        use super::*;

        // JSON output is a single pretty array of per-statement results.
        // Keys are sorted (serde_json `Value`) and identifiers serialize as
        // `{value, quote}` (no source span), so the text is stable.

        #[test]
        fn test_tables_json() {
            sql_insight_cmd()
                .arg("extract")
                .arg("tables")
                .arg("--format")
                .arg("json")
                .arg("SELECT a FROM t1")
                .assert()
                .success()
                .stdout(
                    "[\n  {\n    \"diagnostics\": [],\n    \"tables\": [\n      {\n        \"catalog\": null,\n        \"name\": {\n          \"quote\": null,\n          \"value\": \"t1\"\n        },\n        \"schema\": null\n      }\n    ]\n  }\n]\n",
                )
                .stderr("");
        }

        #[test]
        fn test_column_ops_json_has_lineage_kind() {
            // Spot-check the rich surface: a transformation edge and the
            // resolution tag are present, identifiers carry value (no span).
            sql_insight_cmd()
                .arg("extract")
                .arg("column-ops")
                .arg("--format")
                .arg("json")
                .arg("SELECT a + b AS total FROM t1")
                .assert()
                .success()
                .stdout(predicate::str::contains("\"kind\": \"Transformation\""))
                .stdout(predicate::str::contains("\"resolution\": \"Inferred\""))
                .stdout(predicate::str::contains("\"value\": \"total\""))
                .stdout(predicate::str::contains("\"span\"").not())
                .stderr("");
        }

        #[test]
        fn test_json_unsupported_statement_carries_a_diagnostic() {
            // A best-effort batch keeps every statement: the unsupported
            // `SET` surfaces as an element with an `UnsupportedStatement`
            // diagnostic (not a dropped entry), alongside the resolved `t1`.
            sql_insight_cmd()
                .arg("extract")
                .arg("tables")
                .arg("--format")
                .arg("json")
                .arg("SELECT a FROM t1; SET x = 1")
                .assert()
                .success()
                .stdout(predicate::str::contains("\"value\": \"t1\""))
                .stdout(predicate::str::contains(
                    "\"kind\": \"UnsupportedStatement\"",
                ))
                .stderr("");
        }
    }

    mod interactive_mode {
        use super::*;
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
        use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
        use tokio::time;

        // Resolved by Cargo at compile time, so it tracks any
        // `--target-dir` override the runner applies — notably
        // `cargo-llvm-cov`, which builds into `target/llvm-cov-target/`.
        // The hardcoded `../target/debug/sql-insight` form worked under
        // tarpaulin only because tarpaulin reuses the default target dir.
        const BIN_PATH: &str = env!("CARGO_BIN_EXE_sql-insight");
        const TIMEOUT_DURATION: Duration = Duration::from_secs(1);

        async fn write_to_stdin(
            stdin: &mut ChildStdin,
            message: &str,
        ) -> Result<(), Box<dyn std::error::Error>> {
            time::timeout(TIMEOUT_DURATION, stdin.write_all(message.as_bytes()))
                .await?
                .map_err(Into::into)
        }

        async fn read_from_stdout(
            stdout_reader: &mut Lines<BufReader<ChildStdout>>,
        ) -> Result<String, Box<dyn std::error::Error>> {
            time::timeout(TIMEOUT_DURATION, stdout_reader.next_line())
                .await??
                .ok_or_else(|| "Received None from stdout".into())
        }

        async fn read_from_stderr(
            stderr_reader: &mut Lines<BufReader<ChildStderr>>,
        ) -> Result<String, Box<dyn std::error::Error>> {
            time::timeout(TIMEOUT_DURATION, stderr_reader.next_line())
                .await??
                .ok_or_else(|| "Received None from stderr".into())
        }

        #[tokio::test]
        async fn test_normalize_interactive() -> Result<(), Box<dyn std::error::Error>> {
            // `normalize` reaches the interactive path through a different
            // match arm (via `common_options`), so the `format` interactive
            // test alone does not cover it.
            let mut child = Command::new(BIN_PATH)
                .arg("normalize")
                .arg("-i")
                .stdin(process::Stdio::piped())
                .stdout(process::Stdio::piped())
                .stderr(process::Stdio::piped())
                .spawn()
                .expect("Failed to spawn child process");

            let stdin = child.stdin.as_mut().expect("Failed to open stdin");
            let stdout = child.stdout.take().expect("Failed to open stdout");
            let mut stdout_reader = BufReader::new(stdout).lines();

            let initial_prompt = read_from_stdout(&mut stdout_reader).await?;
            assert!(
                initial_prompt.contains("Entering interactive mode."),
                "Initial prompt not as expected: {initial_prompt:?}"
            );

            write_to_stdin(stdin, "SELECT * FROM t1 WHERE a = 1;\n").await?;
            let query_result = read_from_stdout(&mut stdout_reader).await?;
            assert!(
                query_result.contains("SELECT * FROM t1 WHERE a = ?"),
                "Query result not as expected: {query_result:?}"
            );

            write_to_stdin(stdin, "quit\n").await?;
            child.wait().await?;

            Ok(())
        }

        #[tokio::test]
        async fn test_interactive() -> Result<(), Box<dyn std::error::Error>> {
            let mut child = Command::new(BIN_PATH)
                .arg("format")
                .arg("-i")
                .stdin(process::Stdio::piped())
                .stdout(process::Stdio::piped())
                .stderr(process::Stdio::piped())
                .spawn()
                .expect("Failed to spawn child process");

            let stdin = child.stdin.as_mut().expect("Failed to open stdin");
            let stdout = child.stdout.take().expect("Failed to open stdout");
            let stderr = child.stderr.take().expect("Failed to open stderr");
            let mut stdout_reader = BufReader::new(stdout).lines();
            let mut stderr_reader = BufReader::new(stderr).lines();

            // Initial prompt
            let initial_prompt = read_from_stdout(&mut stdout_reader).await?;
            assert!(
                initial_prompt.contains("Entering interactive mode."),
                "Initial prompt not as expected: {initial_prompt:?}"
            );

            // Check SQL query
            write_to_stdin(stdin, "SELECT *  \n FROM   t1;\n").await?;
            let query_result = read_from_stdout(&mut stdout_reader).await?;
            assert!(
                query_result.contains("SELECT * FROM t1"),
                "Query result not as expected: {query_result:?}"
            );

            // Check invalid SQL query
            write_to_stdin(stdin, "SELECT *  \n FROM t1 WHERE;\n").await?;
            let invalid_query_result = read_from_stderr(&mut stderr_reader).await?;
            assert!(
                invalid_query_result.contains("Error: sql parser error: Expected: an expression,"),
                "Invalid query result not as expected: {invalid_query_result:?}"
            );

            // Empty input do nothing
            write_to_stdin(stdin, "\n").await?;

            // Send quit command
            write_to_stdin(stdin, "quit\n").await?;
            let exit_message = read_from_stdout(&mut stdout_reader).await?;
            assert!(
                exit_message.contains("Bye"),
                "Exit message not as expected: {exit_message:?}"
            );

            child.wait().await?;

            Ok(())
        }
    }

    mod invalid_cases {
        use super::*;

        #[test]
        fn test_both_sql_and_file_provided() {
            let mut temp_file = NamedTempFile::new().unwrap();
            temp_file
                .write_all(b"select  *  \n  from  t1; INSERT INTO t2 ( a )   VALUES  \n (1);")
                .unwrap();
            sql_insight_cmd()
                .arg("format")
                .arg("select  *  \n  from  t1; INSERT INTO t2 ( a )   VALUES  \n (1);")
                .arg("--file")
                .arg(temp_file.path())
                .assert()
                .failure()
                .stdout("")
                .stderr(predicate::str::contains(
                    "the argument '[SQL]' cannot be used with '--file <FILE>'",
                ));
        }

        #[test]
        fn test_invalid_dialect_name_provided() {
            sql_insight_cmd()
                .arg("format")
                .arg("--dialect")
                .arg("invalid_dialect")
                .arg("select  *  \n  from  t1; INSERT INTO t2 ( a )   VALUES  \n (1);")
                .assert()
                .failure()
                .stdout("")
                .stderr(predicate::str::contains(
                    "Error: Dialect not found: invalid_dialect\n",
                ));
        }

        #[test]
        fn test_over_qualified_name_is_best_effort() {
            // Behavior change vs the legacy resolver: an over-qualified table
            // name (more than `catalog.schema.name`) can't be represented as
            // a `TableReference`. The resolver hard-errored ("Too many
            // identifiers provided"); the bound-plan engine is best-effort,
            // dropping the unrepresentable relation, so the statement yields
            // an empty (no-table) line.
            sql_insight_cmd()
                .arg("extract")
                .arg("tables")
                .arg("select * from catalog.schema.table.extra")
                .assert()
                .success()
                .stdout("\n")
                .stderr("");
        }

        #[test]
        fn test_extract_crud_over_qualified_name_is_best_effort() {
            // Behavior change vs the legacy resolver: an over-qualified table
            // name (more than `catalog.schema.name`) can't be represented as
            // a `TableReference`. The resolver hard-errored ("Too many
            // identifiers provided"); the bound-plan engine is best-effort,
            // so it drops the unrepresentable relation and reports an empty
            // result instead of failing the statement.
            sql_insight_cmd()
                .arg("extract")
                .arg("crud")
                .arg("select * from catalog.schema.table.extra")
                .assert()
                .success()
                .stdout("Create: [], Read: [], Update: [], Delete: []\n")
                .stderr("");
        }

        #[test]
        fn test_file_not_found() {
            sql_insight_cmd()
                .arg("extract")
                .arg("tables")
                .arg("--file")
                .arg("non_existent_file.sql")
                .assert()
                .failure()
                .stdout("")
                .stderr(predicate::str::contains(
                    "Failed to read file non_existent_file.sql:",
                ));
        }
    }
}
