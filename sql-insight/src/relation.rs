//! Relation model types shared by SQL inspection features.

use core::fmt;

use crate::error::Error;
use sqlparser::ast::{Insert, ObjectName, TableFactor, TableObject};

/// Physical table identity — the `catalog.schema.name` triplet.
///
/// `TableReference` deliberately carries no alias: aliasing is a
/// use-site decoration, not part of a table's identity. Two SQL
/// fragments that reference the same physical table produce equal
/// `TableReference`s regardless of how they alias it, so `HashSet` /
/// `HashMap` dedup behaves intuitively and cross-statement comparison
/// is direct. Use-site alias information, when needed, is carried by
/// the structures that wrap a `TableReference` (e.g. resolver bindings).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TableReference {
    pub catalog: Option<sqlparser::ast::Ident>,
    pub schema: Option<sqlparser::ast::Ident>,
    pub name: sqlparser::ast::Ident,
}

impl TableReference {
    pub fn has_qualifiers(&self) -> bool {
        self.catalog.is_some() || self.schema.is_some()
    }

    pub fn try_from_name(name: &ObjectName) -> Result<Self, Error> {
        match name.0.len() {
            0 => unreachable!("Parser should not allow empty identifiers"),
            1 => Ok(TableReference {
                catalog: None,
                schema: None,
                name: name.0[0].as_ident().unwrap().clone(),
            }),
            2 => Ok(TableReference {
                catalog: None,
                schema: Some(name.0[0].as_ident().unwrap().clone()),
                name: name.0[1].as_ident().unwrap().clone(),
            }),
            3 => Ok(TableReference {
                catalog: Some(name.0[0].as_ident().unwrap().clone()),
                schema: Some(name.0[1].as_ident().unwrap().clone()),
                name: name.0[2].as_ident().unwrap().clone(),
            }),
            _ => Err(Error::AnalysisError(
                "Too many identifiers provided".to_string(),
            )),
        }
    }
}

impl fmt::Display for TableReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        if let Some(catalog) = &self.catalog {
            parts.push(catalog.to_string());
        }
        if let Some(schema) = &self.schema {
            parts.push(schema.to_string());
        }
        parts.push(self.name.to_string());
        write!(f, "{}", parts.join("."))
    }
}

impl TryFrom<&Insert> for TableReference {
    type Error = Error;

    fn try_from(value: &Insert) -> Result<Self, Self::Error> {
        Self::from_insert_with_alias(value).map(|(table, _)| table)
    }
}

impl TryFrom<&TableFactor> for TableReference {
    type Error = Error;

    fn try_from(table: &TableFactor) -> Result<Self, Self::Error> {
        Self::from_table_factor_with_alias(table).map(|(table, _)| table)
    }
}

impl TryFrom<&ObjectName> for TableReference {
    type Error = Error;

    fn try_from(obj_name: &ObjectName) -> Result<Self, Self::Error> {
        Self::try_from_name(obj_name)
    }
}

impl TableReference {
    /// Parse an INSERT statement's target into (identity, alias) pair.
    pub fn from_insert_with_alias(
        value: &Insert,
    ) -> Result<(Self, Option<sqlparser::ast::Ident>), Error> {
        let name = match &value.table {
            TableObject::TableName(object_name) => object_name,
            TableObject::TableFunction(function) => &function.name,
        };
        Ok((Self::try_from_name(name)?, value.table_alias.clone()))
    }

    /// Parse a TableFactor (must be `TableFactor::Table`) into (identity, alias) pair.
    pub fn from_table_factor_with_alias(
        table: &TableFactor,
    ) -> Result<(Self, Option<sqlparser::ast::Ident>), Error> {
        match table {
            TableFactor::Table { name, alias, .. } => Ok((
                Self::try_from_name(name)?,
                alias.as_ref().map(|a| a.name.clone()),
            )),
            _ => unreachable!("TableFactor::Table expected"),
        }
    }
}
