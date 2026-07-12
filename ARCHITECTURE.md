# Architecture

How `sql-insight` turns a SQL string into structured facts — the why, the
shape, and the rationale. Per-type contracts live in rustdoc (`cargo doc
--document-private-items`); this file doesn't restate them.

The crate has two independent halves:

- **Extraction** — "what does this SQL touch?" The bulk of the design, and
  the rest of this document. A four-stage pipeline below.
- **Rewriting** — `formatter` (pretty-print) and `normalizer` (abstract
  literals to placeholders). These are standalone `VisitorMut` passes over
  the sqlparser AST; they share *nothing* with the extraction engine (no
  bind, no plan, no catalog). Don't look for them below.

## The extraction pipeline

```
SQL ──parse──▶ AST ──bind──▶ LogicalPlan ──walk──▶ surfaces ──wrap──▶ public API
     sqlparser       binder      (the plan)   reads/origins/   extractor
                                              lineage/tables
```

- **parse** — sqlparser. We always work against its AST, never re-parse.
- **bind** (`resolver::binder`) — AST → a materialized `LogicalPlan` tree,
  resolving every column reference against the bind-time scope. One pass.
- **walk** (`resolver::{reads, origins, lineage, tables}`) — pure
  traversals of the plan, each producing one extraction surface.
- **wrap** (`extractor`) — thin public functions bundling the surfaces into
  `TableOperation` / `ColumnOperation` etc.

`resolver.rs` is the **facade** tying bind + walk together: `build` plus
seven free-function surfaces (`reads` / `table_reads` / `writes` /
`table_writes` / `column_lineage` / `table_lineage` / `flat_tables`), each a
thin entry delegating to a walker. `LogicalPlan` stays plain data —
extraction is a pass *over* it, not a method *on* it. Supporting inputs:
`casing` (dialect identifier folding) and `catalog` (optional schema
registry) feed the bind; `reference` / `diagnostic` carry the output
vocabulary.

It is **not** an execution plan — nothing optimizes or runs SQL.

## The plan (`LogicalPlan`)

The central data structure the pipeline pivots on: a **standard
relational-algebra tree** — `Scan` / `Filter` / `Join` / `Aggregate` /
`Projection` / `Sort` / `SetOp` / `SubqueryAlias` / `TableFunction` / `With`
+ `CteRef` / `Values`, plus distinct DML / DDL roots. The point is
**recognizability**: anyone who knows logical plans can read and extend it.
The clause stack is canonical: `Scan → Filter(WHERE) → Aggregate(GROUP BY)
→ Filter(HAVING) → Projection → Sort(ORDER BY)`.

**Why a materialized tree** (rather than fusing bind and extraction into one
AST pass): the tree exists for the **breadth**, not the lineage collapse. It
is walked by seven independent surfaces, each with its own traversal rules; a
complete tree lets each be a separate pure walk instead of seven rule-sets
interleaved into the bind. For column-lineage collapse *alone* a fused
bind+collapse pass would suffice — the tree is the price of that breadth,
paid once and discarded.

So **keep the plan complete**: where a bind shortcut would throw information
away (folding a subquery, dropping an unreferenced CTE, discarding a role),
raise the plan's granularity to carry it *in* the tree rather than route it
around via a side channel.

## Binding (AST → plan)

One pass, split by concern into `binder/{statement, query, expr, resolve}`
(each an `impl Binder` block) over a shared `binder/scope` model.

### Scope is scratch

Bind returns `(LogicalPlan, Scope)` bottom-up. The `Scope` (the subtree's
FROM relations + introduced outputs + USING merge columns) is threaded up,
**never stored on the tree**. Enclosing scopes (correlation) and in-scope
CTEs live on the `Binder` (`outer` / `ctes`). Don't push caller state in via
flag bags — spawn a child binder (`with_ctes` / `with_outer`) and thread the
`Scope`.

A DML target is **not a `Scan`** — it's named on the DML root
(`Insert.target`, …), and the root's `input` carries only the source
relations. Its columns are still bound (for SET / WHERE / RETURNING) via a
scratch target `Scope`, discarded after.

