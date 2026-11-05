//! What a `wado run-*` subcommand shares, so a program behaves the same under
//! each of them and under `wado run`: the command line, the compile, the
//! engine, the WASI context and the guest profile. A runner adds the interface
//! it serves. It compiles nothing itself: `WADO` names the binary that
//! dispatched it.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use lexopt::Arg::{Long, Short, Value};
use lexopt::{Parser, ValueExt as _};
use tempfile::{TempDir, tempdir};
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, GuestProfiler, Store, UpdateDeadline};
use wasmtime_wasi::p3::add_to_linker as add_wasi_to_linker;
use wasmtime_wasi::p3::bindings::Command;
use wasmtime_wasi::{FsPerms, I32Exit, WasiCtx, WasiCtxBuilder, WasiView};

use crate::engine::{default_config, guest_store};
use crate::sync::lock;
use crate::ticker::Ticker;

/// The usage of the runner `wado <command>`. `own` lists what it adds to the
/// options every runner takes.
#[must_use]
pub fn usage(command: &str, summary: &str, own: &str) -> String {
    format!(
        "\
{summary}

Usage: wado {command} [options] <file.wado|file.wasm> [args...]

Options:
  -O<level>              Optimization level: 0, 1, 2, 3 or s (default: 1)
      --dir <path>       Preopen a directory (repeatable; default: the current one)
      --no-dir           Drop that default
{own}      --log-level <level>
                         Log level: debug, info, warn, error, off (default: warn)
      --profile guest[,path[,interval_ms]]
                         Sample the guest into a Firefox Profiler file
                         (default: profile.json, 10ms)
  -h, --help             Show this help
  -V, --version          Show the version

Arguments after the input file go to the program.
"
    )
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

    /// The `-O` flag that passes this level to `wado compile`.
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

    /// The value that passes this level to `wado compile --log-level`.
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

/// One run, as the command line asks for it.
pub struct RunOptions {
    /// The `.wado` source or `.wasm` component to run.
    pub input: PathBuf,
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub profile: Option<GuestProfile>,
    /// Empty means preopen nothing, which `--no-dir` also asks for.
    pub preopens: Vec<PathBuf>,
    /// The arguments after the input file, which belong to the program.
    pub program_args: Vec<String>,
}

/// What the command line asks for.
pub enum Invocation {
    Run(RunOptions),
    Help,
    Version,
}

impl RunOptions {
    /// Parses the command line of the runner `usage` describes. `own` takes each
    /// long option no runner shares, and answers whether it knows it.
    ///
    /// # Errors
    ///
    /// Fails on anything the runner does not take.
    pub fn parse<I>(
        argv: I,
        usage: &str,
        mut own: impl FnMut(&str, &mut Parser) -> Result<bool>,
    ) -> Result<Invocation>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut parser = Parser::from_args(argv);
        let mut input = None;
        let mut opt_level = OptLevel::O1;
        let mut log_level = LogLevel::Warn;
        let mut profile = None;
        let mut preopens = Vec::new();
        let mut no_dir = false;
        let mut program_args = Vec::new();
        let mut options_ended = false;

        loop {
            options_ended |= ends_options(&mut parser);
            let Some(arg) = parser.next()? else {
                break;
            };
            match arg {
                Short('h') | Long("help") => return Ok(Invocation::Help),
                Short('V') | Long("version") => return Ok(Invocation::Version),
                // Attached and explicit, as `wado run` takes it: `-O2`.
                Short('O') => {
                    let Some(level) = parser.optional_value() else {
                        bail!("-O takes its level attached, as '-O2'\n\n{usage}");
                    };
                    let level = level.string()?;
                    let Some(parsed) = OptLevel::parse(&level) else {
                        bail!("unknown optimization level '-O{level}'\n\n{usage}");
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
                Long("profile") => {
                    let value = parser.value()?.string()?;
                    let Some(parsed) = GuestProfile::parse(&value) else {
                        bail!("unknown profile mode '{value}'. Use guest[,path[,interval_ms]]");
                    };
                    profile = Some(parsed);
                }
                Long("dir") => preopens.push(PathBuf::from(parser.value()?)),
                Long("no-dir") => no_dir = true,
                Long(name) => {
                    let name = name.to_owned();
                    if !own(&name, &mut parser)? {
                        bail!("invalid option '--{name}'\n\n{usage}");
                    }
                }
                Value(value) => {
                    input = Some(PathBuf::from(value));
                    program_args = args_after_input(&mut parser, options_ended);
                    break;
                }
                other @ Short(_) => bail!("{}\n\n{usage}", other.unexpected()),
            }
        }

        let Some(input) = input else {
            bail!("no input file\n\n{usage}");
        };
        if preopens.is_empty() && !no_dir {
            preopens.push(std::env::current_dir()?);
        }

        Ok(Invocation::Run(Self {
            input,
            opt_level,
            log_level,
            profile,
            preopens,
            program_args,
        }))
    }
}

/// Whether the next argument is the `--` that ends a command's options. Any
/// other `--` belongs to the program.
pub fn ends_options(parser: &mut Parser) -> bool {
    parser
        .try_raw_args()
        .is_some_and(|raw| raw.peek() == Some(OsStr::new("--")))
}

/// Everything after the input file, flags included, which goes to the program,
/// less the `--` ending the command's options where none has ended them yet.
pub fn args_after_input(parser: &mut Parser, options_ended: bool) -> Vec<String> {
    let Some(mut raw) = parser.try_raw_args() else {
        return Vec::new();
    };
    if !options_ended {
        raw.next_if(|first| first == "--");
    }
    raw.map(|arg| arg.to_string_lossy().into_owned()).collect()
}

/// The status a program chose through `wasi:cli/exit`, which wasmtime reports as
/// an error so that a program that exits on purpose reads like a trap.
#[must_use]
pub fn exit_status(error: &anyhow::Error) -> Option<i32> {
    error.downcast_ref::<I32Exit>().map(|exit| exit.0)
}

/// A runner's `main`, from its parsed command line. `help` is what `--help`
/// prints, and `run` makes the run, its `T` being what `parsed` adds to the
/// options every runner takes.
pub async fn command_line<T>(
    runner: &str,
    version: &str,
    parsed: Result<(Invocation, T)>,
    help: impl FnOnce() -> String,
    run: impl AsyncFnOnce(RunOptions, T) -> Result<()>,
) -> ExitCode {
    let (options, own) = match parsed {
        Ok((Invocation::Run(options), own)) => (options, own),
        Ok((Invocation::Help, _)) => {
            print!("{}", help());
            return ExitCode::SUCCESS;
        }
        Ok((Invocation::Version, _)) => {
            println!("{runner} {version}");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("{runner}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Err(error) = run(options, own).await else {
        return ExitCode::SUCCESS;
    };
    if let Some(status) = exit_status(&error) {
        return ExitCode::from(status as u8);
    }
    eprintln!("{runner}: {error:#}");
    ExitCode::FAILURE
}

/// A component to run. A compiled one holds its temporary directory open for
/// as long as the run needs the file in it.
pub struct Program {
    path: PathBuf,
    _dir: Option<TempDir>,
}

impl Program {
    /// The component file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Compiles the input with `wado` unless it is already a component.
///
/// # Errors
///
/// Fails when `wado` cannot run or the compile does.
pub fn program_for(options: &RunOptions) -> Result<Program> {
    if options.input.extension().is_some_and(|ext| ext == "wasm") {
        return Ok(Program {
            path: options.input.clone(),
            _dir: None,
        });
    }

    let dir = tempdir()?;
    let path = dir.path().join("program.wasm");
    let wado = wado_binary();
    let status = Process::new(&wado)
        .arg("compile")
        .arg(options.opt_level.flag())
        .args(["--log-level", options.log_level.flag_value()])
        .arg("-o")
        .arg(&path)
        .arg("--")
        .arg(&options.input)
        .status()
        .with_context(|| format!("failed to run '{}'", wado.display()))?;
    if !status.success() {
        bail!("{} compile failed", wado.display());
    }

    Ok(Program {
        path,
        _dir: Some(dir),
    })
}

/// `WADO` is the binary that dispatched this subcommand, so a child compiles
/// with the same `wado` the user invoked. Run directly, it falls back to `PATH`.
fn wado_binary() -> PathBuf {
    std::env::var_os("WADO").map_or_else(|| PathBuf::from("wado"), PathBuf::from)
}

/// Let `wasi:sockets` and `wasi:tls` reach the host network, as a native tool
/// can: a guest is confined by what its effects reach, and one that names
/// `TcpSocket` asked for the network. A deployment that must not give it one
/// confines the host process instead.
pub fn grant_host_network(builder: &mut WasiCtxBuilder) {
    builder.inherit_network();
    // wasmtime denies creating a socket by default; `inherit_network`
    // relaxes only the per-address check that comes after it.
    builder.allow_tcp(true);
    builder.allow_udp(true);
    builder.allow_ip_name_lookup(true);
}

/// The WASI context `wado run` gives a guest: the host's stdio, environment and
/// network, the arguments, and the preopened directories.
///
/// # Errors
///
/// Fails when a directory cannot be opened.
pub fn wasi_ctx(options: &RunOptions) -> Result<WasiCtx> {
    let mut builder = WasiCtx::builder();
    builder.inherit_stdio().inherit_env();
    grant_host_network(&mut builder);

    builder.arg(options.input.to_string_lossy());
    builder.args(&options.program_args);

    for dir in &options.preopens {
        let guest = dir.to_string_lossy().into_owned();
        builder.preopened_dir(dir, &guest, FsPerms::ReadWrite)?;
    }
    Ok(builder.build())
}

/// Runs a component that exports `wasi:cli/run` on `host`. `link` adds what the
/// runner serves beside WASI, and any other import traps where it is called.
///
/// # Errors
///
/// Fails when the program does not load, traps or exits with an error, or when
/// its profile cannot be written.
pub async fn run<H>(
    runner: &str,
    component: &Path,
    options: &RunOptions,
    host: H,
    link: impl FnOnce(&Engine, &Component, &mut Linker<H>) -> Result<()>,
) -> Result<()>
where
    H: WasiView + Send + 'static,
{
    let mut config = default_config(options.opt_level.to_wasmtime());
    config.epoch_interruption(options.profile.is_some());
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, component)?;

    let mut linker: Linker<H> = Linker::new(&engine);
    add_wasi_to_linker(&mut linker)?;
    link(&engine, &component, &mut linker)?;
    linker.define_unknown_imports_as_traps(&component)?;

    let mut store = guest_store(&engine, host);
    let sampling = options
        .profile
        .as_ref()
        .map(|profile| {
            Sampling::start(
                runner,
                &engine,
                &component,
                &mut store,
                &profile.path,
                profile.interval,
            )
        })
        .transpose()?;
    let ran = match Command::instantiate_async(&mut store, &component, &linker).await {
        Ok(command) => {
            store
                .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
                .await
        }
        Err(e) => Err(e),
    };
    let outcome = (|| match ran?? {
        Ok(()) => Ok(()),
        Err(()) => bail!("the program exited with an error"),
    })();
    Sampling::conclude(sampling, outcome)
}

/// `--profile guest`, as `wado run` samples: a thread advances the epoch every
/// interval, and the store's deadline callback records where the guest is.
/// `WADO_PROFILE_MAX_SECS` stops the guest once that many seconds have passed.
pub struct Sampling {
    profiler: Arc<Mutex<Option<GuestProfiler>>>,
    ticker: Ticker,
    file: File,
    path: PathBuf,
}

impl Sampling {
    /// Starts sampling the run `store` is about to make.
    ///
    /// # Errors
    ///
    /// Fails when `path` cannot be written or `WADO_PROFILE_MAX_SECS` does not
    /// parse.
    pub fn start<H: 'static>(
        runner: &str,
        engine: &Engine,
        component: &Component,
        store: &mut Store<H>,
        path: &Path,
        interval: Duration,
    ) -> Result<Self> {
        let budget = profile_budget()?;
        // Created before the run, so a path that cannot be written fails here
        // rather than in place of what the run itself reports.
        let file = File::create(path)
            .with_context(|| format!("cannot write the profile to {}", path.display()))?;
        let profiler = GuestProfiler::new_component(
            engine,
            runner,
            interval,
            component.clone(),
            std::iter::empty::<(String, wasmtime::Module)>(),
        )?;
        let profiler = Arc::new(Mutex::new(Some(profiler)));
        let sampled = Arc::clone(&profiler);
        let over_budget = Arc::new(AtomicBool::new(false));
        let budget_reached = Arc::clone(&over_budget);
        store.epoch_deadline_callback(move |store| {
            if let Some(p) = lock(&sampled).as_mut() {
                p.sample(&store, interval);
            }
            if budget_reached.load(Ordering::Relaxed) {
                return Err(wasmtime::Error::msg(
                    "profile time budget reached (WADO_PROFILE_MAX_SECS)",
                ));
            }
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);
        let ticking = engine.clone();
        let started = Instant::now();
        let ticker = Ticker::start(interval, move || {
            if budget.is_some_and(|budget| started.elapsed() >= budget) {
                over_budget.store(true, Ordering::Relaxed);
            }
            ticking.increment_epoch();
        });
        Ok(Self {
            profiler,
            ticker,
            file,
            path: path.to_owned(),
        })
    }

    /// What a run that ended with `outcome` reports, once any sampling of it
    /// has written its profile.
    ///
    /// # Errors
    ///
    /// Fails when the run did or the profile cannot be written.
    pub fn conclude(sampling: Option<Self>, outcome: Result<()>) -> Result<()> {
        reported(outcome, sampling.map_or(Ok(()), Self::finish))
    }

    fn finish(self) -> Result<()> {
        drop(self.ticker);
        let Some(profiler) = lock(&self.profiler).take() else {
            unreachable!("`finish` takes the sampling by value, so it runs once");
        };
        write_guest_profile(profiler, self.file)?;
        eprintln!("Profile written to {}", self.path.display());
        eprintln!("View at https://profiler.firefox.com/");
        Ok(())
    }
}

/// The run's own outcome comes first; a profile it could not write is only the
/// error where the run succeeded, which a program that exited 0 did.
fn reported(outcome: Result<()>, profiled: Result<()>) -> Result<()> {
    match (outcome, profiled) {
        (Err(e), Err(p)) if exit_status(&e) != Some(0) => {
            eprintln!("error: {p:#}");
            Err(e)
        }
        (Err(e), Ok(())) => Err(e),
        (_, profiled) => profiled,
    }
}

/// `profiler`'s samples written to `file`. Flushed here: a `BufWriter` dropped
/// with its last block unwritten reports no error, so a profile that never
/// reached the disk would read as written.
///
/// # Errors
///
/// Fails when the profile cannot be written.
pub fn write_guest_profile(profiler: GuestProfiler, file: File) -> Result<()> {
    let mut out = BufWriter::new(file);
    profiler.finish(&mut out)?;
    out.flush()?;
    Ok(())
}

/// How long `WADO_PROFILE_MAX_SECS` lets a profiled run go on, in whole
/// seconds as `wado run` reads it; unset, as long as it takes.
fn profile_budget() -> Result<Option<Duration>> {
    let Some(value) = std::env::var_os("WADO_PROFILE_MAX_SECS") else {
        return Ok(None);
    };
    let value = value.to_string_lossy();
    let Ok(secs) = value.parse::<u64>() else {
        bail!("WADO_PROFILE_MAX_SECS takes a whole number of seconds, not '{value}'");
    };
    Ok(Some(Duration::from_secs(secs)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn try_parse(argv: &[&str]) -> Result<Invocation> {
        RunOptions::parse(argv.iter().map(OsString::from), "usage", |_, _| Ok(false))
    }

    fn parse(argv: &[&str]) -> RunOptions {
        match try_parse(argv).expect("parsing") {
            Invocation::Run(run) => run,
            Invocation::Help | Invocation::Version => panic!("no run to make"),
        }
    }

    #[test]
    fn the_optimization_level_comes_attached() {
        assert_eq!(parse(&["-O0", "app.wado"]).opt_level, OptLevel::O0);
        assert_eq!(parse(&["app.wado"]).opt_level, OptLevel::O1);
        assert!(try_parse(&["-O", "app.wado"]).is_err());
        assert!(try_parse(&["-O", "2", "app.wado"]).is_err());
        assert!(try_parse(&["-O9", "app.wado"]).is_err());
    }

    #[test]
    fn everything_after_the_input_file_belongs_to_the_program() {
        let options = parse(&["-O0", "app.wado", "--dir", "/data", "-x", "rest"]);

        assert_eq!(options.opt_level, OptLevel::O0);
        assert_eq!(options.program_args, ["--dir", "/data", "-x", "rest"]);
        assert_eq!(options.preopens, [std::env::current_dir().unwrap()]);
    }

    #[test]
    fn one_double_dash_ends_the_options_and_any_other_is_the_programs() {
        assert_eq!(parse(&["app.wado", "--", "x"]).program_args, ["x"]);
        assert_eq!(
            parse(&["--", "app.wado", "--", "x"]).program_args,
            ["--", "x"]
        );
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
        assert_eq!(
            profile("guest,,5"),
            Some(GuestProfile {
                path: PathBuf::from("profile.json"),
                interval: Duration::from_millis(5),
            })
        );
        assert!(try_parse(&["--profile", "jitdump", "app.wado"]).is_err());
        assert!(try_parse(&["--profile", "guest,a,b", "app.wado"]).is_err());
    }

    #[test]
    fn a_lost_profile_fails_a_program_that_exited_zero_and_no_other() {
        let lost = || Err(anyhow::anyhow!("no profile"));
        let exited = |status| Err(anyhow::Error::new(I32Exit(status)));

        let error = reported(exited(0), lost()).expect_err("the profile is lost");
        assert_eq!(exit_status(&error), None);
        assert_eq!(
            exit_status(&reported(exited(3), lost()).unwrap_err()),
            Some(3)
        );
        assert_eq!(
            exit_status(&reported(exited(0), Ok(())).unwrap_err()),
            Some(0)
        );
        assert!(reported(Ok(()), lost()).is_err());
        assert!(reported(Ok(()), Ok(())).is_ok());
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

    #[test]
    fn an_option_no_runner_knows_is_offered_to_the_runner() {
        let parsed = RunOptions::parse(
            ["--own", "value", "app.wado"].map(OsString::from),
            "usage",
            |name, parser| {
                assert_eq!(name, "own");
                assert_eq!(parser.value()?, "value");
                Ok(true)
            },
        );
        assert!(matches!(parsed, Ok(Invocation::Run(_))));
        assert!(try_parse(&["--own", "app.wado"]).is_err());
    }
}
