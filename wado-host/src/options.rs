//! The command-line options `wado` and its `run-*` subcommands share: how an
//! option is declared, how it reaches a parser and a help text, and the knobs
//! themselves. Each knob is declared once here. `wado-cli` applies a compile
//! knob to the compiler's own types, and a runner hands it to `wado build`.

use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::path::PathBuf;
use std::process;
use std::time::Duration;

use lexopt::Parser;
use wasmtime::Collector;

use crate::engine::{DEFAULT_COLLECTOR, DEFAULT_GC_HEAP_INITIAL_SIZE};
use crate::process_host::{ProcessGrant, Trace};

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
    pub fn error(msg: impl fmt::Display) -> Self {
        Self {
            message: format!("Error: {msg}\n"),
            exit_code: 1,
        }
    }

    pub fn error_with_usage(msg: impl fmt::Display, usage: &str) -> Self {
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

impl From<anyhow::Error> for CliExit {
    fn from(error: anyhow::Error) -> Self {
        Self::error(format!("{error:#}"))
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

impl OptSpec {
    /// How this option is spelled on a command line.
    #[must_use]
    pub fn flag(&self) -> String {
        match (self.long, self.short) {
            (Some(long), _) => format!("--{long}"),
            (None, Some(short)) => format!("-{short}"),
            (None, None) => unreachable!("an option has a long or a short name"),
        }
    }
}

pub const HELP_SPEC: OptSpec = OptSpec {
    long: Some("help"),
    short: None,
    value: None,
    desc: "Show this help message",
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

/// The compile-time-parameter options shared by every subcommand that compiles
/// (`compile` / `run` / `serve` / `test` / `dump`, and the runners).
#[derive(Clone, Copy)]
pub enum ParamOpt {
    Define,
    Unknown,
    Invalid,
    Missing,
}

impl ParamOpt {
    pub const ALL: &[Self] = &[Self::Define, Self::Unknown, Self::Invalid, Self::Missing];

    #[must_use]
    pub const fn spec(self) -> OptSpec {
        match self {
            Self::Define => DEFINE_SPEC,
            Self::Unknown => PARAM_UNKNOWN_SPEC,
            Self::Invalid => PARAM_INVALID_SPEC,
            Self::Missing => PARAM_MISSING_SPEC,
        }
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

/// `--dir` / `--no-dir` grants for `run`, the runners and `test`. When neither
/// flag appears, `run` and the runners preopen the current directory and `test`
/// each file's package root; either flag replaces that default outright.
/// `serve` has none — a service starts with no preopens.
#[derive(Default)]
pub struct DirGrants {
    dirs: Vec<(String, String)>,
    explicit: bool,
    suppressed: bool,
}

impl DirGrants {
    pub fn add(&mut self, parser: &mut Parser) -> Result<(), CliExit> {
        self.dirs.push(parse_dir_arg(parser)?);
        self.explicit = true;
        Ok(())
    }

    pub fn suppress_default(&mut self) {
        self.suppressed = true;
    }

    /// The grants `--dir` and `--no-dir` spell, or `None` where neither
    /// appears and the subcommand's own default applies.
    #[must_use]
    pub fn given(self) -> Option<Vec<(String, String)>> {
        (self.explicit || self.suppressed).then_some(self.dirs)
    }

    /// The grants, the current directory where no flag spells any.
    #[must_use]
    pub fn finish(self) -> Vec<(String, String)> {
        self.given()
            .unwrap_or_else(|| vec![(".".to_owned(), ".".to_owned())])
    }
}

/// An optimization level `wado` accepts, so no other spelling reaches a caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OptLevel {
    O0,
    /// All passes on a small budget, and contract checks on. Iterations: 2,
    /// inline threshold: 4.
    O1,
    /// Production: contract checks off. Iterations: 15, inline threshold: 16.
    #[default]
    O2,
    /// Aggressive. Iterations: 20, inline threshold: 26.
    O3,
    /// `O2` plus name-section stripping.
    Os,
}

impl OptLevel {
    #[must_use]
    pub fn parse(level: &str) -> Option<Self> {
        match level {
            "0" => Some(Self::O0),
            "1" => Some(Self::O1),
            "2" => Some(Self::O2),
            "3" => Some(Self::O3),
            "s" => Some(Self::Os),
            _ => None,
        }
    }

    /// The `-O` flag that passes this level to `wado build`.
    #[must_use]
    pub fn flag(self) -> &'static str {
        match self {
            Self::O0 => "-O0",
            Self::O1 => "-O1",
            Self::O2 => "-O2",
            Self::O3 => "-O3",
            Self::Os => "-Os",
        }
    }

    /// wasmtime exposes only `None`/`Speed`/`SpeedAndSize`, so `O1`/`O2`/
    /// `O3` collapse to `Speed` — mirroring the `wasmtime` CLI's own `-O`
    /// mapping.
    #[must_use]
    pub const fn to_wasmtime(self) -> wasmtime::OptLevel {
        match self {
            Self::O0 => wasmtime::OptLevel::None,
            Self::O1 | Self::O2 | Self::O3 => wasmtime::OptLevel::Speed,
            Self::Os => wasmtime::OptLevel::SpeedAndSize,
        }
    }
}

/// Parse `-O<n>`. The level is always attached and always explicit: a bare
/// `-O` is an error rather than a silent default, and the level is never taken
/// from the next argument, so `-O` cannot swallow the input file.
pub fn parse_opt_level_arg(parser: &mut Parser) -> Result<OptLevel, CliExit> {
    let Some(val) = parser.optional_value() else {
        return Err(CliExit::error(
            "-O requires a level. Use -O0, -O1, -O2, -O3, or -Os",
        ));
    };
    let level = val.to_string_lossy();
    OptLevel::parse(&level).ok_or_else(|| {
        CliExit::error(format!(
            "unknown optimization level '-O{level}'. Use -O0, -O1, -O2, -O3, or -Os"
        ))
    })
}

/// The levels and spellings `wado --log-level` takes, in rising verbosity.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    #[must_use]
    pub fn parse(level: &str) -> Option<Self> {
        match level.to_lowercase().as_str() {
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            "off" | "none" => Some(Self::Off),
            _ => None,
        }
    }

    /// The value that passes this level to `wado build --log-level`.
    #[must_use]
    pub fn flag_value(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
}

/// Parse a `--log-level` value.
pub fn parse_log_level_arg(parser: &mut Parser) -> Result<LogLevel, CliExit> {
    let level = require_string(parser)?;
    LogLevel::parse(&level).ok_or_else(|| {
        CliExit::error(format!(
            "unknown log level '{level}'. Use debug, info, warn, error, or off"
        ))
    })
}

/// `--profile guest[,path[,interval_ms]]`. An empty path is the default one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GuestProfile {
    pub path: PathBuf,
    pub interval: Duration,
}

impl GuestProfile {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split(',');
        if parts.next() != Some("guest") {
            return None;
        }
        let path = match parts.next() {
            None | Some("") => PathBuf::from("profile.json"),
            Some(path) => PathBuf::from(path),
        };
        let interval_ms = match parts.next() {
            Some(ms) => ms.parse().ok()?,
            None => 10,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            path,
            interval: Duration::from_millis(interval_ms),
        })
    }
}

/// Profiling mode for wasmtime execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileMode {
    /// No profiling.
    None,
    /// Linux perf jitdump profiling. Use with `perf record -k mono`.
    JitDump,
    /// Linux perf map profiling. Use with `perf record -k mono`.
    PerfMap,
    /// Cross-platform guest profiling. Produces Firefox Profiler JSON.
    Guest(GuestProfile),
}

