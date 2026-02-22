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

fn print_usage() {
    eprintln!("Usage: wado <command> [options]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  compile [options] <file.wado>  Compile a Wado source file");
    eprintln!("  run [options] <file.wado>      Compile and run a Wado CLI program");
    eprintln!("  serve [options] <file.wado>    Compile and serve a Wado HTTP service");
    eprintln!("  test [options] <files...>      Run tests in Wado source files");
    eprintln!("  format [options] <file.wado>   Format a Wado source file");
    eprintln!("  dump [options] <file.wado>     Dump compiler internal state");
    eprintln!("  syntax [options]               Generate syntax definition files");
    eprintln!();
    eprintln!("Compile options:");
    eprintln!("  -o <file>        Output file path (default: <input>.wasm)");
    eprintln!("  --format <fmt>   Output format: wasm, wat (default: guessed from -o extension)");
    eprintln!();
    eprintln!("Run options:");
    eprintln!("  -O<n>            Optimization level: -O0, -O1, -O2, -O3, -Os");
    eprintln!("  --profile <mode> Profiling: guest, jitdump, perfmap");
    eprintln!();
    eprintln!("Serve options:");
    eprintln!("  --addr <addr>    Address to listen on (default: 0.0.0.0:8080)");
    eprintln!("  -O<n>            Optimization level: -O0, -O1, -O2, -O3, -Os");
    eprintln!();
    eprintln!("Test options:");
    eprintln!("  -f, --filter <pattern>  Filter tests by name pattern");
    eprintln!("  -p, --parallel <N>      Number of parallel workers (default: num CPUs)");
    eprintln!();
    eprintln!("Format options:");
    eprintln!("  -w, --write      Write formatted output back to file");
    eprintln!("  --check          Check if file is formatted (exit 1 if not)");
    eprintln!();
    eprintln!("Dump options:");
    eprintln!("  --ast        Show the AST");
    eprintln!("  --symbols    Show the symbol table");
    eprintln!("  --modules    Show loaded modules");
    eprintln!("  --all        Show all information (default)");
    eprintln!();
    eprintln!("Global options:");
    eprintln!("  --help       Show this help message");
    eprintln!("  --version    Show version information");
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
        Value(cmd) => {
            let cmd = cmd.to_string_lossy();
            match cmd.as_ref() {
                "compile" => {
                    let opts = compile::parse_args(parser);
                    compile::run(opts).await;
                }
                "run" => {
                    let opts = run::parse_args(parser);
                    run::run(opts).await;
                }
                "serve" => {
                    let opts = serve::parse_args(parser);
                    serve::run(opts).await;
                }
                "dump" => {
                    let opts = dump::parse_args(parser);
                    dump::run(opts).await;
                }
                "format" => {
                    let opts = format::parse_args(parser);
                    format::run(opts);
                }
                "syntax" => {
                    let opts = syntax::parse_args(parser);
                    syntax::run(opts);
                }
                "test" => {
                    let opts = test::parse_args(parser);
                    test::run(opts).await;
                }
                _ => {
                    eprintln!("Error: unknown command '{cmd}'");
                    print_usage();
                    process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("Error: expected command");
            print_usage();
            process::exit(1);
        }
    }
}
