//! The differential harness (test-only): run the incubating `plan` engine
//! and the live [`crate::resolver`] on the same SQL and assert the public
//! surfaces match. This is the parity net — every later brick extends the
//! corpus, and a regression shows up as a surface mismatch.
//!
//! Compared as multisets (order is non-contractual): `reads`, `column
//! lineage`, `table reads`. The corpus grows with coverage; brick ② holds
//! the catalog-free query core (SELECT / FROM / JOIN / WHERE / projection).
//! Intentional divergences (e.g. LATERAL enforcement) will move to an
//! allow-list as their bricks land.

use sqlparser::dialect::{Dialect, GenericDialect};
use sqlparser::parser::Parser;

use crate::casing::IdentifierCasing;
use crate::catalog::Catalog;
use crate::extractor::{ColumnLineageEdge, ColumnTarget};
use crate::reference::{ColumnRead, TableRead};

/// Assert the two engines agree on every surface for `sql` (GenericDialect,
/// catalog-free).
fn assert_parity(sql: &str) {
    assert_parity_inner(sql, &GenericDialect {}, None);
}

/// Like [`assert_parity`] but with a catalog (catalog-aware resolution).
fn assert_parity_cat(sql: &str, catalog: &Catalog) {
    assert_parity_inner(sql, &GenericDialect {}, Some(catalog));
}

fn assert_parity_inner(sql: &str, dialect: &dyn Dialect, catalog: Option<&Catalog>) {
    let statements =
        Parser::parse_sql(dialect, sql).unwrap_or_else(|e| panic!("parse {sql:?}: {e}"));
    let casing = IdentifierCasing::for_dialect(dialect);
    let stmt = &statements[0];

    // Current engine (design-B `resolver`).
    let (cur, _diagnostics) = crate::resolver::build_plan(stmt, catalog, casing);
    let cur_reads = crate::resolver::extract_reads(&cur);
    let cur_lineage = crate::resolver::extract_lineage(&cur);
    let cur_table_reads = crate::resolver::extract_table_reads(&cur);

    // Incubating engine (option-a `plan`).
    let op = super::binder::build(stmt, catalog, casing);
    let new_reads = super::traverse::reads(&op);
    let new_lineage = super::traverse::column_lineage(&op);
    let new_table_reads = super::traverse::table_reads(&op);

    assert_bag_eq(sql, "reads", read_bag(&cur_reads), read_bag(&new_reads));
    assert_bag_eq(
        sql,
        "lineage",
        lineage_bag(&cur_lineage),
        lineage_bag(&new_lineage),
    );
    assert_bag_eq(
        sql,
        "table_reads",
        table_read_bag(&cur_table_reads),
        table_read_bag(&new_table_reads),
    );
}

fn assert_bag_eq(sql: &str, surface: &str, mut current: Vec<String>, mut new: Vec<String>) {
    current.sort();
    new.sort();
    assert_eq!(
        new, current,
        "\n{surface} mismatch for {sql:?}\n  current (resolver): {current:?}\n  new (plan):         {new:?}\n"
    );
}

fn read_bag(reads: &[ColumnRead]) -> Vec<String> {
    reads
        .iter()
        .map(|r| {
            let table = r
                .reference
                .table
                .as_ref()
                .map_or_else(|| "?".to_string(), |t| t.name.value.clone());
            format!("{}.{}#{:?}", table, r.reference.name.value, r.resolution)
        })
        .collect()
}

fn lineage_bag(edges: &[ColumnLineageEdge]) -> Vec<String> {
    edges
        .iter()
        .map(|e| {
            let src = e
                .source
                .reference
                .table
                .as_ref()
                .map_or_else(|| "?".to_string(), |t| t.name.value.clone());
            let target = match &e.target {
                ColumnTarget::QueryOutput { name, position } => format!(
                    "out[{position}]:{}",
                    name.as_ref().map_or("?", |n| n.value.as_str())
                ),
                ColumnTarget::Relation(r) => {
                    let t = r
                        .table
                        .as_ref()
                        .map_or_else(|| "?".to_string(), |t| t.name.value.clone());
                    format!("{t}.{}", r.name.value)
                }
            };
            format!(
                "{src}.{} -{:?}-> {target}",
                e.source.reference.name.value, e.kind
            )
        })
        .collect()
}

fn table_read_bag(reads: &[TableRead]) -> Vec<String> {
    reads
        .iter()
        .map(|r| format!("{}#{:?}", r.reference.name.value, r.resolution))
        .collect()
}