/// Parse a `--profile` argument value. Shared by `run`, `serve` and the runners.
///
/// # Errors
///
/// Returns an error if the mode name or the interval is unparseable.
pub fn parse_profile(s: &str) -> Result<ProfileMode, CliExit> {
    match s {
        "jitdump" => Ok(ProfileMode::JitDump),
        "perfmap" => Ok(ProfileMode::PerfMap),
        _ => GuestProfile::parse(s).map(ProfileMode::Guest).ok_or_else(|| {
            CliExit::error(format!(
                "unknown profile mode '{s}'. Use guest[,path[,interval_ms]], jitdump, or perfmap"
            ))
        }),
    }
}

/// Parse a `--collector` argument value into a wasmtime [`Collector`].
///
/// # Errors
///
/// Returns an error message if the collector name is unrecognized.
pub fn parse_collector(s: &str) -> Result<Collector, String> {
    match s {
        "copying" => Ok(Collector::Copying),
        "drc" => Ok(Collector::DeferredReferenceCounting),
        "null" => Ok(Collector::Null),
        other => Err(format!(
            "unknown collector '{other}'. Use copying, drc, or null"
        )),
    }
}

/// Parse a `--gc-heap-initial` value: a byte count, or one suffixed `k`, `m`
/// or `g` for binary multiples.
///
/// # Errors
///
/// Returns an error message if the value is not a size.
pub fn parse_gc_heap_size(s: &str) -> Result<u64, String> {
    let lower = s.trim().to_ascii_lowercase();
    let suffixed = |shift: u32| (&lower[..lower.len() - 1], 1u64 << shift);
    let (digits, multiple) = match lower.as_bytes().last() {
        Some(b'k') => suffixed(10),
        Some(b'm') => suffixed(20),
        Some(b'g') => suffixed(30),
        _ => (lower.as_str(), 1),
    };
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid GC heap size '{s}'. Use bytes, or a k / m / g suffix"))?;
    value
        .checked_mul(multiple)
        .ok_or_else(|| format!("GC heap size '{s}' does not fit in 64 bits"))
}

