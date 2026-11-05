//! Helper functions for lexopt argument parsing

use lexopt::Parser;
use wado_compiler::LogLevel;
use wado_compiler::param_resolution::{ParamInputs, ParamPolicyLevel};
use wado_host::options::LogLevel as HostLogLevel;
pub use wado_host::options::{
    CliExit, DIR_SPEC, DirGrants, HELP_SPEC, NO_DIR_SPEC, OptSpec, OptsHelp, ParamOpt,
    format_opts_help, match_opt, next_arg, parse_dir_arg, require_string, unexpected_arg,
};

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

/// Apply a matched [`ParamOpt`] to `params`, consuming its value from the parser.
pub fn apply_param_opt(
    params: &mut ParamInputs,
    opt: ParamOpt,
    parser: &mut Parser,
) -> Result<(), CliExit> {
    match opt {
        ParamOpt::Define => {
            let (name, value) = parse_define_arg(parser)?;
            params.overrides.insert(name, value);
        }
        ParamOpt::Unknown => {
            params.policy.unknown = parse_param_policy_arg("--param-unknown", parser)?;
        }
        ParamOpt::Invalid => {
            params.policy.invalid = parse_param_policy_arg("--param-invalid", parser)?;
        }
        ParamOpt::Missing => {
            params.policy.missing = parse_param_policy_arg("--param-missing", parser)?;
        }
    }
    Ok(())
}

const fn compiler_log_level(level: HostLogLevel) -> LogLevel {
    match level {
        HostLogLevel::Off => LogLevel::Off,
        HostLogLevel::Error => LogLevel::Error,
        HostLogLevel::Warn => LogLevel::Warn,
        HostLogLevel::Info => LogLevel::Info,
        HostLogLevel::Debug => LogLevel::Debug,
    }
}

pub fn parse_log_level_arg(parser: &mut Parser) -> Result<LogLevel, CliExit> {
    wado_host::options::parse_log_level_arg(parser).map(compiler_log_level)
}

pub fn parse_inline_threshold_arg(opt: &str, parser: &mut Parser) -> Result<usize, CliExit> {
    let s = require_string(parser)?;
    s.parse::<usize>()
        .ok()
        .filter(|&n| n <= wado_compiler::MAX_INLINE_THRESHOLD)
        .ok_or_else(|| {
            CliExit::error(format!(
                "{opt} requires an integer from 0 to {}, got '{s}'",
                wado_compiler::MAX_INLINE_THRESHOLD
            ))
        })
}

pub fn parse_u32_arg(opt: &str, parser: &mut Parser) -> Result<u32, CliExit> {
    let s = require_string(parser)?;
    s.parse::<u32>()
        .map_err(|_| CliExit::error(format!("{opt} requires a non-negative integer, got '{s}'")))
}