#[test]
fn query_core_parity() {
    // catalog-free SELECT / FROM / JOIN / WHERE / projection — the constructs
    // the brick-② binder handles. Both engines must agree.
    let corpus = [
        "SELECT a FROM t",
        "SELECT a, b FROM t",
        "SELECT a AS x FROM t",
        "SELECT a + b AS s FROM t",
        "SELECT f(a, b) AS g FROM t",
        "SELECT a FROM t WHERE a > 0",
        "SELECT a FROM t WHERE b > 0 AND c < 1",
        "SELECT t1.x, t2.y FROM t1 JOIN t2 ON t1.id = t2.id",
        "SELECT t1.x FROM t1 JOIN t2 ON t1.id = t2.id WHERE t2.y > 0",
        "SELECT x FROM t1, t2",
        "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id", // unqualified `a` → ambiguous
        "SELECT t.a, t.b + t.c AS s FROM t",
    ];
    for sql in corpus {
        assert_parity(sql);
    }
}

#[test]
fn clause_parity() {
    // GROUP BY / HAVING / ORDER BY + alias visibility (catalog-free).
    let corpus = [
        "SELECT a FROM t GROUP BY a", // identity alias → a read (proj + group by)
        "SELECT a + b AS s FROM t GROUP BY s", // introduced alias → s dropped, a/b at proj
        "SELECT dept, sum(salary) AS total FROM emp GROUP BY dept",
        "SELECT dept FROM emp GROUP BY dept HAVING sum(salary) > 0", // HAVING reads a non-projected column
        "SELECT a FROM t ORDER BY a",                                // identity in ORDER BY
        "SELECT a + b AS s FROM t ORDER BY s",                       // introduced alias in ORDER BY
        "SELECT a, b FROM t ORDER BY b, a",
        "SELECT a FROM t WHERE a > 0 GROUP BY a HAVING count(b) > 1 ORDER BY a",
        "SELECT t.a, sum(t.b) AS s FROM t GROUP BY t.a ORDER BY s",
    ];
    for sql in corpus {
        assert_parity(sql);
    }
}

#[test]
fn derived_cte_setop_parity() {
    // Derived tables, (non-recursive) CTEs, set operations (catalog-free).
    let corpus = [
        // derived tables
        "SELECT x FROM (SELECT a AS x FROM t) d",
        "SELECT d.y FROM (SELECT a + b AS y FROM t) d",
        "SELECT x FROM (SELECT a FROM t) d WHERE x > 0",
        "SELECT x, x FROM (SELECT a FROM t) d", // outer refs synthetic → one base read
        // CTEs
        "WITH c AS (SELECT a FROM t) SELECT a FROM c",
        "WITH c (x) AS (SELECT a FROM t) SELECT x FROM c", // column-list rename
        "WITH recent AS (SELECT id, amount FROM orders), \
              totals AS (SELECT id, sum(amount) AS total FROM recent GROUP BY id) \
         SELECT total FROM totals", // chained
        "WITH c AS (SELECT a FROM t) SELECT a FROM c x JOIN c y ON x.a = y.a", // twice-referenced
        // set operations
        "SELECT a FROM t1 UNION SELECT b FROM t2",
        "SELECT a FROM t1 UNION ALL SELECT a FROM t1",
    ];
    for sql in corpus {
        assert_parity(sql);
    }
}

#[test]
fn catalog_aware_parity() {
    use crate::catalog::CatalogTable;
    let catalog = Catalog::new()
        .table(CatalogTable::new("public", "users").columns(["id", "name"]))
        .table(CatalogTable::new("public", "orders").columns(["id", "user_id", "amount"]))
        .table(CatalogTable::new("public", "known_t").columns(["a", "b"]));
    let corpus = [
        "SELECT name FROM users",              // Cataloged hit (canonicalized)
        "SELECT public.users.name FROM users", // qualified, canonical agrees
        "SELECT nonexistent FROM users",       // Known miss → Unresolved
        "SELECT id FROM users JOIN orders ON users.id = orders.user_id", // Ambiguous (both have id)
        "SELECT name, amount FROM users JOIN orders ON users.id = orders.user_id",
        "SELECT a FROM known_t JOIN other ON known_t.b = other.k", // Known-witness over Open → Inferred
        "SELECT users.name FROM users WHERE users.id > 0",
    ];
    for sql in corpus {
        assert_parity_cat(sql, &catalog);
    }
}
