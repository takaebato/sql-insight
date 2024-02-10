#[cfg(test)]
mod tests {
    use assert_cmd::Command;

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
    }
}
