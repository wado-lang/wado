//! What `wado run` and each `wado run-*` subcommand share, so a program behaves
//! the same under all of them: the command line, the compile, the engine and
//! the guest profile. [`crate::guest`] holds what the guest reaches. A runner
//! adds the interface it serves. It compiles nothing itself: `WADO` names the
//! binary that dispatched it, and `wado build -o -` builds what `wado run` would
//! have run.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use lexopt::Arg::{Long, Short, Value};
use lexopt::Parser;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, GuestProfiler, Store, UpdateDeadline};
use wasmtime_wasi::I32Exit;
use wasmtime_wasi::p3::bindings::Command;

use crate::guest::{Guest, GuestState, create_config, fuel_spent, host_linker, new_store};
use crate::options::{
    CliExit, CompileKnobOpt, DirGrants, HELP_SPEC, HostKnobOpt, LogLevel, NO_DIR_SPEC, OptLevel,
    OptSpec, OptsHelp, ParamOpt, ProfileMode, RuntimeKnobOpt, RuntimeKnobs, match_opt, next_arg,
    parse_log_level_arg, parse_opt_level_arg, parse_profile, require_string, require_value,
    unexpected_arg,
};
use crate::path_search::find_program;
use crate::process_host::ProcessGrant;
use crate::sync::lock;
use crate::ticker::Ticker;

/// An option of `wado run` and the runners that no other subcommand shares.
#[derive(Clone, Copy)]
enum RunOpt {
    Dir,
    NoDir,
    Profile,
    Help,
}

impl RunOpt {
    const ALL: &[Self] = &[Self::Dir, Self::NoDir, Self::Profile, Self::Help];

    const fn spec(self) -> OptSpec {
        match self {
            // Override the shared --dir description to also document the
            // implicit "preopen cwd by default" rule.
            Self::Dir => OptSpec {
                long: Some("dir"),
                short: None,
                value: Some("<path>"),
                desc: "Preopen directory for WASI filesystem access\nUse --dir host::guest to specify different guest path\nOverrides the default of preopening the current directory",
            },
            Self::NoDir => NO_DIR_SPEC,
            Self::Profile => OptSpec {
                long: Some("profile"),
                short: None,
                value: Some("<mode>"),
                desc: "Enable profiling:\n  guest[,path[,interval_ms]]  Cross-platform guest profiling\n                               (default: profile.json, 10ms)\n  jitdump   Linux perf jitdump (use with perf record -k mono)\n  perfmap   Linux perf map (use with perf record -k mono)",
            },
            Self::Help => HELP_SPEC,
        }
    }
}

/// The compile knobs `wado run` and the runners take.
pub const RUN_KNOBS: &[CompileKnobOpt] = &[
    CompileKnobOpt::NoCache,
    CompileKnobOpt::OptLevel,
    CompileKnobOpt::InlineThreshold,
    CompileKnobOpt::InlineHotCallsiteThreshold,
    CompileKnobOpt::InlineColdCallsiteThreshold,
    CompileKnobOpt::InlineGrowth,
    CompileKnobOpt::OptIterations,
    CompileKnobOpt::Specializations,
    CompileKnobOpt::OptThreads,
    CompileKnobOpt::PredictableBranchThreshold,
    CompileKnobOpt::ProfileSummaryCutoffHot,
    CompileKnobOpt::ProfileSummaryCutoffCold,
    CompileKnobOpt::LogLevel,
    CompileKnobOpt::Allocator,
    CompileKnobOpt::Feature,
    CompileKnobOpt::NoLint,
    CompileKnobOpt::Lint,
    CompileKnobOpt::LintGenerated,
];

/// An option that bears on the compile. `wado run` applies it, and a runner
/// passes it on.
#[derive(Clone, Copy)]
pub enum CompileOpt {
    Knob(CompileKnobOpt),
    Param(ParamOpt),
    Host(HostKnobOpt),
}

impl CompileOpt {
    const fn spec(self) -> OptSpec {
        match self {
            Self::Knob(knob) => knob.spec(),
            Self::Param(param) => param.spec(),
            Self::Host(host) => host.spec(),
        }
    }
}

