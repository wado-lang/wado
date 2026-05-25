use std::process;

use lexopt::Arg::{Long, Value};
use mimalloc::MiMalloc;
use wado_cli::args::CliExit;

// `wado serve` is allocation-heavy per request; mimalloc avoids the
// system allocator's cross-thread contention.
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Clone, Copy)]
enum Cmd {
    Init,
    Compile,
    Check,
    Run,
    Serve,
    Test,
    Format,
    Doc,
    Dump,
    Syntax,
    Lsp,
    Query,
}

impl Cmd {
    const ALL: &[Self] = &[
        Self::Init,
        Self::Compile,
        Self::Check,
        Self::Run,
        Self::Serve,
        Self::Test,
        Self::Format,
        Self::Doc,
        Self::Dump,
        Self::Syntax,
        Self::Lsp,
        Self::Query,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Compile => "compile",
            Self::Check => "check",
            Self::Run => "run",
            Self::Serve => "serve",
            Self::Test => "test",
            Self::Format => "format",
            Self::Doc => "doc",
            Self::Dump => "dump",
            Self::Syntax => "syntax",
            Self::Lsp => "lsp",
            Self::Query => "query",
        }
    }

    const fn args(self) -> &'static str {
        match self {
            Self::Compile | Self::Run | Self::Serve | Self::Check => "[options] [file.wado]",
            Self::Test => "[options] [files or dirs...]",
            Self::Format | Self::Doc | Self::Dump => "[options] <file.wado>...",
            Self::Init | Self::Syntax | Self::Lsp => "[options]",
            Self::Query => "<kind> [options] <file.wado>",
        }
    }

    const fn desc(self) -> &'static str {
        match self {
            Self::Init => "Create a new wado.toml manifest",
            Self::Compile => "Compile a Wado source file",
            Self::Check => "Verify Kiln generators match committed source (CI)",
            Self::Run => "Compile and run a Wado CLI program",
            Self::Serve => "Compile and serve a Wado HTTP service",
            Self::Test => "Run tests in Wado source files",
            Self::Format => "Format a Wado source file",
            Self::Doc => "Generate documentation from source files",
            Self::Dump => "Dump compiler internal state",
            Self::Syntax => "Generate syntax definition files",
            Self::Lsp => "Start the language server (LSP over stdio)",
            Self::Query => "Query language service information",
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        Self::ALL.iter().find(|c| c.name() == s).copied()
    }
}

fn write_usage(buf: &mut String) {
    use std::fmt::Write as _;
    writeln!(buf, "Usage: wado <command> [options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Commands:").unwrap();
    let labels: Vec<String> = Cmd::ALL
        .iter()
        .map(|c| format!("{} {}", c.name(), c.args()))
        .collect();
    let max_w = labels.iter().map(String::len).max().unwrap_or(0);
    for (label, cmd) in labels.iter().zip(Cmd::ALL) {
        writeln!(buf, "  {label:<max_w$}  {}", cmd.desc()).unwrap();
    }
    writeln!(buf).unwrap();
    writeln!(buf, "Global options:").unwrap();
    writeln!(buf, "  --help     Show this help message").unwrap();
    writeln!(buf, "  --version  Show version information").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Use 'wado <command> --help' for more information on a command."
    )
    .unwrap();
}

fn print_version() {
    println!("wado {}", env!("CARGO_PKG_VERSION"));
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            eprintln!("Error: failed to create tokio runtime: {e}");
            process::exit(1);
        });
    let outcome = runtime.block_on(async_main());
    outcome.exit();
}

async fn async_main() -> CliExit {
    match dispatch().await {
        Ok(()) => CliExit::silent_failure(0),
        Err(exit) => exit,
    }
}

async fn dispatch() -> Result<(), CliExit> {
    let mut parser = lexopt::Parser::from_env();

    let Some(arg) = parser.next().map_err(CliExit::error)? else {
        let mut usage = String::new();
        write_usage(&mut usage);
        return Err(CliExit::error_with_usage("missing command", &usage));
    };

    match arg {
        Long("help") => {
            let mut usage = String::new();
            write_usage(&mut usage);
            Err(CliExit::help(usage))
        }
        Long("version") => {
            print_version();
            Ok(())
        }
        Value(cmd_val) => {
            let cmd_str = cmd_val.to_string_lossy();
            let Some(cmd) = Cmd::from_name(&cmd_str) else {
                let mut usage = String::new();
                write_usage(&mut usage);
                return Err(CliExit::error_with_usage(
                    format!("unknown command '{cmd_str}'"),
                    &usage,
                ));
            };
            match cmd {
                Cmd::Init => {
                    let opts = wado_cli::init::parse_args(parser)?;
                    wado_cli::init::run(opts)
                }
                // Each subcommand's future is boxed so `dispatch`'s state
                // machine doesn't recursively inline all 12 subcommands'
                // await chains and blow past Rust's query-depth limit on
                // `--release` builds.
                Cmd::Compile => {
                    let opts = wado_cli::compile::parse_args(parser)?;
                    Box::pin(wado_cli::compile::run(opts)).await
                }
                Cmd::Check => {
                    let opts = wado_cli::check::parse_args(parser)?;
                    Box::pin(wado_cli::check::run(opts)).await
                }
                Cmd::Run => {
                    let opts = wado_cli::run::parse_args(parser)?;
                    Box::pin(wado_cli::run::run(opts)).await
                }
                Cmd::Serve => {
                    let opts = wado_cli::serve::parse_args(parser)?;
                    Box::pin(wado_cli::serve::run(opts)).await
                }
                Cmd::Test => {
                    let opts = wado_cli::test::parse_args(parser)?;
                    Box::pin(wado_cli::test::run(opts)).await
                }
                Cmd::Format => {
                    let opts = wado_cli::format::parse_args(parser)?;
                    wado_cli::format::run(opts)
                }
                Cmd::Doc => {
                    let opts = wado_cli::doc::parse_args(parser)?;
                    wado_cli::doc::run(opts)
                }
                Cmd::Dump => {
                    let opts = wado_cli::dump::parse_args(parser)?;
                    Box::pin(wado_cli::dump::run(opts)).await
                }
                Cmd::Syntax => {
                    let opts = wado_cli::syntax::parse_args(parser)?;
                    wado_cli::syntax::run(opts)
                }
                Cmd::Lsp => Box::pin(wado_cli::lsp::run()).await,
                Cmd::Query => {
                    let opts = wado_cli::query::parse_args(parser)?;
                    Box::pin(wado_cli::query::run(opts)).await
                }
            }
        }
        _ => {
            let mut usage = String::new();
            write_usage(&mut usage);
            Err(CliExit::error_with_usage("expected command", &usage))
        }
    }
}
