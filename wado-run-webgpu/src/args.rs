//! The command line, which mirrors the subset of `wado run` that applies here.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use lexopt::Arg::{Long, Short, Value};
use lexopt::ValueExt as _;

/// What `--help` prints ahead of the adapter list.
pub const USAGE: &str = "\
Run a Wado program on a wasi:webgpu host.

Usage: wado run-webgpu [options] <file.wado|file.wasm> [args...]

Options:
  -O<level>              Optimization level: 0, 1, 2, 3 or s (default: 2)
      --dir <path>       Preopen a directory (repeatable; default: the current one)
      --no-dir           Drop that default
      --gpu-adapter <index|name>
                         Give the program one adapter, whatever it requests:
                         an integer is its index in the adapter list, anything
                         else a case-insensitive part of its name
      --log-level <level>
                         Log level: debug, info, warn, error, off (default: warn)
                         info names the adapter each request-adapter returns
      --profile guest[,path[,interval_ms]]
                         Sample the guest into a Firefox Profiler file
                         (default: profile.json, 10ms)
  -h, --help             Show this help and this machine's adapter list
  -V, --version          Show the version

Arguments after the input file go to the program.
";

/// An optimization level `wado` accepts, so no other spelling reaches a caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
    Os,
}

impl OptLevel {
    fn parse(level: &str) -> Option<Self> {
        match level {
            "0" => Some(Self::O0),
            "1" => Some(Self::O1),
            "2" => Some(Self::O2),
            "3" => Some(Self::O3),
            "s" => Some(Self::Os),
            _ => None,
        }
    }

    /// The `-O` flag that passes this level to `wado compile`.
    pub fn flag(self) -> &'static str {
        match self {
            Self::O0 => "-O0",
            Self::O1 => "-O1",
            Self::O2 => "-O2",
            Self::O3 => "-O3",
            Self::Os => "-Os",
        }
    }
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
    fn parse(level: &str) -> Option<Self> {
        match level.to_lowercase().as_str() {
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            "off" | "none" => Some(Self::Off),
            _ => None,
        }
    }

    /// The value that passes this level to `wado compile --log-level`.
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

/// How `--gpu-adapter` names the one adapter every `request-adapter` returns.
#[derive(PartialEq, Eq, Debug)]
pub enum AdapterSelector {
    /// The position in the order the adapters are enumerated and listed.
    Index(usize),
    /// A case-insensitive substring of the adapter's name.
    Name(String),
}

impl AdapterSelector {
    fn parse(value: String) -> Self {
        value.parse().map_or(Self::Name(value), Self::Index)
    }
}

/// `--profile guest[,path[,interval_ms]]`, as `wado run` takes it.
#[derive(PartialEq, Eq, Debug)]
pub struct GuestProfile {
    pub path: PathBuf,
    pub interval: Duration,
}

impl GuestProfile {
    fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split(',');
        if parts.next() != Some("guest") {
            return None;
        }
        let path = PathBuf::from(parts.next().unwrap_or("profile.json"));
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

/// One run, as the command line asks for it.
pub struct Args {
    /// The `.wado` source or `.wasm` component to run.
    pub input: PathBuf,
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub gpu_adapter: Option<AdapterSelector>,
    pub profile: Option<GuestProfile>,
    /// Empty means preopen nothing, which `--no-dir` also asks for.
    pub preopens: Vec<PathBuf>,
    /// The arguments after the input file, which belong to the program.
    pub program_args: Vec<String>,
}

/// What the command line asks for.
pub enum Invocation {
    Run(Args),
    /// The usage, which the caller completes with this machine's adapters.
    Help,
    Version,
}

impl Args {
    pub fn parse<I>(argv: I) -> Result<Invocation>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut parser = lexopt::Parser::from_args(argv);
        let mut input = None;
        let mut opt_level = OptLevel::O2;
        let mut log_level = LogLevel::Warn;
        let mut gpu_adapter = None;
        let mut profile = None;
        let mut preopens = Vec::new();
        let mut no_dir = false;
        let mut program_args = Vec::new();

