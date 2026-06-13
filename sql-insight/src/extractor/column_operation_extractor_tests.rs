use super::*;
use crate::reference::Confidence;
use sqlparser::dialect::GenericDialect;

fn extract(sql: &str) -> ColumnOperation {
    let mut result = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
    result.remove(0).unwrap()
}

fn table(name: &str) -> TableReference {
    TableReference {
        catalog: None,
        schema: None,
        name: name.into(),
    }
}

// Read-side helpers return `ColumnRead` (identity + Confidence).
// `read` and `col` both default to `Confidence::Inferred`, which is
// the catalog-less mode's natural confidence — most tests in this
// file run without a catalog, so the default minimises noise. Tests
// supplying a catalog override with `read_confirmed` / `col_confirmed`.
fn read(table_name: &str, col: &str) -> ColumnRead {
    read_with(table_name, col, Confidence::Inferred)
}

fn read_confirmed(table_name: &str, col: &str) -> ColumnRead {
    read_with(table_name, col, Confidence::Confirmed)
}

fn read_with(table_name: &str, col: &str, confidence: Confidence) -> ColumnRead {
    read_with_ref(table(table_name), col, confidence)
}

fn read_with_ref(table_ref: TableReference, col: &str, confidence: Confidence) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: Some(table_ref),
            name: col.into(),
        },
        confidence,
    }
}

fn col(table_name: &str, name: &str) -> ColumnRead {
    read_with(table_name, name, Confidence::Inferred)
}

fn col_confirmed(table_name: &str, name: &str) -> ColumnRead {
    read_with(table_name, name, Confidence::Confirmed)
}

fn ambiguous(col: &str) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: None,
            name: col.into(),
        },
        confidence: Confidence::Ambiguous,
    }
}

fn unresolved(col: &str) -> ColumnRead {
    ColumnRead {
        reference: ColumnReference {
            table: None,
            name: col.into(),
        },
        confidence: Confidence::Unresolved,
    }
}

// Write-side helpers stay as `ColumnReference` — write targets come
// straight from SQL syntax and are always `Confidence::Confirmed` by
// construction, so attaching a confidence field would be dead weight.
fn write(table_name: &str, col: &str) -> ColumnReference {
    ColumnReference {
        table: Some(table(table_name)),
        name: col.into(),
    }
}

fn out(name: &str, position: usize) -> ColumnTarget {
    ColumnTarget::QueryOutput {
        name: Some(name.into()),
        position,
    }
}

fn out_anon(position: usize) -> ColumnTarget {
    ColumnTarget::QueryOutput {
        name: None,
        position,
    }
}

fn relation(table_name: &str, col: &str) -> ColumnTarget {
    ColumnTarget::Relation(ColumnReference {
        table: Some(table(table_name)),
        name: col.into(),
    })
}

fn passthrough(source: ColumnRead, target: ColumnTarget) -> ColumnLineageEdge {
    ColumnLineageEdge {
        source,
        target,
        kind: ColumnLineageKind::Passthrough,
    }
}

fn transformation(source: ColumnRead, target: ColumnTarget) -> ColumnLineageEdge {
    ColumnLineageEdge {
        source,
        target,
        kind: ColumnLineageKind::Transformation,
    }
}

/// Whole-value-ish assertion: pin down the full
/// `ColumnOperation` for `sql`. reads / writes / lineage /
/// statement_kind compare strictly; diagnostics compare by **kind
/// sequence only** so message wording and span coordinates aren't
/// baked into the expected value.
fn assert_column_ops(sql: &str, expected: ColumnOperation) {
    assert_nth_column_ops(sql, 0, expected);
}

/// Like `assert_column_ops` but for multi-statement batches —
/// targets the statement at `index`. Compose multiple calls to
/// pin down each statement in a batch independently.
fn assert_nth_column_ops(sql: &str, index: usize, expected: ColumnOperation) {
    let actual = extract_column_operations(&GenericDialect {}, sql, None)
        .unwrap()
        .into_iter()
        .nth(index)
        .unwrap_or_else(|| panic!("statement {index} missing in result for SQL: {sql}"))
        .unwrap();
    assert_column_ops_inner(sql, index, actual, expected);
}

fn assert_column_ops_inner(
    sql: &str,
    index: usize,
    actual: ColumnOperation,
    expected: ColumnOperation,
) {
    let ColumnOperation {
        statement_kind,
        reads,
        writes,
        lineage,
        diagnostics,
    } = expected;
    assert_eq!(
        actual.statement_kind, statement_kind,
        "kind for SQL: {sql} (statement {index})"
    );
    assert_eq!(
        actual.reads, reads,
        "reads for SQL: {sql} (statement {index})"
    );
    assert_eq!(
        actual.writes, writes,
        "writes for SQL: {sql} (statement {index})"
    );
    assert_eq!(
        actual.lineage, lineage,
        "lineage for SQL: {sql} (statement {index})"
    );
    let actual_kinds: Vec<_> = actual.diagnostics.iter().map(|d| d.kind.clone()).collect();
    let expected_kinds: Vec<_> = diagnostics.iter().map(|d| d.kind.clone()).collect();
    assert_eq!(
        actual_kinds, expected_kinds,
        "diagnostic kinds for SQL: {sql} (statement {index})"
    );
}

/// Placeholder `ColumnLevelDiagnostic` for `assert_column_ops.expected.diagnostics`.
/// Only the kind is compared; message and span are placeholders.
fn diag(kind: ColumnLevelDiagnosticKind) -> ColumnLevelDiagnostic {
    ColumnLevelDiagnostic {
        kind,
        message: String::new(),
        span: None,
    }
}

mod reads {
    use super::*;