/// The options of `wado run` and every runner, `extra` after the first group,
/// rendered for a help text.
#[must_use]
pub fn run_options_help(extra: &[OptSpec]) -> String {
    OptsHelp::default()
        .add(RunOpt::ALL, |o| o.spec())
        .add(extra, |spec| *spec)
        .add(RUN_KNOBS, |o| o.spec())
        .add(RuntimeKnobOpt::ALL, |o| o.spec())
        .add(HostKnobOpt::ALL, |o| o.spec())
        .add(ParamOpt::ALL, |o| o.spec())
        .render()
}

/// What an `own` callback made of an option [`parse_run_command`] does not know.
pub enum OwnOption {
    /// It took the option, and its value from the parser.
    Taken,
    Help,
    Version,
    /// It does not know the option either.
    Unknown,
}

/// What the command line of `wado run` or a runner says.
pub struct RunCommand {
    /// The `.wado` source or `.wasm` component to run, where one is named.
    pub input: Option<String>,
    pub profile: ProfileMode,
    /// `(host_path, guest_path)` pairs from `--dir host[::guest]`.
    pub preopened_dirs: Vec<(String, String)>,
    /// The arguments after the input file, which belong to the program.
    pub program_args: Vec<String>,
    pub runtime: RuntimeKnobs,
}

/// How a command line ended.
pub enum Parsed {
    Run(RunCommand),
    Help,
    Version,
}

/// Parses what `wado run` and the runners share. `compile` takes each option
/// that bears on the compile, its value still in the parser, and `own` each
/// that is neither shared nor a compile option, by name.
///
/// # Errors
///
/// Fails on anything no callback takes, or on what one rejects.
pub fn parse_run_command(
    parser: &mut Parser,
    usage: &str,
    mut compile: impl FnMut(CompileOpt, &mut Parser) -> Result<(), CliExit>,
    mut own: impl FnMut(&str, &mut Parser) -> Result<OwnOption, CliExit>,
) -> Result<Parsed, CliExit> {
    let mut input = None;
    let mut profile = ProfileMode::None;
    let mut dirs = DirGrants::default();
    let mut program_args = Vec::new();
    let mut runtime = RuntimeKnobs::default();

    // Whether a `--` has already ended this command's options. Only the one
    // that does that job is the command's; any other belongs to the guest.
    let mut options_ended = false;
    loop {
        options_ended |= ends_options(parser);
        let Some(arg) = next_arg(parser)? else {
            break;
        };
        let matched = match_opt(&arg, RUN_KNOBS, |k| k.spec())
            .map(CompileOpt::Knob)
            .or_else(|| match_opt(&arg, HostKnobOpt::ALL, |h| h.spec()).map(CompileOpt::Host))
            .or_else(|| match_opt(&arg, ParamOpt::ALL, |p| p.spec()).map(CompileOpt::Param));
        if let Some(opt) = matched {
            compile(opt, parser)?;
        } else if let Some(r) = match_opt(&arg, RuntimeKnobOpt::ALL, |r| r.spec()) {
            runtime.apply(r, parser)?;
        } else if let Some(opt) = match_opt(&arg, RunOpt::ALL, |o| o.spec()) {
            match opt {
                RunOpt::Dir => dirs.add(parser)?,
                RunOpt::NoDir => dirs.suppress_default(),
                RunOpt::Profile => profile = parse_profile(&require_string(parser)?)?,
                RunOpt::Help => return Ok(Parsed::Help),
            }
        } else if let Value(value) = arg {
            input = Some(value.to_string_lossy().into_owned());
            program_args = args_after_input(parser, options_ended);
            break;
        } else {
            let name = match &arg {
                Long(name) => (*name).to_owned(),
                Short(name) => name.to_string(),
                Value(_) => unreachable!("taken as the input above"),
            };
            let refused = unexpected_arg(arg, usage);
            match own(&name, parser)? {
                OwnOption::Taken => {}
                OwnOption::Help => return Ok(Parsed::Help),
                OwnOption::Version => return Ok(Parsed::Version),
                OwnOption::Unknown => return Err(refused),
            }
        }
    }

    Ok(Parsed::Run(RunCommand {
        input,
        profile,
        preopened_dirs: dirs.finish(),
        program_args,
        runtime,
    }))
}

