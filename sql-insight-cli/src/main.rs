mod executor;

use crate::executor::{
    CasingOverride, CliExecutable, ExtractExecutor, ExtractKind, FormatExecutor, NormalizeExecutor,
};
use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use sql_insight::error::Error;
use sql_insight::normalizer::NormalizerOptions;
use sql_insight::CaseRule;
use std::io::{self, Write};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "sql-insight")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Turn debugging information on
    #[arg(short, long, action = clap::ArgAction::Count)]
    debug: u8,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Parser, Debug)]
#[clap(group(ArgGroup::new("source").args(& ["sql", "file"]).required(false)))]
struct CommonOptions {
    /// The subject SQL to operate on
    #[clap(value_parser, group = "source")]
    sql: Option<String>,
    /// The dialect of the input SQL. Might be required for parsing dialect-specific syntax.
    /// Available dialects: ansi, bigquery, clickhouse, duckdb, generic, hive, mssql, mysql, postgres, redshift, snowflake, sqlite.
    /// Default: generic.
    #[clap(short, long)]
    dialect: Option<String>,
    /// The file containing the SQL to operate on
    #[clap(short, long, value_parser, group = "source")]
    file: Option<String>,
}

#[derive(Parser, Debug)]
struct NormalizeCommandOptions {
    #[clap(flatten)]
    common_options: CommonOptions,
    /// Unify IN lists to a single form when all elements are literal values. For example, `IN (1, 2, 3)` becomes `IN (...)`.
    #[clap(long)]
    unify_in_list: bool,
    /// Unify VALUES lists to a single form when all elements are literal values. For example, `VALUES (1, 2, 3), (4, 5, 6)` becomes `VALUES (...)`.
    #[clap(long)]
    unify_values: bool,
}

enum ProcessType {
    Sql(String),
    File(String),
    Interactive,
}

impl From<&Commands> for ProcessType {
    fn from(command: &Commands) -> Self {
        let opts = command.common();
        if let Some(sql) = &opts.sql {
            ProcessType::Sql(sql.clone())
        } else if let Some(file) = &opts.file {
            ProcessType::File(file.clone())
        } else {
            ProcessType::Interactive
        }
    }
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Format SQL
    Format(CommonOptions),
    /// Normalize SQL
    Normalize(NormalizeCommandOptions),
    /// Extract what a statement touches, at a chosen granularity
    Extract {
        #[command(subcommand)]
        target: ExtractTarget,
    },
}

/// Extraction granularities — thin wrappers over the library's extractors.
#[derive(Subcommand, Debug)]
enum ExtractTarget {
    /// Flat list of tables the statement references
    Tables(ExtractArgs),
    /// Tables bucketed by CRUD verb (Create / Read / Update / Delete)
    Crud(ExtractArgs),
    /// Table-level reads / writes / lineage per statement
    TableOps(ExtractArgs),
    /// Column-level reads / writes / lineage per statement
    ColumnOps(ExtractArgs),
}

/// Options shared by every `extract` subcommand: the source / dialect plus
/// catalog- and casing-aware analysis controls.
#[derive(Parser, Debug)]
struct ExtractArgs {
    #[clap(flatten)]
    common: CommonOptions,
    /// SQL DDL file (CREATE TABLE statements) to resolve against — enables
    /// catalog-aware analysis (canonicalized identities, strict columns).
    #[clap(long = "ddl-file")]
    ddl_file: Option<String>,
    /// Query-side default schema (search-path-style fill before matching);
    /// also names unqualified tables in the DDL. Without it, unqualified
    /// DDL tables register under `public` and bare refs resolve by
    /// right-anchoring. Only meaningful with --ddl-file.
    #[clap(long)]
    default_schema: Option<String>,
    /// Query-side default catalog (search-path-style fill). Only
    /// meaningful with --ddl-file.
    #[clap(long)]
    default_catalog: Option<String>,
    /// Override identifier casing for every class (table / alias / column).
    #[clap(long, value_enum)]
    casing: Option<CasingArg>,
    /// Override casing for catalog / schema / table names only.
    #[clap(long = "casing-table", value_enum)]
    casing_table: Option<CasingArg>,
    /// Override casing for table aliases / CTE / derived names only.
    #[clap(long = "casing-table-alias", value_enum)]
    casing_table_alias: Option<CasingArg>,
    /// Override casing for column names only.
    #[clap(long = "casing-column", value_enum)]
    casing_column: Option<CasingArg>,
}

/// CLI surface of the library's `CaseRule`.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum CasingArg {
    Upper,
    Lower,
    Insensitive,
    Sensitive,
}