**Reads are a source/sink split.** A scanned relation (a source) always reads
— its rows feed or filter, even with no column named (`SELECT COUNT(*) FROM
t`). The write target (the sink) reads only when its *own* data is referenced
— a column in a `WHERE` / `ON` / SET-RHS (`UPDATE t SET a = a + 1`, `DELETE …
WHERE t.flag`, an upsert). A constant `UPDATE t SET a = 1` references no
target column, so the sink stays write-only; a target *also* scanned as a
source (`INSERT INTO t SELECT * FROM t`) reads through that scan.
`collect_table_reads` = every `Scan` ∪ any referenced relation not already
scanned (the sink). A multi-table `UPDATE t1 JOIN t2 SET t2.col = …` writes
(and lineage-targets) the relation each SET qualifier resolves to, carried on
`Assignment.target`, not the root; an *unqualified* SET target among several
writable relations is attributed with the read side's candidate rules (a sole
catalog owner pins; otherwise the write surfaces unattributed — `table: None`,
`Ambiguous` / `Unresolved` — and contributes no table-level write, matching
MySQL's own ambiguity error). The full matrix lives on
`resolve_assignment_column`.

### Value vs filter is structural

The single cleanest distinction: a column that **contributes a value** vs
one that only **filters**. It is positional in the tree, not a tag:

- value operand → `Expr::Call` / `Expr::Window` `arg` / scalar
  `Expr::Subquery` — the `origins` trace follows these.
- filter operand → `Expr::Filter`, or a construct's filter slot (a `CASE`
  `when`, a window `partition` / `order` key, `Expr::Exists` /
  `Expr::InSubquery`) — `origins` skips it.
- clause split too: WHERE / HAVING are `Filter` nodes, the SELECT list is
  the `Projection`.

So the walk needs no clause tag, and the public consequence falls out: a
value contributor is a `lineage` source; a filter-only column is in `reads`
but not `lineage`.

### Catalog and identity

The binder takes an optional `&Catalog`. With one, a matched table's columns
are known (resolution turns strict — typos become `Unresolved`, multi-hits
`Ambiguous`); catalog-free, columns are unknown and resolved reads are
`Inferred`. An absent table is *schema-unknown*, not nonexistent —
open-world.

**Canonicalization.** `table_match` rewrites a uniquely matched reference to
its registered `catalog.schema.name` path, so a bare `users` and an explicit
`public.users` agree (write targets too). The canonical identity is
**case-exact**, so it surfaces *quoted* with the dialect's `canonical_quote`
— otherwise a later qualifier fold under an upper-folding dialect would
re-case it and fail to match its own relation. On a miss / ambiguous match
the ref surfaces *default-normalized* (`surface_with_defaults`): omitted
prefixes filled from the catalog's defaults as *unquoted* (foldable) idents.
With no catalog, the ref stays exactly as written, so `mydb.users` and
`otherdb.users` stay distinct.

**Two-level equality.** References derive *structural* `Eq` / `Hash` (case-
and quote-sensitive) — correct for catalog-backed analysis (matched refs are
canonicalized) and direct comparison. For catalog-free dedup across
fold-equivalent spellings (`users` vs `USERS`), `identity_key` /
`same_table` / `same_column` fold by a dialect's casing; the key is opaque
so the folded text never surfaces.

### Wildcard expansion

