mod executor;

use crate::executor::{
    CasingOverride, CliExecutable, ExtractExecutor, ExtractKind, FormatExecutor, NormalizeExecutor,
    OutputFormat,
};
use clap::{ArgGroup, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{generate, Shell};
use sql_insight::error::Error;
use sql_insight::formatter::FormatterOptions;
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
    /// Available dialects: ansi, bigquery, clickhouse, databricks, duckdb, generic, hive, mssql, mysql, oracle, postgres, redshift, snowflake, sqlite.
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
struct FormatCommandOptions {
    #[clap(flatten)]
    common_options: CommonOptions,
    /// Pretty-print each statement across multiple indented lines (one item
    /// per line) instead of the default single line. For example, `SELECT a,
    /// b FROM t1` becomes a multi-line block with `a` / `b` indented under
    /// `SELECT`.
    #[clap(long)]
    pretty: bool,
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
    Format(FormatCommandOptions),
    /// Normalize SQL
    Normalize(NormalizeCommandOptions),
    /// Extract what a statement touches, at a chosen granularity
    Extract {
        #[command(subcommand)]
        target: ExtractTarget,
    },
    /// Print a shell completion script (bash, zsh, fish, …) to stdout
    Completions {
        /// Shell to generate the completion script for
        shell: Shell,
    },
    /// Print the man page (roff) to stdout
    Man,
}

/// Extraction granularities — thin wrappers over the library's extractors.
#[derive(Subcommand, Debug)]
enum ExtractTarget {
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
            ExtractTarget::Crud(a) | ExtractTarget::TableOps(a) | ExtractTarget::ColumnOps(a) => a,
        }
    }

    fn common(&self) -> &CommonOptions {
        &self.args().common
    }

    fn executor(&self, sql: String) -> Box<dyn CliExecutable> {
        let args = self.args();
        let kind = match self {
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
            Commands::Format(opts) => &opts.common_options,
            Commands::Normalize(opts) => &opts.common_options,
            Commands::Extract { target } => target.common(),
            // Utility commands take no SQL input; main dispatches them before
            // ever reaching the SQL path that calls this.
            Commands::Completions { .. } | Commands::Man => {
                unreachable!("completions/man are handled in main")
            }
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
            "Entering interactive mode. End each statement with `;` to execute — \
             Enter continues a statement across lines until then. \
             Type `exit` or `quit` to exit."
        );
        // Statement continuation is rustyline's multiline mechanism: the
        // helper's `Validator` answers `Incomplete` until the buffer ends a
        // statement, so Enter inserts a newline and editing continues across
        // lines — one history entry per whole statement, content (a string
        // literal's indentation) kept verbatim.
        let mut editor: rustyline::Editor<SqlHelper, rustyline::history::DefaultHistory> =
            rustyline::Editor::new().map_err(|e| Error::IOError(e.to_string()))?;
        let dialect = crate::executor::get_dialect(self.common().dialect.as_deref())?;
        editor.set_helper(Some(SqlHelper { dialect }));
        loop {
            let input = match editor.readline("sql> ") {
                Ok(input) => input,
                // Ctrl-C discards the in-progress statement; Ctrl-D / EOF exits.
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(rustyline::error::ReadlineError::Eof) => break,
                Err(e) => return Err(Error::IOError(e.to_string())),
            };
            let trimmed = input.trim();
            if trimmed.is_empty() {
                continue;
            }
            let _ = editor.add_history_entry(&input);
            if trimmed.eq_ignore_ascii_case("exit") || trimmed.eq_ignore_ascii_case("quit") {
                break;
            }
            match self.executor(input).execute() {
                Ok(result) => result.iter().for_each(|r| println!("{r}")),
                Err(e) => eprintln!("Error: {e}"),
            }
        }

        println!("Bye");
        Ok(())
    }

    fn executor(&self, sql: String) -> Box<dyn CliExecutable> {
        match self {
            Commands::Format(opts) => Box::new(
                FormatExecutor::new(sql, opts.common_options.dialect.clone())
                    .with_options(FormatterOptions::new().with_pretty(opts.pretty)),
            ),
            Commands::Normalize(opts) => Box::new(
                NormalizeExecutor::new(sql, opts.common_options.dialect.clone()).with_options(
                    NormalizerOptions::new()
                        .with_unify_in_list(opts.unify_in_list)
                        .with_unify_values(opts.unify_values)
                        .with_alphabetize_insert_columns(opts.alphabetize_insert_columns),
                ),
            ),
            Commands::Extract { target } => target.executor(sql),
            Commands::Completions { .. } | Commands::Man => {
                unreachable!("completions/man are handled in main")
            }
        }
    }
}

