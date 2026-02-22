// Allow certain pedantic lints that are common in CLI code:
// - ref_option: &Option<T> is fine when coming from struct fields
// - missing_panics_doc: Mutex unwrap panics are obvious
// - struct_excessive_bools: CLI option structs naturally have many bools
// - too_many_lines: Large functions are acceptable in CLI code
// - needless_pass_by_value: Ownership transfer is sometimes intentional
#![allow(
    clippy::ref_option,
    clippy::missing_panics_doc,
    clippy::struct_excessive_bools,
    clippy::too_many_lines,
    clippy::needless_pass_by_value
)]

mod args;
mod compile;
mod compiler_host;
mod dump;
mod format;
mod run;
mod runtime;
mod serve;
mod syntax;
mod test;

pub use compiler_host::FilesystemCompilerHost;

use std::process;

use lexopt::Arg::{Long, Value};

#[derive(Clone, Copy)]
enum Cmd {
    Compile,
    Run,
    Serve,
    Test,
    Format,
    Dump,
    Syntax,
}

impl Cmd {
    const ALL: &[Self] = &[
        Self::Compile,
        Self::Run,
        Self::Serve,
        Self::Test,
        Self::Format,
        Self::Dump,
        Self::Syntax,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Compile => "compile",
            Self::Run => "run",
            Self::Serve => "serve",
            Self::Test => "test",
            Self::Format => "format",
            Self::Dump => "dump",
            Self::Syntax => "syntax",
        }
    }

    const fn args(self) -> &'static str {
        match self {
            Self::Compile | Self::Run | Self::Serve => "[options] <file.wado>",
            Self::Test => "[options] [files...]",
            Self::Format | Self::Dump => "[options] <file.wado>...",
            Self::Syntax => "[options]",
        }
    }

    const fn desc(self) -> &'static str {
        match self {
            Self::Compile => "Compile a Wado source file",
            Self::Run => "Compile and run a Wado CLI program",
            Self::Serve => "Compile and serve a Wado HTTP service",
            Self::Test => "Run tests in Wado source files",
            Self::Format => "Format a Wado source file",
            Self::Dump => "Dump compiler internal state",
            Self::Syntax => "Generate syntax definition files",
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

#[tokio::main]
async fn main() {
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
                    Cmd::Compile => {
                        let opts = compile::parse_args(parser);
                        compile::run(opts).await;
                    }
                    Cmd::Run => {
                        let opts = run::parse_args(parser);
                        run::run(opts).await;
                    }
                    Cmd::Serve => {
                        let opts = serve::parse_args(parser);
                        serve::run(opts).await;
                    }
                    Cmd::Test => {
                        let opts = test::parse_args(parser);
                        test::run(opts).await;
                    }
                    Cmd::Format => {
                        let opts = format::parse_args(parser);
                        format::run(opts);
                    }
                    Cmd::Dump => {
                        let opts = dump::parse_args(parser);
                        dump::run(opts).await;
                    }
                    Cmd::Syntax => {
                        let opts = syntax::parse_args(parser);
                        syntax::run(opts);
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
