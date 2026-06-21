use crate::support::*;

/// Pins the dialect-aware identifier case-folding policy
/// (`IdentifierCasing`) as observed through
/// column resolution. The distinguishing cases are table-name
/// case-sensitivity (BigQuery / MySQL real tables are case-sensitive;
/// most dialects fold) and alias case-insensitivity (BigQuery aliases
/// fold even though its tables don't). Column case-insensitivity is
/// shown via a catalog.
#[cfg(test)]
mod dialect_casing_coverage {
    use super::*;
    use sql_insight::catalog::{Catalog, CatalogTable};
    use sql_insight::sqlparser::dialect::{BigQueryDialect, GenericDialect, MySqlDialect};

    #[derive(Debug, Default)]
    struct TestCatalog {
        catalog: Catalog,
    }

    impl TestCatalog {
        fn with(mut self, name: &str, cols: Vec<&'static str>) -> Self {
            self.catalog = std::mem::take(&mut self.catalog)
                .table(CatalogTable::new("public", name).columns(cols));
            self
        }
    }

    fn reads(sql: &str, dialect: &dyn Dialect, catalog: Option<&TestCatalog>) -> Vec<ColumnRead> {
        let mut options = ExtractorOptions::new();
        if let Some(c) = catalog {
            options = options.with_catalog(&c.catalog);
        }
        extract_column_operations_with_options(dialect, sql, options)
            .unwrap()
            .remove(0)
            .unwrap()
            .reads
    }

    #[test]
    fn bigquery_qualified_table_ref_is_case_sensitive() {
        // BigQuery tables are case-sensitive: qualifier `T1` does not
        // match the binding `t1`, so the ref is unresolved.
        assert_unordered_eq!(
            reads("SELECT T1.id FROM t1", &BigQueryDialect {}, None),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn mysql_qualified_table_ref_is_case_sensitive() {
        // MySQL real-table names default case-sensitive (filesystem
        // fallback), same as BigQuery here.
        assert_unordered_eq!(
            reads("SELECT T1.id FROM t1", &MySqlDialect {}, None),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn generic_qualified_table_ref_is_case_insensitive() {
        // The generic dialect folds lower-case, so `T1` matches `t1`
        // and the ref resolves (Inferred — no catalog).
        assert_unordered_eq!(
            reads("SELECT T1.id FROM t1", &GenericDialect {}, None),
            vec![read("t1", "id")]
        );
    }

    #[test]
    fn bigquery_alias_ref_is_case_insensitive() {
        // BigQuery aliases fold case-insensitively even though its
        // tables don't: `A` matches the alias `a`, resolving to t1.
        assert_unordered_eq!(
            reads("SELECT A.id FROM t1 AS a", &BigQueryDialect {}, None),
            vec![read("t1", "id")]
        );
    }

    #[test]
    fn bigquery_column_is_case_insensitive() {
        // BigQuery columns fold case-insensitively: `Id` matches the
        // catalog's `id`, confirming the resolution on t1. The canonical
        // identity surfaces with BigQuery's quote — a backtick, not the
        // `"` the shared `cataloged_table` helper assumes.
        let catalog = TestCatalog::default().with("t1", vec!["id"]);
        let bq_t1 = TableReference {
            catalog: None,
            schema: Some(Ident::with_quote('`', "public")),
            name: Ident::with_quote('`', "t1"),
        };
        assert_unordered_eq!(
            reads("SELECT Id FROM t1", &BigQueryDialect {}, Some(&catalog)),
            vec![read_with_ref(bq_t1, "Id", ResolutionKind::Cataloged)]
        );
    }

    #[test]
    fn clickhouse_column_lookup_is_case_sensitive_in_lineage() {
        // ClickHouse folds none of its identifier classes (Sensitive). A
        // derived table exposes two aliases differing only in case (`X` and
        // `x`); the outer `SELECT x` must trace to `a AS x`, not to the
        // earlier `b AS X`. (Before threading the dialect column fold through
        // the origin trace, the inner name lookup ASCII-ci-matched `X` first
        // and emitted `t.b -> x` — a real ClickHouse-only bug.)
        use sql_insight::sqlparser::dialect::ClickHouseDialect;
        let ops = extract_column_operations(
            &ClickHouseDialect {},
            "SELECT x FROM (SELECT b AS X, a AS x FROM t) d",
        )
        .unwrap()
        .remove(0)
        .unwrap();
        assert_eq!(ops.lineage.len(), 1);
        let edge = &ops.lineage[0];
        assert_eq!(edge.source.reference.name.value, "a");
        match &edge.target {
            ColumnTarget::QueryOutput { name, .. } => {
                assert_eq!(name.as_ref().unwrap().value, "x")
            }
            other => panic!("expected QueryOutput, got {other:?}"),
        }
    }
}