fn main() -> ExitCode {
    let args = Cli::parse();
    // Utility commands take no SQL input — render straight to stdout.
    match &args.command {
        Commands::Completions { shell } => {
            generate(
                *shell,
                &mut Cli::command(),
                "sql-insight",
                &mut io::stdout(),
            );
            return ExitCode::SUCCESS;
        }
        Commands::Man => {
            return match clap_mangen::Man::new(Cli::command()).render(&mut io::stdout()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Error: {}", e);
                    ExitCode::FAILURE
                }
            };
        }
        Commands::Format(_) | Commands::Normalize(_) | Commands::Extract { .. } => {}
    }
    match args.command.execute() {
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

/// The rustyline helper: everything defaulted except the [`Validator`],
/// which drives multiline editing — `Incomplete` until the input ends a
/// statement (or is an `exit` / `quit` command, or blank), so Enter
/// continues the same edit buffer instead of submitting. Holds the session
/// dialect so completeness is judged by the same lexical rules the
/// executor parses with.
#[derive(rustyline::Completer, rustyline::Helper, rustyline::Highlighter, rustyline::Hinter)]
struct SqlHelper {
    dialect: Box<dyn sql_insight::sqlparser::dialect::Dialect>,
}

impl rustyline::validate::Validator for SqlHelper {
    fn validate(
        &self,
        ctx: &mut rustyline::validate::ValidationContext,
    ) -> rustyline::Result<rustyline::validate::ValidationResult> {
        let trimmed = ctx.input().trim();
        let complete = trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("exit")
            || trimmed.eq_ignore_ascii_case("quit")
            || statement_complete(self.dialect.as_ref(), ctx.input());
        Ok(if complete {
            rustyline::validate::ValidationResult::Valid(None)
        } else {
            rustyline::validate::ValidationResult::Incomplete
        })
    }
}

/// Whether the buffered input ends a statement: its last token outside
/// whitespace and comments is `;`, judged by **sqlparser's own tokenizer**
/// under the session dialect — the same lexical rules the executor will
/// parse with, so string / identifier quoting (dialect escapes included),
/// dollar quoting, brackets, and every comment form can't diverge from the
/// real parse. A *recoverable* tokenizer error — the input is still inside
/// a literal or comment, so more input can fix it — means incomplete, keep
/// editing; any other tokenizer error won't be fixed by more input, so the
/// statement counts as complete and the executor surfaces the real error
/// visibly instead of trapping the prompt. `TokenizerError` carries no
/// error kind, so recoverability is read off the message: the three
/// signatures below cover every unterminated-construct site in sqlparser
/// 0.62, and deliberately exclude mixed messages like `"Invalid space,
/// tab, newline, or EOF after 'q''"` (an Oracle `q'` followed by a
/// newline is *not* fixable by more input — matching its "EOF" would trap
/// the prompt).
fn statement_complete(dialect: &dyn sql_insight::sqlparser::dialect::Dialect, input: &str) -> bool {
    use sql_insight::sqlparser::tokenizer::{Token, Tokenizer};
    match Tokenizer::new(dialect, input).tokenize() {
        Err(e) => {
            let recoverable = e.message.contains("Unterminated")
                || e.message.contains("before EOF")
                || e.message.contains("Unexpected EOF");
            !recoverable
        }
        Ok(tokens) => matches!(
            tokens
                .iter()
                .rev()
                .find(|t| !matches!(t, Token::Whitespace(_))),
            Some(Token::SemiColon)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::statement_complete;
    use sql_insight::sqlparser::dialect::{
        Dialect, GenericDialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect,
    };

    fn complete(dialect: &dyn Dialect, input: &str) -> bool {
        statement_complete(dialect, input)
    }

    #[test]
    fn splits_on_a_top_level_semicolon_only() {
        let g = GenericDialect {};
        assert!(complete(&g, "SELECT 1;"));
        assert!(complete(&g, "SELECT 1 ;  "));
        assert!(!complete(&g, "SELECT 1"));
        // A trailing comment after the terminator doesn't hide it (comments
        // are whitespace tokens).
        assert!(complete(&g, "SELECT 1; -- done"));
        assert!(complete(&g, "SELECT 1; /* done */"));
    }

    #[test]
    fn a_semicolon_inside_a_literal_does_not_terminate() {
        let g = GenericDialect {};
        // The literal continues on the next line — the old line-suffix check
        // executed the unterminated statement here.
        assert!(!complete(&g, "SELECT 'a;\n"));
        assert!(complete(&g, "SELECT 'a;\nb';"));
        // Quoted identifiers likewise ("…" and MySQL `…`).
        assert!(!complete(&g, "SELECT \"a;\n"));
        assert!(!complete(&MySqlDialect {}, "SELECT `a;\n"));
        // `''` is an escaped quote: `'a'';'` is one literal containing `a';`.
        assert!(!complete(&g, "SELECT 'a'';'"));
        assert!(complete(&g, "SELECT 'a'';';"));
    }

    #[test]
    fn comments_hide_their_semicolons() {
        let g = GenericDialect {};
        assert!(!complete(&g, "SELECT 1 -- ;\n"));
        assert!(complete(&g, "SELECT 1 -- ;\n;"));
        assert!(!complete(&g, "SELECT 1 /* ; */"));
        // An unterminated block comment keeps the statement open.
        assert!(!complete(&g, "SELECT 1; /* ;"));
    }

    #[test]
    fn dialect_lexing_is_the_executors() {
        // PostgreSQL dollar quoting — the everyday CREATE FUNCTION shape —
        // and a `$1` placeholder, which is not an opener.
        let pg = PostgreSqlDialect {};
        assert!(!complete(&pg, "SELECT $$a;$$"));
        assert!(complete(&pg, "SELECT $$a;$$;"));
        assert!(complete(&pg, "SELECT $body$ x; y; $body$;"));
        assert!(complete(&pg, "SELECT $1;"));
        // MySQL: `\'` escapes inside the literal, and `#` starts a comment —
        // both judged by the dialect's own lexer (the hand-rolled scanner
        // this replaced had to punt on both).
        let my = MySqlDialect {};
        assert!(!complete(&my, "SELECT 'a\\';"));
        assert!(complete(&my, "SELECT 'a\\'';"));
        assert!(complete(&my, "SELECT 1; # done"));
        // MSSQL bracket identifiers hide their `;`.
        let ms = MsSqlDialect {};
        assert!(!complete(&ms, "SELECT [a;b"));
        assert!(complete(&ms, "SELECT [a;b] FROM t;"));
    }

    #[test]
    fn an_unrecoverable_tokenizer_error_submits_instead_of_trapping() {
        // An Oracle `q'` followed by a newline is a lexical error no amount
        // of further input can fix — its message mentions "EOF" as one of
        // several causes, but it must count as complete so the executor
        // surfaces the real error instead of the prompt swallowing Enter
        // forever.
        use sql_insight::sqlparser::dialect::GenericDialect;
        assert!(complete(&GenericDialect {}, "SELECT q'\n"));
    }
}
