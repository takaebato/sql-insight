use super::*;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

fn bind_one(sql: &str) -> Plan {
    let dialect = GenericDialect {};
    let statements = Parser::parse_sql(&dialect, sql).unwrap();
    let casing = IdentifierCasing::for_dialect(&dialect);
    build_with_diagnostics(&statements[0], None, casing)
        .0
        .expect("supported statement")
}

fn tref(name: &str) -> TableReference {
    TableReference {
        catalog: None,
        schema: None,
        name: name.into(),
    }
}

fn scan(name: &str) -> Plan {
    Plan::Scan(Scan {
        table: tref(name),
        resolution: ResolutionKind::Inferred,
        role: ScanRole::Read,
    })
}

/// A write-target leaf `Scan` (role = Write): kept in the tree for
/// resolution scope but skipped by table-level read extraction.
fn write_scan(name: &str) -> Plan {
    Plan::Scan(Scan {
        table: tref(name),
        resolution: ResolutionKind::Inferred,
        role: ScanRole::Write,
    })
}

fn project(input: Plan, outputs: Vec<BoundColumn>) -> Plan {
    Plan::Project(Project {
        input: Box::new(input),
        outputs,
        subqueries: Vec::new(),
    })
}

fn pass(inputs: Vec<Plan>, reads: Vec<ColumnRead>) -> Plan {
    Plan::PassThrough(PassThrough {
        inputs,
        reads,
        subqueries: Vec::new(),
    })
}

/// A `WITH` node: named CTE bodies wrapping a body plan.
fn with(ctes: Vec<(&str, Plan)>, body: Plan) -> Plan {
    Plan::With(With {
        ctes: ctes
            .into_iter()
            .map(|(name, plan)| CtePlan {
                name: Ident::new(name),
                plan,
            })
            .collect(),
        body: Box::new(body),
    })
}

/// A lightweight FROM reference to an in-scope CTE.
fn cteref(name: &str) -> Plan {
    Plan::CteRef(CteRef {
        name: Ident::new(name),
    })
}

fn inferred(table: &str, column: &str) -> ColumnRead {
    read(&tref(table), &Ident::new(column), ResolutionKind::Inferred)
}

fn passthrough_col(name: &str, reads: Vec<ColumnRead>) -> BoundColumn {
    BoundColumn {
        name: Some(Ident::new(name)),
        provenance: reads.into_iter().map(passthrough).collect(),
    }
}

fn transform_col(name: &str, reads: Vec<ColumnRead>) -> BoundColumn {
    BoundColumn {
        name: Some(Ident::new(name)),
        provenance: reads
            .into_iter()
            .map(|read| ProvenanceSource {
                read,
                kind: ColumnLineageKind::Transformation,
                synthetic_origin: false,
            })
            .collect(),
    }
}

/// A passthrough output whose sources are reached through a synthetic
/// (derived / CTE) relation — the collapse references that are lineage
/// sources but excluded from `reads`.
fn synthetic_col(name: &str, reads: Vec<ColumnRead>) -> BoundColumn {
    BoundColumn {
        name: Some(Ident::new(name)),
        provenance: reads
            .into_iter()
            .map(|read| ProvenanceSource {
                read,
                kind: ColumnLineageKind::Passthrough,
                synthetic_origin: true,
            })
            .collect(),
    }
}

#[test]
fn single_table_projection() {
    // Project over a Scan; each bare column is an Inferred read of t,
    // forwarded as a passthrough output.
    assert_eq!(
        bind_one("SELECT a, b FROM t"),
        project(
            scan("t"),
            vec![
                passthrough_col("a", vec![inferred("t", "a")]),
                passthrough_col("b", vec![inferred("t", "b")]),
            ],
        )
    );
}

#[test]
fn join_on_and_where_become_passthrough_reads() {
    // FROM x JOIN y ON … is one PassThrough (join); WHERE wraps it in
    // another. The projection's qualified `x.a` resolves to x.
    assert_eq!(
        bind_one("SELECT x.a FROM x JOIN y ON x.id = y.id WHERE y.b > 0"),
        project(
            pass(
                vec![pass(
                    vec![scan("x"), scan("y")],
                    vec![inferred("x", "id"), inferred("y", "id")],
                )],
                vec![inferred("y", "b")],
            ),
            vec![passthrough_col("a", vec![inferred("x", "a")])],
        )
    );
}

#[test]
fn derived_table_exposes_inner_columns_collapsed() {
    // `(SELECT a AS x FROM t) d` becomes a synthetic relation whose
    // output column `x` already carries `t.a` as provenance. The outer
    // `d.x` resolves to that — collapse falls out of construction, so
    // both Projects carry the same inner real column.
    assert_eq!(
        bind_one("SELECT d.x FROM (SELECT a AS x FROM t) d"),
        project(
            project(
                scan("t"),
                vec![passthrough_col("x", vec![inferred("t", "a")])]
            ),
            // The outer `d.x` reaches `t.a` through the derived relation,
            // so it's a synthetic-origin source (a lineage source, not a
            // physical read — that read is the inner Project's).
            vec![synthetic_col("x", vec![inferred("t", "a")])],
        )
    );
}

