use std::fs;
use std::path::Path;
use std::process;
use std::time::Instant;

use lexopt::Arg::Value;

use crate::args::{self, next_arg, unexpected_arg};

pub struct FormatOptions {
    pub inputs: Vec<String>,
    pub write_in_place: bool,
    pub check: bool,
}

#[derive(Clone, Copy)]
enum Opt {
    Write,
    Check,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Write, Self::Check, Self::Help];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Write => args::OptSpec {
                long: Some("write"),
                short: Some('w'),
                value: None,
                desc: "Write formatted output back to file",
            },
            Self::Check => args::OptSpec {
                long: Some("check"),
                short: None,
                value: None,
                desc: "Check if file is formatted (exit 1 if not)",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

pub fn print_usage() {
    eprintln!("Usage: wado format [options] <file.wado>...");
    eprintln!();
    eprintln!("Options:");
    args::print_opts_help(Opt::ALL, |o| o.spec());
    eprintln!();
    eprintln!("Without -w, outputs formatted code to stdout (single file only).");
}

pub fn parse_args(mut parser: lexopt::Parser) -> FormatOptions {
    let mut inputs: Vec<String> = Vec::new();
    let mut write_in_place = false;
    let mut check = false;

    while let Some(arg) = next_arg(&mut parser) {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Write => write_in_place = true,
                Opt::Check => check = true,
                Opt::Help => {
                    print_usage();
                    process::exit(0);
                }
            }
        } else if let Value(val) = arg {
            inputs.push(val.to_string_lossy().into_owned());
        } else {
            unexpected_arg(arg, print_usage);
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
