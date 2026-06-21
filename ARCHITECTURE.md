# Architecture

How `sql-insight` turns a SQL string into structured facts. This is the
*design* — the why and the shape. Per-type contracts live in rustdoc
(`cargo doc --document-private-items`); this file doesn't restate them.

## The engine

The private `resolver` module is the whole analysis engine. It binds a
parsed `Statement` into a **materialized logical-plan tree**
(`logical_plan::LogicalPlan`) and walks that tree for the extraction
surfaces. It is *not* an execution plan — nothing optimizes or runs SQL.

Three layers:

- **`resolver.rs` — the facade.** `build` (bind) plus seven free-function
  surfaces (`reads` / `table_reads` / `writes` / `table_writes` /
  `column_lineage` / `table_lineage` / `flat_tables`). Each is a thin entry
  delegating to a concern submodule, so `LogicalPlan` stays plain data —
  extraction is a pass *over* the tree, not a method *on* it.
- **`binder` — bind (AST → tree).** One pass, split by concern into
  `binder/{statement, query, expr, resolve}` (each an `impl Binder` block)
  over the shared `binder/scope` model. The root holds the `Binder` context.
- **`reads` / `origins` / `lineage` / `tables` — the walkers.** `origins`
  is the `getColumnOrigins`-style value trace that `lineage` builds on;
  `tables` covers the write surfaces and the flat table list. The
  `extractor` layer wraps these into the public `*_operations` APIs.

`LogicalPlan` is a **standard relational-algebra tree** — `Scan` / `Filter`
/ `Join` / `Aggregate` / `Projection` / `Sort` / `SetOp` / `SubqueryAlias`
/ `TableFunction` / `With` + `CteRef` / `Values`, plus distinct DML / DDL
roots. The point is **recognizability**: anyone who knows logical plans can
read and extend it. The clause stack is canonical: `Scan → Filter(WHERE) →
Aggregate(GROUP BY) → Filter(HAVING) → Projection → Sort(ORDER BY)`.

## Materialize, then walk

The binder builds a *complete* tree in one pass; extraction is a pure walk.
Where a bind shortcut would throw information away (folding a subquery,
dropping an unreferenced CTE, discarding a role), raise the plan's
granularity so it's carried *in* the tree rather than routed around it via a
side channel.

**Why a materialized tree** (rather than fusing bind and extraction into one
AST pass): the tree exists for the **breadth**, not the lineage collapse. It
is walked by seven independent surfaces, each with its own traversal rules; a
complete tree lets each be a separate pure walk instead of seven rule-sets
interleaved into the bind. For column-lineage collapse *alone* a fused
bind+collapse pass would suffice — the tree is the price of that breadth,
paid once and discarded (nothing optimizes or reuses it).

## Lineage at walk time, not pre-collapsed

A resolved column reference on the tree is a `BoundColumn` whose `binding`
is `Base` / `Derived` / `Unresolved` / `Ambiguous`. No provenance is baked
onto the nodes: the `origins` walk collapses a `Derived` reference to its
real base columns *lazily*, tracing into the producing `Projection` /
`CteRef` / `SubqueryAlias` and composing the lineage kind end-to-end.
`reads` simply drops `Derived` (its physical read was already counted at the
inner producer). So both surfaces fall out of a pure walk of a clean tree.

## Value vs filter is structural

The single cleanest distinction in the design: a column that **contributes a
value** vs one that only **filters**. It is positional in the tree, not a
tag:

- A value operand lowers to `Expr::Call` / `Expr::Window` `arg` / a scalar
  `Expr::Subquery` — the `origins` trace follows these.
- A filter operand is bucketed into `Expr::Filter`, or is a construct's
  filter slot (a `CASE` `when`, a window `partition` / `order` key,
  `Expr::Exists` / `Expr::InSubquery`) — `origins` skips it.
- The clause split is structural too: WHERE / HAVING are `Filter` nodes, the
  SELECT list is the `Projection`.

So the walk needs no clause tag: `reads` walks every column, `origins`
follows only value operands. **A value contributor is a `lineage` source; a
filter-only column is in `reads` but not `lineage`** — that *is* the
value/filter distinction, surfaced.

## Scope is scratch

Bind returns `(LogicalPlan, Scope)` bottom-up. The `Scope` (the subtree's
FROM relations + introduced outputs + USING merge columns) is **scratch** —
threaded up, never stored on the tree. Enclosing scopes (correlation) and
in-scope CTEs live on the `Binder` (`outer` / `ctes`). Don't push caller
state in via flag bags — spawn a child binder (`with_ctes` / `with_outer`)
and thread the `Scope`.

