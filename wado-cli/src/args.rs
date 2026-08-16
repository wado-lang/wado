//! Helper functions for lexopt argument parsing

use std::ffi::OsString;
use std::fmt::Write;
use std::process;

use lexopt::Parser;
use wado_compiler::LogLevel;
use wado_compiler::param_resolution::ParamPolicyLevel;

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

fn require_value(parser: &mut Parser) -> Result<OsString, CliExit> {
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

/// Default log level for CLI subcommands. The CLI is quiet by default:
/// warnings and errors show, but info-level output — including optimizer
/// `remark:`s (WEP `wep-2026-06-03-optimizer-remarks.md`) — needs an explicit
/// `--log-level info`.
pub const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Warn;

pub const WORLD_SPEC: OptSpec = OptSpec {
    long: Some("world"),
    short: None,
    value: Some("<name>"),
    desc: "Target world (default: wasi:cli/command)\nUse 'test' to export test functions only",
};

/// Shared spec: `--collector <mode>`
pub const COLLECTOR_SPEC: OptSpec = OptSpec {
    long: Some("collector"),
    short: None,
    value: Some("<mode>"),
    desc: "GC collector (default: copying):\ncopying, drc (deferred ref-counting), null (never collects)",
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

/// Shared spec: `-D NAME=value` (alias `--define`) — compile-time parameter
/// override for a `#[param]` global. Repeatable.
pub const DEFINE_SPEC: OptSpec = OptSpec {
    long: Some("define"),
    short: Some('D'),
    value: Some("NAME=value"),
    desc: "Override a #[param] compile-time parameter (repeatable)",
};

pub const PARAM_UNKNOWN_SPEC: OptSpec = OptSpec {
    long: Some("param-unknown"),
    short: None,
    value: Some("<level>"),
    desc: "Policy for a -D matching no #[param]: error, warn, ignore (default: error)",
};

pub const PARAM_INVALID_SPEC: OptSpec = OptSpec {
    long: Some("param-invalid"),
    short: None,
    value: Some("<level>"),
    desc: "Policy for an unconvertible override value: error, warn, ignore (default: error)",
};

pub const PARAM_MISSING_SPEC: OptSpec = OptSpec {
    long: Some("param-missing"),
    short: None,
    value: Some("<level>"),
    desc: "Policy for a parameter with no override: error, warn, ignore (default: ignore)",
};

/// Parse `-D NAME=value` / `--define NAME=value`, splitting on the first `=`.
/// A bare `NAME` (no `=`) is an error.
fn parse_define_arg(parser: &mut Parser) -> Result<(String, String), CliExit> {
    let raw = require_string(parser)?;
    match raw.split_once('=') {
        Some((name, value)) if !name.is_empty() => Ok((name.to_owned(), value.to_owned())),
        _ => Err(CliExit::error(format!(
            "invalid -D argument '{raw}'; expected NAME=value"
        ))),
    }
}

/// Parse a `--param-*` policy level (`error` / `warn` / `ignore`).
fn parse_param_policy_arg(opt: &str, parser: &mut Parser) -> Result<ParamPolicyLevel, CliExit> {
    let s = require_string(parser)?;
    ParamPolicyLevel::parse(&s)
        .ok_or_else(|| CliExit::error(format!("{opt} requires error, warn, or ignore, got '{s}'")))
}

/// The compile-time-parameter options shared by every subcommand that compiles
/// (`compile` / `run` / `serve` / `test` / `dump`). Embed [`ParamArgs`] in the
/// parse loop and add `ParamOpt::ALL` to the help output.
#[derive(Clone, Copy)]
pub enum ParamOpt {
    Define,
    Unknown,
    Invalid,
    Missing,
}

impl ParamOpt {
    pub const ALL: &[Self] = &[Self::Define, Self::Unknown, Self::Invalid, Self::Missing];

    pub const fn spec(self) -> OptSpec {
        match self {
            Self::Define => DEFINE_SPEC,
            Self::Unknown => PARAM_UNKNOWN_SPEC,
            Self::Invalid => PARAM_INVALID_SPEC,
            Self::Missing => PARAM_MISSING_SPEC,
        }
    }
}

/// Accumulated `-D` overrides and `--param-*` policy from the command line.
#[derive(Clone, Debug, Default)]
pub struct ParamArgs {
    pub overrides: wado_compiler::hashmap::IndexMap<String, String>,
    pub policy: wado_compiler::param_resolution::ParamPolicy,
}

impl ParamArgs {
    /// Apply a matched [`ParamOpt`], consuming its value from the parser.
    pub fn apply(&mut self, opt: ParamOpt, parser: &mut Parser) -> Result<(), CliExit> {
        match opt {
            ParamOpt::Define => {
                let (name, value) = parse_define_arg(parser)?;
                self.overrides.insert(name, value);
            }
            ParamOpt::Unknown => {
                self.policy.unknown = parse_param_policy_arg("--param-unknown", parser)?;
            }
            ParamOpt::Invalid => {
                self.policy.invalid = parse_param_policy_arg("--param-invalid", parser)?;
            }
            ParamOpt::Missing => {
                self.policy.missing = parse_param_policy_arg("--param-missing", parser)?;
            }
        }
        Ok(())
    }
}

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

/// One help block, accumulated from however many option enums a subcommand
/// draws on. Collecting them before rendering is what keeps the description
/// column aligned: the width is computed once over every option, not per group.
#[derive(Default)]
pub struct OptsHelp {
    specs: Vec<OptSpec>,
}

impl OptsHelp {
    /// Append a group, in the order it should appear.
    #[must_use]
    pub fn add<T>(mut self, all: &[T], spec: impl Fn(&T) -> OptSpec) -> Self {
        self.specs.extend(all.iter().map(spec));
        self
    }

    #[must_use]
    pub fn render(&self) -> String {
        let labels: Vec<String> = self.specs.iter().map(format_opt_label).collect();
        let max_w = labels.iter().map(String::len).max().unwrap_or(0);
        let mut buf = String::new();
        for (label, s) in labels.iter().zip(&self.specs) {
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
}

pub fn format_opts_help<T>(all: &[T], spec: impl Fn(&T) -> OptSpec) -> String {
    OptsHelp::default().add(all, spec).render()
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
