mod compile;
mod run;

use std::process;

use lexopt::prelude::*;

fn print_usage() {
    eprintln!("Usage: wado <command> [options]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  compile [options] <file.wado>  Compile a Wado source file");
    eprintln!("  run [options] <file.wado>      Compile and run a Wado source file");
    eprintln!();
    eprintln!("Compile options:");
    eprintln!("  -o <file>        Output file path (default: <input>.wasm)");
    eprintln!("  --format <fmt>   Output format: wasm, wat (default: guessed from -o extension)");
    eprintln!();
    eprintln!("Global options:");
    eprintln!("  --help       Show this help message");
    eprintln!("  --version    Show version information");
}

fn print_version() {
    println!("wado {}", env!("CARGO_PKG_VERSION"));
}

fn main() {
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
                    compile::run(opts);
                }
                "run" => {
                    let opts = run::parse_args(parser);
                    run::run(opts);
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