#[test]
fn cte_reference_resolves_to_inner_columns() {
    // A WITH-bound CTE is a synthetic relation: the body's `id`
    // resolves through it to the real `t.id`, same as a derived table.
    // The CTE body lives once on the `With` node; the FROM reference is
    // a lightweight `CteRef`, not a clone of the body.
    assert_eq!(
        bind_one("WITH c AS (SELECT id FROM t) SELECT id FROM c"),
        with(
            vec![(
                "c",
                project(
                    scan("t"),
                    vec![passthrough_col("id", vec![inferred("t", "id")])]
                )
            )],
            project(
                cteref("c"),
                // Referenced through the CTE relation → synthetic-origin.
                vec![synthetic_col("id", vec![inferred("t", "id")])],
            ),
        )
    );
}

#[test]
fn cte_referenced_twice_keeps_one_shared_body() {
    // Two references to the same CTE share its single body on the `With`
    // node (each FROM item is a `CteRef`), so the body's `t.a` is walked
    // exactly once — not duplicated per reference.
    assert_eq!(
        bind_one("WITH c AS (SELECT a FROM t) SELECT c1.a FROM c c1 JOIN c c2 ON c1.a = c2.a"),
        with(
            vec![(
                "c",
                project(
                    scan("t"),
                    vec![passthrough_col("a", vec![inferred("t", "a")])]
                )
            )],
            project(
                // JOIN of two `CteRef`s; the ON predicate `c1.a = c2.a`
                // resolves through the CTE relations (synthetic-origin),
                // so it contributes no physical reads here.
                pass(vec![cteref("c"), cteref("c")], vec![]),
                vec![synthetic_col("a", vec![inferred("t", "a")])],
            ),
        )
    );
}

#[test]
fn unreferenced_cte_body_is_still_present() {
    // An unreferenced CTE's body still hangs on the `With` node (so its
    // reads surface), while the body plan reads an unrelated table.
    assert_eq!(
        bind_one("WITH c AS (SELECT a FROM t) SELECT b FROM other"),
        with(
            vec![(
                "c",
                project(
                    scan("t"),
                    vec![passthrough_col("a", vec![inferred("t", "a")])]
                )
            )],
            project(
                scan("other"),
                vec![passthrough_col("b", vec![inferred("other", "b")])],
            ),
        )
    );
}

#[test]
fn chained_ctes_resolve_through_the_chain() {
    // `b`'s body reads CTE `a`, and the outer body reads `b`. B
    // resolves the outer `id` end-to-end to the real `t.id` — an
    // improvement over the resolver (whose flat scope yields
    // Ambiguous), so this is pinned here rather than in the
    // differential-parity corpus.
    let Plan::With(with) =
        bind_one("WITH a AS (SELECT id FROM t), b AS (SELECT id FROM a) SELECT id FROM b")
    else {
        panic!("expected With");
    };
    let Plan::Project(body) = with.body.as_ref() else {
        panic!("expected body Project");
    };
    assert_eq!(
        body.outputs,
        vec![synthetic_col("id", vec![inferred("t", "id")])]
    );
}

#[test]
fn subquery_in_where_is_kept_as_a_sub_plan() {
    // `b IN (SELECT id FROM u)`: the outer `b` is a direct filter read;
    // the subquery is kept whole as a sub-plan on the WHERE PassThrough
    // (walked for its `u.id`), not folded into the reads.
    let Plan::Project(outer) = bind_one("SELECT a FROM t WHERE b IN (SELECT id FROM u)") else {
        panic!("expected Project");
    };
    let Plan::PassThrough(where_pt) = outer.input.as_ref() else {
        panic!("expected WHERE PassThrough");
    };
    assert_eq!(where_pt.reads, vec![inferred("t", "b")]);
    assert_eq!(
        where_pt.subqueries,
        vec![project(
            scan("u"),
            vec![passthrough_col("id", vec![inferred("u", "id")])]
        )]
    );
}

