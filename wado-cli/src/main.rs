use std::process;

use lexopt::Arg::{Long, Value};
use mimalloc::MiMalloc;

// `wado serve` allocates and frees a burst of small per-request objects
// (request/response buffers, header maps, resource tables) on every request.
// The system allocator's cross-thread contention dominates the host CPU
// profile under load, so route all allocation through mimalloc.
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

fn print_usage() {
    eprintln!("Usage: wado <command> [options]");
    eprintln!();
    eprintln!("Commands:");
    let labels: Vec<String> = Cmd::ALL
        .iter()
        .map(|c| format!("{} {}", c.name(), c.args()))
        .collect();
    let max_w = labels.iter().map(String::len).max().unwrap_or(0);
    for (label, cmd) in labels.iter().zip(Cmd::ALL) {
        eprintln!("  {label:<max_w$}  {}", cmd.desc());
    }
    eprintln!();
    eprintln!("Global options:");
    eprintln!("  --help     Show this help message");
    eprintln!("  --version  Show version information");
    eprintln!();
    eprintln!("Use 'wado <command> --help' for more information on a command.");
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
    runtime.block_on(async_main());
}

async fn async_main() {
    let mut parser = lexopt::Parser::from_env();

    let Some(arg) = parser.next().unwrap_or_else(|e| {
        eprintln!("Error: {e}");
        process::exit(1);
    }) else {
        print_usage();
        process::exit(1);
    };

    match arg {
        Long("help") => {
            print_usage();
            process::exit(0);
        }
        Long("version") => {
            print_version();
            process::exit(0);
        }
        Value(cmd_val) => {
            let cmd_str = cmd_val.to_string_lossy();
            if let Some(cmd) = Cmd::from_name(&cmd_str) {
                match cmd {
                    Cmd::Init => {
                        let opts = wado_cli::init::parse_args(parser).unwrap_or_else(|e| e.exit());
                        wado_cli::init::run(opts);
                    }
                    // Each subcommand's `async fn run()` is wrapped in
                    // `Box::pin` so its future is type-erased at this
                    // boundary. Without this, the compiler must compute
                    // the layout of `async_main`'s state machine by
                    // recursively inlining the futures of EVERY subcommand
                    // (12 of them), each of which transitively pulls in
                    // its own await chain (compile → try_compile →
                    // wado_compiler::compile_with_options → ...). That
                    // recursion blows past Rust's default query-depth
                    // limit on `--release` builds. Boxing here costs one
                    // heap allocation per CLI invocation, which is free
                    // for a one-shot binary.
                    Cmd::Compile => {
                        let opts =
                            wado_cli::compile::parse_args(parser).unwrap_or_else(|e| e.exit());
                        Box::pin(wado_cli::compile::run(opts)).await;
                    }
                    Cmd::Check => {
                        let opts = wado_cli::check::parse_args(parser).unwrap_or_else(|e| e.exit());
                        Box::pin(wado_cli::check::run(opts)).await;
                    }
                    Cmd::Run => {
                        let opts = wado_cli::run::parse_args(parser).unwrap_or_else(|e| e.exit());
                        Box::pin(wado_cli::run::run(opts)).await;
                    }
                    Cmd::Serve => {
                        let opts = wado_cli::serve::parse_args(parser).unwrap_or_else(|e| e.exit());
                        Box::pin(wado_cli::serve::run(opts)).await;
                    }
                    Cmd::Test => {
                        let opts = wado_cli::test::parse_args(parser).unwrap_or_else(|e| e.exit());
                        Box::pin(wado_cli::test::run(opts)).await;
                    }
                    Cmd::Format => {
                        let opts =
                            wado_cli::format::parse_args(parser).unwrap_or_else(|e| e.exit());
                        wado_cli::format::run(opts);
                    }
                    Cmd::Doc => {
                        let opts = wado_cli::doc::parse_args(parser).unwrap_or_else(|e| e.exit());
                        wado_cli::doc::run(opts);
                    }
                    Cmd::Dump => {
                        let opts = wado_cli::dump::parse_args(parser).unwrap_or_else(|e| e.exit());
                        Box::pin(wado_cli::dump::run(opts)).await;
                    }
                    Cmd::Syntax => {
                        let opts =
                            wado_cli::syntax::parse_args(parser).unwrap_or_else(|e| e.exit());
                        wado_cli::syntax::run(opts);
                    }
                    Cmd::Lsp => {
                        Box::pin(wado_cli::lsp::run()).await;
                    }
                    Cmd::Query => {
                        let opts = wado_cli::query::parse_args(parser).unwrap_or_else(|e| e.exit());
                        Box::pin(wado_cli::query::run(opts)).await;
                    }
                }
            } else {
                eprintln!("Error: unknown command '{cmd_str}'");
                print_usage();
                process::exit(1);
            }
        }
        _ => {
            eprintln!("Error: expected command");
            print_usage();
            process::exit(1);
        }
    }
}