/// The usage of the runner `wado <command>`. `own` lists what it adds to the
/// options every runner takes.
#[must_use]
pub fn usage(command: &str, summary: &str, own: &[OptSpec]) -> String {
    let version = OptSpec {
        long: Some("version"),
        short: Some('V'),
        value: None,
        desc: "Show the version",
    };
    let extra: Vec<OptSpec> = own.iter().copied().chain([version]).collect();
    format!(
        "\
{summary}

Usage: wado {command} [options] [<file.wado|file.wasm>] [args...]

Options:
{}
With no input file, it runs the `wasi:cli/command` entry of the wado.toml found
from the current directory. Arguments after the input file go to the program.
",
        run_options_help(&extra)
    )
}

/// One run, as a runner's command line asks for it.
pub struct RunOptions {
    command: RunCommand,
    opt_level: OptLevel,
    pub log_level: LogLevel,
    /// The options only a compile reads, as they were spelled.
    compile_args: Vec<OsString>,
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
        mut own: impl FnMut(&str, &mut Parser) -> Result<bool, CliExit>,
    ) -> Result<Invocation, CliExit>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut opt_level = OptLevel::O1;
        let mut log_level = LogLevel::Warn;
        let mut compile_args: Vec<OsString> = Vec::new();
        let parsed = parse_run_command(
            &mut Parser::from_args(argv),
            usage,
            |opt, parser| {
                match opt {
                    CompileOpt::Knob(CompileKnobOpt::OptLevel) => {
                        opt_level = parse_opt_level_arg(parser)?;
                    }
                    CompileOpt::Knob(CompileKnobOpt::LogLevel) => {
                        log_level = parse_log_level_arg(parser)?;
                    }
                    _ => {
                        compile_args.push(opt.spec().flag().into());
                        if opt.spec().value.is_some() {
                            compile_args.push(require_value(parser)?);
                        }
                    }
                }
                Ok(())
            },
            |name, parser| match name {
                "h" => Ok(OwnOption::Help),
                "V" | "version" => Ok(OwnOption::Version),
                _ if own(name, parser)? => Ok(OwnOption::Taken),
                _ => Ok(OwnOption::Unknown),
            },
        )?;
        Ok(match parsed {
            Parsed::Run(command) => Invocation::Run(Self {
                command,
                opt_level,
                log_level,
                compile_args,
            }),
            Parsed::Help => Invocation::Help,
            Parsed::Version => Invocation::Version,
        })
    }

    /// What a guest of `program` holds on this command line.
    ///
    /// # Errors
    ///
    /// Fails when a directory cannot be opened.
    pub fn guest(&self, program: &Program) -> Result<GuestState, CliExit> {
        guest_state(
            &program.entry,
            &self.command.program_args,
            &self.command.preopened_dirs,
            self.command.runtime.process.clone(),
        )
    }

    /// The engine this command line asks for.
    #[must_use]
    pub fn engine(&self) -> RunEngine<'_> {
        RunEngine {
            opt_level: self.opt_level.to_wasmtime(),
            profile: &self.command.profile,
            knobs: &self.command.runtime,
        }
    }
}

/// What a guest holds: `entry` is the program name it reads ahead of
/// `program_args`.
///
/// # Errors
///
/// Fails when a directory cannot be opened.
pub fn guest_state(
    entry: &str,
    program_args: &[String],
    preopened_dirs: &[(String, String)],
    process: ProcessGrant,
) -> Result<GuestState, CliExit> {
    let args: Vec<String> = std::iter::once(entry)
        .chain(program_args.iter().map(String::as_str))
        .map(str::to_owned)
        .collect();
    GuestState::new(preopened_dirs, &args, process).map_err(|e| run_error(&e))
}

/// Whether the next argument is the `--` that ends a command's options. Any
/// other `--` belongs to the program.
fn ends_options(parser: &mut Parser) -> bool {
    parser
        .try_raw_args()
        .is_some_and(|raw| raw.peek() == Some(OsStr::new("--")))
}