impl From<CasingArg> for CaseRule {
    fn from(arg: CasingArg) -> Self {
        match arg {
            CasingArg::Upper => CaseRule::Upper,
            CasingArg::Lower => CaseRule::Lower,
            CasingArg::Insensitive => CaseRule::Insensitive,
            CasingArg::Sensitive => CaseRule::Sensitive,
        }
    }
}

impl ExtractTarget {
    fn args(&self) -> &ExtractArgs {
        match self {
            ExtractTarget::Tables(a)
            | ExtractTarget::Crud(a)
            | ExtractTarget::TableOps(a)
            | ExtractTarget::ColumnOps(a) => a,
        }
    }

    fn common(&self) -> &CommonOptions {
        &self.args().common
    }

    fn executor(&self, sql: String) -> Box<dyn CliExecutable> {
        let args = self.args();
        let kind = match self {
            ExtractTarget::Tables(_) => ExtractKind::Tables,
            ExtractTarget::Crud(_) => ExtractKind::Crud,
            ExtractTarget::TableOps(_) => ExtractKind::TableOps,
            ExtractTarget::ColumnOps(_) => ExtractKind::ColumnOps,
        };
        Box::new(ExtractExecutor {
            kind,
            sql,
            dialect_name: args.common.dialect.clone(),
            ddl_file: args.ddl_file.clone(),
            default_schema: args.default_schema.clone(),
            default_catalog: args.default_catalog.clone(),
            casing: CasingOverride {
                all: args.casing.map(Into::into),
                table: args.casing_table.map(Into::into),
                table_alias: args.casing_table_alias.map(Into::into),
                column: args.casing_column.map(Into::into),
            },
        })
    }
}

impl Commands {
    /// The source / dialect options shared by every command.
    fn common(&self) -> &CommonOptions {
        match self {
            Commands::Format(opts) => opts,
            Commands::Normalize(opts) => &opts.common_options,
            Commands::Extract { target } => target.common(),
        }
    }

    fn execute(&self) -> Result<Vec<String>, Error> {
        match ProcessType::from(self) {
            ProcessType::Sql(sql) => self.execute_sql(sql),
            ProcessType::File(file) => self.execute_file(file),
            ProcessType::Interactive => self.execute_interactive(),
        }
    }

    fn execute_sql(&self, sql: String) -> Result<Vec<String>, Error> {
        self.executor(sql).execute()
    }

    fn execute_file(&self, file: String) -> Result<Vec<String>, Error> {
        match std::fs::read_to_string(file.clone()) {
            Ok(sql) => self.executor(sql).execute(),
            Err(e) => Err(Error::ArgumentError(format!(
                "Failed to read file {}: {}",
                file, e
            ))),
        }
    }

    fn execute_interactive(&self) -> Result<Vec<String>, Error> {
        self.entering_interactive_mode()?;
        Ok(vec![])
    }

    fn entering_interactive_mode(&self) -> Result<(), Error> {
        println!(
            "Entering interactive mode. Type sql statement end with `;` to execute. \
             Type `exit` or `quit` to exit."
        );
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        let mut input_buffer = String::new();
        let mut new_input = true;
        loop {
            if new_input {
                print!("sql> ");
            } else {
                print!("  -> ");
            }
            stdout.flush().map_err(|e| Error::IOError(e.to_string()))?;
            let mut line = String::new();
            stdin
                .read_line(&mut line)
                .map_err(|e| Error::IOError(e.to_string()))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line.to_lowercase() == "exit" || line.to_lowercase() == "quit" {
                println!("Bye");
                break Ok(());
            }
            input_buffer.push_str(line);
            input_buffer.push('\n');
            if line.ends_with(';') {
                match self.executor(input_buffer.clone()).execute() {
                    Ok(result) => {
                        for r in result {
                            println!("{}", r);
                        }
                    }
                    Err(e) => {
                        eprintln!("Error: {}", e);
                    }
                }
                input_buffer.clear();
                new_input = true;
            } else {
                new_input = false;
            }
        }
    }

    fn executor(&self, sql: String) -> Box<dyn CliExecutable> {
        match self {
            Commands::Format(opts) => Box::new(FormatExecutor::new(sql, opts.dialect.clone())),
            Commands::Normalize(opts) => Box::new(
                NormalizeExecutor::new(sql, opts.common_options.dialect.clone()).with_options(
                    NormalizerOptions::new()
                        .with_unify_in_list(opts.unify_in_list)
                        .with_unify_values(opts.unify_values),
                ),
            ),
            Commands::Extract { target } => target.executor(sql),
        }
    }
}

fn main() -> ExitCode {
    let args = Cli::parse();
    let result = args.command.execute();
    match result {
        Ok(result) => {
            for r in result {
                println!("{}", r);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            ExitCode::FAILURE
        }
    }
}