A projection wildcard (`*` / `t.*`) expands at bind time into per-column
output items — exactly as if the column list were written at the `*` — but
only **completely** (all-or-nothing per wildcard): a partial expansion would
present an incomplete set as the whole one, so anything not fully known keeps
the wildcard unexpanded, contributing nothing and flagged
`WildcardSuppressed` (whose meaning is precisely "this wildcard could not be
expanded"). Two knowledge sources qualify:

- **A cataloged table** expands each catalog column as a pinned
  `Base { Cataloged }` read in canonical (quoted) form — the same form the
  write-side catalog fill uses, since the column has no written token. Each
  synthesized identifier carries the *wildcard token's span*, so the
  source-ordered surfaces place the whole block at the `*`, in schema order
  (the facade's sort is stable).
- **A derived table / CTE** whose slot view is complete. `Scope` tracks
  `outputs_complete` (false while an unexpanded wildcard remains) and
  `exposed()` hands a parent the slot view (`Exposed { slots, complete }`,
  one `Option<Ident>` per position — an anonymous output holds its position
  namelessly rather than being dropped). Expanded slots bind as
  `Expr::DerivedSlot { qualifier, index }` — **positional by construction**,
  so a duplicate output name (`SELECT o.id, c.id` inside) or an anonymous
  one can't misattribute the trace the way a name-keyed `Derived` lookup
  would; `origins` traces the producer's `index`-th output
  (`trace_nth_output`, fanning across set-operation branches). Written
  references keep name-keyed resolution — position is knowledge only the
  expansion has.

The `reads` consequence is occurrence-based, as always: one read per
expanded column *per wildcard occurrence*; a derived-relation slot is no
read at all (`Derived` drop — the physical read was counted inside the
producer), so expansion can never fabricate reads. Expansion feeding a DML
source also makes `source_wildcard` mean "an *unexpanded* wildcard remains":
an expanded source has determinate positions, so column-less INSERT
catalog-fill, the arity checks, and positional relation lineage all proceed.

What still refuses (the guards, each honest-by-construction): an `Unknown`
table, an opaque table function, an incomplete derived view, a bare `*` over
`USING` / `NATURAL` merge columns (the standard coalesces them into
join-structure-dependent positions the flat `Scope` no longer encodes —
`t.*` never coalesces, so it expands), any wildcard modifier (`EXCLUDE` /
`EXCEPT` / `RENAME` / `REPLACE` / `ILIKE` / Redshift `AS` — expanding while
ignoring one would misreport), a qualifier matching zero or several
relations, and an *unaliased* derived relation that isn't the scope's sole
relation (its slots would have no qualifier to trace by). Anonymous outputs
never get fabricated names — engine auto-names are dialect-specific — they
surface as `QueryOutput { name: None, position }`.

### Casing

Identifier matching is dialect-aware (`crate::casing`), two orthogonal
concerns bundled internally as `IdentifierStyle { casing, quote }`:

- **casing** — a per-class (`table` / `table_alias` / `column`) fold rule
  (`CaseRule`), a *matching policy* the caller may override via
  `ExtractorOptions::with_casing`. Comparisons fold through
  `CaseRule::normalize`, which keys on quoted-vs-unquoted and the fold,
  never the quote *char*.
- **quote** — the dialect's surface quote char for a canonical identity, a
  *surface* concern, always dialect-derived.

The dialect→rule and dialect→quote maps live in `for_dialect` /
`canonical_quote` (with the matrix unit tests as ground truth) — not
duplicated here. Note `canonical_quote` is *self-maintained*, not
sqlparser's `identifier_quote_style` (unset for most dialects).
Filesystem- / collation- / config-dependent models resolve to a fixed safe
default; a per-deployment override API is a future addition. Folding affects
matching only — a *written* (non-canonical) reference keeps its text.

## Walking (plan → surfaces)

Each surface is a pure, independent traversal of the finished plan.

**Lineage is traced at walk time, not pre-collapsed.** No provenance is
baked onto the nodes. A resolved column reference is a `BoundColumn` whose
`binding` is `Base` / `Derived` / `Unresolved` / `Ambiguous`; the `origins`
walk collapses a `Derived` reference to its real base columns *lazily*,
tracing into the producing `Projection` / `CteRef` / `SubqueryAlias` and
composing the lineage kind end-to-end. `lineage` builds its `source →
target` edges on that trace. `reads` instead drops `Derived` (its physical
read was counted at the inner producer); `tables` covers the write surfaces
and the flat list. All fall out of a pure walk of a clean tree.

## Deliberate non-goals

- **No partial wildcard expansion, no fabricated names.** Expansion (see
  "Wildcard expansion" above) is all-or-nothing per wildcard, never invents
  an engine auto-name for an anonymous output, and refuses the cases whose
  correct shape a SQL-text-only view can't reconstruct — a bare `*` over
  merge columns (the standard's coalesced ordering needs the join tree) and
  the wildcard modifiers. An unexpanded wildcard contributes nothing to
  `reads` / `lineage` and is flagged; consumers needing more supply a
  catalog or expand themselves.
- **NATURAL JOIN's merge set needs both schemas** — a side with unknown
  columns contributes no merge columns. `JOIN … USING (col)` **fans in**: an
  unqualified ref to a merge column resolves to *every* joined relation that
  could own it (one read / lineage source per side, not an ambiguous
  `table: None`). A catalog narrows the fan-in to declaring relations.
  Known limit: a 3+-relation catalog-free fan-in includes every relation,
  not just the two USING operands.
- **Clause-alias visibility** (GROUP BY / HAVING / ORDER BY) is structural:
  those clauses resolve against a scope that includes the `Projection`'s
  outputs, while WHERE (below it) does not. A bare ref naming an
  *introduced* alias (`ORDER BY total` for `a+b AS total`) binds `Derived` —
  a lineage source, dropped from `reads` (no phantom read). **Identity is
  name-equality, not alias presence**: `a AS a` is identity (keeps the
  `GROUP BY a` read), `a AS x` is not. Dialect-specific alias-vs-column
  precedence is not modelled.
