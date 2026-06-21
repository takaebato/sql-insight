use crate::support::*;

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
    use sql_insight::sqlparser::dialect::GenericDialect;

    /// `reads` of the first statement, resolved without a catalog.
    fn reads(sql: &str) -> Vec<ColumnRead> {
        extract_column_operations(&GenericDialect {}, sql)
            .unwrap()
            .remove(0)
            .unwrap()
            .reads
    }

    /// Like [`reads`], but parses under PostgreSQL — a few arms
    /// (e.g. `ARRAY[...]` literals) are syntax `GenericDialect` rejects.
    fn reads_pg(sql: &str) -> Vec<ColumnRead> {
        use sql_insight::sqlparser::dialect::PostgreSqlDialect;
        extract_column_operations(&PostgreSqlDialect {}, sql)
            .unwrap()
            .remove(0)
            .unwrap()
            .reads
    }

    /// `lineage` of the first statement, resolved without a catalog.
    fn lineage(sql: &str) -> Vec<ColumnLineageEdge> {
        extract_column_operations(&GenericDialect {}, sql)
            .unwrap()
            .remove(0)
            .unwrap()
            .lineage
    }

    /// A real-table column read against the single table `t`. The
    /// module runs without a catalog, so every resolved ref carries
    /// [`ResolutionKind::Inferred`].
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
            resolution: ResolutionKind::Inferred,
        }
    }

    #[test]
    fn in_unnest() {
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.x IN UNNEST(t.arr)"),
            vec![c("c"), c("x"), c("arr")]
        );
    }

    #[test]
    fn at_time_zone() {
        assert_unordered_eq!(reads("SELECT t.a AT TIME ZONE 'UTC' FROM t"), vec![c("a")]);
    }

    #[test]
    fn position() {
        assert_unordered_eq!(
            reads("SELECT POSITION(t.a IN t.b) FROM t"),
            vec![c("a"), c("b")]
        );
    }

    #[test]
    fn substring() {
        assert_unordered_eq!(
            reads("SELECT SUBSTRING(t.a FROM 1 FOR 2) FROM t"),
            vec![c("a")]
        );
    }

    #[test]
    fn trim() {
        // visit order: trimmed expr (`t.y`) before the trim-what (`t.x`).
        assert_unordered_eq!(
            reads("SELECT TRIM(BOTH t.x FROM t.y) FROM t"),
            vec![c("y"), c("x")]
        );
    }

    #[test]
    fn overlay() {
        assert_unordered_eq!(
            reads("SELECT OVERLAY(t.a PLACING t.b FROM 1 FOR 2) FROM t"),
            vec![c("a"), c("b")]
        );
    }

    #[test]
    fn tuple() {
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE (t.a, t.b) IN ((1, 2))"),
            vec![c("c"), c("a"), c("b")]
        );
    }

    #[test]
    fn dictionary() {
        assert_unordered_eq!(reads("SELECT {'k': t.a} FROM t"), vec![c("a")]);
    }

    #[test]
    fn map() {
        assert_unordered_eq!(reads("SELECT MAP {'k': t.a} FROM t"), vec![c("a")]);
    }

    #[test]
    fn interval() {
        // `INTERVAL t.a DAY` tokenizes oddly under Generic; the `+` form
        // keeps the Interval arm on a plain literal value while the
        // column read comes through the surrounding BinaryOp.
        assert_unordered_eq!(reads("SELECT t.a + INTERVAL '1' DAY FROM t"), vec![c("a")]);
    }

    #[test]
    fn lambda() {
        assert_unordered_eq!(
            reads("SELECT transform(t.arr, x -> t.a) FROM t"),
            vec![c("arr"), c("a")]
        );
    }

    #[test]
    fn member_of() {
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a MEMBER OF (t.b)"),
            vec![c("c"), c("a"), c("b")]
        );
    }

    #[test]
    fn case_operand_and_else() {
        // operand (`t.a`) and else_result (`t.c`) are the arms the
        // existing CASE tests (condition/result only) left uncovered.
        assert_unordered_eq!(
            reads("SELECT CASE t.a WHEN 1 THEN t.b ELSE t.c END FROM t"),
            vec![c("a"), c("b"), c("c")]
        );
    }

    #[test]
    fn bare_identifier_and_literal() {
        // Bare `x` exercises the `Expr::Identifier` arm; `1` the literal
        // no-op arm. Unqualified `x` resolves to the lone table `t`.
        assert_unordered_eq!(reads("SELECT t.d FROM t WHERE x = 1"), vec![c("d"), c("x")]);
    }

    #[test]
    fn function_limit_clause() {
        assert_unordered_eq!(reads("SELECT ARRAY_AGG(t.a LIMIT 5) FROM t"), vec![c("a")]);
    }

    #[test]
    fn function_having_clause() {
        assert_unordered_eq!(
            reads("SELECT ANY_VALUE(t.a HAVING MAX t.b) FROM t"),
            vec![c("a"), c("b")]
        );
    }

    #[test]
    fn listagg_on_overflow_error() {
        assert_unordered_eq!(
            reads("SELECT LISTAGG(t.a, ',' ON OVERFLOW ERROR) FROM t"),
            vec![c("a")]
        );
    }

    #[test]
    fn listagg_on_overflow_truncate() {
        assert_unordered_eq!(
            reads("SELECT LISTAGG(t.a, ',' ON OVERFLOW TRUNCATE '.' WITH COUNT) FROM t"),
            vec![c("a")]
        );
    }

    #[test]
    fn subscript_index() {
        assert_unordered_eq!(reads("SELECT arr[1] FROM t"), vec![c("arr")]);
    }

    #[test]
    fn subscript_slice() {
        assert_unordered_eq!(reads("SELECT arr[1:2] FROM t"), vec![c("arr")]);
    }

    #[test]
    fn dot_access() {
        assert_unordered_eq!(reads("SELECT (t.a).b FROM t"), vec![c("a"), c("b")]);
    }

    #[test]
    fn json_access() {
        assert_unordered_eq!(reads("SELECT t.a -> 'b' FROM t"), vec![c("a")]);
    }

    #[test]
    fn pipe_where_and_select() {
        assert_unordered_eq!(
            reads("FROM t |> WHERE t.a > 1 |> SELECT t.b"),
            vec![c("a"), c("b")]
        );
    }

    #[test]
    fn pipe_order_by_and_limit() {
        assert_unordered_eq!(
            reads("FROM t |> SELECT t.a |> ORDER BY t.a |> LIMIT 1"),
            vec![c("a"), c("a")]
        );
    }

    #[test]
    fn pipe_aggregate() {
        assert_unordered_eq!(
            reads("FROM t |> AGGREGATE SUM(t.a) GROUP BY t.b"),
            vec![c("a"), c("b")]
        );
    }

    #[test]
    fn pipe_set() {
        assert_unordered_eq!(reads("FROM t |> SET a = t.a + 1"), vec![c("a")]);
    }

    #[test]
    fn pipe_set_replaces_slot_case_insensitively() {
        // `|> SET` rewrites a same-named output in place. The slot match folds
        // by the dialect's column case, so on a case-insensitive dialect (here
        // Generic) `SET COL` rewrites the `col` output — one column out, one
        // lineage edge — rather than appending a second `COL`. (Byte-equality
        // matching missed `COL` vs `col` and appended, duplicating the column.)
        assert_column_ops(
            "SELECT col FROM t |> SET COL = col + 1",
            ColumnOperation {
                statement_kind: StatementKind::Select,
                reads: vec![read("t", "col"), read("t", "col")],
                writes: vec![],
                lineage: vec![transformation(col("t", "col"), out("COL", 0))],
                diagnostics: vec![],
            },
        );
    }

    #[test]
    fn pipe_extend() {
        assert_unordered_eq!(reads("FROM t |> EXTEND t.a + 1 AS x"), vec![c("a")]);
    }

    #[test]
    fn pipe_call() {
        assert_unordered_eq!(reads("FROM t |> CALL my_func(t.a)"), vec![c("a")]);
    }

    #[test]
    fn pipe_pivot() {
        // The pivot's aggregate (`SUM(x)`) and value-source column (`k`) are
        // reads; `x` also appears in the projection.
        assert_unordered_eq!(
            reads("SELECT x, k FROM t |> PIVOT (SUM(x) FOR k IN ('a', 'b'))"),
            vec![c("x"), c("k"), c("x")]
        );
    }

    #[test]
    fn window_frame_bound() {
        // A bounded `ROWS BETWEEN … PRECEDING AND … FOLLOWING` frame walks the
        // bound exprs (here literals); the partition / order keys surface as
        // reads.
        assert_unordered_eq!(
            reads("SELECT SUM(t.x) OVER (ORDER BY t.y ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) FROM t"),
            vec![c("x"), c("y")]
        );
    }

    #[test]
    fn grouping_sets() {
        // `GROUP BY GROUPING SETS ((a), (b))`: each set member is a key read.
        assert_unordered_eq!(
            reads("SELECT t.a FROM t GROUP BY GROUPING SETS ((t.a), (t.b))"),
            vec![c("a"), c("a"), c("b")]
        );
    }

    #[test]
    fn limit_offset_comma() {
        // The `LIMIT <offset>, <limit>` form (`OffsetCommaLimit`): both
        // operands are filter-position reads (here literals).
        assert_unordered_eq!(reads("SELECT t.a FROM t LIMIT 1, 2"), vec![c("a")]);
    }

    #[test]
    fn aggregate_with_order_by_clause() {
        // An aggregate's `ORDER BY` clause: the value arg (`x`) and the
        // suppressed ordering key (`y`) both surface as reads.
        assert_unordered_eq!(
            reads("SELECT ARRAY_AGG(t.x ORDER BY t.y) FROM t"),
            vec![c("x"), c("y")]
        );
    }

    // --- pipe output lineage: an output-producing pipe operator (SELECT /
    // EXTEND / AGGREGATE / SET) projects its value expressions, so they feed
    // `QueryOutput` lineage; filter operators (WHERE / ORDER BY / LIMIT /
    // CALL) reshape nothing and emit no lineage of their own.

    #[test]
    fn pipe_select_emits_output_lineage() {
        assert_unordered_eq!(
            lineage("FROM t |> SELECT t.b"),
            vec![passthrough(c("b"), out("b", 0))]
        );
    }

    #[test]
    fn pipe_where_then_select_lineage() {
        // The WHERE is a filter (its `t.a` is a read, not a source); only
        // the trailing SELECT output feeds lineage.
        assert_unordered_eq!(
            lineage("FROM t |> WHERE t.a > 1 |> SELECT t.b"),
            vec![passthrough(c("b"), out("b", 0))]
        );
    }

    #[test]
    fn pipe_extend_emits_transformation_lineage() {
        assert_unordered_eq!(
            lineage("FROM t |> EXTEND t.a + 1 AS x"),
            vec![transformation(c("a"), out("x", 0))]
        );
    }

    #[test]
    fn pipe_set_emits_transformation_lineage() {
        assert_unordered_eq!(
            lineage("FROM t |> SET a = t.a + 1"),
            vec![transformation(c("a"), out("a", 0))]
        );
    }

    #[test]
    fn pipe_aggregate_emits_lineage() {
        // SUM(t.a) is a transforming output; the GROUP BY key t.b passes
        // through as a second output column.
        assert_unordered_eq!(
            lineage("FROM t |> AGGREGATE SUM(t.a) GROUP BY t.b"),
            vec![
                transformation(c("a"), out_anon(0)),
                passthrough(c("b"), out("b", 1)),
            ]
        );
    }

    #[test]
    fn pipe_select_then_order_and_limit_lineage() {
        // ORDER BY / LIMIT are filters over the SELECT output — they add no
        // lineage; only the SELECT projection feeds it.
        assert_unordered_eq!(
            lineage("FROM t |> SELECT t.a |> ORDER BY t.a |> LIMIT 1"),
            vec![passthrough(c("a"), out("a", 0))]
        );
    }

    #[test]
    fn pipe_call_emits_no_output_lineage() {
        assert_unordered_eq!(lineage("FROM t |> CALL my_func(t.a)"), vec![]);
    }

    #[test]
    fn unary_op() {
        // Expr::UnaryOp — `-t.a`
        assert_unordered_eq!(reads("SELECT -t.a FROM t"), vec![c("a")]);
    }

    #[test]
    fn binary_op() {
        // Expr::BinaryOp — `t.a + t.b`
        assert_unordered_eq!(reads("SELECT t.a + t.b FROM t"), vec![c("a"), c("b")]);
    }

    #[test]
    fn nested() {
        // Expr::Nested — `(t.a + t.b)`
        assert_unordered_eq!(reads("SELECT (t.a + t.b) FROM t"), vec![c("a"), c("b")]);
    }

    #[test]
    fn between() {
        // Expr::Between — `t.a BETWEEN t.lo AND t.hi`
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a BETWEEN t.lo AND t.hi"),
            vec![c("c"), c("a"), c("lo"), c("hi")]
        );
    }

    #[test]
    fn in_list() {
        // Expr::InList — `t.a IN (t.b, t.d)`
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a IN (t.b, t.d)"),
            vec![c("c"), c("a"), c("b"), c("d")]
        );
    }

    #[test]
    fn like() {
        // Expr::Like — `t.a LIKE t.pat`
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a LIKE t.pat"),
            vec![c("c"), c("a"), c("pat")]
        );
    }

    #[test]
    fn ilike() {
        // Expr::ILike — `t.a ILIKE t.pat`
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a ILIKE t.pat"),
            vec![c("c"), c("a"), c("pat")]
        );
    }

    #[test]
    fn similar_to() {
        // Expr::SimilarTo — `t.a SIMILAR TO t.pat`
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a SIMILAR TO t.pat"),
            vec![c("c"), c("a"), c("pat")]
        );
    }

    #[test]
    fn cast() {
        // Expr::Cast — `CAST(t.a AS INT)`
        assert_unordered_eq!(reads("SELECT CAST(t.a AS INT) FROM t"), vec![c("a")]);
    }

    #[test]
    fn extract() {
        // Expr::Extract — `EXTRACT(YEAR FROM t.ts)`
        assert_unordered_eq!(
            reads("SELECT EXTRACT(YEAR FROM t.ts) FROM t"),
            vec![c("ts")]
        );
    }

    #[test]
    fn is_true() {
        // Expr::IsTrue — `t.a IS TRUE`
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS TRUE"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_distinct_from() {
        // Expr::IsDistinctFrom — `t.a IS DISTINCT FROM t.b`
        assert_unordered_eq!(
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
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE EXISTS (SELECT t.b FROM t)"),
            vec![c("c"), c("b")]
        );
    }

    #[test]
    fn any_op() {
        // Expr::AnyOp — `t.a = ANY (SELECT t.b FROM t)`
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a = ANY (SELECT t.b FROM t)"),
            vec![c("c"), c("a"), c("b")]
        );
    }

    #[test]
    fn all_op() {
        // Expr::AllOp — sibling of AnyOp in the chained-pattern arm.
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a = ALL (SELECT t.b FROM t)"),
            vec![c("c"), c("a"), c("b")]
        );
    }

    #[test]
    fn is_not_distinct_from() {
        // Expr::IsNotDistinctFrom — `IS DISTINCT FROM` already covered;
        // the negated form is a distinct AST variant.
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS NOT DISTINCT FROM t.b"),
            vec![c("c"), c("a"), c("b")]
        );
    }

    #[test]
    fn is_false() {
        // Expr::IsFalse — `t.a IS FALSE`
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS FALSE"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_not_false() {
        // Expr::IsNotFalse — `t.a IS NOT FALSE`
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS NOT FALSE"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_not_true() {
        // Expr::IsNotTrue — sibling of IsTrue in the chained-pattern arm.
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS NOT TRUE"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_unknown() {
        // Expr::IsUnknown — SQL three-valued-logic predicate.
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS UNKNOWN"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_not_unknown() {
        // Expr::IsNotUnknown — negated three-valued-logic predicate.
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS NOT UNKNOWN"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn is_normalized() {
        // Expr::IsNormalized — Unicode normalization predicate
        // (`t.a IS [form] NORMALIZED`); arm visits `expr` only.
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a IS NORMALIZED"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn ceil_to_field() {
        // Expr::Ceil — the `CEIL(<expr> TO <field>)` form. Plain
        // `CEIL(<expr>)` parses as a function call (Expr::Function),
        // not the Ceil variant; the `TO <field>` is what triggers it.
        assert_unordered_eq!(reads("SELECT CEIL(t.a TO YEAR) FROM t"), vec![c("a")]);
    }

    #[test]
    fn floor_to_field() {
        // Expr::Floor — sibling of Ceil; same `TO <field>` form.
        assert_unordered_eq!(reads("SELECT FLOOR(t.a TO YEAR) FROM t"), vec![c("a")]);
    }

    #[test]
    fn rlike() {
        // Expr::RLike — MySQL regex match operator; sibling of
        // Like / ILike / SimilarTo in the chained-pattern arm.
        assert_unordered_eq!(
            reads("SELECT t.c FROM t WHERE t.a RLIKE 'pat'"),
            vec![c("c"), c("a")]
        );
    }

    #[test]
    fn convert_using() {
        // Expr::Convert — `CONVERT(<expr> USING <charset>)` form
        // (MySQL / PostgreSQL); arm walks expr + each style.
        assert_unordered_eq!(reads("SELECT CONVERT(t.a USING utf8) FROM t"), vec![c("a")]);
    }

    #[test]
    fn trim_characters() {
        // Expr::Trim — the `TRIM(<expr>, <chars>...)` comma form sets
        // `trim_characters: Some(Vec<Expr>)` (vs the `FROM` form which
        // sets `trim_what` instead). Existing `trim` test covers the
        // FROM path; this covers the chars-list path.
        assert_unordered_eq!(reads("SELECT TRIM(t.y, t.a) FROM t"), vec![c("y"), c("a")]);
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
    use sql_insight::sqlparser::dialect::{Dialect, GenericDialect};

    fn op(sql: &str) -> ColumnOperation {
        op_with(sql, &GenericDialect {})
    }

    fn op_with(sql: &str, dialect: &dyn Dialect) -> ColumnOperation {
        extract_column_operations(dialect, sql)
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
            resolution: ResolutionKind::Inferred,
        }
    }

    /// Write-side build: stays bare [`ColumnReference`] since
    /// `writes` is `Vec<ColumnReference>` (write targets come from
    /// SQL syntax and don't carry a resolution).
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
        assert_unordered_eq!(
            reads("SELECT t1.a FROM t1 JOIN t2 ON t1.id = t2.id"),
            vec![c("t1", "id"), c("t2", "id"), c("t1", "a")]
        );
    }

    #[test]
    fn join_using() {
        assert_unordered_eq!(
            reads("SELECT t1.a FROM t1 JOIN t2 USING (id)"),
            vec![c("t1", "a")]
        );
    }

    #[test]
    fn join_natural() {
        assert_unordered_eq!(
            reads("SELECT t1.a FROM t1 NATURAL JOIN t2"),
            vec![c("t1", "a")]
        );
    }

    #[test]
    fn join_left_outer() {
        assert_unordered_eq!(
            reads("SELECT t1.a FROM t1 LEFT JOIN t2 ON t1.id = t2.id"),
            vec![c("t1", "id"), c("t2", "id"), c("t1", "a")]
        );
    }

    #[test]
    fn join_cross() {
        assert_unordered_eq!(
            reads("SELECT t1.a FROM t1 CROSS JOIN t2"),
            vec![c("t1", "a")]
        );
    }

    #[test]
    fn nested_join() {
        assert_unordered_eq!(
            reads("SELECT t1.a FROM (t1 JOIN t2 ON t1.id = t2.id)"),
            vec![c("t1", "id"), c("t2", "id"), c("t1", "a")]
        );
    }

    // ---- table.rs: derived / function / unnest / pivot / sample ----

    #[test]
    fn derived_table() {
        // The derived table's synthetic column (`x.a`) is dropped; only
        // the real-storage read from inside the subquery surfaces.
        assert_unordered_eq!(
            reads("SELECT x.a FROM (SELECT t.a FROM t) x"),
            vec![c("t", "a")]
        );
    }

    #[test]
    fn derived_table_with_column_aliases() {
        assert_unordered_eq!(
            reads("SELECT x.c1 FROM (SELECT t.a FROM t) x (c1)"),
            vec![c("t", "a")]
        );
    }

    #[test]
    fn alias_columns_rename_through_sort_body() {
        // A derived table with an alias column list whose body is ORDER-BY
        // topped: the rename walks through the `Sort` to the projection.
        assert_unordered_eq!(
            reads("SELECT x.c1 FROM (SELECT t.a FROM t ORDER BY t.a) x (c1)"),
            vec![c("t", "a"), c("t", "a")]
        );
    }

    #[test]
    fn alias_columns_rename_through_setop_body() {
        // A CTE alias column list over a UNION body: the rename walks both
        // `SetOp` branches.
        assert_unordered_eq!(
            reads("WITH cte (c1) AS (SELECT a FROM s1 UNION SELECT b FROM s2) SELECT c1 FROM cte"),
            vec![c("s1", "a"), c("s2", "b")]
        );
    }

    #[test]
    fn alias_columns_rename_through_with_body() {
        // A derived table whose body is a `WITH`: the rename walks through
        // the `With` into its body projection.
        assert_unordered_eq!(
            reads("SELECT x.c1 FROM (WITH w AS (SELECT a FROM s) SELECT w.a FROM w) x (c1)"),
            vec![c("s", "a")]
        );
    }

    #[test]
    fn pipe_union_branch_reads_surface() {
        // A `|> UNION ALL (subquery)` pipe: the branch query's reads surface
        // (modelled as a filter-position subquery) alongside the input's.
        assert_unordered_eq!(
            reads("SELECT t.a FROM t |> UNION ALL (SELECT u.b FROM u)"),
            vec![c("t", "a"), c("u", "b")]
        );
    }

    #[test]
    fn table_function() {
        // The function arg `t.a` is walked, but `t` is not in FROM (the
        // sole relation is the table function `g`), so the qualified ref
        // resolves to nothing → unresolved. Coverage point preserved:
        // the column `a` still surfaces, proving the arg was walked.
        assert_unordered_eq!(
            reads("SELECT g.v FROM TABLE(gen(t.a)) g"),
            vec![unresolved("a")]
        );
    }

    #[test]
    fn unnest() {
        // `t` is not in FROM (only the UNNEST relation `u`), so `t.arr`
        // is unresolved; the column still surfaces (arg walked).
        assert_unordered_eq!(
            reads("SELECT u.x FROM UNNEST(t.arr) u"),
            vec![unresolved("arr")]
        );
    }

    #[test]
    fn pivot() {
        let result = op("SELECT * FROM t PIVOT(SUM(t.amt) FOR t.mon IN ('a', 'b'))");
        assert_unordered_eq!(result.reads, vec![c("t", "amt"), c("t", "mon")]);
    }

    #[test]
    fn unpivot() {
        let result = op("SELECT * FROM t UNPIVOT(v FOR n IN (t.a, t.b))");
        assert_unordered_eq!(result.reads, vec![c("t", "v"), c("t", "a"), c("t", "b")]);
    }

    #[test]
    fn tablesample() {
        assert_unordered_eq!(
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
        assert_unordered_eq!(reads(sql), vec![c("t", "a"), c("t", "b"), c("t", "c")]);
    }

    #[test]
    fn json_table() {
        // TableFactor::JsonTable — the `json_expr` (`t.doc`) is walked;
        // `t` is not in FROM (only the JSON_TABLE relation `j`), so it
        // surfaces unresolved. The `COLUMNS (...)` schema declares
        // synthetic outputs, and the `j.x` projection rides through them
        // so it is dropped.
        assert_unordered_eq!(
            reads("SELECT j.x FROM JSON_TABLE(t.doc, '$' COLUMNS (x INT PATH '$.x')) AS j"),
            vec![unresolved("doc")]
        );
    }

    #[test]
    fn open_json_table() {
        // TableFactor::OpenJsonTable — same visit shape as JsonTable;
        // `t.doc` is walked but `t` is not in FROM → unresolved. The
        // WITH-declared columns are synthetic.
        assert_unordered_eq!(
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
        assert_unordered_eq!(
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
        use sql_insight::sqlparser::dialect::SnowflakeDialect;
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
        assert_unordered_eq!(
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
        assert_unordered_eq!(
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
        assert_unordered_eq!(result.reads, vec![c("t1", "a")]);
        assert_eq!(result.writes, vec![w("t2", "a")]);
    }

    #[test]
    fn create_view() {
        let result = op("CREATE VIEW v AS SELECT t1.a FROM t1");
        assert_unordered_eq!(result.reads, vec![c("t1", "a")]);
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
        assert_unordered_eq!(reads("DELETE FROM t1 WHERE t1.a = 1"), vec![c("t1", "a")]);
    }

    #[test]
    fn update() {
        let result = op("UPDATE t1 SET a = t1.b WHERE t1.c = 1");
        assert_unordered_eq!(result.reads, vec![c("t1", "b"), c("t1", "c")]);
        assert_eq!(result.writes, vec![w("t1", "a")]);
    }

    #[test]
    fn insert_returning() {
        let result = op("INSERT INTO t1 (a) VALUES (1) RETURNING t1.a");
        assert_unordered_eq!(result.reads, vec![c("t1", "a")]);
        assert_eq!(result.writes, vec![w("t1", "a")]);
    }

    #[test]
    fn insert_from_select() {
        let result = op("INSERT INTO t1 (a) SELECT t2.b FROM t2");
        assert_unordered_eq!(result.reads, vec![c("t2", "b")]);
        assert_eq!(result.writes, vec![w("t1", "a")]);
    }

    // ---- query.rs: set operations / clauses ----

    #[test]
    fn union() {
        assert_unordered_eq!(
            reads("SELECT t1.a FROM t1 UNION SELECT t2.b FROM t2"),
            vec![c("t1", "a"), c("t2", "b")]
        );
    }

    #[test]
    fn intersect() {
        assert_unordered_eq!(
            reads("SELECT t1.a FROM t1 INTERSECT SELECT t2.b FROM t2"),
            vec![c("t1", "a"), c("t2", "b")]
        );
    }

    #[test]
    fn except() {
        assert_unordered_eq!(
            reads("SELECT t1.a FROM t1 EXCEPT SELECT t2.b FROM t2"),
            vec![c("t1", "a"), c("t2", "b")]
        );
    }

    #[test]
    fn values() {
        // Bare VALUES has no column references; exercises `visit_values`.
        assert_unordered_eq!(op("VALUES (1, 2), (3, 4)").reads, vec![]);
    }

    #[test]
    fn group_by_cube() {
        assert_unordered_eq!(
            reads("SELECT t.a FROM t GROUP BY CUBE(t.a)"),
            vec![c("t", "a"), c("t", "a")]
        );
    }

    #[test]
    fn group_by_rollup() {
        assert_unordered_eq!(
            reads("SELECT t.a FROM t GROUP BY ROLLUP(t.a)"),
            vec![c("t", "a"), c("t", "a")]
        );
    }

    #[test]
    fn order_by_limit_offset() {
        assert_unordered_eq!(
            reads("SELECT t.a FROM t ORDER BY t.a LIMIT 5 OFFSET 2"),
            vec![c("t", "a"), c("t", "a")]
        );
    }

    #[test]
    fn select_distinct() {
        assert_unordered_eq!(reads("SELECT DISTINCT t.a FROM t"), vec![c("t", "a")]);
    }

    #[test]
    fn having() {
        assert_unordered_eq!(
            reads("SELECT t.a FROM t GROUP BY t.a HAVING t.a > 1"),
            vec![c("t", "a"), c("t", "a"), c("t", "a")]
        );
    }

    #[test]
    fn qualify() {
        assert_unordered_eq!(
            reads("SELECT t.a FROM t QUALIFY ROW_NUMBER() OVER () = 1"),
            vec![c("t", "a")]
        );
    }

    // ---- query.rs: cold query/select arms ----

    #[test]
    fn fetch() {
        // `visit_fetch` walks the FETCH quantity expression; here the
        // literal `10` contributes no reads.
        assert_unordered_eq!(
            reads("SELECT t.a FROM t FETCH FIRST 10 ROWS ONLY"),
            vec![c("t", "a")]
        );
    }

    #[test]
    fn set_expr_query() {
        // A bare `(SELECT ...)` parses with the outer body as
        // `SetExpr::Query`, which bubbles its inner projections through
        // the parenthesized wrapper.
        assert_unordered_eq!(reads("(SELECT t.a FROM t)"), vec![c("t", "a")]);
    }

    #[test]
    fn distinct_on() {
        // `Distinct::On` exprs walk before the projection, so `t.a`
        // (the DISTINCT ON key) lands first.
        assert_unordered_eq!(
            reads("SELECT DISTINCT ON (t.a) t.b FROM t"),
            vec![c("t", "a"), c("t", "b")]
        );
    }

    #[test]
    fn top_with_expr_quantity() {
        // `TopQuantity::Expr` walks the quantity expression — the
        // `Number` variant is the constant path and stays uncovered.
        assert_unordered_eq!(
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
        assert_unordered_eq!(result.reads, vec![c("t", "a")]);
        assert_eq!(result.writes, vec![]);
    }

    #[test]
    fn lateral_view() {
        // `select.lateral_views` walks each `lateral_view` expression
        // (here `EXPLODE(t.arr)`); the `v` alias is not bound as a real
        // table, so we read against `t` to keep the assertion stable.
        assert_unordered_eq!(
            reads("SELECT t.a FROM t LATERAL VIEW EXPLODE(t.arr) v AS x"),
            vec![c("t", "a"), c("t", "arr")]
        );
    }

    #[test]
    fn prewhere() {
        // ClickHouse `PREWHERE` rides the same predicate-walk array as
        // selection / having / qualify.
        assert_unordered_eq!(
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
        assert_unordered_eq!(
            result.reads,
            vec![c("t", "a"), c("t", "b"), c("t", "c"), c("t", "d")]
        );
    }

    #[test]
    fn sort_by() {
        // Hive-style `SORT BY` (per-reducer ordering, distinct from
        // `ORDER BY`); each entry visits as an order-by expression.
        assert_unordered_eq!(
            reads("SELECT t.a FROM t SORT BY t.b"),
            vec![c("t", "a"), c("t", "b")]
        );
    }

    #[test]
    fn named_window() {
        // `NamedWindowExpr::WindowSpec` walks PARTITION BY / ORDER BY
        // inside a `WINDOW w AS (...)` definition.
        assert_unordered_eq!(
            reads("SELECT t.a FROM t WINDOW w AS (PARTITION BY t.b)"),
            vec![c("t", "a"), c("t", "b")]
        );
    }

    #[test]
    fn qualified_wildcard_expr() {
        // Snowflake-only `(expr).*` syntax — `QualifiedWildcard::Expr`
        // arm records WildcardSuppressed and also walks the underlying
        // expression so its real-table refs still surface.
        use sql_insight::sqlparser::dialect::SnowflakeDialect;
        let result = op_with("SELECT (t.a).* FROM t", &SnowflakeDialect {});
        assert_unordered_eq!(result.reads, vec![c("t", "a")]);
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
        assert_unordered_eq!(reads("SELECT t.a FROM t GROUP BY ALL"), vec![c("t", "a")]);
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
        assert_unordered_eq!(result.reads, vec![]);
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
        assert_unordered_eq!(result.reads, vec![c("t", "a")]);
        assert_eq!(result.writes, vec![w("v", "a")]);
    }

    #[test]
    fn create_virtual_table() {
        // `CREATE VIRTUAL TABLE` (SQLite / virtual table modules) binds
        // the table as a write target with no column-level writes —
        // the column schema is module-defined, not part of the SQL.
        let result = op("CREATE VIRTUAL TABLE vt USING mymod");
        assert_unordered_eq!(result.reads, vec![]);
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