/// A knob shared by the compiling subcommands. Each one declares the subset it
/// accepts as a `&[CompileKnobOpt]`, used for both matching and help rendering.
#[derive(Clone, Copy)]
pub enum CompileKnobOpt {
    OptLevel,
    InlineThreshold,
    InlineHotCallsiteThreshold,
    InlineColdCallsiteThreshold,
    InlineGrowth,
    OptIterations,
    Specializations,
    OptThreads,
    PredictableBranchThreshold,
    ProfileSummaryCutoffHot,
    ProfileSummaryCutoffCold,
    LogLevel,
    Allocator,
    Feature,
    NoCache,
    NoValidate,
    NoLint,
    Lint,
    LintGenerated,
}

impl CompileKnobOpt {
    #[must_use]
    pub const fn spec(self) -> OptSpec {
        match self {
            Self::OptLevel => OptSpec {
                long: None,
                short: Some('O'),
                value: Some("<n>"),
                desc: "Optimization level: -O0, -O1, -O2, -O3, -Os\n(default: -O2; -O1 under run, serve and test)",
            },
            Self::InlineThreshold => OptSpec {
                long: Some("optimize-inline-threshold"),
                short: None,
                value: Some("<n>"),
                desc: "Override inlining threshold (max statement count per function)",
            },
            Self::InlineHotCallsiteThreshold => OptSpec {
                long: Some("optimize-inline-hot-callsite-threshold"),
                short: None,
                value: Some("<n>"),
                desc: "Override the inlining threshold at a call site the profile\nmeasured hot (default: 4x the inlining threshold)",
            },
            Self::InlineColdCallsiteThreshold => OptSpec {
                long: Some("optimize-inline-cold-callsite-threshold"),
                short: None,
                value: Some("<n>"),
                desc: "Override the inlining threshold at a call site the profile\nmeasured cold (default: a quarter of the inlining threshold)",
            },
            Self::InlineGrowth => OptSpec {
                long: Some("optimize-inline-growth"),
                short: None,
                value: Some("<pct>"),
                desc: "Override how far inlining may grow the program, in percent",
            },
            Self::OptIterations => OptSpec {
                long: Some("optimize-iterations"),
                short: None,
                value: Some("<n>"),
                desc: "Override number of fixed-point optimization iterations",
            },
            Self::Specializations => OptSpec {
                long: Some("optimize-specializations"),
                short: None,
                value: Some("<n>"),
                desc: "Override how many clones specializing a callee on constant\narguments may mint; reaching it is a remark (--log-level info)",
            },
            Self::OptThreads => OptSpec {
                long: Some("optimize-threads"),
                short: None,
                value: Some("<n>"),
                desc: "Threads the optimizer runs on; the output does not depend on it\n(default: the available cores, shared among the files compiled\nat once: 1 each under `wado test`)",
            },
            Self::PredictableBranchThreshold => OptSpec {
                long: Some("optimize-predictable-branch-threshold"),
                short: None,
                value: Some("<pct>"),
                desc: "The share of a branch's runs, in percent, one side must take for\nthe profile to call the other side unlikely (default: 99)",
            },
            Self::ProfileSummaryCutoffHot => OptSpec {
                long: Some("optimize-profile-summary-cutoff-hot"),
                short: None,
                value: Some("<ppm>"),
                desc: "The share of all runs, in parts per million, whose smallest count\nis the profile's hot line (default: 990000)",
            },
            Self::ProfileSummaryCutoffCold => OptSpec {
                long: Some("optimize-profile-summary-cutoff-cold"),
                short: None,
                value: Some("<ppm>"),
                desc: "The share of all runs, in parts per million, whose smallest count\nis the profile's cold line (default: 999999)",
            },
            Self::LogLevel => OptSpec {
                long: Some("log-level"),
                short: None,
                value: Some("<level>"),
                desc: "Log level: debug, info, warn, error, off (default: warn)",
            },
            Self::Allocator => OptSpec {
                long: Some("allocator"),
                short: None,
                value: Some("<mode>"),
                desc: "Allocator mode (default depends on target world):\nfreelist, debug (test; no-reuse + 0xFF poison)",
            },
            // Repeatable; prefix a flag with `no-` to disable it. The compiler
            // validates the names.
            Self::Feature => OptSpec {
                long: None,
                short: Some('f'),
                value: Some("<flag>"),
                desc: "Toggle a codegen feature flag (repeatable; prefix no- to disable):\nbranch-hinting   emit metadata.code.branch_hint entries (default: on)\ncold-paths       keep the cold_path() markers, the compiler's included (default: on)\nbare-asserts     assertion failures trap without a message (default: on at -Os)\nwide-arithmetic  native i64.mul_wide/add128/sub128 (default: on)\ncontract-checks  check the contract of a call marked unchecked (default: on in the test world, at -O0 and -O1)\npgo              apply the entry's profile, pgo/default.pgo or pgo/<stem>.pgo (default: on; off under wado test --pgo)",
            },
            Self::NoCache => OptSpec {
                long: Some("no-cache"),
                short: None,
                value: None,
                desc: "Bypass all build caches: re-run Kiln generators on every invocation,\nrecompile generator wasm components from source, re-evaluate\nevery `core:eval` program, and rerun every test file that passed before.\nThe cache refreshes automatically, so this is normally unnecessary —\nit exists for benchmarking and cache-bug debugging.",
            },
            Self::NoValidate => OptSpec {
                long: Some("no-validate"),
                short: None,
                value: None,
                desc: "Skip Wasm validation (output raw bytes even if invalid)",
            },
            // The three set one value, so the last one given wins.
            Self::NoLint => OptSpec {
                long: Some("no-lint"),
                short: None,
                value: None,
                desc: "Report no lint warnings, and skip the passes only lints need",
            },
            Self::Lint => OptSpec {
                long: Some("lint"),
                short: None,
                value: None,
                desc: "Report lint warnings in the package's own modules,\n`#![generated]` ones aside (the default; undoes --no-lint)",
            },
            Self::LintGenerated => OptSpec {
                long: Some("lint-generated"),
                short: None,
                value: None,
                desc: "Report lint warnings in `#![generated]` modules too",
            },
        }
    }
}