        while let Some(arg) = parser.next()? {
            match arg {
                Short('h') | Long("help") => return Ok(Invocation::Help),
                Short('V') | Long("version") => return Ok(Invocation::Version),
                // Attached and explicit, as `wado run` takes it: `-O2`.
                Short('O') => {
                    let Some(level) = parser.optional_value() else {
                        bail!("-O takes its level attached, as '-O2'\n\n{USAGE}");
                    };
                    let level = level.string()?;
                    let Some(parsed) = OptLevel::parse(&level) else {
                        bail!("unknown optimization level '-O{level}'\n\n{USAGE}");
                    };
                    opt_level = parsed;
                }
                Long("log-level") => {
                    let level = parser.value()?.string()?;
                    let Some(parsed) = LogLevel::parse(&level) else {
                        bail!("unknown log level '{level}'. Use debug, info, warn, error, or off");
                    };
                    log_level = parsed;
                }
                Long("gpu-adapter") => {
                    gpu_adapter = Some(AdapterSelector::parse(parser.value()?.string()?));
                }
                Long("profile") => {
                    let value = parser.value()?.string()?;
                    let Some(parsed) = GuestProfile::parse(&value) else {
                        bail!("unknown profile mode '{value}'. Use guest[,path[,interval_ms]]");
                    };
                    profile = Some(parsed);
                }
                Long("dir") => preopens.push(PathBuf::from(parser.value()?)),
                Long("no-dir") => no_dir = true,
                // Everything after the input file, flags included, is the
                // program's, exactly as `wado run` forwards it.
                Value(value) => {
                    input = Some(PathBuf::from(value));
                    if let Some(raw) = parser.try_raw_args() {
                        for raw_arg in raw {
                            program_args.push(raw_arg.string()?);
                        }
                    }
                    break;
                }
                other => bail!("{}\n\n{USAGE}", other.unexpected()),
            }
        }

        let Some(input) = input else {
            bail!("no input file\n\n{USAGE}");
        };
        if preopens.is_empty() && !no_dir {
            preopens.push(std::env::current_dir()?);
        }

        Ok(Invocation::Run(Self {
            input,
            opt_level,
            log_level,
            gpu_adapter,
            profile,
            preopens,
            program_args,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn try_parse(argv: &[&str]) -> Result<Invocation> {
        Args::parse(argv.iter().map(OsString::from))
    }

    fn parse(argv: &[&str]) -> Args {
        match try_parse(argv).expect("parsing") {
            Invocation::Run(run) => run,
            Invocation::Help | Invocation::Version => panic!("no run to make"),
        }
    }

    #[test]
    fn the_optimization_level_comes_attached() {
        assert_eq!(parse(&["-O0", "app.wado"]).opt_level, OptLevel::O0);
        assert_eq!(parse(&["app.wado"]).opt_level, OptLevel::O2);
        assert!(try_parse(&["-O", "app.wado"]).is_err());
        assert!(try_parse(&["-O", "2", "app.wado"]).is_err());
        assert!(try_parse(&["-O9", "app.wado"]).is_err());
    }

    #[test]
    fn everything_after_the_input_file_belongs_to_the_program() {
        let args = parse(&["-O0", "app.wado", "--dir", "/data", "-x", "rest"]);

        assert_eq!(args.opt_level, OptLevel::O0);
        assert_eq!(args.program_args, ["--dir", "/data", "-x", "rest"]);
        assert_eq!(args.preopens, [std::env::current_dir().unwrap()]);
    }

    #[test]
    fn the_log_level_takes_what_wado_takes_and_defaults_to_warn() {
        assert_eq!(parse(&["app.wado"]).log_level, LogLevel::Warn);
        assert_eq!(
            parse(&["--log-level", "INFO", "app.wado"]).log_level,
            LogLevel::Info
        );
        assert_eq!(
            parse(&["--log-level", "none", "app.wado"]).log_level,
            LogLevel::Off
        );
        assert!(try_parse(&["--log-level", "loud", "app.wado"]).is_err());
    }

    #[test]
    fn a_gpu_adapter_is_an_index_when_an_integer_and_a_name_otherwise() {
        let adapter = |value: &str| parse(&["--gpu-adapter", value, "app.wado"]).gpu_adapter;
        assert_eq!(parse(&["app.wado"]).gpu_adapter, None);
        assert_eq!(adapter("1"), Some(AdapterSelector::Index(1)));
        assert_eq!(
            adapter("nvidia"),
            Some(AdapterSelector::Name("nvidia".to_owned()))
        );
        assert_eq!(adapter("-1"), Some(AdapterSelector::Name("-1".to_owned())));
        assert!(try_parse(&["--gpu-adapter"]).is_err());
    }

    #[test]
    fn a_guest_profile_takes_wado_runs_spelling() {
        let profile = |value: &str| parse(&["--profile", value, "app.wado"]).profile;
        assert_eq!(parse(&["app.wado"]).profile, None);
        assert_eq!(
            profile("guest"),
            Some(GuestProfile {
                path: PathBuf::from("profile.json"),
                interval: Duration::from_millis(10),
            })
        );
        assert_eq!(
            profile("guest,out.json,2"),
            Some(GuestProfile {
                path: PathBuf::from("out.json"),
                interval: Duration::from_millis(2),
            })
        );
        assert!(try_parse(&["--profile", "jitdump", "app.wado"]).is_err());
        assert!(try_parse(&["--profile", "guest,a,b", "app.wado"]).is_err());
    }

    #[test]
    fn no_dir_drops_the_default_grant_and_keeps_every_explicit_one() {
        let preopens = parse(&["--no-dir", "app.wado"]).preopens;
        assert!(preopens.is_empty(), "{preopens:?}");
        assert_eq!(
            parse(&["--no-dir", "--dir", "/data", "app.wado"]).preopens,
            [PathBuf::from("/data")]
        );
    }
}
