mod executor;

use crate::executor::{
    CliExecutable, ColumnOperationExtractExecutor, CrudTableExtractExecutor, FormatExecutor,
    NormalizeExecutor, TableExtractExecutor, TableOperationExtractExecutor,
};
use clap::{ArgGroup, Parser, Subcommand};
use sql_insight::error::Error;
use sql_insight::normalizer::NormalizerOptions;
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
    Tables(CommonOptions),
    /// Tables bucketed by CRUD verb (Create / Read / Update / Delete)
    Crud(CommonOptions),
    /// Table-level reads / writes / lineage per statement
    TableOps(CommonOptions),
    /// Column-level reads / writes / lineage per statement
    ColumnOps(CommonOptions),
}

impl ExtractTarget {
    fn common(&self) -> &CommonOptions {
        match self {
            ExtractTarget::Tables(o)
            | ExtractTarget::Crud(o)
            | ExtractTarget::TableOps(o)
            | ExtractTarget::ColumnOps(o) => o,
        }
    }

    fn executor(&self, sql: String) -> Box<dyn CliExecutable> {
        let dialect = self.common().dialect.clone();
        match self {
            ExtractTarget::Tables(_) => Box::new(TableExtractExecutor::new(sql, dialect)),
            ExtractTarget::Crud(_) => Box::new(CrudTableExtractExecutor::new(sql, dialect)),
            ExtractTarget::TableOps(_) => {
                Box::new(TableOperationExtractExecutor::new(sql, dialect))
            }
            ExtractTarget::ColumnOps(_) => {
                Box::new(ColumnOperationExtractExecutor::new(sql, dialect))
            }
        }
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
