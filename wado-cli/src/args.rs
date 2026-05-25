//! Helper functions for lexopt argument parsing

use std::ffi::OsString;
use std::fmt::Write;
use std::process;

use lexopt::Parser;
use wado_compiler::LogLevel;

/// Outcome of a subcommand: a message to print and an exit code.
///
/// Returned by both `parse_args()` and `run()` so the only `process::exit()`
/// call lives in `main()`. `silent_failure` covers the case where the
/// subcommand has already printed its own diagnostics and only needs to
/// signal a non-zero exit.
#[derive(Debug)]
pub struct CliExit {
    pub message: String,
    pub exit_code: i32,
}

impl CliExit {
    pub fn error(msg: impl std::fmt::Display) -> Self {
        Self {
            message: format!("Error: {msg}\n"),
            exit_code: 1,
        }
    }

    pub fn error_with_usage(msg: impl std::fmt::Display, usage: &str) -> Self {
        Self {
            message: format!("Error: {msg}\n{usage}"),
            exit_code: 1,
        }
    }

    #[must_use]
    pub fn help(usage: String) -> Self {
        Self {
            message: usage,
            exit_code: 0,
        }
    }

    #[must_use]
    pub const fn silent_failure(exit_code: i32) -> Self {
        Self {
            message: String::new(),
            exit_code,
        }
    }

    pub fn exit(&self) -> ! {
        eprint!("{}", self.message);
        process::exit(self.exit_code);
    }
}

pub fn next_arg(parser: &mut Parser) -> Result<Option<lexopt::Arg<'_>>, CliExit> {
    parser.next().map_err(CliExit::error)
}

pub fn require_value(parser: &mut Parser) -> Result<OsString, CliExit> {
    parser.value().map_err(CliExit::error)
}

pub fn require_string(parser: &mut Parser) -> Result<String, CliExit> {
    Ok(require_value(parser)?.to_string_lossy().into_owned())
}

pub fn require_input(input: Option<String>, usage: &str) -> Result<String, CliExit> {
    input.ok_or_else(|| CliExit::error_with_usage("no input file specified", usage))
}

pub fn require_inputs(inputs: Vec<String>, usage: &str) -> Result<Vec<String>, CliExit> {
    if inputs.is_empty() {
        Err(CliExit::error_with_usage("no input file specified", usage))
    } else {
        Ok(inputs)
    }
}

pub fn reject_multiple_inputs(input: &Option<String>) -> Result<(), CliExit> {
    if input.is_some() {
        Err(CliExit::error("multiple input files not supported"))
    } else {
        Ok(())
    }
}

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
    pub long: Option<&'static str>,
    pub short: Option<char>,
    /// Value placeholder for help text. `None` for flags.
    pub value: Option<&'static str>,
    /// Help description. `\n` introduces continuation lines.
    pub desc: &'static str,
}

pub const HELP_SPEC: OptSpec = OptSpec {
    long: Some("help"),
    short: None,
    value: None,
    desc: "Show this help message",
};

pub const OPT_LEVEL_SPEC: OptSpec = OptSpec {
    long: None,
    short: Some('O'),
    value: Some("<n>"),
    desc: "Optimization level: -O0, -O1, -O2, -O3, -Os",
};

pub const INLINE_THRESHOLD_SPEC: OptSpec = OptSpec {
    long: Some("optimize-inline-threshold"),
    short: None,
    value: Some("<n>"),
    desc: "Override inlining threshold (max statement count per function)",
};

pub const OPT_ITERATIONS_SPEC: OptSpec = OptSpec {
    long: Some("optimize-iterations"),
    short: None,
    value: Some("<n>"),
    desc: "Override number of fixed-point optimization iterations",
};

pub const LOG_LEVEL_SPEC: OptSpec = OptSpec {
    long: Some("log-level"),
    short: None,
    value: Some("<level>"),
    desc: "Log level: debug, info, warn, error, off (default: info)",
};

pub const WORLD_SPEC: OptSpec = OptSpec {
    long: Some("world"),
    short: None,
    value: Some("<name>"),
    desc: "Target world (default: wasi:cli/command)\nUse 'test' to export test functions only",
};

pub const NO_VALIDATE_SPEC: OptSpec = OptSpec {
    long: Some("no-validate"),
    short: None,
    value: None,
    desc: "Skip Wasm validation (output raw bytes even if invalid)",
};

pub const ALLOCATOR_SPEC: OptSpec = OptSpec {
    long: Some("allocator"),
    short: None,
    value: Some("<mode>"),
    desc: "Allocator mode (default depends on target world):\nbump (CLI), freelist (HTTP), debug (test; no-reuse + 0xFF poison)",
};

pub const DIR_SPEC: OptSpec = OptSpec {
    long: Some("dir"),
    short: None,
    value: Some("<path>"),
    desc: "Preopen directory for WASI filesystem access\nUse --dir host::guest to specify different guest path",
};

pub const NO_DIR_SPEC: OptSpec = OptSpec {
    long: Some("no-dir"),
    short: None,
    value: None,
    desc: "Do not preopen any directories (disables the default)",
};

pub const NO_CACHE_SPEC: OptSpec = OptSpec {
    long: Some("no-cache"),
    short: None,
    value: None,
    desc: "Bypass all build caches: re-run Kiln generators on every invocation\nand recompile generator wasm components from source.\nIntended for profiling and cache-bug debugging.",
};

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

pub fn print_opts_help<T>(all: &[T], spec: impl Fn(&T) -> OptSpec) {
    eprint!("{}", format_opts_help(all, &spec));
}

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

pub fn parse_log_level_arg(parser: &mut Parser) -> Result<LogLevel, CliExit> {
    let level_str = require_string(parser)?;
    parse_log_level(&level_str).ok_or_else(|| {
        CliExit::error(format!(
            "unknown log level '{level_str}'. Use debug, info, warn, error, or off"
        ))
    })
}

pub fn parse_inline_threshold_arg(opt: &str, parser: &mut Parser) -> Result<usize, CliExit> {
    let s = require_string(parser)?;
    s.parse::<usize>()
        .map_err(|_| CliExit::error(format!("{opt} requires a non-negative integer, got '{s}'")))
}

pub fn parse_opt_iterations_arg(opt: &str, parser: &mut Parser) -> Result<u32, CliExit> {
    let s = require_string(parser)?;
    s.parse::<u32>()
        .map_err(|_| CliExit::error(format!("{opt} requires a non-negative integer, got '{s}'")))
}

/// Parse `--dir <host_path>[::<guest_path>]`. When `::guest_path` is
/// omitted, guest = host (the conventional WASI preopen behaviour).
pub fn parse_dir_arg(parser: &mut Parser) -> Result<(String, String), CliExit> {
    let dir_spec = require_string(parser)?;
    if let Some((h, g)) = dir_spec.split_once("::") {
        Ok((h.to_owned(), g.to_owned()))
    } else {
        Ok((dir_spec.clone(), dir_spec))
    }
}