/// Everything after the input file, flags included, which goes to the program,
/// less the `--` ending the command's options where none has ended them yet.
fn args_after_input(parser: &mut Parser, options_ended: bool) -> Vec<String> {
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
fn exit_status(error: &anyhow::Error) -> Option<i32> {
    error.downcast_ref::<I32Exit>().map(|exit| exit.0)
}

/// Propagate the code of a program that exits on purpose, and leave its own
/// diagnostics as the only output.
fn run_error(error: &anyhow::Error) -> CliExit {
    match exit_status(error) {
        Some(code) => CliExit::silent_failure(code),
        None => CliExit::error(format!("Runtime error: {error:?}")),
    }
}

/// A runner's `main`, from its parsed command line. `help` is what `--help`
/// prints, and `run` makes the run, its `T` being what `parsed` adds to the
/// options every runner takes.
pub async fn command_line<T>(
    runner: &str,
    version: &str,
    parsed: Result<(Invocation, T), CliExit>,
    help: impl FnOnce() -> String,
    run: impl AsyncFnOnce(RunOptions, T) -> Result<(), CliExit>,
) -> ExitCode {
    let exit = match parsed {
        Ok((Invocation::Run(options), own)) => match run(options, own).await {
            Ok(()) => return ExitCode::SUCCESS,
            Err(exit) => exit,
        },
        Ok((Invocation::Help, _)) => {
            print!("{}", help());
            return ExitCode::SUCCESS;
        }
        Ok((Invocation::Version, _)) => {
            println!("{runner} {version}");
            return ExitCode::SUCCESS;
        }
        Err(exit) => exit,
    };
    eprint!("{}", exit.message);
    ExitCode::from(exit.exit_code as u8)
}

/// A component to run, and the entry `wado` built it from.
pub struct Program {
    pub entry: String,
    pub wasm: Vec<u8>,
}

/// The custom section `wado build --embed-entry` names the entry in, so a runner
/// reads it from the bytes it runs.
pub const ENTRY_SECTION: &str = "wado:entry";

/// The component the input names, built by `wado build` unless it already is
/// one. It comes back on stdout rather than through a file, which something
/// else could replace before the runner read it.
///
/// # Errors
///
/// Fails when `wado` cannot run or the compile does, whose own diagnostics are
/// then the output.
pub fn program_for(options: &RunOptions) -> Result<Program, CliExit> {
    let input = options.command.input.as_deref();
    if let Some(entry) = input.filter(|input| Path::new(input).extension() == Some("wasm".as_ref()))
    {
        if let Some(flag) = options.compile_args.first() {
            return Err(CliExit::error(format!(
                "{} configures a compile, and {entry} is already compiled",
                flag.display()
            )));
        }
        return Ok(Program {
            entry: entry.to_owned(),
            wasm: std::fs::read(entry).map_err(|e| CliExit::error(format!("{entry}: {e}")))?,
        });
    }

    let wado = wado_binary()?;
    let mut build = Process::new(&wado);
    build
        .args(["build", "--world", "wasi:cli/command", "--embed-entry"])
        .args(["-o", "-"])
        // Always spelled: `wado build`'s default level is not `wado run`'s.
        .arg(options.opt_level.flag())
        .args(["--log-level", options.log_level.flag_value()])
        .args(&options.compile_args)
        .stderr(Stdio::inherit());
    if let Some(input) = input {
        build.arg("--").arg(input);
    }
    let built = build
        .output()
        .map_err(|e| CliExit::error(format!("failed to run '{}': {e}", wado.display())))?;
    if !built.status.success() {
        // A code means it ended on its own, its diagnostics already printed.
        return Err(built.status.code().map_or_else(
            || CliExit::error(format!("'{}' ended by {}", wado.display(), built.status)),
            CliExit::silent_failure,
        ));
    }

    let Some(entry) = embedded_entry(&built.stdout) else {
        return Err(CliExit::error(format!(
            "'{}' built a component that names no entry; is it older than this runner?",
            wado.display()
        )));
    };
    Ok(Program {
        entry,
        wasm: built.stdout,
    })
}

/// The entry `wado build --embed-entry` named in `wasm`. Its section is the last
/// one: a component the program imports may carry one of its own, further in.
#[must_use]
pub fn embedded_entry(wasm: &[u8]) -> Option<String> {
    wasmparser::Parser::new(0)
        .parse_all(wasm)
        .filter_map(|payload| match payload {
            Ok(wasmparser::Payload::CustomSection(section)) if section.name() == ENTRY_SECTION => {
                Some(String::from_utf8_lossy(section.data()).into_owned())
            }
            _ => None,
        })
        .last()
}

/// `WADO` is the binary that dispatched this subcommand, so a child compiles
/// with the same `wado` the user invoked. Run directly, it falls back to `PATH`.
fn wado_binary() -> Result<PathBuf, CliExit> {
    std::env::var_os("WADO")
        .map(PathBuf::from)
        .or_else(|| find_program(&format!("wado{}", std::env::consts::EXE_SUFFIX)))
        .ok_or_else(|| CliExit::error("no `wado` on PATH; set WADO to the binary"))
}

/// A run's engine: its optimization level, profile and runtime knobs.
pub struct RunEngine<'a> {
    pub opt_level: wasmtime::OptLevel,
    pub profile: &'a ProfileMode,
    pub knobs: &'a RuntimeKnobs,
}