/// A knob shared by the subcommands that host a guest (`run` / `serve` /
/// `test`, and the runners), used for both matching and help rendering.
#[derive(Clone, Copy)]
pub enum RuntimeKnobOpt {
    Collector,
    GcHeapInitial,
    ReportFuel,
    DenyProcess,
    ProcessRead,
    ProcessTrace,
}

impl RuntimeKnobOpt {
    pub const ALL: &[Self] = &[
        Self::Collector,
        Self::GcHeapInitial,
        Self::ReportFuel,
        Self::DenyProcess,
        Self::ProcessRead,
        Self::ProcessTrace,
    ];

    #[must_use]
    pub const fn spec(self) -> OptSpec {
        match self {
            Self::Collector => OptSpec {
                long: Some("collector"),
                short: None,
                value: Some("<mode>"),
                desc: "GC collector (default: copying):\ncopying, drc (deferred ref-counting), null (never collects)",
            },
            Self::GcHeapInitial => OptSpec {
                long: Some("gc-heap-initial"),
                short: None,
                value: Some("<size>"),
                desc: "GC heap a guest starts with (default: 256m)\nBytes, or a k / m / g suffix. The copying collector splits it\ninto two semi-spaces, so a program allocates through half",
            },
            Self::ReportFuel => OptSpec {
                long: Some("report-fuel"),
                short: None,
                value: None,
                desc: "Meter the guest in wasmtime fuel (about one unit per Wasm\ninstruction) and report what it spent: per program (run), per\ntest (test), per request (serve, one request at a time).\nA test's wasi:random/insecure and insecure-seed answer the same on\nevery run, and its core:fuel regions are reported too.\nPure computation spends the same fuel on every machine; a guest\nwaiting on I/O can loop more or less, as the host answers",
            },
            Self::DenyProcess => OptSpec {
                long: Some("deny-process"),
                short: None,
                value: None,
                desc: "Turn off core:process: the guest may not spawn a host program,\neven one confined to its own grants",
            },
            Self::ProcessRead => OptSpec {
                long: Some("process-read"),
                short: None,
                value: Some("<path>"),
                desc: "Let a child the guest spawns read and execute <path>, beyond\nthe guest's grants and the system baseline (a toolchain under\nHOME, say). Repeatable; the guest itself gains nothing.\n`[process] read` in ~/.config/wado/config.toml adds the same",
            },
            Self::ProcessTrace => OptSpec {
                long: Some("process-trace"),
                short: None,
                value: None,
                desc: "Watch each file a child the guest spawns opens, creates or\nremoves, and name each path the sandbox refused it, with the\ngrant that would allow it. Every such call waits on wado, so\nchildren run slower. Linux only",
            },
        }
    }
}

