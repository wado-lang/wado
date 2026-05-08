//! Helper functions for lexopt argument parsing

use std::ffi::OsString;
use std::fmt::Write;
use std::process;

use lexopt::Parser;
use wado_compiler::LogLevel;

/// Represents a CLI exit (help or error) without calling `process::exit()`.
///
/// Returned by `parse_args` functions to enable in-process testing.
#[derive(Debug)]
pub struct CliExit {
    /// The full message to display (to stderr).
    pub message: String,
    /// Process exit code (0 for help, 1 for errors).
    pub exit_code: i32,
}

impl CliExit {
    /// Create an error exit with a message.
    pub fn error(msg: impl std::fmt::Display) -> Self {
        Self {
            message: format!("Error: {msg}\n"),
            exit_code: 1,
        }
    }

    /// Create an error exit with a message followed by usage text.
    pub fn error_with_usage(msg: impl std::fmt::Display, usage: &str) -> Self {
        Self {
            message: format!("Error: {msg}\n{usage}"),
            exit_code: 1,
        }
    }

    /// Create a help exit (exit code 0).
    #[must_use]
    pub fn help(usage: String) -> Self {
        Self {
            message: usage,
            exit_code: 0,
        }
    }

    /// Exit the process with the stored message and code.
    pub fn exit(&self) -> ! {
        eprint!("{}", self.message);
        process::exit(self.exit_code);
    }
}

/// Exit with an error message (for use in `run()` functions that still need `-> !`).
pub fn exit_error(msg: &str) -> ! {
    CliExit::error(msg).exit()
}

/// Get the next argument, returning an error on failure.
///
/// # Errors
///
/// Returns an error if the parser fails to retrieve the next argument.
pub fn next_arg(parser: &mut Parser) -> Result<Option<lexopt::Arg<'_>>, CliExit> {
    parser.next().map_err(CliExit::error)
}

/// Get a required value for an option.
///
/// # Errors
///
/// Returns an error if no value is provided for the option.
pub fn require_value(parser: &mut Parser) -> Result<OsString, CliExit> {
    parser.value().map_err(CliExit::error)
}

/// Get a required string value for an option.
///
/// # Errors
///
/// Returns an error if no value is provided for the option.
pub fn require_string(parser: &mut Parser) -> Result<String, CliExit> {
    Ok(require_value(parser)?.to_string_lossy().into_owned())
}

/// Require that an input file was specified.
///
/// # Errors
///
/// Returns an error if no input file was provided.
pub fn require_input(input: Option<String>, usage: &str) -> Result<String, CliExit> {
    input.ok_or_else(|| CliExit::error_with_usage("no input file specified", usage))
}

/// Require that at least one input file was specified.
///
/// # Errors
///
/// Returns an error if no input files were provided.
pub fn require_inputs(inputs: Vec<String>, usage: &str) -> Result<Vec<String>, CliExit> {
    if inputs.is_empty() {
        Err(CliExit::error_with_usage("no input file specified", usage))
    } else {
        Ok(inputs)
    }
}

/// Check for multiple input files and error.
///
/// # Errors
///
/// Returns an error if multiple input files were specified.
pub fn reject_multiple_inputs(input: &Option<String>) -> Result<(), CliExit> {
    if input.is_some() {
        Err(CliExit::error("multiple input files not supported"))
    } else {
        Ok(())
    }
}

