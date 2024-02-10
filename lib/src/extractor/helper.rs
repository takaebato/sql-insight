use crate::TableReference;
use std::collections::HashMap;

pub(crate) fn resolve_aliased_tables(
    possibly_aliased_tables: Vec<TableReference>,
    original_tables: Vec<TableReference>,
) -> Vec<TableReference> {
    possibly_aliased_tables
        .iter()
        .map(|possibly_aliased_table| {
            if possibly_aliased_table.has_qualifiers() || possibly_aliased_table.has_alias() {
                return possibly_aliased_table.clone();
            }
            if let Some(resolved_table) = original_tables.iter().find_map(|original_table| {
                original_table.alias.as_ref().and_then(|alias| {
                    if *alias == possibly_aliased_table.name {
                        Some(original_table.clone())
                    } else {
                        None
                    }
                })
            }) {
                return resolved_table;
            }
            possibly_aliased_table.clone()
        })
        .collect()
}

pub(crate) fn calc_difference_of_tables(
    base_tables: Vec<TableReference>,
    exclude_tables: Vec<TableReference>,
) -> Vec<TableReference> {
    let mut exclude_tables_count = HashMap::new();
    for exclude_table in exclude_tables.iter() {
        *exclude_tables_count.entry(exclude_table).or_insert(0) += 1;
    }
    base_tables
        .into_iter()
        .filter(|base_table| {
            if let Some(count) = exclude_tables_count.get_mut(base_table) {
                if *count > 0 {
                    *count -= 1;
                    return false;
                }
            }
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::Ident;

    mod resolve_aliased_tables {
        use super::*;

        #[test]
        fn test_single_aliased_table() {
            let possibly_aliased_tables = vec![TableReference {
                catalog: None,
                schema: None,
                name: Ident::new("t1_alias"),
                alias: None,
            }];
            let original_tables = vec![TableReference {
                catalog: None,
                schema: None,
                name: Ident::new("t1"),
                alias: Some(Ident::new("t1_alias")),
            }];
            let expected_resolved_tables = vec![TableReference {
                catalog: None,
                schema: None,
                name: Ident::new("t1"),
                alias: Some(Ident::new("t1_alias")),
            }];
            let result = resolve_aliased_tables(possibly_aliased_tables, original_tables);
            assert_eq!(result, expected_resolved_tables);
        }

        #[test]
        fn test_multiple_aliased_tables() {
            let possibly_aliased_tables = vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t1_alias"),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t2_alias"),
                    alias: None,
                },
            ];
            let original_tables = vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t1"),
                    alias: Some(Ident::new("t1_alias")),
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t2"),
                    alias: Some(Ident::new("t2_alias")),
                },
            ];
            let expected_resolved_tables = vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t1"),
                    alias: Some(Ident::new("t1_alias")),
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t2"),
                    alias: Some(Ident::new("t2_alias")),
                },
            ];
            let result = resolve_aliased_tables(possibly_aliased_tables, original_tables);
            assert_eq!(result, expected_resolved_tables);
        }

        #[test]
        fn test_catalog_and_schema_qualified_table_in_original_tables() {
            let possibly_aliased_tables = vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t1_alias"),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t2_alias"),
                    alias: None,
                },
            ];
            let original_tables = vec![
                TableReference {
                    catalog: Some(Ident::new("c1")),
                    schema: Some(Ident::new("s1")),
                    name: Ident::new("t1"),
                    alias: Some(Ident::new("t1_alias")),
                },
                TableReference {
                    catalog: None,
                    schema: Some(Ident::new("s2")),
                    name: Ident::new("t2"),
                    alias: Some(Ident::new("t2_alias")),
                },
            ];
            let expected_resolved_tables = vec![
                TableReference {
                    catalog: Some(Ident::new("c1")),
                    schema: Some(Ident::new("s1")),
                    name: Ident::new("t1"),
                    alias: Some(Ident::new("t1_alias")),
                },
                TableReference {
                    catalog: None,
                    schema: Some(Ident::new("s2")),
                    name: Ident::new("t2"),
                    alias: Some(Ident::new("t2_alias")),
                },
            ];
            let result = resolve_aliased_tables(possibly_aliased_tables, original_tables);
            assert_eq!(result, expected_resolved_tables);
        }

        #[test]
        fn test_catalog_and_schema_qualified_table_in_possible_aliased_tables() {
            // qualified alias is not valid syntax in standard SQL,
            // so qualified tables are not regarded as aliased tables, hence they are not resolved.
            let possibly_aliased_tables = vec![
                TableReference {
                    catalog: Some(Ident::new("c1")),
                    schema: Some(Ident::new("s1")),
                    name: Ident::new("t1_alias"),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: Some(Ident::new("s2")),
                    name: Ident::new("t2_alias"),
                    alias: None,
                },
            ];
            let original_tables = vec![
                TableReference {
                    catalog: Some(Ident::new("c1")),
                    schema: Some(Ident::new("s1")),
                    name: Ident::new("t1"),
                    alias: Some(Ident::new("t1_alias")),
                },
                TableReference {
                    catalog: None,
                    schema: Some(Ident::new("s2")),
                    name: Ident::new("t2"),
                    alias: Some(Ident::new("t2_alias")),
                },
            ];
            let expected_resolved_tables = vec![
                TableReference {
                    catalog: Some(Ident::new("c1")),
                    schema: Some(Ident::new("s1")),
                    name: Ident::new("t1_alias"),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: Some(Ident::new("s2")),
                    name: Ident::new("t2_alias"),
                    alias: None,
                },
            ];
            let result = resolve_aliased_tables(possibly_aliased_tables, original_tables);
            assert_eq!(result, expected_resolved_tables);
        }
    }

    mod calc_difference_of_tables {
        use super::*;

        #[test]
        fn test_single_table() {
            let base_tables = vec![TableReference {
                catalog: None,
                schema: None,
                name: Ident::new("t1"),
                alias: None,
            }];
            let exclude_tables = vec![TableReference {
                catalog: None,
                schema: None,
                name: Ident::new("t1"),
                alias: None,
            }];
            let expected_result = vec![];
            let result = calc_difference_of_tables(base_tables, exclude_tables);
            assert_eq!(result, expected_result);
        }

        #[test]
        fn test_multiple_unique_tables() {
            let base_tables = vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t1"),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t2"),
                    alias: None,
                },
            ];
            let exclude_tables = vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t1"),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t2"),
                    alias: None,
                },
            ];
            let expected_result = vec![];
            let result = calc_difference_of_tables(base_tables, exclude_tables);
            assert_eq!(result, expected_result);
        }

        #[test]
        fn test_multiple_tables_with_duplicates() {
            let base_tables = vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t1"),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t1"),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t2"),
                    alias: None,
                },
            ];
            let exclude_tables = vec![
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t1"),
                    alias: None,
                },
                TableReference {
                    catalog: None,
                    schema: None,
                    name: Ident::new("t2"),
                    alias: None,
                },
            ];
            let expected_result = vec![TableReference {
                catalog: None,
                schema: None,
                name: Ident::new("t1"),
                alias: None,
            }];
            let result = calc_difference_of_tables(base_tables, exclude_tables);
            assert_eq!(result, expected_result);
        }
    }
}