/// Runs a component that exports `wasi:cli/run` on `state`. `link` adds what a
/// runner serves, and any import nothing serves traps where it is called.
/// `runner` names the profile a guest profile writes.
///
/// # Errors
///
/// Fails when the program does not load, traps or exits with an error, or when
/// its profile cannot be written.
pub async fn run<T: Guest>(
    runner: &str,
    wasm: &[u8],
    engine: &RunEngine<'_>,
    state: T,
    link: impl FnOnce(&Engine, &Component, &mut Linker<T>) -> Result<()>,
) -> Result<(), CliExit> {
    run_component(runner, wasm, engine, state, link)
        .await
        .map_err(|e| run_error(&e))
}

async fn run_component<T: Guest>(
    runner: &str,
    wasm: &[u8],
    run_engine: &RunEngine<'_>,
    state: T,
    link: impl FnOnce(&Engine, &Component, &mut Linker<T>) -> Result<()>,
) -> Result<()> {
    let config = create_config(run_engine.opt_level, run_engine.profile, run_engine.knobs);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, wasm)?;

    let mut linker = host_linker(&engine)?;
    link(&engine, &component, &mut linker)?;
    linker.define_unknown_imports_as_traps(&component)?;

    let mut store = new_store(&engine, state);
    let sampling = if let ProfileMode::Guest(profile) = run_engine.profile {
        Some(Sampling::start(
            runner,
            &engine,
            &component,
            &mut store,
            &profile.path,
            profile.interval,
        )?)
    } else {
        None
    };
    // `run` is exported through the `wasi:cli/run` instance, so bind via the
    // `Command` bindings; the async export is driven through `run_concurrent`.
    let ran = match Command::instantiate_async(&mut store, &component, &linker).await {
        Ok(command) => {
            store
                .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
                .await
        }
        Err(e) => Err(e),
    };
    if let Some(fuel) = fuel_spent(&store) {
        eprintln!("fuel {fuel}");
    }
    let outcome = (|| match ran?? {
        Ok(()) => Ok(()),
        Err(()) => bail!("the program exited with an error"),
    })();
    Sampling::conclude(sampling, outcome)
}

/// `--profile guest`, as `wado run` samples: a thread advances the epoch every
/// interval, and the store's deadline callback records where the guest is.
/// `WADO_PROFILE_MAX_SECS` stops the guest once that many seconds have passed.
struct Sampling {
    profiler: Arc<Mutex<Option<GuestProfiler>>>,
    ticker: Ticker,
    file: File,
    path: PathBuf,
}