/// Create an error for an unexpected argument.
#[must_use]
pub fn unexpected_arg(arg: lexopt::Arg, usage: &str) -> CliExit {
    CliExit::error_with_usage(arg.unexpected(), usage)
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

/// Shared spec: `--optimize-inline-threshold <n>`
pub const INLINE_THRESHOLD_SPEC: OptSpec = OptSpec {
    long: Some("optimize-inline-threshold"),
    short: None,
    value: Some("<n>"),
    desc: "Override inlining threshold (max statement count per function)",
};

/// Shared spec: `--optimize-iterations <n>`
pub const OPT_ITERATIONS_SPEC: OptSpec = OptSpec {
    long: Some("optimize-iterations"),
    short: None,
    value: Some("<n>"),
    desc: "Override number of fixed-point optimization iterations",
};

/// Shared spec: `--log-level <level>`
pub const LOG_LEVEL_SPEC: OptSpec = OptSpec {
    long: Some("log-level"),
    short: None,
    value: Some("<level>"),
    desc: "Log level: debug, info, warn, error, off (default: info)",
};

/// Shared spec: `--world <name>`
pub const WORLD_SPEC: OptSpec = OptSpec {
    long: Some("world"),
    short: None,
    value: Some("<name>"),
    desc: "Target world (default: wasi:cli/command)\nUse 'test' to export test functions only",
};

/// Shared spec: `--no-validate`
pub const NO_VALIDATE_SPEC: OptSpec = OptSpec {
    long: Some("no-validate"),
    short: None,
    value: None,
    desc: "Skip Wasm validation (output raw bytes even if invalid)",
};

/// Shared spec: `--allocator <mode>`
pub const ALLOCATOR_SPEC: OptSpec = OptSpec {
    long: Some("allocator"),
    short: None,
    value: Some("<mode>"),
    desc: "Allocator mode: bump (default for CLI), freelist (default for HTTP), debug (no-reuse + 0xFF poison)",
};

/// Shared spec: `--dir <path>` (preopen for WASI filesystem access).
pub const DIR_SPEC: OptSpec = OptSpec {
    long: Some("dir"),
    short: None,
    value: Some("<path>"),
    desc: "Preopen directory for WASI filesystem access\nUse --dir host::guest to specify different guest path",
};

/// Shared spec: `--no-dir`.
pub const NO_DIR_SPEC: OptSpec = OptSpec {
    long: Some("no-dir"),
    short: None,
    value: None,
    desc: "Do not preopen any directories (disables the default)",
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

/// Format help entries from option variants into a string, auto-aligned.
pub fn format_opts_help<T>(all: &[T], spec: impl Fn(&T) -> OptSpec) -> String {
    let labels: Vec<String> = all.iter().map(|t| format_opt_label(&spec(t))).collect();
    let max_w = labels.iter().map(String::len).max().unwrap_or(0);
    let mut buf = String::new();
    for (label, t) in labels.iter().zip(all) {
        let s = spec(t);
        let mut lines = s.desc.lines();
        if let Some(first) = lines.next() {
            writeln!(buf, "  {label:<max_w$}  {first}").unwrap();
            for cont in lines {
                writeln!(buf, "  {:<max_w$}  {cont}", "").unwrap();
            }
        }
    }
    buf
}

/// Print help entries from option variants, auto-aligned.
///
/// Each option's label is generated from its spec and descriptions are
/// aligned to the widest label. Multi-line descriptions (using `\n`) are
/// printed with continuation lines indented to the description column.
pub fn print_opts_help<T>(all: &[T], spec: impl Fn(&T) -> OptSpec) {
    eprint!("{}", format_opts_help(all, &spec));
}

/// Parse log level from string (consolidated from duplicate implementations).
#[must_use]
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

/// Parse `--log-level` argument value from parser.
///
/// # Errors
///
/// Returns an error if the log level string is not recognized.
pub fn parse_log_level_arg(parser: &mut Parser) -> Result<LogLevel, CliExit> {
    let level_str = require_string(parser)?;
    parse_log_level(&level_str).ok_or_else(|| {
        CliExit::error(format!(
            "unknown log level '{level_str}'. Use debug, info, warn, error, or off"
        ))
    })
}

/// Parse `--optimize-inline-threshold <n>` argument value from parser.
///
/// # Errors
///
/// Returns an error if the value is not a valid non-negative integer.
pub fn parse_inline_threshold_arg(opt: &str, parser: &mut Parser) -> Result<usize, CliExit> {
    let s = require_string(parser)?;
    s.parse::<usize>()
        .map_err(|_| CliExit::error(format!("{opt} requires a non-negative integer, got '{s}'")))
}

/// Parse `--optimize-iterations <n>` argument value from parser.
///
/// # Errors
///
/// Returns an error if the value is not a valid non-negative integer.
pub fn parse_opt_iterations_arg(opt: &str, parser: &mut Parser) -> Result<u32, CliExit> {
    let s = require_string(parser)?;
    s.parse::<u32>()
        .map_err(|_| CliExit::error(format!("{opt} requires a non-negative integer, got '{s}'")))
}

/// Parse `--dir <host_path>[::<guest_path>]` into a `(host, guest)` pair.
///
/// When the user omits `::guest_path`, the guest path defaults to the host
/// path — matching the conventional WASI preopen behaviour where the guest
/// sees the same name it would on the host.
///
/// # Errors
///
/// Returns an error if no value is provided.
pub fn parse_dir_arg(parser: &mut Parser) -> Result<(String, String), CliExit> {
    let dir_spec = require_string(parser)?;
    if let Some((h, g)) = dir_spec.split_once("::") {
        Ok((h.to_owned(), g.to_owned()))
    } else {
        Ok((dir_spec.clone(), dir_spec))
    }
}
