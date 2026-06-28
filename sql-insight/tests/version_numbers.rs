//! Keeps version references in sync with the crate version (run via `cargo test`).
//!
//! `assert_markdown_deps_updated!` parses the `sql-insight = "…"` snippets in the
//! README (as TOML) and checks them against the crate version, so a release that
//! outgrows them (e.g. the next breaking 0.x bump) fails CI instead of silently
//! leaving the README stale.

#[test]
fn readme_deps_are_updated() {
    version_sync::assert_markdown_deps_updated!("README.md");
}
