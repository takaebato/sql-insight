mod executor;

use crate::executor::{
    CliExecutable, CrudTableExtractExecutor, FormatExecutor, NormalizeExecutor,
    TableExtractExecutor,
};
use clap::{ArgGroup, Parser, Subcommand};
use sql_insight::error::Error;
use sql_insight::NormalizerOptions;
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
}

enum ProcessType {
    Sql(String),
    File(String),
    Interactive,
}

impl From<&Commands> for ProcessType {
    fn from(command: &Commands) -> Self {
        match command {
            Commands::Format(opts)
            | Commands::ExtractCrud(opts)
            | Commands::ExtractTables(opts) => {
                if opts.sql.is_some() {
                    ProcessType::Sql(opts.sql.clone().unwrap())
                } else if opts.file.is_some() {
                    ProcessType::File(opts.file.clone().unwrap())
                } else {
                    ProcessType::Interactive
                }
            }
            Commands::Normalize(opts) => {
                if opts.common_options.sql.is_some() {
                    ProcessType::Sql(opts.common_options.sql.clone().unwrap())
                } else if opts.common_options.file.is_some() {
                    ProcessType::File(opts.common_options.file.clone().unwrap())
                } else {
                    ProcessType::Interactive
                }
            }
        }
    }
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Format SQL
    Format(CommonOptions),
    /// Normalize SQL
    Normalize(NormalizeCommandOptions),
    /// Extract CRUD operations from SQL
    ExtractCrud(CommonOptions),
    /// Extract tables from SQL
    ExtractTables(CommonOptions),
}

impl Commands {
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
                NormalizeExecutor::new(sql, opts.common_options.dialect.clone())
                    .with_options(NormalizerOptions::new().with_unify_in_list(opts.unify_in_list)),
            ),
            Commands::ExtractCrud(opts) => {
                Box::new(CrudTableExtractExecutor::new(sql, opts.dialect.clone()))
            }
            Commands::ExtractTables(opts) => {
                Box::new(TableExtractExecutor::new(sql, opts.dialect.clone()))
            }
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
