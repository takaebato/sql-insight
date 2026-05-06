//! Relation model types shared by SQL inspection features.

use core::fmt;

use crate::error::Error;
use sqlparser::ast::{Ident, Insert, ObjectName, TableFactor, TableObject};

/// [`TableReference`] represents a qualified table with alias.
///
/// In this crate, this is the canonical representation of a table reference.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TableReference {
    pub catalog: Option<Ident>,
    pub schema: Option<Ident>,
    pub name: Ident,
    pub alias: Option<Ident>,
}

impl TableReference {
    pub fn has_alias(&self) -> bool {
        self.alias.is_some()
    }

    pub fn has_qualifiers(&self) -> bool {
        self.catalog.is_some() || self.schema.is_some()
    }

    pub fn try_from_name_and_alias(
        name: &ObjectName,
        alias: &Option<Ident>,
    ) -> Result<Self, Error> {
        match name.0.len() {
            0 => unreachable!("Parser should not allow empty identifiers"),
            1 => Ok(TableReference {
                catalog: None,
                schema: None,
                name: name.0[0].as_ident().unwrap().clone(),
                alias: alias.clone(),
            }),
            2 => Ok(TableReference {
                catalog: None,
                schema: Some(name.0[0].as_ident().unwrap().clone()),
                name: name.0[1].as_ident().unwrap().clone(),
                alias: alias.clone(),
            }),
            3 => Ok(TableReference {
                catalog: Some(name.0[0].as_ident().unwrap().clone()),
                schema: Some(name.0[1].as_ident().unwrap().clone()),
                name: name.0[2].as_ident().unwrap().clone(),
                alias: alias.clone(),
            }),
            _ => Err(Error::AnalysisError(
                "Too many identifiers provided".to_string(),
            )),
        }
    }

    pub fn try_from_name(name: &ObjectName) -> Result<Self, Error> {
        Self::try_from_name_and_alias(name, &None)
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
        let table = parts.join(".");
        if let Some(alias) = &self.alias {
            write!(f, "{} AS {}", table, alias)
        } else {
            write!(f, "{}", table)
        }
    }
}

impl TryFrom<&Insert> for TableReference {
    type Error = Error;

    fn try_from(value: &Insert) -> Result<Self, Self::Error> {
        let name = match &value.table {
            TableObject::TableName(object_name) => object_name,
            TableObject::TableFunction(function) => &function.name,
        };
        Self::try_from_name_and_alias(name, &value.table_alias)
    }
}

impl TryFrom<&TableFactor> for TableReference {
    type Error = Error;

    fn try_from(table: &TableFactor) -> Result<Self, Self::Error> {
        match table {
            TableFactor::Table { name, alias, .. } => {
                Self::try_from_name_and_alias(name, &alias.as_ref().map(|a| a.name.clone()))
            }
            _ => unreachable!("TableFactor::Table expected"),
        }
    }
}

impl TryFrom<&ObjectName> for TableReference {
    type Error = Error;

    fn try_from(obj_name: &ObjectName) -> Result<Self, Self::Error> {
        Self::try_from_name(obj_name)
    }
}
