//! Helper functions for lexopt argument parsing

use std::ffi::OsString;
use std::process;

use lexopt::Parser;

/// Exit with an error message
pub fn exit_error(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    process::exit(1);
}

/// Get the next argument, exiting on error
pub fn next_arg(parser: &mut Parser) -> Option<lexopt::Arg<'_>> {
    match parser.next() {
        Ok(arg) => arg,
        Err(e) => exit_error(&e.to_string()),
    }
}

/// Get a required value for an option, exiting on error
pub fn require_value(parser: &mut Parser) -> OsString {
    parser
        .value()
        .unwrap_or_else(|e| exit_error(&e.to_string()))
}

/// Get a required string value for an option
pub fn require_string(parser: &mut Parser) -> String {
    require_value(parser).to_string_lossy().into_owned()
}

/// Require that an input file was specified
pub fn require_input(input: Option<String>, print_usage: fn()) -> String {
    if let Some(f) = input { f } else {
        eprintln!("Error: no input file specified");
        print_usage();
        process::exit(1);
    }
}

/// Require that at least one input file was specified
pub fn require_inputs(inputs: Vec<String>, print_usage: fn()) -> Vec<String> {
    if inputs.is_empty() {
        eprintln!("Error: no input file specified");
        print_usage();
        process::exit(1);
    }
    inputs
}

/// Check for multiple input files and error
pub fn reject_multiple_inputs(input: &Option<String>) {
    if input.is_some() {
        exit_error("multiple input files not supported");
    }
}

/// Handle unexpected argument
pub fn unexpected_arg(arg: lexopt::Arg, print_usage: fn()) -> ! {
    eprintln!("Error: {}", arg.unexpected());
    print_usage();
    process::exit(1);
}
