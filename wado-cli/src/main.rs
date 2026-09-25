use std::process;

use lexopt::Arg::{Long, Short, Value};
use mimalloc::MiMalloc;
use wado_cli::args::CliExit;

// `wado serve` is allocation-heavy per request; mimalloc avoids the
// system allocator's cross-thread contention.
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Clone, Copy)]
enum Cmd {
    Init,
    Update,
    Fetch,
    Clean,
    Build,
    Compile,
    Check,
    Run,
    Serve,
    Test,
    Format,
    Doc,
    Dump,
    Wit,
    Syntax,
    Lsp,
    Query,
    Publish,
    Help,
}

impl Cmd {
    const ALL: &[Self] = &[
        Self::Init,
        Self::Update,
        Self::Fetch,
        Self::Clean,
        Self::Build,
        Self::Compile,
        Self::Check,
        Self::Run,
        Self::Serve,
        Self::Test,
        Self::Format,
        Self::Doc,
        Self::Dump,
        Self::Wit,
        Self::Syntax,
        Self::Lsp,
        Self::Query,
        Self::Publish,
        Self::Help,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Update => "update",
            Self::Fetch => "fetch",
            Self::Clean => "clean",
            Self::Build => "build",
            Self::Compile => "compile",
            Self::Check => "check",
            Self::Run => "run",
            Self::Serve => "serve",
            Self::Test => "test",
            Self::Format => "format",
            Self::Doc => "doc",
            Self::Dump => "dump",
            Self::Wit => "wit",
            Self::Syntax => "syntax",
            Self::Lsp => "lsp",
            Self::Query => "query",
            Self::Publish => "publish",
            Self::Help => "help",
        }
    }

    const fn args(self) -> &'static str {
        match self {
            Self::Run | Self::Serve => "[options] [file.wado]",
            Self::Check => "[options] [file.wado | dir]",
            Self::Compile => "[options] <file.wado>",
            Self::Wit => "[options] [file.wado | dir]",
            Self::Test => "[options] [files or dirs...]",
            Self::Format | Self::Doc | Self::Dump => "[options] <file.wado>...",
            Self::Init
            | Self::Update
            | Self::Fetch
            | Self::Clean
            | Self::Build
            | Self::Syntax
            | Self::Lsp
            | Self::Publish => "[options]",
            Self::Query => "<kind> [options] <file.wado>",
            Self::Help => "[command]",
        }
    }

    const fn desc(self) -> &'static str {
        match self {
            Self::Init => "Create a new wado.toml manifest",
            Self::Update => "Resolve dependencies and write wado.lock",
            Self::Fetch => "Download the project's registry dependencies",
            Self::Clean => "Evict derived cache state (git worktrees)",
            Self::Build => "Build the project's worlds from wado.toml",
            Self::Compile => "Compile a single Wado source file",
            Self::Check => "Verify a source file and its Kiln generators",
            Self::Run => "Compile and run a Wado CLI program",
            Self::Serve => "Compile and serve a Wado HTTP service",
            Self::Test => "Run tests in Wado source files",
            Self::Format => "Format a Wado source file",
            Self::Doc => "Generate documentation from source files",
            Self::Dump => "Dump compiler internal state",
            Self::Wit => "Emit the WIT contract for a Wado program",
            Self::Syntax => "Generate syntax definition files",
            Self::Lsp => "Start the language server (LSP over stdio)",
            Self::Query => "Query language service information",
            Self::Publish => "Check whether the package can be published",
            Self::Help => "Show a command's help, builtin or external",
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        Self::ALL.iter().find(|c| c.name() == s).copied()
    }
}

fn usage() -> String {
    use std::fmt::Write as _;
    let mut buf = String::new();
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
    writeln!(buf, "  --list     List every command, builtin and external").unwrap();
    writeln!(buf, "  --version  Show version information").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Use 'wado <command> --help' for more information on a command."
    )
    .unwrap();
    buf
}

/// Print the builtins and every `wado-<name>` on `PATH`, a shadowed external
/// included: nothing else would say the file is there and will never run.
fn print_list() {
    let externals = wado_cli::external::discover();
    let width = Cmd::ALL
        .iter()
        .map(|c| c.name().len())
        .chain(externals.iter().map(|e| e.name.len()))
        .max()
        .unwrap_or(0);

    println!("Commands:");
    for cmd in Cmd::ALL {
        println!("  {:<width$}  {}", cmd.name(), cmd.desc());
    }

    if externals.is_empty() {
        return;
    }
    println!();
    println!("External commands:");
    for ext in &externals {
        let shadowed = if Cmd::from_name(&ext.name).is_some() {
            "  (shadowed by the builtin; never run)"
        } else {
            ""
        };
        println!("  {:<width$}  {}{shadowed}", ext.name, ext.path.display());
    }
}

fn print_version() {
    println!("wado {}", env!("CARGO_PKG_VERSION"));
}

