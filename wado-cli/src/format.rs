use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::time::Instant;

use lexopt::Arg::Value;

use crate::args::{self, CliExit};

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

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado format [options] <file.wado>...").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Without -w, outputs formatted code to stdout (single file only)."
    )
    .unwrap();
    buf
}

pub fn print_usage() {
    eprint!("{}", format_usage());
}

/// Parse command-line arguments for the `format` subcommand.
///
/// # Errors
///
/// Returns an error if the arguments are invalid or required arguments are missing.
pub fn parse_args(mut parser: lexopt::Parser) -> Result<FormatOptions, CliExit> {
    let usage = format_usage();
    let mut inputs: Vec<String> = Vec::new();
    let mut write_in_place = false;
    let mut check = false;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Write => write_in_place = true,
                Opt::Check => check = true,
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            inputs.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    if inputs.is_empty() {
        return Err(CliExit::error_with_usage("no input file specified", &usage));
    }

    // Multiple files require -w or --check
    if inputs.len() > 1 && !write_in_place && !check {
        return Err(CliExit::error_with_usage(
            "multiple files require -w or --check",
            &usage,
        ));
    }

    Ok(FormatOptions {
        inputs,
        write_in_place,
        check,
    })
}

pub fn run(opts: FormatOptions) -> Result<(), CliExit> {
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

    if any_error || (opts.check && any_would_reformat) {
        return Err(CliExit::silent_failure(1));
    }
    Ok(())
}
