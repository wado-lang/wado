mod build;
mod run;

use std::env;
use std::process;

fn print_usage() {
    eprintln!("Usage: wado <command> [options]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  build [options] <file.wado>  Compile a Wado source file");
    eprintln!("  run [options] <file.wado>    Compile and run a Wado source file");
    eprintln!();
    eprintln!("Build options:");
    eprintln!("  -o <file>  Output file path (default: <input>.wasm)");
    eprintln!();
    eprintln!("Global options:");
    eprintln!("  --help, -h  Show this help message");
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    match args[1].as_str() {
        "--help" | "-h" => {
            print_usage();
            process::exit(0);
        }
        "build" => {
            let opts = build::parse_args(&args[2..]);
            build::run(opts);
        }
        "run" => {
            let opts = run::parse_args(&args[2..]);
            run::run(opts);
        }
        cmd => {
            eprintln!("Error: unknown command '{cmd}'");
            print_usage();
            process::exit(1);
        }
    }
}
