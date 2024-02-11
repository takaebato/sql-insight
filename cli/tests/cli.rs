#[cfg(test)]
mod tests {
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
                .arg("extract-crud")
                .arg("select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
                .assert()
                .success()
                .stdout("Create: [], Read: [t1, t2], Update: [], Delete: []\nCreate: [t1], Read: [t2], Update: [], Delete: []\n")
                .stderr("");
        }

        #[test]
        fn test_extract_crud_tables_with_dialect() {
            sql_insight_cmd()
                .arg("extract-crud")
                .arg("--dialect")
                .arg("mysql")
                .arg("select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
                .assert()
                .success()
                .stdout("Create: [], Read: [t1, t2], Update: [], Delete: []\nCreate: [t1], Read: [t2], Update: [], Delete: []\n")
                .stderr("");
        }

        #[test]
        fn test_extract_crud_tables_from_file() {
            let mut temp_file = NamedTempFile::new().unwrap();
            temp_file
                .write_all(b"select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
                .unwrap();
            sql_insight_cmd()
                .arg("extract-crud")
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
                .arg("extract-tables")
                .arg("select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
                .assert()
                .success()
                .stdout("t1, t2\nt1, t2\n")
                .stderr("");
        }

        #[test]
        fn test_extract_tables_with_full_identifiers_and_alis() {
            sql_insight_cmd()
                .arg("extract-tables")
                .arg("select * from catalog.schema.t1 as t1 inner join catalog.schema.t2 as t2 using(id); \
                      insert into catalog.schema.t1 (a) select b from catalog.schema.t2;")
                .assert()
                .success()
                .stdout("catalog.schema.t1 AS t1, catalog.schema.t2 AS t2\ncatalog.schema.t1, catalog.schema.t2\n")
                .stderr("");
        }

        #[test]
        fn test_extract_tables_with_dialect() {
            sql_insight_cmd()
                .arg("extract-tables")
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
                .arg("extract-tables")
                .arg("--file")
                .arg(temp_file.path())
                .assert()
                .success()
                .stdout("t1, t2\nt1, t2\n")
                .stderr("");
        }
    }

    mod interactive_mode {
        use super::*;
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
        use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
        use tokio::time;

        const BIN_PATH: &str = "../target/debug/sql-insight";
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
        async fn test_interactive() -> Result<(), Box<dyn std::error::Error>> {
            let mut child = Command::new(BIN_PATH)
                .arg("format")
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
                invalid_query_result.contains("Error: sql parser error: Expected an expression:"),
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
        fn test_fail_to_analyze_sql() {
            sql_insight_cmd()
                .arg("extract-tables")
                .arg("select * from catalog.schema.table.extra")
                .assert()
                .success()
                .stdout("Error: Too many identifiers provided\n")
                .stderr("");
        }

        #[test]
        fn test_file_not_found() {
            sql_insight_cmd()
                .arg("extract-tables")
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