/// Every knob a guest-hosting subcommand parses and forwards to the wasmtime
/// engine.
#[derive(Clone, Debug)]
pub struct RuntimeKnobs {
    pub collector: Collector,
    pub gc_heap_initial_size: u64,
    /// `--report-fuel`: meter the guest in fuel and report what it spent.
    pub report_fuel: bool,
    /// `--deny-process` / `--process-read` / `--process-trace`: whether a
    /// guest may spawn, what its children may read beyond its grants, and
    /// whether the host watches their file calls.
    pub process: ProcessGrant,
}

impl Default for RuntimeKnobs {
    fn default() -> Self {
        Self {
            collector: DEFAULT_COLLECTOR,
            gc_heap_initial_size: DEFAULT_GC_HEAP_INITIAL_SIZE,
            report_fuel: false,
            process: ProcessGrant::Offered {
                read: Vec::new(),
                trace: Trace::Off,
            },
        }
    }
}

impl RuntimeKnobs {
    /// Apply a matched [`RuntimeKnobOpt`], consuming its value from the parser.
    pub fn apply(&mut self, opt: RuntimeKnobOpt, parser: &mut Parser) -> Result<(), CliExit> {
        match opt {
            RuntimeKnobOpt::Collector => {
                self.collector =
                    parse_collector(&require_string(parser)?).map_err(CliExit::error)?;
            }
            RuntimeKnobOpt::GcHeapInitial => {
                self.gc_heap_initial_size =
                    parse_gc_heap_size(&require_string(parser)?).map_err(CliExit::error)?;
            }
            RuntimeKnobOpt::ReportFuel => self.report_fuel = true,
            RuntimeKnobOpt::DenyProcess => match &self.process {
                ProcessGrant::Offered {
                    read,
                    trace: Trace::Off,
                } if read.is_empty() => {
                    self.process = ProcessGrant::Denied;
                }
                ProcessGrant::Offered { .. } => return Err(child_knob_with_deny()),
                ProcessGrant::Denied => {}
            },
            RuntimeKnobOpt::ProcessRead => {
                let path = require_string(parser)?;
                let ProcessGrant::Offered { read, .. } = &mut self.process else {
                    return Err(child_knob_with_deny());
                };
                // Resolved here, so a symlink the child would follow names the
                // tree the rule opens, and a typo fails before the guest runs.
                read.push(
                    std::fs::canonicalize(&path)
                        .map_err(|e| CliExit::error(format!("--process-read {path}: {e}")))?,
                );
            }
            RuntimeKnobOpt::ProcessTrace => {
                let ProcessGrant::Offered { trace, .. } = &mut self.process else {
                    return Err(child_knob_with_deny());
                };
                *trace = Trace::Files;
            }
        }
        Ok(())
    }
}

