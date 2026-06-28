# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/takaebato/sql-insight/compare/sql-insight-cli-v0.2.0...sql-insight-cli-v0.2.1) - 2026-06-28

### Added

- *(cli)* add --pretty option to format command ([#44](https://github.com/takaebato/sql-insight/pull/44)) by @takaebato in #44
- carry catalog resolution on written columns and lineage targets by @takaebato in #34
- *(cli)* show catalog resolution on write targets by @takaebato in #34
- carry catalog resolution on write targets (TableWrite) by @takaebato in #34

### Fixed

- correct extraction edge cases in CTEs, DML targets, lineage, and catalog ([#37](https://github.com/takaebato/sql-insight/pull/37)) by @takaebato in #37

### Other Changes

- add PR-title lint and release-plz release automation ([#40](https://github.com/takaebato/sql-insight/pull/40)) by @takaebato in #40
- remove the redundant flat table extractor by @takaebato in #34
