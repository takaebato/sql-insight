# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

### Documentation

- note that normalization replaces all literals, including structural ones by @takaebato in #35

### Other Changes

- add PR-title lint and release-plz release automation ([#40](https://github.com/takaebato/sql-insight/pull/40)) by @takaebato in #40
- cover untested SQL constructs and public reference conversions by @takaebato in #35
- remove the redundant flat table extractor by @takaebato in #34
- migrate extractor/mod.rs to extractor.rs ([#25](https://github.com/takaebato/sql-insight/pull/25)) by @takaebato in #25

## [v0.2.0](https://github.com/takaebato/sql-insight/tree/v0.2.0) (2024-07-05)

## What's Changed

* Add unify_values option to normalization (and update tarpaulin to v0.30.0) by @takaebato in https://github.com/takaebato/sql-insight/pull/6

## [v0.1.1](https://github.com/takaebato/sql-insight/tree/v0.1.1) (2024-02-12)

Updates to the documentation and removal of any remaining debug print in the code.

## [v0.1.0](https://github.com/takaebato/sql-insight/tree/v0.1.0) (2024-02-12)

Initial release.