fn child_knob_with_deny() -> CliExit {
    CliExit::error(
        "--process-read and --process-trace act on a spawned child, which --deny-process forbids",
    )
}

/// A knob shared by the subcommands that compile, for how the `wado` process
/// itself runs, used for both matching and help rendering.
#[derive(Clone, Copy)]
pub enum HostKnobOpt {
    RetainMemory,
    NoRetainMemory,
}

impl HostKnobOpt {
    pub const ALL: &[Self] = &[Self::RetainMemory, Self::NoRetainMemory];

    #[must_use]
    pub const fn spec(self) -> OptSpec {
        match self {
            // The two set one value, so the last one given wins.
            Self::RetainMemory => OptSpec {
                long: Some("retain-memory"),
                short: None,
                value: None,
                desc: "Keep the memory wado frees for reuse instead of returning it to\nthe OS, which saves a compile rebuilding it (default: on, but\noff under serve, whose resident set would stay at its peak).\nA build without the mimalloc feature ignores it",
            },
            Self::NoRetainMemory => OptSpec {
                long: Some("no-retain-memory"),
                short: None,
                value: None,
                desc: "Return the memory wado frees to the OS (undoes --retain-memory)",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_runtime(opts: &[RuntimeKnobOpt]) -> Result<RuntimeKnobs, String> {
        let mut knobs = RuntimeKnobs::default();
        for &opt in opts {
            knobs
                .apply(opt, &mut Parser::from_args::<[&str; 0]>([]))
                .map_err(|e| e.message)?;
        }
        Ok(knobs)
    }

    #[test]
    fn deny_process_refuses_a_trace_in_either_order() {
        use RuntimeKnobOpt::{DenyProcess, ProcessTrace};
        for opts in [[DenyProcess, ProcessTrace], [ProcessTrace, DenyProcess]] {
            let err = apply_runtime(&opts).unwrap_err();
            assert!(err.contains("--deny-process forbids"), "{err}");
        }
    }

    /// Drive `parse_opt_level_arg` the way the parse loop does: the parser sits
    /// just past the matched `-O`.
    fn parse_opt_level(args: &[&str]) -> Result<OptLevel, String> {
        let mut parser = Parser::from_args(args);
        parser.next().unwrap().unwrap();
        parse_opt_level_arg(&mut parser).map_err(|e| e.message)
    }

    #[test]
    fn every_level_is_spelled_out() {
        for (arg, expected) in [
            ("-O0", OptLevel::O0),
            ("-O1", OptLevel::O1),
            ("-O2", OptLevel::O2),
            ("-O3", OptLevel::O3),
            ("-Os", OptLevel::Os),
        ] {
            assert_eq!(parse_opt_level(&[arg]), Ok(expected), "failed for {arg}");
        }
    }

    #[test]
    fn a_bare_dash_o_is_rejected() {
        let err = parse_opt_level(&["-O", "input.wado"]).unwrap_err();
        assert!(err.contains("requires a level"), "{err}");
    }

    #[test]
    fn a_bare_dash_o_does_not_swallow_the_input_file() {
        let mut parser = Parser::from_args(["-O", "input.wado"]);
        parser.next().unwrap().unwrap();
        assert!(parse_opt_level_arg(&mut parser).is_err());
        assert_eq!(parser.value().unwrap(), "input.wado");
    }

    #[test]
    fn an_unknown_level_is_rejected() {
        let err = parse_opt_level(&["-O9"]).unwrap_err();
        assert!(err.contains("unknown optimization level '-O9'"), "{err}");
    }

    #[test]
    fn an_option_is_spelled_by_its_long_name_where_it_has_one() {
        assert_eq!(CompileKnobOpt::Allocator.spec().flag(), "--allocator");
        assert_eq!(CompileKnobOpt::Feature.spec().flag(), "-f");
        assert_eq!(DEFINE_SPEC.flag(), "--define");
    }
}
