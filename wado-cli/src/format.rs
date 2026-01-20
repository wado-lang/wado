use std::fs;
use std::path::Path;
use std::process;
use std::time::Instant;

use lexopt::Arg::{Long, Short, Value};

use crate::args::{next_arg, unexpected_arg};

pub struct FormatOptions {
    pub inputs: Vec<String>,
    pub write_in_place: bool,
    pub check: bool,
}

pub fn print_usage() {
    eprintln!("Usage: wado format [options] <file.wado>...");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -w, --write    Write formatted output back to file");
    eprintln!("  --check        Check if file is formatted (exit 1 if not)");
    eprintln!("  --help         Show this help message");
    eprintln!();
    eprintln!("Without -w, outputs formatted code to stdout (single file only).");
}

pub fn parse_args(mut parser: lexopt::Parser) -> FormatOptions {
    let mut inputs: Vec<String> = Vec::new();
    let mut write_in_place = false;
    let mut check = false;

    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Short('w') | Long("write") => write_in_place = true,
            Long("check") => check = true,
            Value(val) => inputs.push(val.to_string_lossy().into_owned()),
            _ => unexpected_arg(arg, print_usage),
        }
    }

    if inputs.is_empty() {
        eprintln!("Error: no input file specified");
        print_usage();
        process::exit(1);
    }

    // Multiple files require -w or --check
    if inputs.len() > 1 && !write_in_place && !check {
        eprintln!("Error: multiple files require -w or --check");
        print_usage();
        process::exit(1);
    }

    FormatOptions {
        inputs,
        write_in_place,
        check,
    }
}

pub fn run(opts: FormatOptions) {
    let mut any_would_reformat = false;
    let mut any_error = false;

    for input in &opts.inputs {
        let path = Path::new(input);

        // Read original source
        let original = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error reading '{input}': {e}");
                any_error = true;
                continue;
            }
        };

        // Format
        let start = Instant::now();
        let formatted = match wado_compiler::format(&original) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{e}");
                any_error = true;
                continue;
            }
        };
        let elapsed_ms = start.elapsed().as_millis();

        if opts.check {
            // Check mode: track if any file would change
            if original != formatted {
                eprintln!("{input}: would reformat");
                any_would_reformat = true;
            }
        } else if opts.write_in_place {
            // Write back to file only if changed
            if original != formatted {
                match fs::write(path, &formatted) {
                    Ok(()) => {
                        eprintln!("Formatted: {input} ({elapsed_ms}ms)");
                    }
                    Err(e) => {
                        eprintln!("Error writing '{input}': {e}");
                        any_error = true;
                    }
                }
            }
        } else {
            // Output to stdout (single file only)
            print!("{formatted}");
        }
    }

    if any_error {
        process::exit(1);
    }
    if opts.check && any_would_reformat {
        process::exit(1);
    }
}