impl Sampling {
    /// Starts sampling the run `store` is about to make.
    fn start<H: 'static>(
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
    fn conclude(sampling: Option<Self>, outcome: Result<()>) -> Result<()> {
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
    use crate::options::GuestProfile;

    fn try_parse(argv: &[&str]) -> Result<Invocation, CliExit> {
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
        assert_eq!(
            options.command.program_args,
            ["--dir", "/data", "-x", "rest"]
        );
        assert_eq!(
            options.command.preopened_dirs,
            [(".".to_owned(), ".".to_owned())]
        );
    }

    #[test]
    fn one_double_dash_ends_the_options_and_any_other_is_the_programs() {
        assert_eq!(parse(&["app.wado", "--", "x"]).command.program_args, ["x"]);
        assert_eq!(
            parse(&["--", "app.wado", "--", "x"]).command.program_args,
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
    fn a_profile_takes_wado_runs_spelling() {
        let profile = |value: &str| parse(&["--profile", value, "app.wado"]).command.profile;
        assert_eq!(parse(&["app.wado"]).command.profile, ProfileMode::None);
        assert_eq!(
            profile("guest"),
            ProfileMode::Guest(GuestProfile {
                path: PathBuf::from("profile.json"),
                interval: Duration::from_millis(10),
            })
        );
        assert_eq!(
            profile("guest,out.json,2"),
            ProfileMode::Guest(GuestProfile {
                path: PathBuf::from("out.json"),
                interval: Duration::from_millis(2),
            })
        );
        assert_eq!(
            profile("guest,,5"),
            ProfileMode::Guest(GuestProfile {
                path: PathBuf::from("profile.json"),
                interval: Duration::from_millis(5),
            })
        );
        assert_eq!(profile("jitdump"), ProfileMode::JitDump);
        assert_eq!(profile("perfmap"), ProfileMode::PerfMap);
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
        let dirs = parse(&["--no-dir", "app.wado"]).command.preopened_dirs;
        assert!(dirs.is_empty(), "{dirs:?}");
        assert_eq!(
            parse(&["--no-dir", "--dir", "/data", "app.wado"])
                .command
                .preopened_dirs,
            [("/data".to_owned(), "/data".to_owned())]
        );
        assert_eq!(
            parse(&["--dir", "/data::/mnt", "app.wado"])
                .command
                .preopened_dirs,
            [("/data".to_owned(), "/mnt".to_owned())]
        );
    }

    #[test]
    fn an_option_no_runner_knows_is_offered_to_the_runner() {
        let parsed = RunOptions::parse(
            ["--own", "value", "app.wado"].map(OsString::from),
            "usage",
            |name, parser| {
                assert_eq!(name, "own");
                assert_eq!(require_string(parser)?, "value");
                Ok(true)
            },
        );
        assert!(matches!(parsed, Ok(Invocation::Run(_))));
        assert!(try_parse(&["--own", "app.wado"]).is_err());
    }

    #[test]
    fn a_compile_option_reaches_wado_as_it_was_spelled() {
        let options = parse(&[
            "-O3",
            "--allocator",
            "debug",
            "-f",
            "no-pgo",
            "-D",
            "A=1",
            "--no-lint",
            "--no-retain-memory",
            "--log-level",
            "info",
            "app.wado",
        ]);
        let spelled: Vec<_> = options
            .compile_args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            spelled,
            [
                "--allocator",
                "debug",
                "-f",
                "no-pgo",
                "--define",
                "A=1",
                "--no-lint",
                "--no-retain-memory",
            ]
        );
    }

    #[test]
    fn the_runtime_knobs_are_wado_runs() {
        let options = parse(&[
            "--collector",
            "drc",
            "--gc-heap-initial",
            "1m",
            "--report-fuel",
            "--deny-process",
            "app.wado",
        ]);
        let knobs = &options.command.runtime;
        assert!(matches!(
            knobs.collector,
            wasmtime::Collector::DeferredReferenceCounting
        ));
        assert_eq!(knobs.gc_heap_initial_size, 1 << 20);
        assert!(knobs.report_fuel);
        assert_eq!(knobs.process, ProcessGrant::Denied);
    }

    #[test]
    fn a_wasm_input_refuses_what_only_a_compile_reads() {
        let refused = |argv| match program_for(&parse(argv)) {
            Ok(_) => panic!("no such file to read"),
            Err(exit) => exit.message,
        };
        assert!(refused(&["-D", "A=1", "missing.wasm"]).contains("--define"));
        // The engine and the host read these two as well.
        assert!(refused(&["-O3", "--log-level", "info", "missing.wasm"]).contains("missing.wasm:"));
    }

    #[test]
    fn no_input_file_is_left_to_the_manifest() {
        assert_eq!(parse(&["-O0"]).command.input, None);
    }
}
