# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0](https://github.com/takaebato/sql-insight/compare/sql-insight-v0.3.0...sql-insight-v0.4.0) - 2026-07-05

### ⚠️ Breaking Changes

#### attribute unqualified SET targets with the read-side rules ([#59](https://github.com/takaebato/sql-insight/pull/59)) by @takaebato

An unqualified SET column in a multi-table UPDATE (`UPDATE t1 JOIN t2 SET a = 1`)
no longer pins to the statement's root table. It now resolves like a read over
the writable relations: a sole catalog owner pins it; otherwise the column write
surfaces unattributed (`table: None`, `Ambiguous` / `Unresolved`) and contributes
no table-level write. Qualify the SET column or supply a catalog to keep a
pinned table.

#### upgrade sqlparser to 0.62 ([#55](https://github.com/takaebato/sql-insight/pull/55)) by @takaebato

The re-exported `sql_insight::sqlparser` moves 0.61 → 0.62; code matching on its
AST must adapt (notably `Insert::columns` is now `Vec<ObjectName>`, `VALUES` rows
carry `Parens`, visitors see `ValueWithSpan`, and there are new variants such as
`SelectItem::ExprWithAliases` / `TableObject::TableQuery`). See the
[sqlparser 0.62.0 changelog](https://github.com/apache/datafusion-sqlparser-rs/blob/main/changelog/0.62.0.md).

### Added

- resolve Oracle join-view INSERT targets by column attribution ([#61](https://github.com/takaebato/sql-insight/pull/61)) by @takaebato
- fan out multi-column-alias lineage to every alias ([#60](https://github.com/takaebato/sql-insight/pull/60)) by @takaebato
- resolve Oracle inline-view INSERT targets to their base table ([#58](https://github.com/takaebato/sql-insight/pull/58)) by @takaebato

### Fixed

- don't surface a ClickHouse ARRAY JOIN operand as a table read ([#57](https://github.com/takaebato/sql-insight/pull/57)) by @takaebato

### Other Changes

- changelog breaking-change workflow, version-bump docs, and keywords ([#51](https://github.com/takaebato/sql-insight/pull/51)) by @takaebato
- tidy keywords, README versions, and add a version-sync check ([#46](https://github.com/takaebato/sql-insight/pull/46)) by @takaebato

## [0.3.0](https://github.com/takaebato/sql-insight/compare/v0.2.0...v0.3.0) - 2026-06-28

### Added

- bucket REPLACE / INSERT OVERWRITE as Create + Delete in CRUD by @takaebato in #36
- add Catalog::from_ddl_with_casing for casing-override alignment by @takaebato in #36
- fan in NATURAL JOIN merge columns (catalog-aware) by @takaebato in #36
- count GROUP BY / ORDER BY positional ordinals as reads by @takaebato in #35
- flag INSERT/MERGE arity mismatches for VALUES and column-less sources by @takaebato in #35
- carry catalog resolution on written columns and lineage targets by @takaebato in #34
- read the LIKE / CLONE shape source; CLONE feeds lineage by @takaebato in #34
- carry catalog resolution on write targets (TableWrite) by @takaebato in #34
- implement operator extraction: table & column lineage from SQL by @takaebato in #31
- normalize column lists, nested `IN` expressions, and unary operators by @piki in #7

### Fixed

- correct extraction edge cases in CTEs, DML targets, lineage, and catalog ([#37](https://github.com/takaebato/sql-insight/pull/37)) by @takaebato in #37
- don't duplicate a recursive CTE's anchor diagnostics by @takaebato in #36
- flag a non-table UPDATE target instead of dropping it silently by @takaebato in #36
- normalize a TOP constant quantity by @takaebato in #36
- flow the WITHIN GROUP value of an ordered-set aggregate by @takaebato in #36
- treat a parameterized FROM function as an opaque table factor by @takaebato in #35
- extract tuple SET (a, b) = … assignments by @takaebato in #35
- flow the LHS of IN (subquery) in value position by @takaebato in #35
- bind DELETE/UPDATE ORDER BY and LIMIT clauses by @takaebato in #35
- extract wildcard REPLACE (expr AS col) outputs by @takaebato in #35
- extract MERGE RETURNING / OUTPUT columns by @takaebato in #35
- bind QUALIFY against the post-projection scope by @takaebato in #35
- model DML reads as a data-flow source/sink split by @takaebato in #34
- attribute a multi-table UPDATE's SET targets to their own tables by @takaebato in #34
- extraction correctness pass: reads / lineage / DML classification / catalog, + binder refactors by @takaebato in #33

### Documentation

- note that normalization replaces all literals, including structural ones by @takaebato in #35

### Other Changes

- add PR-title lint and release-plz release automation ([#40](https://github.com/takaebato/sql-insight/pull/40)) by @takaebato in #40
- cover untested SQL constructs and public reference conversions by @takaebato in #35
- remove the redundant flat table extractor by @takaebato in #34
- migrate extractor/mod.rs to extractor.rs ([#25](https://github.com/takaebato/sql-insight/pull/25)) by @takaebato in #25
- upgrade sqlparser to 0.61.0 by @takaebato in #23
- upgrade sqlparser dependency to 0.56.0 by @piki in #9

## [v0.2.0](https://github.com/takaebato/sql-insight/tree/v0.2.0) (2024-07-05)

## What's Changed

* Add unify_values option to normalization (and update tarpaulin to v0.30.0) by @takaebato in https://github.com/takaebato/sql-insight/pull/6

## [v0.1.1](https://github.com/takaebato/sql-insight/tree/v0.1.1) (2024-02-12)

Updates to the documentation and removal of any remaining debug print in the code.

## [v0.1.0](https://github.com/takaebato/sql-insight/tree/v0.1.0) (2024-02-12)

Initial release.