#[test]
fn correlated_subquery_resolves_outward() {
    // The EXISTS subquery is a sub-plan on the WHERE PassThrough; inside
    // it, `t.a` finds no `t` in the subquery's own scope `[u]`, so it
    // falls through the correlation stack to the outer `t`.
    let Plan::Project(project) =
        bind_one("SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.x = t.a)")
    else {
        panic!("expected Project");
    };
    let Plan::PassThrough(where_pt) = project.input.as_ref() else {
        panic!("expected WHERE PassThrough");
    };
    // The subquery's own WHERE resolves `u.x` locally and `t.a` outward.
    let [Plan::Project(subquery)] = where_pt.subqueries.as_slice() else {
        panic!("expected one subquery sub-plan");
    };
    let Plan::PassThrough(sub_where) = subquery.input.as_ref() else {
        panic!("expected the subquery's WHERE PassThrough");
    };
    assert_eq!(
        sub_where.reads,
        vec![inferred("u", "x"), inferred("t", "a")]
    );
}

#[test]
fn union_merges_branch_provenance() {
    // A derived table over `UNION` exposes one output `x` whose
    // provenance unions both branches' base columns.
    let Plan::Project(project) =
        bind_one("SELECT x FROM (SELECT a AS x FROM t UNION SELECT b AS x FROM u) d")
    else {
        panic!("expected Project");
    };
    assert_eq!(
        project.outputs,
        vec![synthetic_col(
            "x",
            vec![inferred("t", "a"), inferred("u", "b")]
        )]
    );
}

#[test]
fn recursive_cte_self_reference_traces_the_anchor() {
    // The recursive branch's `FROM c` resolves to the anchor's `id`
    // (→ `t.id`); the body's `id` then unions both branches, both
    // tracing to the same real column.
    let Plan::With(with) = bind_one(
        "WITH RECURSIVE c AS (SELECT id FROM t UNION ALL SELECT id FROM c) SELECT id FROM c",
    ) else {
        panic!("expected With");
    };
    let Plan::Project(body) = with.body.as_ref() else {
        panic!("expected body Project");
    };
    assert_eq!(
        body.outputs,
        vec![synthetic_col(
            "id",
            vec![inferred("t", "id"), inferred("t", "id")]
        )]
    );
}

#[test]
fn insert_select_writes_target_over_source_reads() {
    // The source SELECT is the read-carrying input; the column list is
    // the write target. No reads come from the target.
    assert_eq!(
        bind_one("INSERT INTO target (a, b) SELECT x, y FROM source"),
        Plan::Write(Write {
            target: tref("target"),
            target_columns: vec![Ident::new("a"), Ident::new("b")],
            input: Box::new(project(
                scan("source"),
                vec![
                    passthrough_col("x", vec![inferred("source", "x")]),
                    passthrough_col("y", vec![inferred("source", "y")]),
                ],
            )),
            returning: vec![],
            conflict_updates: vec![],
        })
    );
}

#[test]
fn update_reads_set_rhs_and_predicate() {
    // The SET assignment is a Project output named by its target `c`,
    // whose provenance (the transforming `a + b`) is the lineage
    // source; the WHERE predicate is a filter PassThrough below it.
    assert_eq!(
        bind_one("UPDATE t SET c = a + b WHERE d > 0"),
        Plan::Write(Write {
            target: tref("t"),
            target_columns: vec![Ident::new("c")],
            input: Box::new(project(
                pass(vec![write_scan("t")], vec![inferred("t", "d")]),
                vec![transform_col(
                    "c",
                    vec![inferred("t", "a"), inferred("t", "b")]
                )],
            )),
            returning: vec![],
            conflict_updates: vec![],
        })
    );
}

#[test]
fn delete_reads_predicate_and_writes_no_columns() {
    // DELETE removes whole rows: the predicate is a read, but there are
    // no column writes. The target is a write-role scan in the input
    // (in scope, not a read) and surfaces via `targets`.
    assert_eq!(
        bind_one("DELETE FROM t WHERE d > 0"),
        Plan::Delete(DeletePlan {
            input: Box::new(pass(vec![write_scan("t")], vec![inferred("t", "d")])),
            targets: vec![tref("t")],
            returning: vec![],
        })
    );
}

#[test]
fn using_merge_column_fans_in() {
    // `JOIN y USING (a)` makes the unqualified `a` fan in to both
    // sides (one Inferred source each), not resolve to an ambiguous
    // single column.
    let Plan::Project(project) = bind_one("SELECT a FROM x JOIN y USING (a)") else {
        panic!("expected Project");
    };
    assert_eq!(
        project.outputs,
        vec![passthrough_col(
            "a",
            vec![inferred("x", "a"), inferred("y", "a")]
        )]
    );
}

#[test]
fn unqualified_ref_over_join_is_ambiguous() {
    // Two open relations in scope and no catalog → an unqualified
    // `a` can't be pinned to one, so its provenance is Ambiguous.
    let Plan::Project(project) = bind_one("SELECT a FROM x JOIN y ON x.id = y.id") else {
        panic!("expected Project");
    };
    assert_eq!(
        project.outputs,
        vec![BoundColumn {
            name: Some(Ident::new("a")),
            provenance: vec![passthrough(ambiguous(&Ident::new("a")))],
        }]
    );
}
