#[cfg(test)]
mod tests {
    use std::process::Command;

    static BIN_PATH: &str = "./target/debug/sql-insight";

    #[test]
    fn test_format() {
        let output = Command::new(BIN_PATH)
            .arg("format")
            .arg("select  *  \n  from  t1; INSERT INTO t2 ( a )   VALUES  \n (1);")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "SELECT * FROM t1\nINSERT INTO t2 (a) VALUES (1)\n"
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }

    #[test]
    fn test_format_with_dialect() {
        let output = Command::new(BIN_PATH)
            .arg("format")
            .arg("--dialect")
            .arg("mysql")
            .arg("select  *  \n  from  t1; INSERT INTO t2 ( a )   VALUES  \n (1);")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "SELECT * FROM t1\nINSERT INTO t2 (a) VALUES (1)\n"
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }

    #[test]
    fn test_normalize() {
        let output = Command::new(BIN_PATH)
            .arg("normalize")
            .arg("select * from t1 where a = 1 and b in (2, 3); insert into t2 (a) values (4);")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "SELECT * FROM t1 WHERE a = ? AND b IN (?, ?)\nINSERT INTO t2 (a) VALUES (?)\n"
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }

    #[test]
    fn test_normalize_with_dialect() {
        let output = Command::new(BIN_PATH)
            .arg("normalize")
            .arg("--dialect")
            .arg("mysql")
            .arg("select * from t1 where a = 1 and b in (2, 3); insert into t2 (a) values (4);")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "SELECT * FROM t1 WHERE a = ? AND b IN (?, ?)\nINSERT INTO t2 (a) VALUES (?)\n"
        );
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }

    #[test]
    fn test_extract_crud_tables() {
        let output = Command::new(BIN_PATH)
            .arg("extract-crud")
            .arg("select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "Create: [], Read: [t1, t2], Update: [], Delete: []\nCreate: [t1], Read: [t2], Update: [], Delete: []\n");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }

    #[test]
    fn test_extract_crud_tables_with_dialect() {
        let output = Command::new(BIN_PATH)
            .arg("extract-crud")
            .arg("--dialect")
            .arg("mysql")
            .arg("select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "Create: [], Read: [t1, t2], Update: [], Delete: []\nCreate: [t1], Read: [t2], Update: [], Delete: []\n");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }

    #[test]
    fn test_extract_tables() {
        let output = Command::new(BIN_PATH)
            .arg("extract-tables")
            .arg("select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "t1, t2\nt1, t2\n");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }

    #[test]
    fn test_extract_tables_with_dialect() {
        let output = Command::new(BIN_PATH)
            .arg("extract-tables")
            .arg("--dialect")
            .arg("mysql")
            .arg("select * from t1 inner join t2 using(id); insert into t1 (a) select b from t2;")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "t1, t2\nt1, t2\n");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "");
    }
}
