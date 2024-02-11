use sql_insight::error::Error;
use sql_insight::sqlparser::dialect;
use sql_insight::NormalizerOptions;

pub trait CliExecutable {
    fn execute(&self) -> Result<Vec<String>, Error>;
}

fn get_dialect(dialect_name: Option<&str>) -> Result<Box<dyn dialect::Dialect>, Error> {
    let dialect_name = dialect_name.unwrap_or("generic");
    dialect::dialect_from_str(dialect_name)
        .ok_or_else(|| Error::ArgumentError(format!("Dialect not found: {}", dialect_name)))
}

pub struct FormatExecutor {
    sql: String,
    dialect_name: Option<String>,
}

impl FormatExecutor {
    pub fn new(sql: String, dialect_name: Option<String>) -> Self {
        Self { sql, dialect_name }
    }
}

impl CliExecutable for FormatExecutor {
    fn execute(&self) -> Result<Vec<String>, Error> {
        sql_insight::format(
            get_dialect(self.dialect_name.as_deref())?.as_ref(),
            self.sql.as_ref(),
        )
    }
}

pub struct NormalizeExecutor {
    sql: String,
    dialect_name: Option<String>,
    options: NormalizerOptions,
}

impl NormalizeExecutor {
    pub fn new(sql: String, dialect_name: Option<String>) -> Self {
        Self {
            sql,
            dialect_name,
            options: NormalizerOptions::new(),
        }
    }

    pub fn with_options(mut self, options: NormalizerOptions) -> Self {
        self.options = options;
        self
    }
}

impl CliExecutable for NormalizeExecutor {
    fn execute(&self) -> Result<Vec<String>, Error> {
        sql_insight::normalize(
            get_dialect(self.dialect_name.as_deref())?.as_ref(),
            self.sql.as_ref(),
            self.options.clone(),
        )
    }
}

pub struct TableExtractExecutor {
    pub sql: String,
    pub dialect_name: Option<String>,
}

impl TableExtractExecutor {
    pub fn new(sql: String, dialect_name: Option<String>) -> Self {
        Self { sql, dialect_name }
    }
}

impl CliExecutable for TableExtractExecutor {
    fn execute(&self) -> Result<Vec<String>, Error> {
        let result = sql_insight::extract_tables(
            get_dialect(self.dialect_name.as_deref())?.as_ref(),
            self.sql.as_ref(),
        )?;
        Ok(result
            .iter()
            .map(|r| match r {
                Ok(tables) => format!("{}", tables),
                Err(e) => format!("Error: {}", e),
            })
            .collect())
    }
}

pub struct CrudTableExtractExecutor {
    sql: String,
    dialect_name: Option<String>,
}

impl CrudTableExtractExecutor {
    pub fn new(sql: String, dialect_name: Option<String>) -> Self {
        Self { sql, dialect_name }
    }
}

impl CliExecutable for CrudTableExtractExecutor {
    fn execute(&self) -> Result<Vec<String>, Error> {
        let result = sql_insight::extract_crud_tables(
            get_dialect(self.dialect_name.as_deref())?.as_ref(),
            self.sql.as_ref(),
        )?;
        Ok(result
            .iter()
            .map(|r| match r {
                Ok(crud_tables) => format!("{}", crud_tables),
                Err(e) => format!("Error: {}", e),
            })
            .collect())
    }
}