    #[test]
    fn qualified_select_collects_qualified_reads() {
        assert_column_ops(
            "SELECT t1.a, t1.b FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualified_join_collects_reads_from_both_sides() {
        // Resolver walks FROM (including JOIN ON) before the projection,
        // so the predicate columns appear ahead of the projected ones —
        // and are tagged Filter while projection refs are Projection.
        assert_column_ops(
            "SELECT t1.a, t2.b FROM t1 JOIN t2 ON t1.id = t2.id",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "id"),
                    read("t2", "id"),
                    read("t1", "a"),
                    read("t2", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualified_ref_through_alias_resolves_to_real_table() {
        // `u` is an alias of `t1`; the qualified ref `u.a` resolves
        // to the alias-free real table `t1`, matching how an
        // unqualified ref resolves. Alias is use-site decoration,
        // not part of the column's identity.
        assert_column_ops(
            "SELECT u.a FROM t1 AS u",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn qualified_refs_through_aliases_on_both_join_sides_resolve_to_real_tables() {
        // Implicit aliases (`t1 a`, `t2 b`) on both join sides; every
        // qualified ref canonicalizes to its real table. JOIN ON is
        // walked during FROM, so the predicate reads precede the
        // projection reads.
        assert_column_ops(
            "SELECT a.x, b.y FROM t1 a JOIN t2 b ON a.id = b.id",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "id"),
                    read("t2", "id"),
                    read("t1", "x"),
                    read("t2", "y"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "x"), out("x", 0)),
                    passthrough(col("t2", "y"), out("y", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aliased_filter_ref_resolves_to_real_table_and_stays_out_of_lineage() {
        // A WHERE-only column through an alias resolves to the real
        // table for `reads`, but a filter column is not a value
        // contributor, so it never appears in `lineage`.
        assert_column_ops(
            "SELECT u.a FROM t1 AS u WHERE u.b > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn schema_qualified_ref_resolves_to_schema_dot_table() {
        let table_ref = TableReference {
            catalog: None,
            schema: Some("s1".into()),
            name: "t1".into(),
        };
        assert_column_ops(
            "SELECT s1.t1.a FROM s1.t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_with_ref(table_ref.clone(), "a", Confidence::Inferred)],
                writes: vec![],
                lineage: vec![passthrough(
                    read_with_ref(table_ref, "a", Confidence::Inferred),
                    out("a", 0),
                )],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_qualified_ref_resolves_to_catalog_dot_schema_dot_table() {
        // `c1.s1.t1.a` — 4-part ref. parts.last() is the column;
        // the preceding 3 parts decode into TableReference's
        // catalog / schema / name fields.
        let table_ref = TableReference {
            catalog: Some("c1".into()),
            schema: Some("s1".into()),
            name: "t1".into(),
        };
        assert_column_ops(
            "SELECT c1.s1.t1.a FROM c1.s1.t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_with_ref(table_ref.clone(), "a", Confidence::Inferred)],
                writes: vec![],
                lineage: vec![passthrough(
                    read_with_ref(table_ref, "a", Confidence::Inferred),
                    out("a", 0),
                )],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unqualified_ref_against_catalog_qualified_table_inherits_full_qualifier() {
        // `SELECT a FROM c1.s1.t1` — the unqualified `a` resolves
        // to the catalog-qualified binding, picking up the full
        // qualifier in the ColumnReference.
        let table_ref = TableReference {
            catalog: Some("c1".into()),
            schema: Some("s1".into()),
            name: "t1".into(),
        };
        assert_column_ops(
            "SELECT a FROM c1.s1.t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_with_ref(table_ref.clone(), "a", Confidence::Inferred)],
                writes: vec![],
                lineage: vec![passthrough(
                    read_with_ref(table_ref, "a", Confidence::Inferred),
                    out("a", 0),
                )],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn five_part_ref_overshoots_qualifier_decoder_and_is_unresolved() {
        // sqlparser parses `extra.c1.s1.t1.a` into 5 parts. The
        // qualifier decoder caps at 3 parts (catalog / schema /
        // name) — anything longer is a struct-field access on a
        // fully qualified column, which we don't model. The ref
        // is recorded with `table: None`.
        assert_column_ops(
            "SELECT extra.c1.s1.t1.a FROM c1.s1.t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![unresolved("a")],
                writes: vec![],
                lineage: vec![passthrough(unresolved("a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn where_predicate_qualified_ref_is_a_read() {
        assert_column_ops(
            "SELECT t1.a FROM t1 WHERE t1.b > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unqualified_single_table_resolves_to_that_table() {
        assert_column_ops(
            "SELECT a, b FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unqualified_in_where_resolves_to_single_table() {
        assert_column_ops(
            "SELECT a FROM t1 WHERE b > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unqualified_with_multiple_tables_is_ambiguous() {
        // Two `Unknown`-schema tables — without a catalog the
        // resolver can't pick between them, so `a` surfaces with
        // `table: None` and `Confidence::Ambiguous`. The lineage
        // source inherits the same ambiguous read.
        assert_column_ops(
            "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t2", "id"), ambiguous("a")],
                writes: vec![],
                lineage: vec![passthrough(ambiguous("a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unqualified_uses_alias_binding_but_returns_real_table() {
        // Alias is just a binding key; the resolver returns the
        // alias-free TableReference of the binding's underlying table.
        assert_column_ops(
            "SELECT a FROM t1 AS u",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_ref_does_not_surface_in_reads() {
        // The outer `id` resolves to the cte binding (a synthetic
        // intermediate, not real storage), so it's dropped from reads.
        // Reads surface only references with real Table owners or
        // unresolved column names. `unknown_col` doesn't match the
        // cte's Known schema [id], so it surfaces unresolved
        // (table: None) with Confidence::Unresolved on the read.
        assert_column_ops(
            "WITH cte AS (SELECT id FROM t1) SELECT id, unknown_col FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), unresolved("unknown_col")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "id"), out("id", 0)),
                    passthrough(unresolved("unknown_col"), out("unknown_col", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn derived_table_ref_does_not_surface_in_reads() {
        // Outer `id` resolves to derived alias `d` — synthetic, dropped.
        // Only the inner SELECT's t1.id is a real read.
        assert_column_ops(
            "SELECT id FROM (SELECT id FROM t1) AS d",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn unqualified_inner_scope_shadows_outer() {
        // Inner subquery has its own t2 in scope; the unqualified `y`
        // inside the IN-subquery resolves to t2 even though t1 is
        // also in the outer scope. Standard SQL inner-shadows-outer.
        // The predicate subquery emits no lineage (it feeds a filter);
        // it still surfaces its refs in reads. The outer `*` is a
        // suppressed wildcard, so there is no lineage at all.
        assert_column_ops(
            "SELECT * FROM t1 WHERE id IN (SELECT id FROM t2 WHERE y > 0)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t2", "id"), read("t2", "y")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn unqualified_correlated_walks_to_outer_when_inner_has_no_candidate() {
        // Inner CTE has Known schema [zz]; `outer_col` doesn't fit it,
        // so resolution walks to the outer scope and picks the t1
        // (Unknown) binding. The predicate subquery emits no lineage;
        // the outer `*` is a suppressed wildcard, so no lineage at all.
        assert_column_ops(
            "SELECT * FROM t1 WHERE id IN (\
            WITH inner_cte AS (SELECT zz FROM t1) \
            SELECT zz FROM inner_cte WHERE outer_col > 0)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t1", "zz"), read("t1", "outer_col")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }
}

mod writes {
    use super::*;

    #[test]
    fn insert_with_explicit_columns_writes_those_columns_on_target() {
        assert_column_ops(
            "INSERT INTO t1 (a, b) VALUES (1, 2)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t1", "a"), write("t1", "b")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_records_target_writes_and_qualified_source_reads() {
        assert_column_ops(
            "INSERT INTO t1 (a) SELECT t2.b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2", "b")],
                writes: vec![write("t1", "a")],
                lineage: vec![passthrough(col("t2", "b"), relation("t1", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_without_explicit_columns_yields_no_writes() {
        // Without an explicit column list AND without a catalog, the
        // resolver can't pair source projections to target columns;
        // writes / lineage stay empty.
        assert_column_ops(
            "INSERT INTO t1 SELECT t2.b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2", "b")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_set_targets_become_writes_on_update_table() {
        assert_column_ops(
            "UPDATE t1 SET a = 1",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![],
                writes: vec![write("t1", "a")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_set_qualified_target_keeps_qualifier() {
        assert_column_ops(
            "UPDATE t1 SET t1.a = 1",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![],
                writes: vec![write("t1", "a")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_set_rhs_qualified_ref_is_a_read() {
        // SET RHS is value-producing (Projection-like); WHERE refs are
        // Filter-tagged.
        assert_column_ops(
            "UPDATE t1 SET a = t2.b FROM t2 WHERE t1.id = t2.id",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t2", "b"), read("t1", "id"), read("t2", "id")],
                writes: vec![write("t1", "a")],
                lineage: vec![passthrough(col("t2", "b"), relation("t1", "a"))],
                diagnostics: vec![],
            },
        );
    }
}

mod delete {
    use super::*;

    #[test]
    fn delete_qualified_predicate_is_a_read() {
        assert_column_ops(
            "DELETE FROM t1 WHERE t1.id = 5",
            ColumnOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }
}

// Columns from every clause (projection / WHERE / GROUP BY /
// ORDER BY / OVER / CASE / HAVING / …) surface in `reads` as plain
// occurrence entries — `reads` no longer tags a syntactic clause.
// These tests pin down WHICH refs surface (occurrence-based, dups
// kept) and the lineage they produce.
mod reads_by_clause {
    use super::*;

    #[test]
    fn same_column_in_projection_and_where_is_two_reads() {
        // The two textual `a` references each get their own `reads`
        // entry (occurrence-based — duplicates are kept).
        assert_column_ops(
            "SELECT a FROM t1 WHERE a > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn predicate_subquery_surfaces_reads_but_no_lineage() {
        // The IN-subquery feeds a filter, so it emits NO lineage
        // (Option B: nested subqueries resolve raw, no intermediate
        // QueryOutput edge). Its refs (s.id, s.flag) still surface
        // in reads. Only the outer projection `a` contributes a lineage edge.
        assert_column_ops(
            "SELECT a FROM t WHERE id IN (SELECT id FROM s WHERE flag = 1)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t", "a"),
                    read("t", "id"),
                    read("s", "id"),
                    read("s", "flag"),
                ],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn scalar_subquery_in_projection_feeds_only_outer() {
        // `SELECT a, (SELECT max(x) FROM s) AS m FROM t`:
        //  - the scalar subquery does NOT emit its own QueryOutput
        //    edge (Option B: raw resolve). Its source `s.x` is
        //    captured by the enclosing projection item, which emits
        //    the single meaningful edge `s.x → out("m", 1)`,
        //    Transformation (the item is a subquery expression).
        //  - `a` is a plain passthrough at position 0.
        assert_column_ops(
            "SELECT a, (SELECT max(x) FROM s) AS m FROM t",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a"), read("s", "x")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t", "a"), out("a", 0)),
                    transformation(col("s", "x"), out("m", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn is_null_predicate_ref_surfaces_as_read() {
        // `WHERE x IS NULL` — x surfaces in reads like any other
        // WHERE ref; it is not a lineage source (predicate-only).
        assert_column_ops(
            "SELECT a FROM t1 WHERE b IS NULL",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn is_not_null_predicate_ref_surfaces_as_read() {
        assert_column_ops(
            "SELECT a FROM t1 WHERE b IS NOT NULL",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_ref_surfaces_as_read() {
        assert_column_ops(
            "SELECT a, COUNT(*) FROM t1 GROUP BY a",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn order_by_ref_surfaces_as_read() {
        assert_column_ops(
            "SELECT a FROM t1 ORDER BY b",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_key_not_projected_is_filter() {
        // `g` is used only as a partition key — it appears in
        // reads but doesn't flow to the output. `x` is the
        // aggregated value (lineage source).
        assert_column_ops(
            "SELECT SUM(x) FROM t1 GROUP BY g",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "g")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn distinct_on_key_is_filter() {
        // DISTINCT ON (k) chooses which row of each duplicate
        // group survives. `k` decides; it doesn't flow as value.
        // (Qualified `t1.k` so it resolves even though the walker
        // visits DISTINCT ON before binding FROM tables.)
        assert_column_ops(
            "SELECT DISTINCT ON (t1.k) a FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "k"), read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn limit_subquery_column_is_filter() {
        // LIMIT (SELECT n FROM cfg) — the subquery's value
        // determines the row count, but `cfg.n` itself doesn't
        // flow to the output.
        assert_column_ops(
            "SELECT a FROM t1 LIMIT (SELECT n FROM cfg)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("cfg", "n")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_and_having_refs_both_surface() {
        // `a` (projection + GROUP BY) and `b` (HAVING) all surface.
        // Walk order: projection → HAVING → GROUP BY (the visitor
        // hits HAVING before GROUP BY), so the read order reflects
        // that, not the textual SQL order.
        assert_column_ops(
            "SELECT a FROM t1 GROUP BY a HAVING SUM(b) > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b"), read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_rollup_modifier_refs_surface() {
        assert_column_ops(
            "SELECT a, b FROM t1 GROUP BY ROLLUP(a, b)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "a"),
                    read("t1", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_cube_modifier_refs_surface() {
        assert_column_ops(
            "SELECT a, b FROM t1 GROUP BY CUBE(a, b)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "a"),
                    read("t1", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_grouping_sets_walks_each_set_member() {
        // GROUPING SETS ((a, b), (a), ()) — every named column
        // inside any set surfaces as a read. The empty set
        // contributes nothing.
        assert_column_ops(
            "SELECT a, b FROM t1 GROUP BY GROUPING SETS ((a, b), (a), ())",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "a"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn group_by_mixed_plain_and_rollup_collects_both() {
        // `GROUP BY a, ROLLUP(b, c)` — `a` is a plain GROUP BY ref;
        // `b`, `c` are inside the ROLLUP expression. All three
        // surface as reads.
        assert_column_ops(
            "SELECT a, b, c FROM t1 GROUP BY a, ROLLUP(b, c)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "c"),
                    read("t1", "a"),
                    read("t1", "b"),
                    read("t1", "c"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t1", "b"), out("b", 1)),
                    passthrough(col("t1", "c"), out("c", 2)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn subquery_in_group_by_surfaces_reads_but_no_inner_lineage() {
        // GROUP BY (SELECT z FROM s) — the subquery's `z` surfaces in
        // reads, but the subquery emits no lineage (Option B: raw
        // resolve, no intermediate QueryOutput). Only the outer
        // projection `a` contributes a lineage edge.
        assert_column_ops(
            "SELECT a FROM t GROUP BY (SELECT z FROM s)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "a"), read("s", "z")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn case_in_projection_refs_surface_and_transform() {
        // All three columns surface in `reads`, but only THEN (`b`)
        // and ELSE (`c`) carry value to the output. The WHEN
        // condition (`a`) is a predicate — it decides which
        // result is selected, its own value doesn't flow.
        assert_column_ops(
            "SELECT CASE WHEN a > 0 THEN b ELSE c END FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b"), read("t1", "c")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "b"), out_anon(0)),
                    transformation(col("t1", "c"), out_anon(0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn case_in_where_refs_surface_as_reads() {
        // The CASE sits in WHERE: its condition (`x`) and results
        // (`y`, `z`) surface as reads (not lineage sources — the CASE
        // feeds a predicate). `b` is the outer projection.
        assert_column_ops(
            "SELECT b FROM t WHERE CASE WHEN x > 0 THEN y ELSE z END = 1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t", "b"),
                    read("t", "x"),
                    read("t", "y"),
                    read("t", "z"),
                ],
                writes: vec![],
                lineage: vec![passthrough(col("t", "b"), out("b", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn scalar_subquery_in_case_condition_contributes_no_lineage() {
        // The scalar subquery lives entirely inside the CASE WHEN
        // cond, which is a predicate. Inner refs (`s.x` from its
        // projection, `s.y` from its WHERE) are tagged
        // `is_lineage_source = false` at capture and dropped from the outer
        // projection's `source_refs`. The output is `1 / NULL`,
        // determined by the subquery's IS NULL test — no inner
        // column's value flows out.
        assert_column_ops(
            "SELECT CASE WHEN (SELECT x FROM s WHERE y > 0) IS NULL THEN 1 END FROM t",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn exists_subquery_inside_projection_case_excludes_inner() {
        // The EXISTS subquery sits in the CASE WHEN cond — a
        // doubly-predicate position. `x.id` is in the EXISTS
        // subquery's WHERE, `s.flag` in the EXISTS subquery's
        // projection — both `is_lineage_source = false`. THEN / ELSE
        // carry values from `s.a` / `s.b`, which flow.
        assert_column_ops(
            "SELECT \
             CASE WHEN EXISTS (SELECT s.flag FROM x WHERE x.id = s.id) THEN s.a ELSE s.b END \
         FROM s",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("s", "flag"),
                    read("x", "id"),
                    read("s", "id"),
                    read("s", "a"),
                    read("s", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    transformation(col("s", "a"), out_anon(0)),
                    transformation(col("s", "b"), out_anon(0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn simple_case_operand_and_results_surface() {
        // `CASE x WHEN 1 THEN a WHEN 2 THEN b END` — `x` is the
        // value being matched (predicate), `1` and `2` are literal
        // patterns, `a` and `b` are the values that flow when the
        // match succeeds. All three columns appear in reads; only
        // `a` and `b` feed lineage.
        assert_column_ops(
            "SELECT CASE x WHEN 1 THEN a WHEN 2 THEN b END FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out_anon(0)),
                    transformation(col("t1", "b"), out_anon(0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn simple_case_with_column_when_pattern_all_surface() {
        // `CASE x WHEN y THEN a ELSE b END` — `x` (operand) and `y`
        // (WHEN-pattern) are both predicate-position: they decide
        // the match, their values don't flow. `a` and `b` are the
        // value-position results.
        assert_column_ops(
            "SELECT CASE x WHEN y THEN a ELSE b END FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "x"),
                    read("t1", "y"),
                    read("t1", "a"),
                    read("t1", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out_anon(0)),
                    transformation(col("t1", "b"), out_anon(0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn window_partition_by_is_filter() {
        // OVER (PARTITION BY p) — `x` is the aggregated value,
        // `p` is a partition key. Both surface in reads, but `p`
        // doesn't flow to output (it decides per-row grouping,
        // not the produced value), so only `x` is a lineage source.
        assert_column_ops(
            "SELECT SUM(x) OVER (PARTITION BY p) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "p")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn window_order_by_is_filter() {
        // OVER (ORDER BY o) — `o` is a sort key for the window
        // frame, not a value source. Same disposition as a top-
        // level ORDER BY column.
        assert_column_ops(
            "SELECT SUM(x) OVER (ORDER BY o) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "o")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn window_partition_and_order_are_filters() {
        // Combined: `p` and `o` both surface in reads; neither
        // contributes value to the output.
        assert_column_ops(
            "SELECT SUM(x) OVER (PARTITION BY p ORDER BY o) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "p"), read("t1", "o")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn window_with_literal_frame_bounds_does_not_add_refs() {
        // Frame bounds with literal integers (`3 PRECEDING`,
        // `CURRENT ROW`) walk via visit_expr but produce no
        // column refs — same shape as the no-frame version.
        // PARTITION BY / ORDER BY columns are filters, so the
        // lineage source list is `x` alone.
        assert_column_ops(
            "SELECT SUM(x) OVER (PARTITION BY p ORDER BY o \
                                 ROWS BETWEEN 3 PRECEDING AND CURRENT ROW) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "p"), read("t1", "o")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn window_with_unbounded_frame_bounds_does_not_add_refs() {
        // UNBOUNDED PRECEDING / UNBOUNDED FOLLOWING are bound
        // variants without an associated expr — visit_window_frame_bound
        // returns Ok without walking anything. ORDER BY `o` is
        // a filter (sort key).
        assert_column_ops(
            "SELECT SUM(x) OVER (ORDER BY o \
                                 ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) \
             FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "o")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_on_clause_refs_surface_as_reads_not_lineage() {
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET t.a = s.a",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![read("t", "id"), read("s", "id"), read("s", "a")],
                writes: vec![write("t", "a")],
                lineage: vec![passthrough(col("s", "a"), relation("t", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn create_table_definitions_are_not_writes() {
        assert_column_ops(
            "CREATE TABLE t1 (a INT, b INT)",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }
}

mod diagnostics {
    use super::*;

    #[test]
    fn unsupported_statement_reports_diagnostic() {
        assert_column_ops(
            "CREATE INDEX idx ON t1 (a)",
            ColumnOperation {
                statement_kind: StatementKind::Unsupported,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::UnsupportedStatement)],
            },
        );
    }

    #[test]
    fn wildcard_in_projection_reports_diagnostic() {
        // Whole-value pin-down on the structural shape; assert_column_ops
        // compares diagnostics by kind only. The message text and span
        // coordinates are verified separately below since this test's
        // *purpose* is to confirm both are populated.
        let ops = extract("SELECT * FROM t1");
        assert_column_ops(
            "SELECT * FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
        // Span info ("at L1:C8") is duplicated in message and surfaced
        // as structured data for programmatic consumers.
        assert!(
            ops.diagnostics[0].message.contains("at L1:C8"),
            "expected span suffix in message, got: {}",
            ops.diagnostics[0].message
        );
        let span = ops.diagnostics[0]
            .span
            .expect("wildcard token carries a span");
        assert_eq!(span.start.line, 1);
        assert_eq!(span.start.column, 8);
    }

    #[test]
    fn qualified_wildcard_in_projection_reports_diagnostic() {
        assert_column_ops(
            "SELECT t1.* FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn multiple_statements_produce_multiple_results() {
        let sql = "SELECT t1.a FROM t1; SELECT t2.b FROM t2";
        assert_nth_column_ops(
            sql,
            0,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
        assert_nth_column_ops(
            sql,
            1,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t2", "b")],
                writes: vec![],
                lineage: vec![passthrough(col("t2", "b"), out("b", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn wildcard_select_yields_no_column_ops() {
        assert_column_ops(
            "SELECT * FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }
}

mod lineage {
    use super::*;

    #[test]
    fn select_bare_column_emits_passthrough_edge_to_query_output() {
        assert_column_ops(
            "SELECT a FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn select_aliased_column_uses_alias_as_output_name() {
        assert_column_ops(
            "SELECT a AS x FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("x", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn select_arithmetic_emits_one_transformation_edge_per_source() {
        assert_column_ops(
            "SELECT a + b FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out_anon(0)),
                    transformation(col("t1", "b"), out_anon(0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn select_mixed_projection_separates_targets_by_position() {
        assert_column_ops(
            "SELECT a, a + b FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    transformation(col("t1", "a"), out_anon(1)),
                    transformation(col("t1", "b"), out_anon(1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn select_qualified_ref_in_expression_resolves_directly() {
        assert_column_ops(
            "SELECT t1.a + t1.b AS sum FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out("sum", 0)),
                    transformation(col("t1", "b"), out("sum", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_pairs_target_cols_positionally() {
        assert_column_ops(
            "INSERT INTO t1 (a, b) SELECT x, y FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2", "x"), read("t2", "y")],
                writes: vec![write("t1", "a"), write("t1", "b")],
                lineage: vec![
                    passthrough(col("t2", "x"), relation("t1", "a")),
                    passthrough(col("t2", "y"), relation("t1", "b")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_transformation_marks_kind_per_source() {
        assert_column_ops(
            "INSERT INTO t1 (a) SELECT x + y FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2", "x"), read("t2", "y")],
                writes: vec![write("t1", "a")],
                lineage: vec![
                    transformation(col("t2", "x"), relation("t1", "a")),
                    transformation(col("t2", "y"), relation("t1", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_union_pairs_both_branches_with_target_cols() {
        // Both UNION branches feed the same INSERT target positions,
        // so each branch's projection should pair `position N → t.col_N`.
        assert_column_ops(
            "INSERT INTO t1 (a, b) \
             SELECT x, y FROM t2 \
             UNION ALL \
             SELECT p, q FROM t3",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![
                    read("t2", "x"),
                    read("t2", "y"),
                    read("t3", "p"),
                    read("t3", "q"),
                ],
                writes: vec![write("t1", "a"), write("t1", "b")],
                lineage: vec![
                    passthrough(col("t2", "x"), relation("t1", "a")),
                    passthrough(col("t2", "y"), relation("t1", "b")),
                    passthrough(col("t3", "p"), relation("t1", "a")),
                    passthrough(col("t3", "q"), relation("t1", "b")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_without_explicit_cols_emits_no_lineage() {
        // Target column names would need catalog-driven positional
        // mapping; without catalog the resolver emits nothing.
        assert_column_ops(
            "INSERT INTO t1 SELECT x FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t2", "x")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_values_with_literals_emits_no_lineage() {
        assert_column_ops(
            "INSERT INTO t1 (a, b) VALUES (1, 2)",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t1", "a"), write("t1", "b")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_set_literal_emits_no_lineage() {
        assert_column_ops(
            "UPDATE t1 SET a = 1",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![],
                writes: vec![write("t1", "a")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn delete_emits_no_lineage() {
        assert_column_ops(
            "DELETE FROM t1 WHERE id = 5",
            ColumnOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn wildcard_select_emits_no_lineage() {
        assert_column_ops(
            "SELECT * FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn update_set_passthrough_lineage() {
        assert_column_ops(
            "UPDATE t1 SET a = b",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t1", "b")],
                writes: vec![write("t1", "a")],
                lineage: vec![passthrough(col("t1", "b"), relation("t1", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_set_transformation_lineage() {
        assert_column_ops(
            "UPDATE t1 SET a = b + 1",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t1", "b")],
                writes: vec![write("t1", "a")],
                lineage: vec![transformation(col("t1", "b"), relation("t1", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn update_set_with_qualified_rhs_resolves_to_other_table() {
        assert_column_ops(
            "UPDATE t1 SET a = t2.b FROM t2 WHERE t1.id = t2.id",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("t2", "b"), read("t1", "id"), read("t2", "id")],
                writes: vec![write("t1", "a")],
                lineage: vec![passthrough(col("t2", "b"), relation("t1", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_call_in_projection_emits_transformation_edge() {
        assert_column_ops(
            "SELECT SUM(a) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "a"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_with_alias_carries_aliased_name() {
        assert_column_ops(
            "SELECT COUNT(b) AS n FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "b")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "b"), out("n", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_wrapped_in_expression_is_transformation() {
        // `SUM(a) + 1` is a value-changing expression, so the lineage edge
        // is Transformation — same kind a bare aggregate call would
        // produce, since the model no longer sub-classifies them.
        assert_column_ops(
            "SELECT SUM(a) + 1 FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "a"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_in_insert_select_propagates_transformation() {
        assert_column_ops(
            "INSERT INTO t2 (n) SELECT COUNT(a) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t1", "a")],
                writes: vec![write("t2", "n")],
                lineage: vec![transformation(col("t1", "a"), relation("t2", "n"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_aggregate_collapses_to_outer_as_transformation() {
        // CTE body's `s` is Transformation (SUM(a)); outer's bare `s`
        // would be Passthrough, but collapse keeps the chain a
        // Transformation (any transforming step dominates).
        assert_column_ops(
            "WITH cte AS (SELECT SUM(a) AS s FROM t1) SELECT s FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "a"), out("s", 0))],
                diagnostics: vec![],
            },
        );
    }
}

mod cte_derived_rename {
    use super::*;

    #[test]
    fn cte_column_rename_collapses_through_renamed_name() {
        // Outer `a` refers to cte's renamed column at position 0,
        // which body-positionally is `x` from t. Composition follows
        // the renamed name back to the body item, then to t.x.
        // Reads surface only the real-table ref (CTE binding is
        // synthetic, dropped).
        assert_column_ops(
            "WITH cte (a) AS (SELECT x FROM t) SELECT a FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "x")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "x"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_column_alias_matched_case_insensitively() {
        // The CTE projects `x AS Foo`; the outer query references it
        // as unquoted `foo`. Composition's name-match folds both
        // sides to the same key, so `foo` collapses back to the real
        // source `t1.x`.
        assert_column_ops(
            "WITH cte AS (SELECT x AS Foo FROM t1) SELECT foo FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "x"), out("foo", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_column_rename_partial_keeps_remaining_body_names() {
        // Rename `(p)` covers position 0 only. Position 1's body name
        // `y` survives; outer can reference `p` or `y`.
        assert_column_ops(
            "WITH cte (p) AS (SELECT x, y FROM t) SELECT p, y FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "x"), read("t", "y")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t", "x"), out("p", 0)),
                    passthrough(col("t", "y"), out("y", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn derived_table_column_rename_collapses() {
        // `(SELECT x FROM t) AS d(a)` — outer `a` resolves via d's
        // renamed column at position 0 → body item x → t.x.
        assert_column_ops(
            "SELECT a FROM (SELECT x FROM t) d(a)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "x")],
                writes: vec![],
                lineage: vec![passthrough(col("t", "x"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_column_rename_into_insert() {
        // `INSERT INTO t2 (col) WITH cte(a) AS (SELECT x FROM t1)
        //  SELECT a FROM cte` collapses through both the CTE rename
        //  and the INSERT pairing: t1.x → t2.col.
        assert_column_ops(
            "INSERT INTO t2 (col) WITH cte (a) AS (SELECT x FROM t1) \
             SELECT a FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t1", "x")],
                writes: vec![write("t2", "col")],
                lineage: vec![passthrough(col("t1", "x"), relation("t2", "col"))],
                diagnostics: vec![],
            },
        );
    }
}

mod with_in_dml {
    //! `WITH cte AS (...) <DML>` — Postgres / Sqlite / standard
    //! SQL syntax for binding CTEs visible to a DML statement.
    //! sqlparser typically parses these as Query-with-WITH at the
    //! source level for INSERT, and wraps Update / Delete in
    //! various ways. These tests pin down what actually surfaces
    //! through the resolver.
    use super::*;

    #[test]
    fn with_in_insert_select_collapses_cte_to_target() {
        assert_column_ops(
            "WITH cte AS (SELECT x FROM s) INSERT INTO t (a) SELECT x FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "x")],
                writes: vec![write("t", "a")],
                lineage: vec![passthrough(col("s", "x"), relation("t", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn with_in_update_via_scalar_subquery_collapses() {
        // CTE referenced from the SET RHS scalar subquery. The
        // subquery emits no QueryOutput edge of its own (Option B);
        // the UPDATE SET assignment captures its source (collapsed
        // through cte to s.x) and emits the single Relation edge.
        // Transformation (the value is derived through max + the
        // subquery wrapping).
        assert_column_ops(
            "WITH cte AS (SELECT max(x) AS m FROM s) \
             UPDATE t SET a = (SELECT m FROM cte) WHERE id = 1",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![read("s", "x"), read("t", "id")],
                writes: vec![write("t", "a")],
                lineage: vec![transformation(col("s", "x"), relation("t", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn with_in_delete_via_predicate_subquery_keeps_cte_source_as_read() {
        // The DELETE target `t` lives in its own scope (the SetExpr
        // DML scope), so the outer predicate `id` resolves
        // unambiguously to `t`. The predicate subquery feeds a
        // filter, so it emits no lineage (Option B); its refs (s.id
        // via the cte) still surface in reads. DELETE has no column
        // lineage of its own — so lineage is empty.
        assert_column_ops(
            "WITH cte AS (SELECT id FROM s WHERE flag) \
             DELETE FROM t WHERE id IN (SELECT id FROM cte)",
            ColumnOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("s", "id"), read("s", "flag"), read("t", "id")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn with_multiple_ctes_chained_into_insert() {
        // Two CTEs where `b` references `a`. INSERT then pulls
        // from `b`. Composition walks back through both layers
        // to the real table.
        assert_column_ops(
            "WITH a AS (SELECT id FROM t1), \
                  b AS (SELECT id + 1 AS x FROM a) \
             INSERT INTO t2 (col) SELECT x FROM b",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t1", "id")],
                writes: vec![write("t2", "col")],
                lineage: vec![transformation(col("t1", "id"), relation("t2", "col"))],
                diagnostics: vec![],
            },
        );
    }
}

mod merge {
    use super::*;

    #[test]
    fn merge_when_matched_update_emits_lineage_and_write() {
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET t.a = s.a",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![read("t", "id"), read("s", "id"), read("s", "a")],
                writes: vec![write("t", "a")],
                lineage: vec![passthrough(col("s", "a"), relation("t", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_when_not_matched_insert_emits_lineage_and_write() {
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a)",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![
                    read("t", "id"),
                    read("s", "id"),
                    read("s", "id"),
                    read("s", "a"),
                ],
                writes: vec![write("t", "id"), write("t", "a")],
                lineage: vec![
                    passthrough(col("s", "id"), relation("t", "id")),
                    passthrough(col("s", "a"), relation("t", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_delete_action_emits_no_lineage_no_write() {
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DELETE",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![read("t", "id"), read("s", "id")],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_combined_clauses_emit_per_clause_lineage_and_writes() {
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET t.a = s.a \
             WHEN NOT MATCHED THEN INSERT (id, a) VALUES (s.id, s.a)",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![
                    read("t", "id"),
                    read("s", "id"),
                    read("s", "a"),
                    read("s", "id"),
                    read("s", "a"),
                ],
                writes: vec![write("t", "a"), write("t", "id"), write("t", "a")],
                lineage: vec![
                    passthrough(col("s", "a"), relation("t", "a")),
                    passthrough(col("s", "id"), relation("t", "id")),
                    passthrough(col("s", "a"), relation("t", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn merge_update_transformation_kind_propagates() {
        assert_column_ops(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN MATCHED THEN UPDATE SET t.a = s.a + 1",
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![read("t", "id"), read("s", "id"), read("s", "a")],
                writes: vec![write("t", "a")],
                lineage: vec![transformation(col("s", "a"), relation("t", "a"))],
                diagnostics: vec![],
            },
        );
    }
}

mod ctas_view {
    use super::*;

    #[test]
    fn ctas_pairs_source_projection_with_inferred_column_names() {
        // CREATE TABLE AS SELECT — no explicit column list, so target
        // columns follow the source projection's inferred names
        // (alias > bare ident).
        assert_column_ops(
            "CREATE TABLE t AS SELECT x AS a, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![write("t", "a"), write("t", "y")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("t", "a")),
                    passthrough(col("s", "y"), relation("t", "y")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ctas_with_explicit_columns_overrides_projection_names() {
        // Explicit column list wins over inferred names.
        assert_column_ops(
            "CREATE TABLE t (p INT, q INT) AS SELECT x, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![write("t", "p"), write("t", "q")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("t", "p")),
                    passthrough(col("s", "y"), relation("t", "q")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ctas_propagates_transformation_kind() {
        assert_column_ops(
            "CREATE TABLE t AS SELECT SUM(x) AS total FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("s", "x")],
                writes: vec![write("t", "total")],
                lineage: vec![transformation(col("s", "x"), relation("t", "total"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn create_view_pairs_source_projection() {
        assert_column_ops(
            "CREATE VIEW v AS SELECT x AS a, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateView,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![write("v", "a"), write("v", "y")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("v", "a")),
                    passthrough(col("s", "y"), relation("v", "y")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn create_view_with_explicit_columns_uses_list() {
        assert_column_ops(
            "CREATE VIEW v (a, b) AS SELECT x, y FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateView,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![write("v", "a"), write("v", "b")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("v", "a")),
                    passthrough(col("s", "y"), relation("v", "b")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_view_pairs_replacement_query_projection() {
        assert_column_ops(
            "ALTER VIEW v AS SELECT x AS a FROM s",
            ColumnOperation {
                statement_kind: StatementKind::AlterView,
                reads: vec![read("s", "x")],
                writes: vec![write("v", "a")],
                lineage: vec![passthrough(col("s", "x"), relation("v", "a"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ctas_unnamed_projection_yields_no_paired_lineage() {
        // `SELECT 1` has no column ref and no inferable name, so the
        // CTAS source produces no lineage / no write for that slot.
        assert_column_ops(
            "CREATE TABLE t AS SELECT 1 FROM s",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_with_distinct_args_marker() {
        // COUNT(DISTINCT user_id) — an aggregate call, so the source
        // feeds into the output as a Transformation.
        assert_column_ops(
            "SELECT COUNT(DISTINCT user_id) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "user_id")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "user_id"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn aggregate_with_filter_clause_marker() {
        // SUM(x) FILTER (WHERE y > 0) — `x` is the aggregated
        // value (lineage source). `y` is the filter predicate; same
        // disposition as a WHERE clause's column — it gates which
        // rows are summed but its value doesn't flow. `y` surfaces
        // in `reads` only.
        assert_column_ops(
            "SELECT SUM(x) FILTER (WHERE y > 0) FROM t1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "x"), read("t1", "y")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "x"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_aggregate_then_outer_expression_still_transformation() {
        // Outer wraps the CTE column in an expression (s + 1) —
        // collapse: outer Transformation × inner Transformation =
        // Transformation.
        assert_column_ops(
            "WITH cte AS (SELECT SUM(a) AS s FROM t1) SELECT s + 1 FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![transformation(col("t1", "a"), out_anon(0))],
                diagnostics: vec![],
            },
        );
    }
}

mod collapse {
    use super::*;

    #[test]
    fn cte_passthrough_collapses_to_real_table() {
        // The outer edge's source `id` resolves to cte, then collapses
        // through the CTE body's projection back to t1.id. No
        // intermediate cte.id → out edge survives.
        assert_column_ops(
            "WITH cte AS (SELECT id FROM t1) SELECT id FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_transformation_propagates_kind_after_collapse() {
        // CTE body's `sum` is a transformation of a, b. Outer's bare
        // `sum` collapses back into two edges, each Transformation
        // because the body item is (outer.bare && item.bare = false).
        assert_column_ops(
            "WITH cte AS (SELECT a + b AS sum FROM t1) SELECT sum FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out("sum", 0)),
                    transformation(col("t1", "b"), out("sum", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_to_insert_collapses_end_to_end() {
        // Composition reaches past the CTE boundary into the INSERT
        // target — t1.id → t2.x directly, no cte.id step.
        assert_column_ops(
            "INSERT INTO t2 (x) WITH cte AS (SELECT id FROM t1) SELECT id FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t1", "id")],
                writes: vec![write("t2", "x")],
                lineage: vec![passthrough(col("t1", "id"), relation("t2", "x"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_chain_collapses_through_all_levels() {
        // a → b → outer: outer's `b.id` collapses via b's body back to
        // a, then via a's body back to t1. Outer is qualified because
        // having both `a` and `b` in scope with the same column name
        // makes the unqualified form ambiguous under our scope model
        // (outer SELECT sees both CTE bindings, not just b).
        assert_column_ops(
            "WITH a AS (SELECT id FROM t1), b AS (SELECT id FROM a) SELECT b.id FROM b",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn derived_table_collapses_to_real_table() {
        // The outer projection's `col` collapses through derived `d`'s
        // body (a + b AS col) into two Transformation edges on t1.
        assert_column_ops(
            "SELECT col FROM (SELECT a + b AS col FROM t1) d",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t1", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out("col", 0)),
                    transformation(col("t1", "b"), out("col", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn cte_referenced_twice_collapses_each_use() {
        // Each cte reference in the projection collapses independently
        // back to t1.id.
        assert_column_ops(
            "WITH cte AS (SELECT id FROM t1) SELECT cte.id AS a, cte.id AS b FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "id"), out("a", 0)),
                    passthrough(col("t1", "id"), out("b", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn recursive_cte_does_not_panic_and_skips_collapse() {
        // Recursive CTEs have output_columns = None (fixpoint is
        // deferred), so collapse falls back to leaving the lineage edge
        // source pointing at the CTE binding (`r.id`) rather than
        // tracing into a real table. Reads still get the synthetic
        // filter, so only `t1.id` from the non-recursive branch
        // surfaces in reads. No infinite recursion either.
        assert_column_ops(
            "WITH RECURSIVE r AS (SELECT id FROM t1 UNION SELECT id FROM r) SELECT id FROM r",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![passthrough(read("r", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }
}

mod set_operations {
    use super::*;

    #[test]
    fn union_two_branches_emit_query_output_per_branch() {
        // Each branch contributes its own output-column list, so both
        // branches' projections fan out independently into
        // QueryOutput edges. Position is per-group, so both land at
        // position 0; name follows each branch's own projection.
        assert_column_ops(
            "SELECT a FROM t1 UNION SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_all_behaves_same_as_union() {
        // UNION ALL only differs from UNION at runtime (dedup vs
        // not); structurally the resolver should treat them identically.
        assert_column_ops(
            "SELECT a FROM t1 UNION ALL SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn intersect_behaves_same_as_union() {
        assert_column_ops(
            "SELECT a FROM t1 INTERSECT SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn except_behaves_same_as_union() {
        assert_column_ops(
            "SELECT a FROM t1 EXCEPT SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn three_way_union_emits_one_lineage_edge_per_branch() {
        // Chained UNION parses left-associatively as
        // `(t1 UNION t2) UNION t3`, so the resolver recursively
        // visits each base SELECT and each contributes its own group.
        assert_column_ops(
            "SELECT a FROM t1 UNION SELECT b FROM t2 UNION SELECT c FROM t3",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b"), read("t3", "c")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                    passthrough(col("t3", "c"), out("c", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_with_where_classifies_per_branch_kind() {
        // Each branch's WHERE is its own filter scope, so each
        // branch produces a Projection read plus a Filter read for
        // its own column.
        assert_column_ops(
            "SELECT a FROM t1 WHERE a > 0 UNION SELECT b FROM t2 WHERE b < 10",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read("t1", "a"),
                    read("t1", "a"),
                    read("t2", "b"),
                    read("t2", "b"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_mixed_passthrough_and_transformation_kinds() {
        // Branch lineage kinds are independent. Left passthrough, right
        // transformation; both contribute to the same output position.
        assert_column_ops(
            "SELECT a FROM t1 UNION SELECT b + 1 AS a FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    transformation(col("t2", "b"), out("a", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_with_aggregate_branch_emits_transformation_edge() {
        assert_column_ops(
            "SELECT id FROM t1 UNION SELECT COUNT(id) AS id FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t2", "id")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "id"), out("id", 0)),
                    transformation(col("t2", "id"), out("id", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_in_subquery_collapses_both_branches_to_outer() {
        // The inner UNION lives in a derived subquery; the outer
        // SELECT projects from it and collapses back to the base
        // tables of both branches — no intermediate QueryOutput
        // edge for the subquery survives.
        assert_column_ops(
            "SELECT x FROM (SELECT a AS x FROM t1 UNION SELECT b AS x FROM t2) sub",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("x", 0)),
                    passthrough(col("t2", "b"), out("x", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_in_cte_collapses_to_outer_use() {
        // CTE body is a UNION. Outer SELECT pulls `x` from the cte.
        // Composition should walk back through both branches to t1/t2.
        assert_column_ops(
            "WITH cte AS (SELECT a AS x FROM t1 UNION SELECT b AS x FROM t2) \
             SELECT x FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("x", 0)),
                    passthrough(col("t2", "b"), out("x", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ctas_with_union_body_pairs_left_branch_names_for_all_branches() {
        // CTAS schema follows the LEFT branch's projection names
        // (SQL standard). The inferred-name path uses the first
        // the branch's column names for every branch's
        // positional pairing — same as INSERT-SELECT-UNION. So:
        //   - writes: only `dst.a` (left branch's name)
        //   - lineage: BOTH branches feed `Relation(dst.a)`
        assert_column_ops(
            "CREATE TABLE dst AS SELECT a FROM t1 UNION SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![write("dst", "a")],
                lineage: vec![
                    passthrough(col("t1", "a"), relation("dst", "a")),
                    passthrough(col("t2", "b"), relation("dst", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn ctas_with_explicit_columns_and_union_body_pairs_left_target_for_all_branches() {
        // When CTAS specifies its own column list, both branches
        // pair positionally against the same target columns — same
        // pattern as INSERT-SELECT-UNION.
        assert_column_ops(
            "CREATE TABLE dst (x INT) AS SELECT a FROM t1 UNION SELECT b FROM t2",
            ColumnOperation {
                statement_kind: StatementKind::CreateTable,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![write("dst", "x")],
                lineage: vec![
                    passthrough(col("t1", "a"), relation("dst", "x")),
                    passthrough(col("t2", "b"), relation("dst", "x")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_with_trailing_order_by_ref_is_unresolved() {
        // ORDER BY on the whole UNION is visited in the outer query
        // scope, AFTER both branch scopes have been popped. The
        // ORDER BY column refers to a UNION output column, not a
        // real table — so `a` resolves to None (no in-scope
        // binding).
        assert_column_ops(
            "SELECT a FROM t1 UNION SELECT b FROM t2 ORDER BY a",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b"), unresolved("a")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn union_with_trailing_limit_literal_adds_nothing() {
        // LIMIT 10 is a literal — no column refs, no extra lineage.
        assert_column_ops(
            "SELECT a FROM t1 UNION SELECT b FROM t2 LIMIT 10",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t1", "a"), out("a", 0)),
                    passthrough(col("t2", "b"), out("b", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }
}

mod join_using_and_natural {
    //! USING / NATURAL JOIN merge expansion is documented as
    //! future work (see the module-level note in
    //! column_operation_extractor). These tests pin down the
    //! *current* shape so when USING / NATURAL JOIN expansion lands
    //! (merged refs splitting into both source tables), the diff
    //! will surface here.
    use super::*;

    #[test]
    fn join_using_id_in_projection_is_unresolved_due_to_ambiguity() {
        // `id` in the projection is unqualified with two candidate
        // tables (t1, t2) — the resolver leaves it unresolved
        // (`table: None`) because no catalog disambiguates and
        // USING is not yet expanded into a merged-column binding.
        assert_column_ops(
            "SELECT id FROM t1 JOIN t2 USING (id)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![ambiguous("id")],
                writes: vec![],
                lineage: vec![passthrough(ambiguous("id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_using_id_in_projection_and_where_yields_two_independent_unresolved_refs() {
        // The same `id` ref in projection vs. WHERE produces two
        // SEPARATE CapturedColumnRefs, each with a single-kind `kinds`
        // vec. There is no merge into one ref-with-multi-kinds
        // here — that would require resolver-level tracking of
        // ref identity across clauses, which we don't do.
        assert_column_ops(
            "SELECT id FROM t1 JOIN t2 USING (id) WHERE id > 0",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![ambiguous("id"), ambiguous("id")],
                writes: vec![],
                lineage: vec![passthrough(ambiguous("id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn join_using_qualified_id_resolves_to_named_table() {
        // Qualifying the ref sidesteps the USING ambiguity: `t1.id`
        // resolves to t1 unambiguously. Use this in real-world
        // queries until USING expansion is available.
        assert_column_ops(
            "SELECT t1.id FROM t1 JOIN t2 USING (id)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn natural_join_no_catalog_leaves_unqualified_refs_unresolved() {
        // NATURAL JOIN's merge set comes from the intersection of
        // both tables' column lists — only knowable with a
        // catalog. Without one, the resolver doesn't expand, and
        // unqualified `id` is multi-candidate-unresolved (same
        // shape as plain JOIN ON without USING).
        assert_column_ops(
            "SELECT id FROM t1 NATURAL JOIN t2",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![ambiguous("id")],
                writes: vec![],
                lineage: vec![passthrough(ambiguous("id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }
}

mod lateral_and_correlation {
    use super::*;

    #[test]
    fn lateral_subquery_resolves_inner_ref_to_inner_table() {
        // The existing-style LATERAL: the inner subquery only
        // references its own tables. The outer FROM joins it as
        // a derived source. The inner `id` resolves to t1 from
        // the LATERAL subquery's own scope.
        assert_column_ops(
            "SELECT d.id FROM LATERAL (SELECT id FROM t1) AS d JOIN t2 ON d.id = t2.id",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t2", "id")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn lateral_with_outer_scope_reference_resolves_via_scope_chain() {
        // The interesting LATERAL case: the inner subquery references
        // `t1.x` from the OUTER FROM. Without LATERAL this is invalid
        // SQL, but the resolver doesn't enforce LATERAL semantics —
        // it walks the scope chain regardless.
        assert_column_ops(
            "SELECT sub.x FROM t1, LATERAL (SELECT t1.a + t2.b AS x FROM t2) sub",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out("x", 0)),
                    transformation(col("t2", "b"), out("x", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn non_lateral_derived_also_resolves_outer_ref_permissively() {
        // The resolver doesn't distinguish LATERAL from non-LATERAL
        // — both walk the scope chain identically. This is more
        // lenient than strict SQL semantics (where this should be
        // an error), but reasonable for lineage purposes: a
        // best-effort resolution is more useful than silently
        // dropping the reference.
        assert_column_ops(
            "SELECT sub.x FROM t1, (SELECT t1.a + t2.b AS x FROM t2) sub",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "b")],
                writes: vec![],
                lineage: vec![
                    transformation(col("t1", "a"), out("x", 0)),
                    transformation(col("t2", "b"), out("x", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn correlated_where_subquery_resolves_outer_ref() {
        // Classic correlated subquery in WHERE: the inner SELECT
        // references the outer t1.id. The resolver walks the
        // scope chain to find t1.id in the outer scope.
        assert_column_ops(
            "SELECT a FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.fk = t1.id)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a"), read("t2", "fk"), read("t1", "id")],
                writes: vec![],
                lineage: vec![passthrough(col("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }
}

mod on_conflict {
    //! ON CONFLICT (Postgres / Sqlite) and ON DUPLICATE KEY UPDATE
    //! (MySQL) both sit in `Insert.on: Option<OnInsert>`. The
    //! resolver walks both, with subtle differences:
    //!
    //! - Postgres: `EXCLUDED.<col>` is a pseudo-table for the
    //!   would-be-inserted row. Bound as synthetic so refs
    //!   through it filter out of `reads` but still emit valid
    //!   Relation lineage edges into the target. The synthetic
    //!   binding's columns mirror the INSERT target's columns.
    //! - MySQL: `VALUES(<col>)` is a function-call form for the
    //!   same concept. No EXCLUDED binding (it would make
    //!   unqualified refs ambiguous against the INSERT target);
    //!   the inner ref resolves to the INSERT target like a
    //!   regular self-reference.
    //!
    //! DO UPDATE SET targets become writes on the INSERT target
    //! table — same role as a standalone UPDATE SET. The optional
    //! DO UPDATE WHERE clause walks in filter context.
    use super::*;
    use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};

    fn assert_column_ops_with_dialect(
        sql: &str,
        dialect: &dyn sqlparser::dialect::Dialect,
        expected: ColumnOperation,
    ) {
        let actual = extract_column_operations(dialect, sql, None)
            .unwrap()
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("no statements in result for SQL: {sql}"))
            .unwrap();
        assert_column_ops_inner(sql, 0, actual, expected);
    }

    /// Construct a `ColumnRead` for the synthetic EXCLUDED
    /// pseudo-table — used only as a Source in lineage edges, not
    /// as a real table. The EXCLUDED binding inherits its
    /// `output_columns` from the INSERT source's per-operand
    /// projections; for VALUES sources (no projection captured) the
    /// binding ends up with `output_columns: None`, so refs against
    /// it can't be `Confirmed` and surface as `Inferred`.
    fn excluded(name: &str) -> ColumnRead {
        ColumnRead {
            reference: ColumnReference {
                table: Some(TableReference {
                    catalog: None,
                    schema: None,
                    name: "EXCLUDED".into(),
                }),
                name: name.into(),
            },
            confidence: Confidence::Inferred,
        }
    }

    #[test]
    fn pg_on_conflict_do_update_set_excluded_emits_lineage_and_write() {
        // DO UPDATE SET b = EXCLUDED.b
        //   - writes: t.a, t.b from INSERT columns plus another
        //     t.b for the SET target.
        //   - reads: empty (EXCLUDED is synthetic-filtered;
        //     VALUES (1, 2) are literals).
        //   - lineage: EXCLUDED.b → Relation(t.b), Passthrough.
        assert_column_ops_with_dialect(
            "INSERT INTO t (a, b) VALUES (1, 2) ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                lineage: vec![passthrough(excluded("b"), relation("t", "b"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn pg_on_conflict_do_nothing_is_indistinguishable_from_plain_insert() {
        assert_column_ops_with_dialect(
            "INSERT INTO t (a, b) VALUES (1, 2) ON CONFLICT (a) DO NOTHING",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t", "a"), write("t", "b")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn pg_insert_select_with_on_conflict_collapses_excluded_to_source() {
        // EXCLUDED's output_columns come from the INSERT source
        // renamed to the target columns positionally. So
        // `EXCLUDED.b` collapses through to the source's position-1
        // projection (`y` from s) — the conflict-action lineage edge
        // bottoms out at the same real table as the
        // source-projection lineage edge.
        assert_column_ops_with_dialect(
            "INSERT INTO t (a, b) SELECT x, y FROM s \
             ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "x"), read("s", "y")],
                writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("t", "a")),
                    passthrough(col("s", "y"), relation("t", "b")),
                    passthrough(col("s", "y"), relation("t", "b")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn mysql_on_duplicate_key_update_values_func_self_references_target() {
        // MySQL `VALUES(<col>)` is the implicit-row form. Without
        // an EXCLUDED binding, the inner `b` ref resolves to t.b
        // (the INSERT target). Result: t.b shows up as a read
        // (the VALUES function call is a value-changing wrapper) and
        // the SET clause adds a Relation-target lineage edge t.b → t.b.
        assert_column_ops_with_dialect(
            "INSERT INTO t (a, b) VALUES (1, 2) \
             ON DUPLICATE KEY UPDATE b = VALUES(b)",
            &MySqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t", "b")],
                writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                lineage: vec![transformation(col("t", "b"), relation("t", "b"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn pg_insert_union_with_on_conflict_excluded_fans_out_to_each_branch() {
        // The source has TWO branches (one per UNION
        // branch), so EXCLUDED's output_columns also have two
        // groups — each with a position-0 item named after the
        // INSERT target column. `EXCLUDED.a` then collapses to
        // BOTH branches' position-0 source refs.
        assert_column_ops_with_dialect(
            "INSERT INTO t (a) SELECT x FROM s1 UNION SELECT y FROM s2 \
             ON CONFLICT (a) DO UPDATE SET a = EXCLUDED.a",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s1", "x"), read("s2", "y")],
                writes: vec![write("t", "a"), write("t", "a")],
                lineage: vec![
                    passthrough(col("s1", "x"), relation("t", "a")),
                    passthrough(col("s2", "y"), relation("t", "a")),
                    passthrough(col("s1", "x"), relation("t", "a")),
                    passthrough(col("s2", "y"), relation("t", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn pg_insert_aggregate_with_on_conflict_excluded_keeps_transformation_kind() {
        // SUM(x) makes the source projection a Transformation. When
        // EXCLUDED.total collapses back, collapse_lineage_kinds keeps the
        // transforming step → lineage kind stays Transformation even on
        // the conflict-action path.
        assert_column_ops_with_dialect(
            "INSERT INTO t (total) SELECT SUM(x) FROM s \
             ON CONFLICT (id) DO UPDATE SET total = EXCLUDED.total",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "x")],
                writes: vec![write("t", "total"), write("t", "total")],
                lineage: vec![
                    transformation(col("s", "x"), relation("t", "total")),
                    transformation(col("s", "x"), relation("t", "total")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn pg_on_conflict_do_update_with_where_clause_emits_read() {
        // DO UPDATE ... WHERE walks in filter context: `t.a` in the
        // WHERE expression surfaces as a read but not a lineage source.
        assert_column_ops_with_dialect(
            "INSERT INTO t (a, b) VALUES (1, 2) \
             ON CONFLICT (a) DO UPDATE SET b = EXCLUDED.b WHERE t.a > 0",
            &PostgreSqlDialect {},
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t", "a")],
                writes: vec![write("t", "a"), write("t", "b"), write("t", "b")],
                lineage: vec![passthrough(excluded("b"), relation("t", "b"))],
                diagnostics: vec![],
            },
        );
    }
}

mod values_as_relation {
    //! `VALUES` can stand in for a row-source in three positions:
    //! - INSERT … VALUES (already covered in `lineage` / `on_conflict`)
    //! - SELECT … FROM (VALUES …) AS t(x, y)   — derived table
    //! - WITH cte(x, y) AS (VALUES …) SELECT … — CTE body
    //!
    //! VALUES doesn't carry projection items the resolver can
    //! capture (literals have no source refs), so lineage from these
    //! variants bottom out at the synthetic binding — no
    //! collapse to a real table is possible.
    use super::*;

    #[test]
    fn values_as_derived_table_with_aliases_emits_synthetic_refs_only() {
        // The derived table `t` carries schema [x, y] from the
        // alias rename, but its output_columns is None (VALUES
        // contributes no OutputColumns). So `t.x` is recorded as
        // a synthetic ref pointing at the derived binding; reads
        // filter it out, and lineage keeps `t.x` as the source
        // (collapse can't collapse further).
        assert_column_ops(
            "SELECT x, y FROM (VALUES (1, 'a'), (2, 'b')) AS t(x, y)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![
                    passthrough(read("t", "x"), out("x", 0)),
                    passthrough(read("t", "y"), out("y", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn values_as_cte_body_with_aliases_emits_synthetic_refs_only() {
        assert_column_ops(
            "WITH cte(id, val) AS (VALUES (1, 'a'), (2, 'b')) SELECT id FROM cte",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![],
                writes: vec![],
                lineage: vec![passthrough(read("cte", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn values_with_column_ref_in_row_picks_up_outer_ref() {
        // A column ref inside a VALUES row (rare in practice but
        // syntactically valid) does get walked and surfaces in
        // reads — the outer table `t1` is in scope of the derived
        // table per the resolver's permissive scope-chain rule.
        assert_column_ops(
            "SELECT v.x FROM t1, (VALUES (t1.a)) AS v(x)",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(read("v", "x"), out("x", 0))],
                diagnostics: vec![],
            },
        );
    }
}

mod alter_table {
    //! ALTER TABLE produces column-level writes for column-naming
    //! operations: ADD COLUMN, DROP COLUMN, RENAME COLUMN, CHANGE
    //! COLUMN, MODIFY COLUMN, ALTER COLUMN. RENAME / CHANGE surface
    //! BOTH the old and new names — both ends of the rename are
    //! useful for downstream lineage consumers tracking column
    //! history. Schema-level operations (constraints, partitions,
    //! RENAME TABLE) contribute no column writes.
    use super::*;

    #[test]
    fn alter_table_add_column_emits_write() {
        assert_column_ops(
            "ALTER TABLE t ADD COLUMN c INT",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![write("t", "c")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_table_drop_column_emits_write() {
        assert_column_ops(
            "ALTER TABLE t DROP COLUMN c",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![write("t", "c")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_table_rename_column_emits_both_old_and_new() {
        // RENAME moves data from old to new; surface both for
        // downstream consumers tracking column history.
        assert_column_ops(
            "ALTER TABLE t RENAME COLUMN a TO b",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![write("t", "a"), write("t", "b")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_table_alter_column_emits_write_for_target_column() {
        assert_column_ops(
            "ALTER TABLE t ALTER COLUMN a SET NOT NULL",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![write("t", "a")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_table_multiple_ops_collects_all_target_columns() {
        // sqlparser parses multi-op ALTER as a single statement
        // with `operations: Vec<AlterTableOperation>`.
        assert_column_ops(
            "ALTER TABLE t ADD COLUMN c INT, DROP COLUMN d",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![write("t", "c"), write("t", "d")],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn alter_table_add_constraint_emits_no_column_writes() {
        // AddConstraint is schema-level — no column-level writes
        // surface (the table itself stays in table_op writes).
        assert_column_ops(
            "ALTER TABLE t ADD CONSTRAINT uq UNIQUE (a)",
            ColumnOperation {
                statement_kind: StatementKind::AlterTable,
                reads: vec![],
                writes: vec![],
                lineage: vec![],
                diagnostics: vec![],
            },
        );
    }
}

mod returning {
    //! `RETURNING <select_items>` on INSERT / UPDATE / DELETE
    //! (Postgres / Sqlite extension) projects from the affected
    //! rows of the target table — treated like a top-level SELECT
    //! projection: each item contributes refs to `reads` and a
    //! `QueryOutput` lineage edge. Walked BEFORE the ON-clause for
    //! INSERT so any EXCLUDED binding doesn't ambify unqualified
    //! refs that collide with INSERT column names.
    use super::*;

    #[test]
    fn insert_values_with_returning_emits_target_reads_and_query_output() {
        assert_column_ops(
            "INSERT INTO t (a, b) VALUES (1, 2) RETURNING id",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t", "id")],
                writes: vec![write("t", "a"), write("t", "b")],
                lineage: vec![passthrough(col("t", "id"), out("id", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn returning_aliased_uses_alias_as_output_name() {
        assert_column_ops(
            "INSERT INTO t (a) VALUES (1) RETURNING id AS pk",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t", "id")],
                writes: vec![write("t", "a")],
                lineage: vec![passthrough(col("t", "id"), out("pk", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn returning_with_expression_marks_kind_transformation() {
        assert_column_ops(
            "INSERT INTO t (a) VALUES (1) RETURNING id + 1 AS bumped",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("t", "id")],
                writes: vec![write("t", "a")],
                lineage: vec![transformation(col("t", "id"), out("bumped", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn returning_wildcard_records_wildcard_suppressed_diagnostic() {
        assert_column_ops(
            "INSERT INTO t (a) VALUES (1) RETURNING *",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![],
                writes: vec![write("t", "a")],
                lineage: vec![],
                diagnostics: vec![diag(ColumnLevelDiagnosticKind::WildcardSuppressed)],
            },
        );
    }

    #[test]
    fn update_returning_walks_target_columns() {
        assert_column_ops(
            "UPDATE t SET a = b + 1 WHERE id = 5 RETURNING id, a",
            ColumnOperation {
                statement_kind: StatementKind::Update,
                reads: vec![
                    read("t", "b"),
                    read("t", "id"),
                    read("t", "id"),
                    read("t", "a"),
                ],
                writes: vec![write("t", "a")],
                lineage: vec![
                    transformation(col("t", "b"), relation("t", "a")),
                    passthrough(col("t", "id"), out("id", 0)),
                    passthrough(col("t", "a"), out("a", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn delete_returning_walks_target_columns() {
        assert_column_ops(
            "DELETE FROM t WHERE id = 5 RETURNING id, val",
            ColumnOperation {
                statement_kind: StatementKind::Delete,
                reads: vec![read("t", "id"), read("t", "id"), read("t", "val")],
                writes: vec![],
                lineage: vec![
                    passthrough(col("t", "id"), out("id", 0)),
                    passthrough(col("t", "val"), out("val", 1)),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn insert_select_with_returning_keeps_source_lineage_and_target_returning() {
        // Source SELECT's tables are out of scope by the time
        // RETURNING walks (their nested scope was popped after
        // resolve_query). So RETURNING refs resolve to the target
        // table alone, even when the bare name `id` exists in the
        // source too.
        assert_column_ops(
            "INSERT INTO t (a) SELECT x FROM s RETURNING id",
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "x"), read("t", "id")],
                writes: vec![write("t", "a")],
                lineage: vec![
                    passthrough(col("s", "x"), relation("t", "a")),
                    passthrough(col("t", "id"), out("id", 0)),
                ],
                diagnostics: vec![],
            },
        );
    }
}

mod catalog_strict {
    use super::*;
    use crate::catalog::{Catalog, ColumnSchema};
    use sqlparser::ast::Ident;
    use std::collections::HashMap;

    #[derive(Debug, Default)]
    struct TestCatalog {
        tables: HashMap<String, Vec<&'static str>>,
    }

    impl TestCatalog {
        fn with(mut self, name: &str, cols: Vec<&'static str>) -> Self {
            self.tables.insert(name.to_string(), cols);
            self
        }
    }

    impl Catalog for TestCatalog {
        fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>> {
            self.tables.get(table.name.value.as_str()).map(|cols| {
                cols.iter()
                    .map(|c| ColumnSchema {
                        name: c.to_string(),
                    })
                    .collect()
            })
        }
    }

    fn assert_column_ops_with_catalog(sql: &str, catalog: &dyn Catalog, expected: ColumnOperation) {
        let actual = extract_column_operations(&GenericDialect {}, sql, Some(catalog))
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .unwrap();
        assert_column_ops_inner(sql, 0, actual, expected);
    }

    #[test]
    fn catalog_known_schema_rejects_columns_not_in_table() {
        // Without catalog `SELECT a FROM t1` resolves a → t1.a
        // unconditionally (single Unknown binding heuristic). With a
        // catalog that says t1's columns are [x, y], `a` cannot come
        // from t1 — it surfaces as unresolved (table=None, confidence=
        // Unresolved on the read).
        let catalog = TestCatalog::default().with("t1", vec!["x", "y"]);
        assert_column_ops_with_catalog(
            "SELECT a FROM t1",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![unresolved("a")],
                writes: vec![],
                lineage: vec![passthrough(unresolved("a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_known_schema_resolves_columns_present_in_table() {
        let catalog = TestCatalog::default().with("t1", vec!["a", "b"]);
        assert_column_ops_with_catalog(
            "SELECT a FROM t1",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_confirmed("t1", "a")],
                writes: vec![],
                lineage: vec![passthrough(col_confirmed("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_resolves_unquoted_ref_case_insensitively() {
        // The catalog declares `id` (lowercase); an unquoted `ID`
        // folds to the same key, so it resolves to t1. The column
        // name surfaces as written (`ID`) — folding governs matching,
        // not the surfaced identity.
        let catalog = TestCatalog::default().with("t1", vec!["id"]);
        assert_column_ops_with_catalog(
            "SELECT ID FROM t1",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_confirmed("t1", "ID")],
                writes: vec![],
                lineage: vec![passthrough(col_confirmed("t1", "ID"), out("ID", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_does_not_match_quoted_ref_against_unquoted_column() {
        // A quoted `"ID"` matches exactly (case-sensitive), so it does
        // not match the catalog's `id`; it stays unresolved (table=None,
        // Confidence::Unresolved). Placed in WHERE so it is a read but
        // not a lineage source.
        let catalog = TestCatalog::default().with("t1", vec!["a", "id"]);
        let unresolved_quoted_id = ColumnRead {
            reference: ColumnReference {
                table: None,
                name: Ident::with_quote('"', "ID"),
            },
            confidence: Confidence::Unresolved,
        };
        assert_column_ops_with_catalog(
            r#"SELECT a FROM t1 WHERE "ID" > 0"#,
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read_confirmed("t1", "a"), unresolved_quoted_id],
                writes: vec![],
                lineage: vec![passthrough(col_confirmed("t1", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_insert_without_explicit_columns_pairs_via_catalog_schema() {
        // INSERT INTO t SELECT a, b FROM s — no explicit column
        // list. With t = [x, y, z] in catalog, the resolver pairs
        // source projections positionally (s.a → t.x, s.b → t.y).
        // Unpaired catalog cols (z) get no lineage / no write.
        let catalog = TestCatalog::default().with("t", vec!["x", "y", "z"]);
        assert_column_ops_with_catalog(
            "INSERT INTO t SELECT a, b FROM s",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "a"), read("s", "b")],
                writes: vec![write("t", "x"), write("t", "y")],
                lineage: vec![
                    passthrough(col("s", "a"), relation("t", "x")),
                    passthrough(col("s", "b"), relation("t", "y")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_insert_without_explicit_columns_source_longer_than_target() {
        // 3 source projections vs t = [x, y] — pair what fits,
        // surplus source column gets no lineage.
        let catalog = TestCatalog::default().with("t", vec!["x", "y"]);
        assert_column_ops_with_catalog(
            "INSERT INTO t SELECT a, b, c FROM s",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "a"), read("s", "b"), read("s", "c")],
                writes: vec![write("t", "x"), write("t", "y")],
                lineage: vec![
                    passthrough(col("s", "a"), relation("t", "x")),
                    passthrough(col("s", "b"), relation("t", "y")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_insert_explicit_columns_override_catalog_schema() {
        // Explicit (q) wins over catalog [x, y, z].
        let catalog = TestCatalog::default().with("t", vec!["x", "y", "z"]);
        assert_column_ops_with_catalog(
            "INSERT INTO t (q) SELECT a FROM s",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Insert,
                reads: vec![read("s", "a")],
                writes: vec![write("t", "q")],
                lineage: vec![passthrough(col("s", "a"), relation("t", "q"))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_merge_not_matched_insert_no_cols_pairs_via_catalog() {
        // Same catalog fallback applies to MERGE's INSERT clause:
        // lineage is paired via catalog. Surprise surfaced by whole-
        // value compare: writes stay empty for catalog-paired MERGE
        // INSERT — only `INSERT (cols) VALUES (...)` with an
        // explicit column list populates writes.
        let catalog = TestCatalog::default().with("t", vec!["id", "a"]);
        assert_column_ops_with_catalog(
            "MERGE INTO t USING s ON t.id = s.id \
             WHEN NOT MATCHED THEN INSERT VALUES (s.id, s.a)",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Merge,
                reads: vec![
                    read_confirmed("t", "id"),
                    read("s", "id"),
                    read("s", "id"),
                    read("s", "a"),
                ],
                writes: vec![],
                lineage: vec![
                    passthrough(col("s", "id"), relation("t", "id")),
                    passthrough(col("s", "a"), relation("t", "a")),
                ],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_disambiguates_join_unqualified_ref() {
        // Both tables are Known via catalog; only t2 has `a`, so
        // unqualified `a` in `t1 JOIN t2` resolves to t2 (no
        // catalog: same SQL would be ambiguous).
        let catalog = TestCatalog::default()
            .with("t1", vec!["id"])
            .with("t2", vec!["id", "a"]);
        assert_column_ops_with_catalog(
            "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read_confirmed("t1", "id"),
                    read_confirmed("t2", "id"),
                    read_confirmed("t2", "a"),
                ],
                writes: vec![],
                lineage: vec![passthrough(col_confirmed("t2", "a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_confirmed_ambiguity_surfaces_as_ambiguous_read() {
        // Both tables Known and both declare `a`. The unqualified `a`
        // can't be resolved to a single owner; the read surfaces with
        // table=None and Confidence::Ambiguous. (Without catalog the
        // same query also yields Ambiguous: two Unknown suspects with
        // no Known tiebreaker.)
        let catalog = TestCatalog::default()
            .with("t1", vec!["a"])
            .with("t2", vec!["a"]);
        assert_column_ops_with_catalog(
            "SELECT a FROM t1 JOIN t2 ON t1.a = t2.a",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![
                    read_confirmed("t1", "a"),
                    read_confirmed("t2", "a"),
                    ambiguous("a"),
                ],
                writes: vec![],
                lineage: vec![passthrough(ambiguous("a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn catalog_unresolved_unqualified_surfaces_as_unresolved_read() {
        // Catalog says t1 has [x, y]; unqualified `z` belongs to
        // nothing in scope — the read surfaces with table=None and
        // Confidence::Unresolved.
        let catalog = TestCatalog::default().with("t1", vec!["x", "y"]);
        assert_column_ops_with_catalog(
            "SELECT z FROM t1",
            &catalog,
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![unresolved("z")],
                writes: vec![],
                lineage: vec![passthrough(unresolved("z"), out("z", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn no_catalog_unqualified_is_ambiguous_under_unknown_suspects() {
        // No catalog → all real-table schemas are Unknown. For the
        // unqualified `a`, both t1 and t2 are Unknown suspects with
        // no Known tiebreaker, so the read surfaces with table=None
        // and Confidence::Ambiguous. No diagnostic fires:
        // diagnostics are tool-side gaps (wildcard / unsupported),
        // resolution outcomes live on the read's confidence.
        assert_column_ops(
            "SELECT a FROM t1 JOIN t2 ON t1.id = t2.id",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t1", "id"), read("t2", "id"), ambiguous("a")],
                writes: vec![],
                lineage: vec![passthrough(ambiguous("a"), out("a", 0))],
                diagnostics: vec![],
            },
        );
    }
}

/// Per-arm coverage for `Resolver::visit_expr` and its helpers.
///
/// One terse test per `Expr` variant (and per pipe operator / function
/// clause) that was otherwise unexercised, so that adding a new
/// sqlparser variant — which forces a new match arm — also surfaces an
/// uncovered line here. SQL is `GenericDialect`-only: it is the most
/// permissive built-in dialect and parses every construct below, so no
/// per-dialect fan-out is needed. Operands are qualified (`t.col`)
/// where that yields a deterministic, real-table read; a few arms whose
/// Generic parse is quirky are reshaped (`Interval` via `+`, `Subscript`
/// on a bare identifier) or omitted (`Convert` parses its type arg as a
/// value and drops the real column; `Interpolate` rejects a qualified
/// name) and are left to higher-level tests.
#[cfg(test)]
mod expr_arm_coverage {
    use super::*;
    use sqlparser::dialect::GenericDialect;

    /// `reads` of the first statement, resolved without a catalog.
    fn reads(sql: &str) -> Vec<ColumnRead> {
        extract_column_operations(&GenericDialect {}, sql, None)
            .unwrap()
            .remove(0)
            .unwrap()
            .reads
    }

    /// Like [`reads`], but parses under PostgreSQL — a few arms
    /// (e.g. `ARRAY[...]` literals) are syntax `GenericDialect` rejects.
    fn reads_pg(sql: &str) -> Vec<ColumnRead> {
        use sqlparser::dialect::PostgreSqlDialect;
        extract_column_operations(&PostgreSqlDialect {}, sql, None)
            .unwrap()
            .remove(0)
            .unwrap()
            .reads
    }

    /// A real-table column read against the single table `t`. The
    /// module runs without a catalog, so every resolved ref carries
    /// [`Confidence::Inferred`].
    fn c(name: &str) -> ColumnRead {
        ColumnRead {
            reference: ColumnReference {
                table: Some(TableReference {
                    catalog: None,
                    schema: None,
                    name: "t".into(),
                }),
                name: name.into(),
            },
            confidence: Confidence::Inferred,
        }
    }

    #[test]
    fn in_unnest() {
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.x IN UNNEST(t.arr)"),
            vec![c("c"), c("x"), c("arr")]
        );
    }

    #[test]
    fn at_time_zone() {
        assert_eq!(reads("SELECT t.a AT TIME ZONE 'UTC' FROM t"), vec![c("a")]);
    }

    #[test]
    fn position() {
        assert_eq!(
            reads("SELECT POSITION(t.a IN t.b) FROM t"),
            vec![c("a"), c("b")]
        );
    }

    #[test]
    fn substring() {
        assert_eq!(
            reads("SELECT SUBSTRING(t.a FROM 1 FOR 2) FROM t"),
            vec![c("a")]
        );
    }

    #[test]
    fn trim() {
        // visit order: trimmed expr (`t.y`) before the trim-what (`t.x`).
        assert_eq!(
            reads("SELECT TRIM(BOTH t.x FROM t.y) FROM t"),
            vec![c("y"), c("x")]
        );
    }

    #[test]
    fn overlay() {
        assert_eq!(
            reads("SELECT OVERLAY(t.a PLACING t.b FROM 1 FOR 2) FROM t"),
            vec![c("a"), c("b")]
        );
    }

    #[test]
    fn tuple() {
        assert_eq!(
            reads("SELECT t.c FROM t WHERE (t.a, t.b) IN ((1, 2))"),
            vec![c("c"), c("a"), c("b")]
        );
    }

    #[test]
    fn dictionary() {
        assert_eq!(reads("SELECT {'k': t.a} FROM t"), vec![c("a")]);
    }

    #[test]
    fn map() {
        assert_eq!(reads("SELECT MAP {'k': t.a} FROM t"), vec![c("a")]);
    }

    #[test]
    fn interval() {
        // `INTERVAL t.a DAY` tokenizes oddly under Generic; the `+` form
        // keeps the Interval arm on a plain literal value while the
        // column read comes through the surrounding BinaryOp.
        assert_eq!(reads("SELECT t.a + INTERVAL '1' DAY FROM t"), vec![c("a")]);
    }

    #[test]
    fn lambda() {
        assert_eq!(
            reads("SELECT transform(t.arr, x -> t.a) FROM t"),
            vec![c("arr"), c("a")]
        );
    }

    #[test]
    fn member_of() {
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a MEMBER OF (t.b)"),
            vec![c("c"), c("a"), c("b")]
        );
    }

    #[test]
    fn case_operand_and_else() {
        // operand (`t.a`) and else_result (`t.c`) are the arms the
        // existing CASE tests (condition/result only) left uncovered.
        assert_eq!(
            reads("SELECT CASE t.a WHEN 1 THEN t.b ELSE t.c END FROM t"),
            vec![c("a"), c("b"), c("c")]
        );
    }

    #[test]
    fn bare_identifier_and_literal() {
        // Bare `x` exercises the `Expr::Identifier` arm; `1` the literal
        // no-op arm. Unqualified `x` resolves to the lone table `t`.
        assert_eq!(reads("SELECT t.d FROM t WHERE x = 1"), vec![c("d"), c("x")]);
    }

    #[test]
    fn function_limit_clause() {
        assert_eq!(reads("SELECT ARRAY_AGG(t.a LIMIT 5) FROM t"), vec![c("a")]);
    }

    #[test]
    fn function_having_clause() {
        assert_eq!(
            reads("SELECT ANY_VALUE(t.a HAVING MAX t.b) FROM t"),
            vec![c("a"), c("b")]
        );
    }

    #[test]
    fn listagg_on_overflow_error() {
        assert_eq!(
            reads("SELECT LISTAGG(t.a, ',' ON OVERFLOW ERROR) FROM t"),
            vec![c("a")]
        );
    }

    #[test]
    fn listagg_on_overflow_truncate() {
        assert_eq!(
            reads("SELECT LISTAGG(t.a, ',' ON OVERFLOW TRUNCATE '.' WITH COUNT) FROM t"),
            vec![c("a")]
        );
    }

    #[test]
    fn subscript_index() {
        assert_eq!(reads("SELECT arr[1] FROM t"), vec![c("arr")]);
    }

    #[test]
    fn subscript_slice() {
        assert_eq!(reads("SELECT arr[1:2] FROM t"), vec![c("arr")]);
    }

    #[test]
    fn dot_access() {
        assert_eq!(reads("SELECT (t.a).b FROM t"), vec![c("a"), c("b")]);
    }

    #[test]
    fn json_access() {
        assert_eq!(reads("SELECT t.a -> 'b' FROM t"), vec![c("a")]);
    }

    #[test]
    fn pipe_where_and_select() {
        assert_eq!(
            reads("FROM t |> WHERE t.a > 1 |> SELECT t.b"),
            vec![c("a"), c("b")]
        );
    }

    #[test]
    fn pipe_order_by_and_limit() {
        assert_eq!(
            reads("FROM t |> SELECT t.a |> ORDER BY t.a |> LIMIT 1"),
            vec![c("a"), c("a")]
        );
    }

    #[test]
    fn pipe_aggregate() {
        assert_eq!(
            reads("FROM t |> AGGREGATE SUM(t.a) GROUP BY t.b"),
            vec![c("a"), c("b")]
        );
    }

    #[test]
    fn pipe_set() {
        assert_eq!(reads("FROM t |> SET a = t.a + 1"), vec![c("a")]);
    }

    #[test]
    fn pipe_extend() {
        assert_eq!(reads("FROM t |> EXTEND t.a + 1 AS x"), vec![c("a")]);
    }

    #[test]
    fn pipe_call() {
        assert_eq!(reads("FROM t |> CALL my_func(t.a)"), vec![c("a")]);
    }

    #[test]
    fn unary_op() {
        // Expr::UnaryOp — `-t.a`
        assert_eq!(reads("SELECT -t.a FROM t"), vec![c("a")]);
    }

    #[test]
    fn binary_op() {
        // Expr::BinaryOp — `t.a + t.b`
        assert_eq!(reads("SELECT t.a + t.b FROM t"), vec![c("a"), c("b")]);
    }

    #[test]
    fn nested() {
        // Expr::Nested — `(t.a + t.b)`
        assert_eq!(reads("SELECT (t.a + t.b) FROM t"), vec![c("a"), c("b")]);
    }

    #[test]
    fn between() {
        // Expr::Between — `t.a BETWEEN t.lo AND t.hi`
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a BETWEEN t.lo AND t.hi"),
            vec![c("c"), c("a"), c("lo"), c("hi")]
        );
    }

    #[test]
    fn in_list() {
        // Expr::InList — `t.a IN (t.b, t.d)`
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a IN (t.b, t.d)"),
            vec![c("c"), c("a"), c("b"), c("d")]
        );
    }

    #[test]
    fn like() {
        // Expr::Like — `t.a LIKE t.pat`
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a LIKE t.pat"),
            vec![c("c"), c("a"), c("pat")]
        );
    }

    #[test]
    fn ilike() {
        // Expr::ILike — `t.a ILIKE t.pat`
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a ILIKE t.pat"),
            vec![c("c"), c("a"), c("pat")]
        );
    }

    #[test]
    fn similar_to() {
        // Expr::SimilarTo — `t.a SIMILAR TO t.pat`
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a SIMILAR TO t.pat"),
            vec![c("c"), c("a"), c("pat")]
        );
    }

    #[test]
    fn cast() {
        // Expr::Cast — `CAST(t.a AS INT)`
        assert_eq!(reads("SELECT CAST(t.a AS INT) FROM t"), vec![c("a")]);
    }

    #[test]
    fn extract() {
        // Expr::Extract — `EXTRACT(YEAR FROM t.ts)`
        assert_eq!(
            reads("SELECT EXTRACT(YEAR FROM t.ts) FROM t"),
            vec![c("ts")]
        );
    }

    #[test]
    fn is_true() {
        // Expr::IsTrue — `t.a IS TRUE`
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS TRUE"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_distinct_from() {
        // Expr::IsDistinctFrom — `t.a IS DISTINCT FROM t.b`
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS DISTINCT FROM t.b"),
            vec![c("c"), c("a"), c("b")]
        );
    }

    #[test]
    fn array() {
        // Expr::Array — `ARRAY[t.a, t.b]` (PostgreSQL: GenericDialect rejects it)
        assert_eq!(
            reads_pg("SELECT ARRAY[t.a, t.b] FROM t"),
            vec![c("a"), c("b")]
        );
    }

    #[test]
    fn exists() {
        // Expr::Exists — the subquery's refs surface; inner `FROM t`
        // shadows so `t.b` stays a `t` column.
        assert_eq!(
            reads("SELECT t.c FROM t WHERE EXISTS (SELECT t.b FROM t)"),
            vec![c("c"), c("b")]
        );
    }

    #[test]
    fn any_op() {
        // Expr::AnyOp — `t.a = ANY (SELECT t.b FROM t)`
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a = ANY (SELECT t.b FROM t)"),
            vec![c("c"), c("a"), c("b")]
        );
    }

    #[test]
    fn all_op() {
        // Expr::AllOp — sibling of AnyOp in the chained-pattern arm.
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a = ALL (SELECT t.b FROM t)"),
            vec![c("c"), c("a"), c("b")]
        );
    }

    #[test]
    fn is_not_distinct_from() {
        // Expr::IsNotDistinctFrom — `IS DISTINCT FROM` already covered;
        // the negated form is a distinct AST variant.
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS NOT DISTINCT FROM t.b"),
            vec![c("c"), c("a"), c("b")]
        );
    }

    #[test]
    fn is_false() {
        // Expr::IsFalse — `t.a IS FALSE`
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS FALSE"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_not_false() {
        // Expr::IsNotFalse — `t.a IS NOT FALSE`
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS NOT FALSE"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_not_true() {
        // Expr::IsNotTrue — sibling of IsTrue in the chained-pattern arm.
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS NOT TRUE"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_unknown() {
        // Expr::IsUnknown — SQL three-valued-logic predicate.
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS UNKNOWN"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_not_unknown() {
        // Expr::IsNotUnknown — negated three-valued-logic predicate.
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS NOT UNKNOWN"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_normalized() {
        // Expr::IsNormalized — Unicode normalization predicate
        // (`t.a IS [form] NORMALIZED`); arm visits `expr` only.
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS NORMALIZED"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn ceil_to_field() {
        // Expr::Ceil — the `CEIL(<expr> TO <field>)` form. Plain
        // `CEIL(<expr>)` parses as a function call (Expr::Function),
        // not the Ceil variant; the `TO <field>` is what triggers it.
        assert_eq!(reads("SELECT CEIL(t.a TO YEAR) FROM t"), vec![c("a")]);
    }

    #[test]
    fn floor_to_field() {
        // Expr::Floor — sibling of Ceil; same `TO <field>` form.
        assert_eq!(reads("SELECT FLOOR(t.a TO YEAR) FROM t"), vec![c("a")]);
    }

    #[test]
    fn rlike() {
        // Expr::RLike — MySQL regex match operator; sibling of
        // Like / ILike / SimilarTo in the chained-pattern arm.
        assert_eq!(
            reads("SELECT t.c FROM t WHERE t.a RLIKE 'pat'"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn convert_using() {
        // Expr::Convert — `CONVERT(<expr> USING <charset>)` form
        // (MySQL / PostgreSQL); arm walks expr + each style.
        assert_eq!(reads("SELECT CONVERT(t.a USING utf8) FROM t"), vec![c("a")]);
    }

    #[test]
    fn trim_characters() {
        // Expr::Trim — the `TRIM(<expr>, <chars>...)` comma form sets
        // `trim_characters: Some(Vec<Expr>)` (vs the `FROM` form which
        // sets `trim_what` instead). Existing `trim` test covers the
        // FROM path; this covers the chars-list path.
        assert_eq!(reads("SELECT TRIM(t.y, t.a) FROM t"), vec![c("y"), c("a")]);
    }
}

/// Per-construct coverage for `visit_table_factor` / `visit_join`
/// (table.rs), the statement dispatch in `visit_statement` and its DML
/// helpers (statement.rs), and the set-operation / clause walking in
/// `resolve_query` / `visit_select` (query.rs).
///
/// Same conventions as [`expr_arm_coverage`]: `GenericDialect`-only,
/// qualified operands, whole-`Vec` comparison of the surface each
/// construct exercises (`reads` for FROM-clause / SELECT shapes,
/// `writes` for DML targets). Two constructs are out of reach here and
/// left to other layers: `TABLE t1` (the bare table command does not
/// parse under Generic) and `TRUNCATE` (already covered elsewhere).
#[cfg(test)]
mod relation_arm_coverage {
    use super::*;
    use sqlparser::dialect::{Dialect, GenericDialect};

    fn op(sql: &str) -> ColumnOperation {
        op_with(sql, &GenericDialect {})
    }

    fn op_with(sql: &str, dialect: &dyn Dialect) -> ColumnOperation {
        extract_column_operations(dialect, sql, None)
            .unwrap()
            .remove(0)
            .unwrap()
    }

    fn reads(sql: &str) -> Vec<ColumnRead> {
        op(sql).reads
    }

    fn c(table: &str, name: &str) -> ColumnRead {
        ColumnRead {
            reference: ColumnReference {
                table: Some(TableReference {
                    catalog: None,
                    schema: None,
                    name: table.into(),
                }),
                name: name.into(),
            },
            confidence: Confidence::Inferred,
        }
    }

    /// Write-side build: stays bare [`ColumnReference`] since
    /// `writes` is `Vec<ColumnReference>` (write targets come from
    /// SQL syntax and don't carry a confidence).
    fn w(table: &str, name: &str) -> ColumnReference {
        ColumnReference {
            table: Some(TableReference {
                catalog: None,
                schema: None,
                name: table.into(),
            }),
            name: name.into(),
        }
    }

    // ---- table.rs: joins ----

    #[test]
    fn join_on() {
        assert_eq!(
            reads("SELECT t1.a FROM t1 JOIN t2 ON t1.id = t2.id"),
            vec![c("t1", "id"), c("t2", "id"), c("t1", "a")]
        );
    }

    #[test]
    fn join_using() {
        assert_eq!(
            reads("SELECT t1.a FROM t1 JOIN t2 USING (id)"),
            vec![c("t1", "a")]
        );
    }

    #[test]
    fn join_natural() {
        assert_eq!(
            reads("SELECT t1.a FROM t1 NATURAL JOIN t2"),
            vec![c("t1", "a")]
        );
    }

    #[test]
    fn join_left_outer() {
        assert_eq!(
            reads("SELECT t1.a FROM t1 LEFT JOIN t2 ON t1.id = t2.id"),
            vec![c("t1", "id"), c("t2", "id"), c("t1", "a")]
        );
    }

    #[test]
    fn join_cross() {
        assert_eq!(
            reads("SELECT t1.a FROM t1 CROSS JOIN t2"),
            vec![c("t1", "a")]
        );
    }

    #[test]
    fn nested_join() {
        assert_eq!(
            reads("SELECT t1.a FROM (t1 JOIN t2 ON t1.id = t2.id)"),
            vec![c("t1", "id"), c("t2", "id"), c("t1", "a")]
        );
    }

    // ---- table.rs: derived / function / unnest / pivot / sample ----

    #[test]
    fn derived_table() {
        // The derived table's synthetic column (`x.a`) is dropped; only
        // the real-storage read from inside the subquery surfaces.
        assert_eq!(
            reads("SELECT x.a FROM (SELECT t.a FROM t) x"),
            vec![c("t", "a")]
        );
    }

    #[test]
    fn derived_table_with_column_aliases() {
        assert_eq!(
            reads("SELECT x.c1 FROM (SELECT t.a FROM t) x (c1)"),
            vec![c("t", "a")]
        );
    }

    #[test]
    fn table_function() {
        // The function arg `t.a` is walked, but `t` is not in FROM (the
        // sole relation is the table function `g`), so the qualified ref
        // resolves to nothing → unresolved. Coverage point preserved:
        // the column `a` still surfaces, proving the arg was walked.
        assert_eq!(
            reads("SELECT g.v FROM TABLE(gen(t.a)) g"),
            vec![unresolved("a")]
        );
    }

    #[test]
    fn unnest() {
        // `t` is not in FROM (only the UNNEST relation `u`), so `t.arr`
        // is unresolved; the column still surfaces (arg walked).
        assert_eq!(
            reads("SELECT u.x FROM UNNEST(t.arr) u"),
            vec![unresolved("arr")]
        );
    }

    #[test]
    fn pivot() {
        let result = op("SELECT * FROM t PIVOT(SUM(t.amt) FOR t.mon IN ('a', 'b'))");
        assert_eq!(result.reads, vec![c("t", "amt"), c("t", "mon")]);
    }

    #[test]
    fn unpivot() {
        let result = op("SELECT * FROM t UNPIVOT(v FOR n IN (t.a, t.b))");
        assert_eq!(result.reads, vec![c("t", "v"), c("t", "a"), c("t", "b")]);
    }

    #[test]
    fn tablesample() {
        assert_eq!(
            reads("SELECT t.a FROM t TABLESAMPLE BERNOULLI (10)"),
            vec![c("t", "a")]
        );
    }

    #[test]
    fn match_recognize() {
        // TableFactor::MatchRecognize — visits inner table first (no
        // reads of its own), then partition_by / order_by / measures.
        // The outer `g.m` projection rides through the synthetic
        // MATCH_RECOGNIZE alias and is dropped.
        let sql = "SELECT g.m FROM t \
                   MATCH_RECOGNIZE ( \
                     PARTITION BY t.a \
                     ORDER BY t.b \
                     MEASURES t.c AS m \
                     PATTERN (X+) \
                     DEFINE X AS true \
                   ) AS g";
        assert_eq!(reads(sql), vec![c("t", "a"), c("t", "b"), c("t", "c")]);
    }

    #[test]
    fn json_table() {
        // TableFactor::JsonTable — the `json_expr` (`t.doc`) is walked;
        // `t` is not in FROM (only the JSON_TABLE relation `j`), so it
        // surfaces unresolved. The `COLUMNS (...)` schema declares
        // synthetic outputs, and the `j.x` projection rides through them
        // so it is dropped.
        assert_eq!(
            reads("SELECT j.x FROM JSON_TABLE(t.doc, '$' COLUMNS (x INT PATH '$.x')) AS j"),
            vec![unresolved("doc")]
        );
    }

    #[test]
    fn open_json_table() {
        // TableFactor::OpenJsonTable — same visit shape as JsonTable;
        // `t.doc` is walked but `t` is not in FROM → unresolved. The
        // WITH-declared columns are synthetic.
        assert_eq!(
            reads("SELECT o.x FROM OPENJSON(t.doc) WITH (x INT '$.x') AS o"),
            vec![unresolved("doc")]
        );
    }

    #[test]
    fn xml_table() {
        // TableFactor::XmlTable — visits `row_expression` (here a string
        // literal — no read) then each PASSING argument expression
        // (`t.doc`). `t` is not in FROM (only the XMLTABLE relation `x`),
        // so it surfaces unresolved.
        assert_eq!(
            reads("SELECT x.v FROM XMLTABLE('/r' PASSING t.doc COLUMNS v INT PATH '@v') AS x"),
            vec![unresolved("doc")]
        );
    }

    #[test]
    fn semantic_view() {
        // TableFactor::SemanticView — Snowflake-only syntax. Visit order
        // is dimensions → metrics → facts → where_clause. `t` is not in
        // FROM (the relation is the semantic view itself), so the
        // `t.a` / `t.b` refs surface unresolved.
        use sqlparser::dialect::SnowflakeDialect;
        assert_eq!(
            op_with(
                "SELECT * FROM SEMANTIC_VIEW(my_view DIMENSIONS t.a METRICS t.b)",
                &SnowflakeDialect {},
            )
            .reads,
            vec![unresolved("a"), unresolved("b")]
        );
    }

    #[test]
    fn lateral_function_call() {
        // TableFactor::Function — distinct from `Table { args }`. The
        // sqlparser produces it for lateral function-call relations like
        // Snowflake's `LATERAL FLATTEN(...)`. Only the function args
        // surface; the synthetic `f.value` projection is dropped.
        assert_eq!(
            reads("SELECT f.value FROM t, LATERAL FLATTEN(input => t.arr) AS f"),
            vec![c("t", "arr")]
        );
    }

    // ---- statement.rs: DML write / lineage paths ----

    #[test]
    fn merge() {
        let result = op("MERGE INTO t1 USING t2 ON t1.id = t2.id \
             WHEN MATCHED THEN UPDATE SET a = t2.b \
             WHEN NOT MATCHED THEN INSERT (a) VALUES (t2.b)");
        assert_eq!(
            result.reads,
            vec![c("t1", "id"), c("t2", "id"), c("t2", "b"), c("t2", "b")]
        );
        assert_eq!(result.writes, vec![w("t1", "a"), w("t1", "a")]);
    }

    #[test]
    fn insert_on_conflict() {
        assert_eq!(
            op("INSERT INTO t1 (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = EXCLUDED.a").writes,
            vec![w("t1", "a"), w("t1", "a")]
        );
    }

    #[test]
    fn create_table_as_select() {
        let result = op("CREATE TABLE t2 AS SELECT t1.a FROM t1");
        assert_eq!(result.reads, vec![c("t1", "a")]);
        assert_eq!(result.writes, vec![w("t2", "a")]);
    }

    #[test]
    fn create_view() {
        let result = op("CREATE VIEW v AS SELECT t1.a FROM t1");
        assert_eq!(result.reads, vec![c("t1", "a")]);
        assert_eq!(result.writes, vec![w("v", "a")]);
    }

    #[test]
    fn alter_table_add_column() {
        assert_eq!(
            op("ALTER TABLE t1 ADD COLUMN c INT").writes,
            vec![w("t1", "c")]
        );
    }

    #[test]
    fn delete() {
        assert_eq!(reads("DELETE FROM t1 WHERE t1.a = 1"), vec![c("t1", "a")]);
    }

    #[test]
    fn update() {
        let result = op("UPDATE t1 SET a = t1.b WHERE t1.c = 1");
        assert_eq!(result.reads, vec![c("t1", "b"), c("t1", "c")]);
        assert_eq!(result.writes, vec![w("t1", "a")]);
    }

    #[test]
    fn insert_returning() {
        let result = op("INSERT INTO t1 (a) VALUES (1) RETURNING t1.a");
        assert_eq!(result.reads, vec![c("t1", "a")]);
        assert_eq!(result.writes, vec![w("t1", "a")]);
    }

    #[test]
    fn insert_from_select() {
        let result = op("INSERT INTO t1 (a) SELECT t2.b FROM t2");
        assert_eq!(result.reads, vec![c("t2", "b")]);
        assert_eq!(result.writes, vec![w("t1", "a")]);
    }

    // ---- query.rs: set operations / clauses ----

    #[test]
    fn union() {
        assert_eq!(
            reads("SELECT t1.a FROM t1 UNION SELECT t2.b FROM t2"),
            vec![c("t1", "a"), c("t2", "b")]
        );
    }

    #[test]
    fn intersect() {
        assert_eq!(
            reads("SELECT t1.a FROM t1 INTERSECT SELECT t2.b FROM t2"),
            vec![c("t1", "a"), c("t2", "b")]
        );
    }

    #[test]
    fn except() {
        assert_eq!(
            reads("SELECT t1.a FROM t1 EXCEPT SELECT t2.b FROM t2"),
            vec![c("t1", "a"), c("t2", "b")]
        );
    }

    #[test]
    fn values() {
        // Bare VALUES has no column references; exercises `visit_values`.
        assert_eq!(op("VALUES (1, 2), (3, 4)").reads, vec![]);
    }

    #[test]
    fn group_by_cube() {
        assert_eq!(
            reads("SELECT t.a FROM t GROUP BY CUBE(t.a)"),
            vec![c("t", "a"), c("t", "a")]
        );
    }

    #[test]
    fn group_by_rollup() {
        assert_eq!(
            reads("SELECT t.a FROM t GROUP BY ROLLUP(t.a)"),
            vec![c("t", "a"), c("t", "a")]
        );
    }

    #[test]
    fn order_by_limit_offset() {
        assert_eq!(
            reads("SELECT t.a FROM t ORDER BY t.a LIMIT 5 OFFSET 2"),
            vec![c("t", "a"), c("t", "a")]
        );
    }

    #[test]
    fn select_distinct() {
        assert_eq!(reads("SELECT DISTINCT t.a FROM t"), vec![c("t", "a")]);
    }

    #[test]
    fn having() {
        assert_eq!(
            reads("SELECT t.a FROM t GROUP BY t.a HAVING t.a > 1"),
            vec![c("t", "a"), c("t", "a"), c("t", "a")]
        );
    }

    #[test]
    fn qualify() {
        assert_eq!(
            reads("SELECT t.a FROM t QUALIFY ROW_NUMBER() OVER () = 1"),
            vec![c("t", "a")]
        );
    }

    // ---- query.rs: cold query/select arms ----

    #[test]
    fn fetch() {
        // `visit_fetch` walks the FETCH quantity expression; here the
        // literal `10` contributes no reads.
        assert_eq!(
            reads("SELECT t.a FROM t FETCH FIRST 10 ROWS ONLY"),
            vec![c("t", "a")]
        );
    }

    #[test]
    fn set_expr_query() {
        // A bare `(SELECT ...)` parses with the outer body as
        // `SetExpr::Query`, which bubbles its inner projections through
        // the parenthesized wrapper.
        assert_eq!(reads("(SELECT t.a FROM t)"), vec![c("t", "a")]);
    }

    #[test]
    fn distinct_on() {
        // `Distinct::On` exprs walk before the projection, so `t.a`
        // (the DISTINCT ON key) lands first.
        assert_eq!(
            reads("SELECT DISTINCT ON (t.a) t.b FROM t"),
            vec![c("t", "a"), c("t", "b")]
        );
    }

    #[test]
    fn top_with_expr_quantity() {
        // `TopQuantity::Expr` walks the quantity expression — the
        // `Number` variant is the constant path and stays uncovered.
        assert_eq!(
            reads("SELECT TOP (t.a + 1) t.b FROM t"),
            vec![c("t", "a"), c("t", "b")]
        );
    }

    #[test]
    fn select_into() {
        // `SELECT ... INTO new_t` binds `new_t` as a write target but
        // generates no column-level writes (no projection pairing). The
        // projection still surfaces as a read.
        let result = op("SELECT t.a INTO new_t FROM t");
        assert_eq!(result.reads, vec![c("t", "a")]);
        assert_eq!(result.writes, vec![]);
    }

    #[test]
    fn lateral_view() {
        // `select.lateral_views` walks each `lateral_view` expression
        // (here `EXPLODE(t.arr)`); the `v` alias is not bound as a real
        // table, so we read against `t` to keep the assertion stable.
        assert_eq!(
            reads("SELECT t.a FROM t LATERAL VIEW EXPLODE(t.arr) v AS x"),
            vec![c("t", "a"), c("t", "arr")]
        );
    }

    #[test]
    fn prewhere() {
        // ClickHouse `PREWHERE` rides the same predicate-walk array as
        // selection / having / qualify.
        assert_eq!(
            reads("SELECT t.a FROM t PREWHERE t.b = 1"),
            vec![c("t", "a"), c("t", "b")]
        );
    }

    #[test]
    fn connect_by_start_with() {
        // Oracle / Snowflake hierarchical query — `START WITH` and
        // `CONNECT BY` populate two separate `ConnectByKind` entries in
        // `select.connect_by`, so a single SQL exercises both arms.
        let result = op("SELECT t.a FROM t START WITH t.b = 1 CONNECT BY PRIOR t.c = t.d");
        assert_eq!(
            result.reads,
            vec![c("t", "a"), c("t", "b"), c("t", "c"), c("t", "d")]
        );
    }

    #[test]
    fn sort_by() {
        // Hive-style `SORT BY` (per-reducer ordering, distinct from
        // `ORDER BY`); each entry visits as an order-by expression.
        assert_eq!(
            reads("SELECT t.a FROM t SORT BY t.b"),
            vec![c("t", "a"), c("t", "b")]
        );
    }

    #[test]
    fn named_window() {
        // `NamedWindowExpr::WindowSpec` walks PARTITION BY / ORDER BY
        // inside a `WINDOW w AS (...)` definition.
        assert_eq!(
            reads("SELECT t.a FROM t WINDOW w AS (PARTITION BY t.b)"),
            vec![c("t", "a"), c("t", "b")]
        );
    }

    #[test]
    fn qualified_wildcard_expr() {
        // Snowflake-only `(expr).*` syntax — `QualifiedWildcard::Expr`
        // arm records WildcardSuppressed and also walks the underlying
        // expression so its real-table refs still surface.
        use sqlparser::dialect::SnowflakeDialect;
        let result = op_with("SELECT (t.a).* FROM t", &SnowflakeDialect {});
        assert_eq!(result.reads, vec![c("t", "a")]);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].kind,
            ColumnLevelDiagnosticKind::WildcardSuppressed
        );
    }

    #[test]
    fn group_by_all() {
        // `GROUP BY ALL` — `GroupByExpr::All` arm has no operand exprs
        // of its own; only the projection surfaces as a read.
        assert_eq!(reads("SELECT t.a FROM t GROUP BY ALL"), vec![c("t", "a")]);
    }

    // ---- statement.rs: DML helper paths ----

    #[test]
    fn update_3part_assignment_target() {
        // 3-segment assignment target — `assignment_target_table`'s
        // `parts.len() == 3` arm yields a TableReference with the schema
        // qualifier populated.
        let result = op("UPDATE schema.t SET schema.t.col = 1");
        assert_eq!(
            result.writes,
            vec![ColumnReference {
                table: Some(TableReference {
                    catalog: None,
                    schema: Some("schema".into()),
                    name: "t".into(),
                }),
                name: "col".into(),
            }]
        );
    }

    #[test]
    fn update_4part_assignment_target() {
        // 4-segment assignment target — exercises the full
        // catalog.schema.table.column qualifier path.
        let result = op("UPDATE catalog.schema.t SET catalog.schema.t.col = 1");
        assert_eq!(
            result.writes,
            vec![ColumnReference {
                table: Some(TableReference {
                    catalog: Some("catalog".into()),
                    schema: Some("schema".into()),
                    name: "t".into(),
                }),
                name: "col".into(),
            }]
        );
    }

    #[test]
    fn update_tuple_assignment_target_is_skipped() {
        // `AssignmentTarget::Tuple(_)` returns `None` from
        // `assignment_target_parts`, so the SET position is skipped:
        // no writes emitted (tuple targets are not yet supported for
        // column-level pairing). The RHS literals contribute no reads.
        let result = op("UPDATE t SET (a, b) = (1, 2)");
        assert_eq!(result.reads, vec![]);
        assert_eq!(result.writes, vec![]);
    }

    #[test]
    fn create_view_with_to_target() {
        // ClickHouse-style materialized view `CREATE VIEW v TO dst`:
        // both `v` (the view) and `dst` (the storage target) bind as
        // write targets via `bind_real_table`. Only `v.a` surfaces as a
        // column-level write — projections pair against the view's
        // column list, not the `TO` target.
        let result = op("CREATE VIEW v TO dst AS SELECT t.a FROM t");
        assert_eq!(result.reads, vec![c("t", "a")]);
        assert_eq!(result.writes, vec![w("v", "a")]);
    }

    #[test]
    fn create_virtual_table() {
        // `CREATE VIRTUAL TABLE` (SQLite / virtual table modules) binds
        // the table as a write target with no column-level writes —
        // the column schema is module-defined, not part of the SQL.
        let result = op("CREATE VIRTUAL TABLE vt USING mymod");
        assert_eq!(result.reads, vec![]);
        assert_eq!(result.writes, vec![]);
    }

    // ---- column_operation_extractor.rs: write-derivation helpers ----

    #[test]
    fn alter_table_change_column_rename_emits_old_and_new() {
        // `AlterTableOperation::ChangeColumn` with `old != new` mirrors
        // RenameColumn — both ends of the rename surface as writes.
        let result = op("ALTER TABLE t CHANGE COLUMN old new INT");
        assert_eq!(result.writes, vec![w("t", "old"), w("t", "new")]);
    }

    #[test]
    fn alter_table_change_column_same_name_emits_one_write() {
        // When `old == new`, ChangeColumn is a type / nullability change
        // rather than a rename — emit only the single column name.
        let result = op("ALTER TABLE t CHANGE COLUMN col col VARCHAR(255)");
        assert_eq!(result.writes, vec![w("t", "col")]);
    }

    #[test]
    fn alter_table_modify_column() {
        // MySQL `MODIFY COLUMN` — type change on a single column.
        let result = op("ALTER TABLE t MODIFY COLUMN col VARCHAR(255)");
        assert_eq!(result.writes, vec![w("t", "col")]);
    }

    #[test]
    fn with_in_merge_unwraps_to_inner_writes() {
        // `WITH cte AS (...) MERGE ...` parses as Statement::Query
        // wrapping SetExpr::Merge; `collect_writes` unwraps it to find
        // the inner MERGE's write target.
        let result = op("WITH src AS (SELECT a, b FROM t2) \
             MERGE INTO t1 USING src ON t1.id = src.a \
             WHEN MATCHED THEN UPDATE SET col = src.b");
        assert_eq!(result.writes, vec![w("t1", "col")]);
    }

    #[test]
    fn update_5part_assignment_target_skipped() {
        // UPDATE SET with a 5-segment qualified target lands in the
        // catch-all `_ => None` arm of `column_ref_from_assignment_target`
        // (the resolver's target decoder caps at 4 parts =
        // catalog.schema.table.column).
        let result = op("UPDATE t SET a.b.c.d.e = 1");
        assert_eq!(result.writes, vec![]);
    }
}

/// Coverage for the large "unsupported statement" dispatch arm in
/// `Resolver::visit_statement` (statement.rs:105-219), which folds ~110
/// DDL / session / transaction / show / utility `Statement` variants
/// into a single `record_unsupported_statement` call.
///
/// Each statement here parses under `GenericDialect` and must resolve to
/// `StatementKind::Unsupported` with exactly one
/// `UnsupportedStatement` diagnostic and no reads / writes / lineage.
/// Driving every variant individually means adding a new `Statement`
/// case to that arm (a compile-forced edit) also wants a line here,
/// keeping the arm honest. Variants that need a non-Generic dialect to
/// parse (`LOCK TABLES`, `LISTEN` / `NOTIFY`, `ALTER ROLE` / `SESSION`,
/// MySQL `LOAD DATA`, …) are out of scope for this Generic-only sweep.
#[cfg(test)]
mod unsupported_statement_coverage {
    use super::*;
    use sqlparser::dialect::GenericDialect;

    /// Statements that should all land in the unsupported arm: one
    /// `Unsupported` op carrying a single `UnsupportedStatement`
    /// diagnostic, with the three data surfaces empty.
    const UNSUPPORTED: &[&str] = &[
        "ANALYZE TABLE t",
        "SET x = 1",
        "MSCK REPAIR TABLE t",
        "INSTALL foo",
        "LOAD foo",
        "CALL foo()",
        "COPY t FROM 'f'",
        "OPEN cur",
        "CLOSE cur",
        "CREATE INDEX i ON t (a)",
        "CREATE ROLE r",
        "CREATE SERVER s FOREIGN DATA WRAPPER w",
        "CREATE POLICY p ON t",
        "CREATE EXTENSION ext",
        "DROP EXTENSION ext",
        "DROP FUNCTION f",
        "DROP DOMAIN d",
        "DROP PROCEDURE p",
        "DROP POLICY p ON t",
        "DECLARE c CURSOR FOR SELECT 1",
        "FETCH 1 IN c",
        "DISCARD ALL",
        "SHOW FUNCTIONS",
        "SHOW VARIABLE x",
        "SHOW STATUS",
        "SHOW VARIABLES",
        "SHOW CREATE TABLE t",
        "SHOW COLUMNS FROM t",
        "SHOW DATABASES",
        "SHOW SCHEMAS",
        "SHOW TABLES",
        "SHOW VIEWS",
        "SHOW COLLATION",
        "USE db",
        "START TRANSACTION",
        "BEGIN",
        "COMMENT ON TABLE t IS 'x'",
        "COMMIT",
        "ROLLBACK",
        "CREATE SCHEMA s",
        "CREATE DATABASE d",
        "CREATE FUNCTION f() RETURNS INT AS 'x' LANGUAGE SQL",
        "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
        "DROP TRIGGER tr ON t",
        "CREATE PROCEDURE p() AS BEGIN SELECT 1; END",
        "ASSERT 1 = 1",
        "GRANT SELECT ON t TO u",
        "DENY SELECT ON t TO u",
        "REVOKE SELECT ON t FROM u",
        "DEALLOCATE p",
        "EXECUTE p",
        "PREPARE p AS SELECT 1",
        "KILL 5",
        "EXPLAIN SELECT 1",
        "SAVEPOINT s",
        "RELEASE SAVEPOINT s",
        "CACHE TABLE t",
        "UNCACHE TABLE t",
        "CREATE SEQUENCE seq",
        "CREATE DOMAIN d AS INT",
        "CREATE TYPE ty AS (a INT)",
        "PRAGMA x",
        "UNLOAD (SELECT 1) TO 's'",
        "OPTIMIZE TABLE t",
        "RENAME TABLE t TO t2",
        "PRINT 'x'",
        "RETURN 1",
        "CREATE USER u",
        "ALTER USER u",
        "VACUUM",
        "RESET x",
        "FLUSH TABLES",
        "ALTER POLICY p ON t RENAME TO p2",
        "ALTER TYPE ty ADD VALUE 'z'",
        "DROP SECRET s",
        "ATTACH DATABASE 'f' AS d",
    ];

    #[test]
    fn unsupported_statements_report_only_a_diagnostic() {
        for sql in UNSUPPORTED {
            assert_unsupported(sql, &GenericDialect {});
        }
    }

    /// Statements that only parse under a specific dialect. Each entry
    /// exercises a distinct `Statement::*` pattern in the unsupported
    /// fold arm that `GenericDialect` cannot reach.
    const UNSUPPORTED_DIALECT_SPECIFIC: &[(&str, &str)] = &[
        ("mysql", "LOCK TABLES t READ"),
        ("mysql", "UNLOCK TABLES"),
        ("postgres", "LISTEN channel1"),
        ("postgres", "NOTIFY channel1"),
        ("postgres", "ALTER ROLE r WITH PASSWORD 'p'"),
    ];

    #[test]
    fn unsupported_statements_dialect_specific() {
        use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};
        for (dialect_name, sql) in UNSUPPORTED_DIALECT_SPECIFIC {
            match *dialect_name {
                "mysql" => assert_unsupported(sql, &MySqlDialect {}),
                "postgres" => assert_unsupported(sql, &PostgreSqlDialect {}),
                other => panic!("unknown dialect tag in fixture: {other}"),
            }
        }
    }

    fn assert_unsupported(sql: &str, dialect: &dyn sqlparser::dialect::Dialect) {
        let op = extract_column_operations(dialect, sql, None)
            .unwrap()
            .remove(0)
            .unwrap();
        assert_eq!(
            op.statement_kind,
            StatementKind::Unsupported,
            "for SQL: {sql}"
        );
        assert!(op.reads.is_empty(), "for SQL: {sql}");
        assert!(op.writes.is_empty(), "for SQL: {sql}");
        assert!(op.lineage.is_empty(), "for SQL: {sql}");
        let kinds: Vec<_> = op.diagnostics.iter().map(|d| &d.kind).collect();
        assert_eq!(
            kinds,
            vec![&ColumnLevelDiagnosticKind::UnsupportedStatement],
            "for SQL: {sql}"
        );
    }
}

mod supported_kind_only_coverage {
    //! Statements that classify as a supported `StatementKind` but
    //! produce no column-level reads / writes / lineage / diagnostics.
    //! Pins that these don't accidentally fall through to
    //! `Unsupported`, and that the column-level path returns empty
    //! surfaces for DDL-only / DROP / TRUNCATE / bare VALUES shapes.

    use super::*;
    use sqlparser::dialect::GenericDialect;

    const KIND_ONLY: &[(&str, StatementKind)] = &[
        ("CREATE TABLE t (a INT)", StatementKind::CreateTable),
        ("DROP TABLE t", StatementKind::Drop),
        ("DROP VIEW v", StatementKind::Drop),
        ("DROP MATERIALIZED VIEW mv", StatementKind::Drop),
        ("TRUNCATE TABLE t", StatementKind::Truncate),
        ("VALUES (1, 2)", StatementKind::Select),
    ];

    #[test]
    fn supported_kind_only_statements_produce_empty_surfaces() {
        for (sql, expected_kind) in KIND_ONLY {
            let result = extract_column_operations(&GenericDialect {}, sql, None).unwrap();
            let op = result[0].as_ref().unwrap();
            assert_eq!(op.statement_kind, *expected_kind, "kind for SQL: {sql}");
            assert!(op.reads.is_empty(), "reads for SQL: {sql}");
            assert!(op.writes.is_empty(), "writes for SQL: {sql}");
            assert!(op.lineage.is_empty(), "lineage for SQL: {sql}");
            assert!(op.diagnostics.is_empty(), "diagnostics for SQL: {sql}");
        }
    }
}

/// Pins one row per case from the [`Confidence`] rustdoc's behavior
/// table. Each test is the minimal SQL that exercises that arm and
/// asserts the full expected reads vector — so this module doubles as
/// behavior documentation: a reader can recover the catalog-less /
/// catalog-aware semantics by reading the test bodies.
#[cfg(test)]
mod confidence_arm_coverage {
    use super::*;
    use crate::catalog::{Catalog, ColumnSchema};
    use sqlparser::dialect::GenericDialect;
    use std::collections::HashMap;

    #[derive(Debug, Default)]
    struct TestCatalog {
        tables: HashMap<String, Vec<&'static str>>,
    }

    impl TestCatalog {
        fn with(mut self, name: &str, cols: Vec<&'static str>) -> Self {
            self.tables.insert(name.to_string(), cols);
            self
        }
    }

    impl Catalog for TestCatalog {
        fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>> {
            self.tables.get(table.name.value.as_str()).map(|cols| {
                cols.iter()
                    .map(|c| ColumnSchema {
                        name: c.to_string(),
                    })
                    .collect()
            })
        }
    }

    fn extract_reads(sql: &str, catalog: Option<&dyn Catalog>) -> Vec<ColumnRead> {
        extract_column_operations(&GenericDialect {}, sql, catalog)
            .unwrap()
            .remove(0)
            .unwrap()
            .reads
    }

    #[test]
    fn catalog_less_sole_unknown_candidate_is_inferred() {
        // Real `Unknown` table is the sole candidate → Inferred.
        assert_eq!(
            extract_reads("SELECT id FROM t1", None),
            vec![read("t1", "id")]
        );
    }

    #[test]
    fn catalog_less_two_unknown_candidates_is_ambiguous() {
        // Both candidates Unknown, no Known tiebreaker → Ambiguous.
        // The `t1.id` / `t2.id` qualified refs in the ON predicate
        // remain Inferred (qualifier-bound but Unknown schema).
        assert_eq!(
            extract_reads("SELECT id FROM t1 JOIN t2 ON t1.id = t2.id", None),
            vec![read("t1", "id"), read("t2", "id"), ambiguous("id")]
        );
    }

    #[test]
    fn catalog_less_cte_known_body_drops_synthetic_confirmed_ref() {
        // CTE `cte` has a `Known` body ([id]) derived from its
        // projection. The outer `cte.id` qualified ref hits the CTE
        // binding and would be internally Confirmed, but synthetic
        // refs are dropped from public reads — only the inner real
        // `t1.id` (Inferred) surfaces.
        assert_eq!(
            extract_reads("WITH cte AS (SELECT id FROM t1) SELECT id FROM cte", None),
            vec![read("t1", "id")]
        );
    }

    #[test]
    fn catalog_less_cte_known_denies_column_is_unresolved() {
        // CTE body = [id]; `unknown_col` cannot belong to the CTE.
        // The outer ref surfaces with table=None / Unresolved.
        assert_eq!(
            extract_reads(
                "WITH cte AS (SELECT id FROM t1) SELECT id, unknown_col FROM cte",
                None
            ),
            vec![read("t1", "id"), unresolved("unknown_col")]
        );
    }

    #[test]
    fn catalog_aware_known_binding_lists_column_is_confirmed() {
        let catalog = TestCatalog::default().with("t1", vec!["a"]);
        assert_eq!(
            extract_reads("SELECT a FROM t1", Some(&catalog)),
            vec![read_confirmed("t1", "a")]
        );
    }

    #[test]
    fn catalog_aware_known_binding_missing_column_is_unresolved() {
        let catalog = TestCatalog::default().with("t1", vec!["x", "y"]);
        assert_eq!(
            extract_reads("SELECT a FROM t1", Some(&catalog)),
            vec![unresolved("a")]
        );
    }

    #[test]
    fn catalog_aware_known_witness_over_unknown_suspect_is_inferred() {
        // t1 in catalog confirms `a`; t2 is catalog-less → Unknown
        // suspect. Single Known winner adopted with Inferred (the
        // Unknown suspect could in principle also contain `a`, so we
        // don't claim Confirmed).
        let catalog = TestCatalog::default().with("t1", vec!["a"]);
        assert_eq!(
            extract_reads("SELECT a FROM t1, t2", Some(&catalog)),
            vec![read("t1", "a")]
        );
    }

    #[test]
    fn catalog_aware_two_known_confirms_is_ambiguous() {
        let catalog = TestCatalog::default()
            .with("t1", vec!["a"])
            .with("t2", vec!["a"]);
        // Both t1 and t2 confirm `a` — genuine ambiguity. The
        // qualified `t1.a` / `t2.a` in ON are Confirmed individually.
        assert_eq!(
            extract_reads("SELECT a FROM t1 JOIN t2 ON t1.a = t2.a", Some(&catalog)),
            vec![
                read_confirmed("t1", "a"),
                read_confirmed("t2", "a"),
                ambiguous("a"),
            ]
        );
    }

    #[test]
    fn qualified_ref_to_unknown_table_is_inferred() {
        // `t.col` where t is `Unknown` (no catalog). The qualifier
        // binds, but the column existence is assumed → Inferred.
        assert_eq!(
            extract_reads("SELECT t.id FROM t", None),
            vec![read("t", "id")]
        );
    }

    #[test]
    fn qualified_ref_to_known_table_listing_column_is_confirmed() {
        let catalog = TestCatalog::default().with("t", vec!["id"]);
        assert_eq!(
            extract_reads("SELECT t.id FROM t", Some(&catalog)),
            vec![read_confirmed("t", "id")]
        );
    }
}

/// Pins the qualifier-matching behavior table: for a *qualified*
/// column reference, which `FROM` binding (if any) owns it. Every row
/// is decidable from SQL structure alone, so these run catalog-free and
/// assert table identity + catalog-less confidence
/// (`Inferred` for a resolved ref, `Ambiguous` / `Unresolved` for the
/// failure modes). Catalog-confirmed (`Confirmed`) placement is covered
/// separately in [`confidence_arm_coverage`].
///
/// The matching rule is ANSI right-anchored: a qualifier matches a
/// non-aliased table when name segments are equal and each of
/// schema / catalog is equal-or-either-absent; an alias hides the
/// original name and matches only a single-segment qualifier equal to
/// the alias.
#[cfg(test)]
mod qualified_ref_arm_coverage {
    use super::*;

    fn reads(sql: &str) -> Vec<ColumnRead> {
        extract(sql).reads
    }

    /// A schema-qualified read (`schema.name.col`), catalog-less →
    /// `Inferred`.
    fn read2(schema: &str, name: &str, col: &str) -> ColumnRead {
        read_with_ref(
            TableReference {
                catalog: None,
                schema: Some(schema.into()),
                name: name.into(),
            },
            col,
            Confidence::Inferred,
        )
    }

    #[test]
    fn row1_bare_qualifier_matches_schema_qualified_binding() {
        // `users` (no schema) matches `FROM mydb.users` — the binding
        // fills the omitted schema. Surfaces the binding's full identity.
        assert_eq!(
            reads("SELECT users.id FROM mydb.users"),
            vec![read2("mydb", "users", "id")]
        );
    }

    #[test]
    fn row2_full_qualifier_matches_exactly() {
        assert_eq!(
            reads("SELECT mydb.users.id FROM mydb.users"),
            vec![read2("mydb", "users", "id")]
        );
    }

    #[test]
    fn row3_contradicting_schema_is_unresolved() {
        // `otherdb.users` vs binding `mydb.users` — schema present on
        // both and differs. Contradiction a catalog can't fix.
        assert_eq!(
            reads("SELECT otherdb.users.id FROM mydb.users"),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn row4_bare_qualifier_matches_bare_binding() {
        assert_eq!(
            reads("SELECT users.id FROM users"),
            vec![read("users", "id")]
        );
    }

    #[test]
    fn row5_over_qualified_ref_resolves_to_binding_identity() {
        // `mydb.users` vs bare binding `users` — the binding leaves the
        // schema unspecified, so the ref's extra `mydb` matches as a
        // wildcard. We surface the *binding's* identity (`users`), not
        // the ref's unverified `mydb` qualifier — binding wins, for
        // consistency with row 1 (always surface what FROM declared).
        assert_eq!(
            reads("SELECT mydb.users.id FROM users"),
            vec![read("users", "id")]
        );
    }

    #[test]
    fn row6_alias_matches_alias() {
        assert_eq!(
            reads("SELECT u.id FROM mydb.users AS u"),
            vec![read2("mydb", "users", "id")]
        );
    }

    #[test]
    fn row7_alias_hides_original_bare_name() {
        // `users.id` against `FROM mydb.users AS u` — the alias hides
        // the original name, so `users` matches nothing.
        assert_eq!(
            reads("SELECT users.id FROM mydb.users AS u"),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn row8_alias_hides_original_full_name() {
        assert_eq!(
            reads("SELECT mydb.users.id FROM mydb.users AS u"),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn row9_two_unaliased_same_name_tables_collide_in_scope() {
        // IDEAL (real engine): `users` matches both `s1.users` and
        // `s2.users` → AMBIGUOUS. KNOWN LIMITATION: the scope arena
        // keys bindings by exposed name, and both tables expose
        // `users`, so binding the second merges it into the first
        // (role-merge) and drops `s2.users`. Only `s1.users` survives,
        // so the ref resolves to it (Inferred) and the ambiguity is
        // not detected. Aliasing the tables (row 13) sidesteps the
        // collision. Pre-existing — independent of qualifier matching.
        assert_eq!(
            reads("SELECT users.id FROM s1.users, s2.users"),
            vec![read2("s1", "users", "id")]
        );
    }

    #[test]
    fn row10_explicit_schema_disambiguates() {
        assert_eq!(
            reads("SELECT s1.users.id FROM s1.users, s2.users"),
            vec![read2("s1", "users", "id")]
        );
    }

    #[test]
    fn row11_schema_matching_no_candidate_is_unresolved() {
        assert_eq!(
            reads("SELECT s3.users.id FROM s1.users, s2.users"),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn row12_alias_disambiguates_two_aliased_bindings() {
        assert_eq!(
            reads("SELECT u1.id FROM s1.users u1, s2.users u2"),
            vec![read2("s1", "users", "id")]
        );
    }

    #[test]
    fn row13_bare_name_hidden_by_both_aliases_is_unresolved() {
        assert_eq!(
            reads("SELECT users.id FROM s1.users u1, s2.users u2"),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn row14_last_segment_uniquely_matches_one_binding() {
        // `users` matches `mydb.users` but not `mydb.orders` (name
        // differs). Unique → resolve.
        assert_eq!(
            reads("SELECT users.id FROM mydb.users, mydb.orders"),
            vec![read2("mydb", "users", "id")]
        );
    }
}

/// Pins the dialect-aware identifier case-folding policy
/// ([`crate::resolver`]'s `IdentifierCasing`) as observed through
/// column resolution. The distinguishing cases are table-name
/// case-sensitivity (BigQuery / MySQL real tables are case-sensitive;
/// most dialects fold) and alias case-insensitivity (BigQuery aliases
/// fold even though its tables don't). Column case-insensitivity is
/// shown via a catalog.
#[cfg(test)]
mod dialect_casing_coverage {
    use super::*;
    use crate::catalog::{Catalog, ColumnSchema};
    use sqlparser::dialect::{BigQueryDialect, GenericDialect, MySqlDialect};
    use std::collections::HashMap;

    #[derive(Debug, Default)]
    struct TestCatalog {
        tables: HashMap<String, Vec<&'static str>>,
    }

    impl TestCatalog {
        fn with(mut self, name: &str, cols: Vec<&'static str>) -> Self {
            self.tables.insert(name.to_string(), cols);
            self
        }
    }

    impl Catalog for TestCatalog {
        fn columns(&self, table: &TableReference) -> Option<Vec<ColumnSchema>> {
            self.tables.get(table.name.value.as_str()).map(|cols| {
                cols.iter()
                    .map(|c| ColumnSchema {
                        name: c.to_string(),
                    })
                    .collect()
            })
        }
    }

    fn reads(sql: &str, dialect: &dyn Dialect, catalog: Option<&dyn Catalog>) -> Vec<ColumnRead> {
        extract_column_operations(dialect, sql, catalog)
            .unwrap()
            .remove(0)
            .unwrap()
            .reads
    }

    #[test]
    fn bigquery_qualified_table_ref_is_case_sensitive() {
        // BigQuery tables are case-sensitive: qualifier `T1` does not
        // match the binding `t1`, so the ref is unresolved.
        assert_eq!(
            reads("SELECT T1.id FROM t1", &BigQueryDialect {}, None),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn mysql_qualified_table_ref_is_case_sensitive() {
        // MySQL real-table names default case-sensitive (filesystem
        // fallback), same as BigQuery here.
        assert_eq!(
            reads("SELECT T1.id FROM t1", &MySqlDialect {}, None),
            vec![unresolved("id")]
        );
    }

    #[test]
    fn generic_qualified_table_ref_is_case_insensitive() {
        // The generic dialect folds lower-case, so `T1` matches `t1`
        // and the ref resolves (Inferred — no catalog).
        assert_eq!(
            reads("SELECT T1.id FROM t1", &GenericDialect {}, None),
            vec![read("t1", "id")]
        );
    }

    #[test]
    fn bigquery_alias_ref_is_case_insensitive() {
        // BigQuery aliases fold case-insensitively even though its
        // tables don't: `A` matches the alias `a`, resolving to t1.
        assert_eq!(
            reads("SELECT A.id FROM t1 AS a", &BigQueryDialect {}, None),
            vec![read("t1", "id")]
        );
    }

    #[test]
    fn bigquery_column_is_case_insensitive() {
        // BigQuery columns fold case-insensitively: `Id` matches the
        // catalog's `id`, confirming the resolution on t1.
        let catalog = TestCatalog::default().with("t1", vec!["id"]);
        assert_eq!(
            reads("SELECT Id FROM t1", &BigQueryDialect {}, Some(&catalog)),
            vec![read_confirmed("t1", "Id")]
        );
    }
}
