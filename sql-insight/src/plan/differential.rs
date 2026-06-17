//! The differential harness (test-only): run the incubating `plan` engine
//! and the live [`crate::resolver`] on the same SQL and assert the public
//! surfaces match. This is the parity net — every later brick extends the
//! corpus, and a regression shows up as a surface mismatch.
//!
//! Compared as multisets (order is non-contractual): all six public surfaces
//! — `reads`, `column lineage`, `table reads`, `writes`, `table writes`, and
//! `table lineage`. The corpus grows with coverage (query core → clauses →
//! derived / CTE / set-op → subqueries → catalog → INSERT). Intentional
//! divergences (e.g. LATERAL enforcement) are asserted on the new engine
//! directly rather than against the resolver.

use sqlparser::dialect::{Dialect, GenericDialect};
use sqlparser::parser::Parser;

use crate::casing::IdentifierCasing;
use crate::catalog::Catalog;
use crate::extractor::{ColumnLineageEdge, ColumnTarget, TableLineageEdge};
use crate::reference::{ColumnRead, ColumnReference, TableRead, TableReference};

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
    let cur_writes = crate::resolver::extract_writes(&cur);
    let cur_table_writes = crate::resolver::extract_table_writes(&cur);
    let cur_table_lineage = crate::resolver::extract_table_lineage(&cur);

    // Incubating engine (option-a `plan`).
    let op = super::binder::build(stmt, catalog, casing);
    let new_reads = super::traverse::reads(&op);
    let new_lineage = super::traverse::column_lineage(&op);
    let new_table_reads = super::traverse::table_reads(&op);
    let new_writes = super::traverse::writes(&op);
    let new_table_writes = super::traverse::table_writes(&op);
    let new_table_lineage = super::traverse::table_lineage(&op);

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
    assert_bag_eq(
        sql,
        "writes",
        write_bag(&cur_writes),
        write_bag(&new_writes),
    );
    assert_bag_eq(
        sql,
        "table_writes",
        table_write_bag(&cur_table_writes),
        table_write_bag(&new_table_writes),
    );
    assert_bag_eq(
        sql,
        "table_lineage",
        table_lineage_bag(&cur_table_lineage),
        table_lineage_bag(&new_table_lineage),
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

fn write_bag(writes: &[ColumnReference]) -> Vec<String> {
    writes
        .iter()
        .map(|w| {
            let t = w
                .table
                .as_ref()
                .map_or_else(|| "?".to_string(), |t| t.name.value.clone());
            format!("{t}.{}", w.name.value)
        })
        .collect()
}

fn table_write_bag(writes: &[TableReference]) -> Vec<String> {
    writes.iter().map(|w| w.name.value.clone()).collect()
}

fn table_lineage_bag(edges: &[TableLineageEdge]) -> Vec<String> {
    edges
        .iter()
        .map(|e| {
            format!(
                "{}#{:?} -> {}",
                e.source.reference.name.value, e.source.resolution, e.target.name.value
            )
        })
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
fn subquery_parity() {
    // Subqueries in expressions: scalar (value) vs predicate (filter), and
    // correlation (catalog-free).
    let corpus = [
        "SELECT a FROM s WHERE id IN (SELECT id FROM x)", // IN: x read, not a lineage feeder
        "SELECT (SELECT max(v) FROM u) AS m FROM s",      // scalar: u feeds the value
        "SELECT a, (SELECT max(v) FROM u) AS m FROM s",
        "SELECT a FROM s WHERE a > (SELECT avg(b) FROM s2)", // scalar in predicate
        "SELECT a FROM s WHERE EXISTS (SELECT 1 FROM x WHERE x.id = s.id)", // correlated EXISTS
        "SELECT a FROM s WHERE id IN (SELECT id FROM x WHERE x.k = s.k)", // correlated IN
    ];
    for sql in corpus {
        assert_parity(sql);
    }
}

/// Build the new-engine surfaces directly (for divergence cases asserted on
/// their own, not against resolver).
fn plan_surfaces(sql: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let stmts = Parser::parse_sql(&GenericDialect {}, sql).unwrap();
    let casing = IdentifierCasing::for_dialect(&GenericDialect {});
    let op = super::binder::build(&stmts[0], None, casing);
    (
        read_bag(&super::traverse::reads(&op)),
        lineage_bag(&super::traverse::column_lineage(&op)),
        table_read_bag(&super::traverse::table_reads(&op)),
    )
}

#[test]
fn recursive_cte_terminates() {
    // Accepted improvement divergence: the new engine collapses the output to
    // the real anchor base once and terminates the self-reference (active-set),
    // where resolver emits a duplicate (anchor + self-ref-collapsed-to-anchor).
    let (mut reads, mut lineage, mut tables) = plan_surfaces(
        "WITH RECURSIVE c AS (SELECT id FROM base UNION ALL SELECT id FROM c) SELECT id FROM c",
    );
    reads.sort();
    lineage.sort();
    tables.sort();
    assert_eq!(reads, vec!["base.id#Inferred"]);
    assert_eq!(lineage, vec!["base.id -Passthrough-> out[0]:id"]);
    assert_eq!(tables, vec!["base#Inferred"]);
}

#[test]
fn lateral_correlation_parity() {
    // A LATERAL factor referencing a left sibling — both engines resolve it
    // (resolver doesn't enforce lateral, the new engine threads the left
    // siblings for a lateral factor), so they agree here.
    let corpus = [
        "SELECT d.s FROM t1 u, LATERAL (SELECT u.x AS s FROM t2) d",
        "SELECT d.s FROM t1 u JOIN LATERAL (SELECT u.x AS s FROM t2) d ON true",
    ];
    for sql in corpus {
        assert_parity(sql);
    }
}

#[test]
fn lateral_enforcement_divergence() {
    // A NON-lateral derived table referencing a left sibling is invalid SQL;
    // the new engine leaves it `Unresolved` (correct), where resolver
    // mis-resolves it to the sibling. This is the accepted improvement
    // divergence (asserted on the new engine directly, not against resolver).
    let (reads, _lineage, _tables) =
        plan_surfaces("SELECT d.s FROM t1 u, (SELECT u.x AS s FROM t2) d");
    // `u.x` inside the non-lateral derived table can't see the sibling `u` →
    // it is `Unresolved` (resolver mis-resolves it to t1.x). `d.s` is derived
    // (dropped). So the only column read is the unresolved `x`.
    assert_eq!(reads, vec!["?.x#Unresolved"]);
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

#[test]
fn insert_parity() {
    // INSERT (catalog-free): SELECT / VALUES / UNION / JOIN sources, the
    // value-vs-filter feeding split, and a statement-level WITH. Both engines
    // must agree on reads / column lineage / table reads / writes / table
    // writes / table lineage.
    let corpus = [
        "INSERT INTO t (a, b) SELECT x, y FROM s",
        "INSERT INTO t (a) SELECT x FROM s WHERE x > 0", // predicate not a feeder
        "INSERT INTO t VALUES (1, 2)",                   // VALUES: target write, no lineage
        "INSERT INTO t (a, b) VALUES (1, 2)",            // explicit cols, no column lineage
        "INSERT INTO t SELECT x FROM s", // column-less, no catalog → table lineage only
        "INSERT INTO t (a) SELECT x FROM s1 UNION SELECT y FROM s2", // each branch pairs
        "INSERT INTO t (a, b) SELECT x, y FROM s JOIN s2 ON s.id = s2.id", // join feeds both
        "INSERT INTO t (a) SELECT (SELECT max(v) FROM u) FROM s", // scalar value subquery feeds
        "INSERT INTO t (a) SELECT x FROM s WHERE x IN (SELECT id FROM u)", // filter subquery: read, not feeder
        "WITH c AS (SELECT x FROM s) INSERT INTO t (a) SELECT x FROM c",   // statement-level WITH
    ];
    for sql in corpus {
        assert_parity(sql);
    }
}

#[test]
fn update_parity() {
    // UPDATE (catalog-free): the SET value path vs the WHERE filter, FROM
    // relations, joins, and value subqueries.
    let corpus = [
        "UPDATE t SET a = b",                    // self-column RHS, no table lineage
        "UPDATE t SET a = b, c = d WHERE e > 0", // multiple SET + WHERE
        "UPDATE t SET a = b + c WHERE d < 1",    // transformation RHS
        "UPDATE t SET a = s.x FROM s WHERE t.id = s.id", // FROM feeds
        "UPDATE t SET a = 5 FROM s WHERE t.id = s.id", // FROM feeds even with literal RHS
        "UPDATE t SET a = (SELECT max(v) FROM u)", // scalar value subquery feeds
        "UPDATE t SET a = b WHERE c IN (SELECT id FROM u)", // filter subquery: read, not feeder
    ];
    for sql in corpus {
        assert_parity(sql);
    }
}

#[test]
fn delete_parity() {
    // DELETE (catalog-free): the FROM-is-target vs USING / explicit-list
    // shapes. No column writes / lineage — only target writes and table reads.
    let corpus = [
        "DELETE FROM t",                               // FROM is the target
        "DELETE FROM t WHERE a > 0",                   // target in scope for predicate
        "DELETE FROM t1 USING s WHERE t1.id = s.id",   // USING is a read
        "DELETE FROM t1, t2 USING s WHERE t1.k = s.k", // multi-target
    ];
    for sql in corpus {
        assert_parity(sql);
    }
    // `DELETE t1 FROM …` (explicit-list shape, FROM relations are reads) needs
    // the MySQL dialect to parse.
    let mysql = sqlparser::dialect::MySqlDialect {};
    for sql in [
        "DELETE t1 FROM t1 JOIN t2 ON t1.id = t2.id WHERE t2.x > 0",
        "DELETE t1, t2 FROM t1 JOIN t2 ON t1.id = t2.id",
    ] {
        assert_parity_inner(sql, &mysql, None);
    }
}

#[test]
fn update_catalog_parity() {
    use crate::catalog::CatalogTable;
    let catalog = Catalog::new()
        .table(CatalogTable::new("public", "users").columns(["id", "name"]))
        .table(CatalogTable::new("public", "staging").columns(["id", "name"]));
    let corpus = [
        "UPDATE users SET name = 'x' WHERE id > 0",
        "UPDATE users SET name = staging.name FROM staging WHERE users.id = staging.id",
        "DELETE FROM users WHERE id > 0",
        "DELETE FROM users USING staging WHERE users.id = staging.id",
    ];
    for sql in corpus {
        assert_parity_cat(sql, &catalog);
    }
}

#[test]
fn ddl_parity() {
    // CTAS / CREATE VIEW (data movers, like INSERT) + ALTER TABLE / DROP /
    // TRUNCATE (target / column writes, no lineage). Catalog-free.
    let corpus = [
        "CREATE TABLE dst AS SELECT a, b FROM s",
        "CREATE TABLE t (a INT, b INT)", // plain create: target only, no writes
        "CREATE VIEW v AS SELECT a FROM s",
        "CREATE VIEW v (x) AS SELECT a FROM s WHERE a > 0",
        "CREATE TABLE dst AS SELECT a FROM s1 UNION SELECT b FROM s2",
        "ALTER TABLE t ADD COLUMN c INT",
        "ALTER TABLE t DROP COLUMN c",
        "ALTER TABLE t RENAME COLUMN a TO b",
        "DROP TABLE t",
        "DROP TABLE a, b",
        "DROP VIEW v",
        "TRUNCATE TABLE t",
    ];
    for sql in corpus {
        assert_parity(sql);
    }
    // An explicit CREATE VIEW column list (names come from the statement, not
    // the source) — covers the `columns` non-empty bind branch.
    assert_parity_inner(
        "CREATE VIEW v (x, y) AS SELECT a, b FROM s",
        &sqlparser::dialect::PostgreSqlDialect {},
        None,
    );
}

#[test]
fn ddl_catalog_parity() {
    use crate::catalog::CatalogTable;
    let catalog =
        Catalog::new().table(CatalogTable::new("public", "staging").columns(["id", "name"]));
    let corpus = [
        "CREATE TABLE public.dst AS SELECT id, name FROM staging",
        "CREATE VIEW public.v AS SELECT id FROM staging",
        "DROP TABLE staging",
    ];
    for sql in corpus {
        assert_parity_cat(sql, &catalog);
    }
}

#[test]
fn insert_catalog_parity() {
    use crate::catalog::CatalogTable;
    let catalog = Catalog::new()
        .table(CatalogTable::new("public", "users").columns(["id", "name"]))
        .table(CatalogTable::new("public", "staging").columns(["id", "name"]));
    let corpus = [
        "INSERT INTO users (id, name) SELECT id, name FROM staging", // explicit, both cataloged
        "INSERT INTO users SELECT id, name FROM staging",            // column-less → catalog-fill
        "INSERT INTO users (id) SELECT id FROM staging WHERE name > 'a'",
    ];
    for sql in corpus {
        assert_parity_cat(sql, &catalog);
    }
}