A DML target is **not a `Scan`** in the tree — it's named on the DML root
(`Insert.target`, …), and the root's `input` carries only the *read*
relations. So a write target never lands in `reads` by construction; it
surfaces only through `writes` / `table_writes`. Its columns are still bound
(for SET / WHERE / RETURNING) via a scratch target `Scope`, then discarded.

## Catalog and identity

The binder takes an optional `&Catalog`. With one, a matched table's columns
are known (resolution turns strict — typos become `Unresolved`, multi-hits
`Ambiguous`); catalog-free, columns are unknown and resolved reads are
`Inferred`. A table the catalog doesn't contain is *schema-unknown*, not
nonexistent — open-world.

**Canonicalization.** `table_match` rewrites a uniquely matched reference to
its registered `catalog.schema.name` path, so a bare `users` and an explicit
`public.users` agree (write targets too). The canonical identity is
**case-exact**, so it surfaces *quoted* with the dialect's `canonical_quote`
— otherwise a later qualifier fold under an upper-folding dialect would
re-case it and fail to match its own relation. On a miss / ambiguous match
the ref surfaces *default-normalized* (`surface_with_defaults`): omitted
prefixes filled from the catalog's defaults as *unquoted* (foldable) idents
— an unconfirmed search-path qualifier that must still fold-match a plain
column qualifier. With no catalog, the ref stays exactly as written, so
`mydb.users` and `otherdb.users` stay distinct.

**Two-level equality.** References derive *structural* `Eq` / `Hash` (case-
and quote-sensitive) — correct for catalog-backed analysis (matched refs are
canonicalized) and direct comparison. For catalog-free dedup across
fold-equivalent spellings (`users` vs `USERS`), `identity_key` /
`same_table` / `same_column` fold by a dialect's casing; the key is opaque
so the folded text never surfaces.

## Casing

Identifier matching is dialect-aware (`crate::casing`). Two orthogonal
concerns, bundled internally as `IdentifierStyle { casing, quote }`:

- **casing** — a per-class (`table` / `table_alias` / `column`) fold rule
  (`CaseRule`), a *matching policy* the caller may override via
  `ExtractorOptions::with_casing`. Comparisons fold through
  `CaseRule::normalize`, which keys on quoted-vs-unquoted and the fold,
  never the quote *char*.
- **quote** — the dialect's surface quote char for a canonical identity, a
  *surface* concern, always dialect-derived.

The dialect→rule and dialect→quote maps live in `for_dialect` /
`canonical_quote` (with the matrix unit tests as their ground truth) — not
duplicated here. Note `canonical_quote` is *self-maintained*, not
sqlparser's `identifier_quote_style` (which is unset for most dialects).
Filesystem- / collation- / config-dependent models (MySQL table names, SQL
Server, Redshift `enable_case_sensitive_identifier`, Snowflake
`QUOTED_IDENTIFIERS_IGNORE_CASE`) resolve to a fixed safe default; a
per-deployment override API is a future addition. Folding affects matching
only — a *written* (non-canonical) reference keeps its original text.

## Deliberate non-goals

- **Wildcards (`SELECT *`, `t.*`) are not expanded** — even with a catalog.
  The rigor cost (USING / NATURAL merge, EXCLUDE / REPLACE / RENAME, CTE
  column rename, multi-segment qualifiers) is too high for a SQL-text-only
  library to get right. Wildcards contribute nothing to `reads` / `lineage`;
  consumers needing per-column lineage supply resolved plans or expand
  themselves.
- **NATURAL JOIN is not expanded** (its merge set needs both schemas — same
  reason as wildcards). `JOIN … USING (col)`, however, **fans in**: an
  unqualified ref to a merge column resolves to *every* joined relation that
  could own it (one read / lineage source per side, not an ambiguous
  `table: None`). A catalog narrows the fan-in to declaring relations;
  catalog-free reaches every joined relation. Known limit: a 3+-relation
  catalog-free fan-in includes every relation, not just the two USING
  operands.
- **Clause-alias visibility** (GROUP BY / HAVING / ORDER BY) is structural:
  those clauses resolve against a scope that includes the `Projection`'s
  outputs, while WHERE (below the `Projection`) does not. A bare ref naming
  an *introduced* alias (`ORDER BY total` for `a+b AS total`) binds
  `Derived` — a lineage source, dropped from `reads` (no phantom read; the
  dependency is already at the projection). **Identity is name-equality, not
  alias presence**: `a AS a` is identity (keeps the `GROUP BY a` read), `a AS
  x` is not. Dialect-specific alias-vs-column precedence is not modelled.
