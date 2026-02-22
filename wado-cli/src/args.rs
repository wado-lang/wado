//! Helper functions for lexopt argument parsing

use std::ffi::OsString;
use std::process;

use lexopt::Parser;
use wado_compiler::LogLevel;

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
    if let Some(f) = input {
        f
    } else {
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

/// Specification for a single CLI option, used for both matching and help generation.
///
/// Define option specs as `const fn spec(&self) -> OptSpec` on per-subcommand enums.
/// This ensures exhaustive match catches missing definitions at compile time.
#[derive(Clone, Copy)]
pub struct OptSpec {
    /// Long option name (without `--`), e.g., `Some("format")`
    pub long: Option<&'static str>,
    /// Short option character, e.g., `Some('o')`
    pub short: Option<char>,
    /// Value placeholder for help text, e.g., `Some("<fmt>")`. `None` for flags.
    pub value: Option<&'static str>,
    /// Description for help text. Use `\n` for continuation lines.
    pub desc: &'static str,
}

/// Shared spec: `--help`
pub const HELP_SPEC: OptSpec = OptSpec {
    long: Some("help"),
    short: None,
    value: None,
    desc: "Show this help message",
};

/// Shared spec: `-O <n>`
pub const OPT_LEVEL_SPEC: OptSpec = OptSpec {
    long: None,
    short: Some('O'),
    value: Some("<n>"),
    desc: "Optimization level: -O0, -O1, -O2, -O3, -Os",
};

/// Shared spec: `--log-level <level>`
pub const LOG_LEVEL_SPEC: OptSpec = OptSpec {
    long: Some("log-level"),
    short: None,
    value: Some("<level>"),
    desc: "Log level: debug, info, warn, error, off (default: info)",
};

/// Match a `lexopt::Arg` against a set of option variants using their specs.
///
/// Returns the matching variant if the argument matches a long or short option
/// defined in the specs. Returns `None` for positional `Value` arguments.
pub fn match_opt<T: Copy>(
    arg: &lexopt::Arg<'_>,
    all: &[T],
    spec: impl Fn(&T) -> OptSpec,
) -> Option<T> {
    match arg {
        lexopt::Arg::Long(name) => all.iter().find(|t| spec(t).long == Some(*name)).copied(),
        lexopt::Arg::Short(c) => all.iter().find(|t| spec(t).short == Some(*c)).copied(),
        lexopt::Arg::Value(_) => None,
    }
}

/// Format an option spec as a help label.
///
/// - `-o <file>`, `--format <fmt>`, `-f, --filter <pattern>`, `--help`
fn format_opt_label(spec: &OptSpec) -> String {
    let mut result = String::new();
    if let Some(c) = spec.short {
        result.push('-');
        result.push(c);
    }
    if spec.short.is_some() && spec.long.is_some() {
        result.push_str(", ");
    }
    if let Some(l) = spec.long {
        result.push_str("--");
        result.push_str(l);
    }
    if let Some(v) = spec.value {
        result.push(' ');
        result.push_str(v);
    }
    result
}

/// Print help entries from option variants, auto-aligned.
///
/// Each option's label is generated from its spec and descriptions are
/// aligned to the widest label. Multi-line descriptions (using `\n`) are
/// printed with continuation lines indented to the description column.
pub fn print_opts_help<T>(all: &[T], spec: impl Fn(&T) -> OptSpec) {
    let labels: Vec<String> = all.iter().map(|t| format_opt_label(&spec(t))).collect();
    let max_w = labels.iter().map(String::len).max().unwrap_or(0);
    for (label, t) in labels.iter().zip(all) {
        let s = spec(t);
        let mut lines = s.desc.lines();
        if let Some(first) = lines.next() {
            eprintln!("  {label:<max_w$}  {first}");
            for cont in lines {
                eprintln!("  {:<max_w$}  {cont}", "");
            }
        }
    }
}

/// Parse log level from string (consolidated from duplicate implementations).
pub fn parse_log_level(s: &str) -> Option<LogLevel> {
    match s.to_lowercase().as_str() {
        "debug" => Some(LogLevel::Debug),
        "info" => Some(LogLevel::Info),
        "warn" | "warning" => Some(LogLevel::Warn),
        "error" => Some(LogLevel::Error),
        "off" | "none" => Some(LogLevel::Off),
        _ => None,
    }
}

/// Parse `--log-level` argument value from parser, exiting on error.
pub fn parse_log_level_arg(parser: &mut Parser) -> LogLevel {
    let level_str = require_string(parser);
    if let Some(level) = parse_log_level(&level_str) {
        level
    } else {
        eprintln!(
            "Error: unknown log level '{level_str}'. Use debug, info, warn, error, or off"
        );
        process::exit(1);
    }
}