fn main() {
    // Resolve the Wado root from the config file into `$WADO_ROOT` before any
    // threads (the tokio runtime below) start, so the whole process — including
    // the embedded LSP server — shares one configured cache location.
    wado_cli::cache::init_root_from_config();

    // A dev build takes the stdlib from its host. Installing it here means no
    // subcommand can reach the stdlib before it is there.
    wado_lsp::host::install_dev_stdlib();

    // The compiler recurses as deep as the source nests, so every thread that
    // compiles gets this stack: the pool's, and the one `block_on` runs on.
    const STACK_SIZE: usize = 64 * 1024 * 1024;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(STACK_SIZE)
        .build()
        .unwrap_or_else(|e| {
            eprintln!("Error: failed to create tokio runtime: {e}");
            process::exit(1);
        });
    let driver = std::thread::Builder::new()
        .name("main".to_string())
        .stack_size(STACK_SIZE)
        .spawn(move || runtime.block_on(async_main()))
        .unwrap_or_else(|e| {
            eprintln!("Error: failed to start the driver thread: {e}");
            process::exit(1);
        });
    let outcome = driver
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
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
        return Err(CliExit::error_with_usage("missing command", &usage()));
    };

    match arg {
        Long("help") => Err(CliExit::help(usage())),
        Long("list") => {
            print_list();
            Ok(())
        }
        Long("version") => {
            print_version();
            Ok(())
        }
        Value(cmd_val) => {
            let name = cmd_val.to_string_lossy().into_owned();
            // The builtin table answers first, so `wado-<name>` on `PATH` only
            // ever adds a name and can never change what a builtin does.
            if let Some(cmd) = Cmd::from_name(&name) {
                return run_cmd(cmd, parser).await;
            }
            let Some(path) = wado_cli::external::find(&name) else {
                return Err(unknown_command(&name));
            };
            wado_cli::external::run(&path, parser.raw_args().map_err(CliExit::error)?)
        }
        _ => Err(CliExit::error_with_usage("expected command", &usage())),
    }
}

/// `wado help <name>` is `wado <name> --help`, and for an external subcommand
/// the child is what answers.
async fn run_help(mut parser: lexopt::Parser) -> Result<(), CliExit> {
    let Some(arg) = parser.next().map_err(CliExit::error)? else {
        return Err(CliExit::help(usage()));
    };
    let name = match arg {
        Value(name_val) => name_val.to_string_lossy().into_owned(),
        // Asking `help` about itself asks what `wado help` asks, and the
        // command list is the answer.
        Long("help") | Short('h') => return Err(CliExit::help(usage())),
        other => return Err(CliExit::error_with_usage(other.unexpected(), &usage())),
    };
    if let Some(surplus) = parser.next().map_err(CliExit::error)? {
        return Err(CliExit::error_with_usage(surplus.unexpected(), &usage()));
    }

    if let Some(cmd) = Cmd::from_name(&name) {
        return Box::pin(run_cmd(cmd, lexopt::Parser::from_args(["--help"]))).await;
    }
    let Some(path) = wado_cli::external::find(&name) else {
        return Err(unknown_command(&name));
    };
    wado_cli::external::run(&path, ["--help"])
}

/// The error for a name that is neither builtin nor on `PATH`, which says why
/// nothing ran.
fn unknown_command(name: &str) -> CliExit {
    let reason = wado_cli::external::not_found_reason(name);
    CliExit::error_with_usage(format!("unknown command '{name}' ({reason})"), &usage())
}

async fn run_cmd(cmd: Cmd, parser: lexopt::Parser) -> Result<(), CliExit> {
    match cmd {
        Cmd::Init => {
            let opts = wado_cli::init::parse_args(parser)?;
            wado_cli::init::run(opts)
        }
        Cmd::Update => {
            let opts = wado_cli::update::parse_args(parser)?;
            Box::pin(wado_cli::update::run(opts)).await
        }
        Cmd::Fetch => {
            let opts = wado_cli::fetch::parse_args(parser)?;
            Box::pin(wado_cli::fetch::run(opts)).await
        }
        Cmd::Clean => {
            let opts = wado_cli::clean::parse_args(parser)?;
            wado_cli::clean::run(opts)
        }
        Cmd::Build => {
            let opts = wado_cli::build::parse_args(parser)?;
            Box::pin(wado_cli::build::run(opts)).await
        }
        // Each subcommand's future is boxed, or this state machine inlines
        // every subcommand's await chain into one and a `--release` build
        // passes Rust's query-depth limit.
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
        Cmd::Wit => {
            let opts = wado_cli::wit::parse_args(parser)?;
            Box::pin(wado_cli::wit::run(opts)).await
        }
        Cmd::Syntax => {
            let opts = wado_cli::syntax::parse_args(parser)?;
            wado_cli::syntax::run(opts)
        }
        Cmd::Lsp => {
            let opts = wado_cli::lsp::parse_args(parser)?;
            Box::pin(wado_cli::lsp::run(opts)).await
        }
        Cmd::Query => {
            let opts = wado_cli::query::parse_args(parser)?;
            Box::pin(wado_cli::query::run(opts)).await
        }
        Cmd::Publish => {
            let opts = wado_cli::publish::parse_args(parser)?;
            Box::pin(wado_cli::publish::run(opts)).await
        }
        Cmd::Help => Box::pin(run_help(parser)).await,
    }
}
