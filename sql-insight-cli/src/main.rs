mod executor;

use crate::executor::{
    CasingOverride, CliExecutable, ExtractExecutor, ExtractKind, FormatExecutor, NormalizeExecutor,
    OutputFormat,
};
use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use sql_insight::error::Error;
use sql_insight::normalizer::NormalizerOptions;
use sql_insight::CaseRule;
use std::io::{self, IsTerminal, Read};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(name = "sql-insight")]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Parser, Debug)]
#[clap(group(ArgGroup::new("source").args(& ["sql", "file", "interactive"]).required(false)))]
struct CommonOptions {
    /// The subject SQL to operate on. If omitted (and not --interactive),
    /// SQL is read from stdin when it is piped; an interactive terminal
    /// with no input is an error.
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
    /// Read statements interactively from a prompt (terminate each with `;`).
    #[clap(short, long, group = "source")]
    interactive: bool,
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
    /// Alphabetize INSERT column lists so column-order-only variants normalize alike. Only takes effect together with `--unify-values`: `INSERT INTO t (c, b, a) VALUES (1, 2, 3)` becomes `INSERT INTO t (a, b, c) VALUES (...)`.
    #[clap(long)]
    alphabetize_insert_columns: bool,
}

enum ProcessType {
    Sql(String),
    File(String),
    /// Read the SQL from piped stdin (no explicit source given).
    Stdin,
    Interactive,
}

impl ProcessType {
    /// Resolve the input source. The `source` ArgGroup already makes
    /// `sql` / `--file` / `--interactive` mutually exclusive, so at most
    /// one is set. With none, fall back to stdin when it's piped; an
    /// interactive terminal with no input is an error (rather than the
    /// old surprise of dropping into the REPL).
    fn resolve(command: &Commands) -> Result<Self, Error> {
        let opts = command.common();
        if opts.interactive {
            Ok(ProcessType::Interactive)
        } else if let Some(sql) = &opts.sql {
            Ok(ProcessType::Sql(sql.clone()))
        } else if let Some(file) = &opts.file {
            Ok(ProcessType::File(file.clone()))
        } else if !io::stdin().is_terminal() {
            Ok(ProcessType::Stdin)
        } else {
            Err(Error::ArgumentError(
                "no SQL given — pass it as an argument, pipe it on stdin, \
                 use --file <path>, or --interactive"
                    .to_string(),
            ))
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
    /// Output format: human-readable text (default) or JSON.
    #[clap(long, value_enum, default_value_t = FormatArg::Text)]
    format: FormatArg,
    /// SQL DDL file (CREATE TABLE statements) to resolve against — enables
    /// catalog-aware analysis (canonicalized identities, strict columns).
    #[clap(long = "ddl-file")]
    ddl_file: Option<String>,
    /// Query-side default schema: a search-path-style fill applied to a
    /// bare query reference before matching, so it surfaces qualified
    /// (e.g. `users` -> `public.users`). Unqualified DDL tables register
    /// schema-less regardless; without this they still match bare refs by
    /// right-anchoring.
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

/// CLI surface of the executor's `OutputFormat`.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum FormatArg {
    Text,
    Json,
}

impl From<FormatArg> for OutputFormat {
    fn from(arg: FormatArg) -> Self {
        match arg {
            FormatArg::Text => OutputFormat::Text,
            FormatArg::Json => OutputFormat::Json,
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
            format: args.format.into(),
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
        match ProcessType::resolve(self)? {
            ProcessType::Sql(sql) => self.execute_sql(sql),
            ProcessType::File(file) => self.execute_file(file),
            ProcessType::Stdin => self.execute_stdin(),
            ProcessType::Interactive => self.execute_interactive(),
        }
    }

    fn execute_sql(&self, sql: String) -> Result<Vec<String>, Error> {
        self.executor(sql).execute()
    }

    fn execute_stdin(&self) -> Result<Vec<String>, Error> {
        let mut sql = String::new();
        io::stdin()
            .read_to_string(&mut sql)
            .map_err(|e| Error::IOError(e.to_string()))?;
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
        let mut editor =
            rustyline::DefaultEditor::new().map_err(|e| Error::IOError(e.to_string()))?;

        let mut input_buffer = String::new();
        loop {
            let prompt = if input_buffer.is_empty() {
                "sql> "
            } else {
                "  -> "
            };
            let line = match editor.readline(prompt) {
                Ok(line) => line,
                // Ctrl-C clears the in-progress statement; Ctrl-D / EOF exits.
                Err(rustyline::error::ReadlineError::Interrupted) => {
                    input_buffer.clear();
                    continue;
                }
                Err(rustyline::error::ReadlineError::Eof) => break,
                Err(e) => return Err(Error::IOError(e.to_string())),
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let _ = editor.add_history_entry(trimmed);
            if trimmed.eq_ignore_ascii_case("exit") || trimmed.eq_ignore_ascii_case("quit") {
                break;
            }
            input_buffer.push_str(trimmed);
            input_buffer.push('\n');
            if trimmed.ends_with(';') {
                match self.executor(std::mem::take(&mut input_buffer)).execute() {
                    Ok(result) => result.iter().for_each(|r| println!("{r}")),
                    Err(e) => eprintln!("Error: {e}"),
                }
            }
        }

        println!("Bye");
        Ok(())
    }

    fn executor(&self, sql: String) -> Box<dyn CliExecutable> {
        match self {
            Commands::Format(opts) => Box::new(FormatExecutor::new(sql, opts.dialect.clone())),
            Commands::Normalize(opts) => Box::new(
                NormalizeExecutor::new(sql, opts.common_options.dialect.clone()).with_options(
                    NormalizerOptions::new()
                        .with_unify_in_list(opts.unify_in_list)
                        .with_unify_values(opts.unify_values)
                        .with_alphabetize_insert_columns(opts.alphabetize_insert_columns),
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
