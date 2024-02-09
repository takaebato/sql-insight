use clap::{Parser, Subcommand};
use sql_insight::NormalizerOptions;

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

#[derive(Parser, Debug)]
struct CommonOptions {
    /// The subject SQL to operate on
    sql: String,
    /// The dialect of the input SQL. Might be required for parsing dialect-specific syntax.
    #[clap(short, long)]
    dialect: Option<String>,
}

#[derive(Parser, Debug)]
struct NormalizeCommandOptions {
    #[clap(flatten)]
    common_options: CommonOptions,
    /// Unify IN lists to a single form when all elements are literal values. For example, `IN (1, 2, 3)` becomes `IN (...)`.
    #[clap(long)]
    unify_in_list: bool,
}

fn main() {
    let args = Cli::parse();

    let result = match args.command {
        Commands::Format(opts) => {
            sql_insight::format_from_cli(opts.dialect.as_deref(), opts.sql.as_str())
        }
        Commands::Normalize(opts) => sql_insight::normalize_from_cli(
            opts.common_options.dialect.as_deref(),
            opts.common_options.sql.as_str(),
            NormalizerOptions::new().with_unify_in_list(opts.unify_in_list),
        ),
        Commands::ExtractCrud(opts) => {
            sql_insight::extract_crud_tables_from_cli(opts.dialect.as_deref(), opts.sql.as_str())
        }
        Commands::ExtractTables(opts) => {
            sql_insight::extract_tables_from_cli(opts.dialect.as_deref(), opts.sql.as_str())
        }
    };

    match result {
        Ok(result) => {
            for r in result {
                println!("{}", r);
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
        }
    }
}
