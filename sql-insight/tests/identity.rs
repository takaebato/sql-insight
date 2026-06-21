//! Dialect-aware identity keys: `TableReference::identity_key` /
//! `same_table` and the column equivalents. These let consumers dedup
//! references by *fold-equivalence* (e.g. `users` == `USERS` under a
//! lower-folding dialect), where the structural `Eq` would over-count.

use std::collections::HashSet;

use sql_insight::{CaseRule, ColumnReference, IdentifierCasing, TableReference};

fn table(schema: Option<&str>, name: &str) -> TableReference {
    TableReference {
        catalog: None,
        schema: schema.map(Into::into),
        name: name.into(),
    }
}

fn column(table_ref: Option<TableReference>, name: &str) -> ColumnReference {
    ColumnReference {
        table: table_ref,
        name: name.into(),
    }
}

#[test]
fn case_folding_dialect_treats_spellings_as_one_table() {
    // Lower fold: `users` and `USERS` denote the same table — structural
    // `Eq` disagrees (case-sensitive), the identity key agrees.
    let lower = IdentifierCasing::uniform(CaseRule::Lower);
    let a = table(None, "users");
    let b = table(None, "USERS");

    assert_ne!(a, b, "structural Eq is case-sensitive");
    assert!(a.same_table(&b, &lower));
    assert_eq!(a.identity_key(&lower), b.identity_key(&lower));
}

#[test]
fn sensitive_dialect_keeps_spellings_distinct() {
    let sensitive = IdentifierCasing::uniform(CaseRule::Sensitive);
    let a = table(None, "users");
    let b = table(None, "USERS");

    assert!(!a.same_table(&b, &sensitive));
    assert_ne!(a.identity_key(&sensitive), b.identity_key(&sensitive));
}

#[test]
fn identity_key_dedups_fold_equivalent_refs_in_a_hashset() {
    let lower = IdentifierCasing::uniform(CaseRule::Lower);
    let refs = [
        table(None, "users"),
        table(None, "USERS"),
        table(None, "Users"),
        table(Some("public"), "orders"),
    ];
    let distinct: HashSet<_> = refs.iter().map(|r| r.identity_key(&lower)).collect();
    // The three `users` spellings collapse to one; `public.orders` is its own.
    assert_eq!(distinct.len(), 2);
}

#[test]
fn presence_of_a_qualifier_is_significant() {
    // Identity, not wildcard matching: a bare `users` and a qualified
    // `public.users` are different identities even though right-anchored
    // *matching* would relate them.
    let lower = IdentifierCasing::uniform(CaseRule::Lower);
    let bare = table(None, "users");
    let qualified = table(Some("public"), "users");
    assert!(!bare.same_table(&qualified, &lower));
}

#[test]
fn column_key_folds_table_and_name_by_their_own_rules() {
    // MySQL-like split: table case-sensitive, column case-insensitive.
    let split = IdentifierCasing {
        table: CaseRule::Sensitive,
        table_alias: CaseRule::Insensitive,
        column: CaseRule::Insensitive,
    };
    let owner = table(None, "Users");
    // Same table spelling, column differs only in case → same column.
    let a = column(Some(owner.clone()), "Id");
    let b = column(Some(owner.clone()), "id");
    assert!(a.same_column(&b, &split));

    // Table spelling differs in case → different (table rule is Sensitive).
    let c = column(Some(table(None, "USERS")), "id");
    assert!(!a.same_column(&c, &split));
}

#[test]
fn column_key_distinguishes_bare_from_qualified() {
    let lower = IdentifierCasing::uniform(CaseRule::Lower);
    let bare = column(None, "id");
    let qualified = column(Some(table(None, "users")), "id");
    assert!(!bare.same_column(&qualified, &lower));
}
