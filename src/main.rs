use clap::{Parser, Subcommand};

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
    Format(CommandOptions),
    /// Normalize SQL
    Normalize(CommandOptions),
    /// Extract CRUD operations from SQL
    ExtractCrud(CommandOptions),
    /// Extract tables from SQL
    ExtractTables(CommandOptions),
}

#[derive(Parser, Debug)]
struct CommandOptions {
    /// The subject SQL to operate on
    sql: String,
    /// The dialect of the input SQL. Might be required for parsing dialect-specific syntax.
    #[clap(short, long)]
    dialect: Option<String>,
}

fn main() {
    let args = Cli::parse();

    let result = match args.command {
        Commands::Format(opts) => {
            sql_insight::format_from_cli(opts.dialect.as_deref(), opts.sql.as_str())
        }
        Commands::Normalize(opts) => {
            sql_insight::normalize_from_cli(opts.dialect.as_deref(), opts.sql.as_str())
        }
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
