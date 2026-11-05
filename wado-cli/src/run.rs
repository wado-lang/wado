use std::fmt::Write as _;

use anyhow::{Result, bail};
use lexopt::Arg::Value;
use wado_host::runner::{Sampling, args_after_input, ends_options, exit_status};
use wasmtime::component::Component;

use crate::args::{self, CliExit};
use crate::build::build_for_driver;
use crate::compile::CompileFlags;
use crate::knobs::{
    CompileKnobOpt, CompileKnobs, HostKnobOpt, HostKnobs, RuntimeKnobOpt, RuntimeKnobs,
};
use crate::manifest;
use crate::runtime::{self, ProfileMode};

pub struct RunOptions {
    pub input: String,
    pub knobs: CompileKnobs,
    pub host: HostKnobs,
    pub profile: ProfileMode,
    /// `(host_path, guest_path)` pairs from `--dir host[::guest]`.
    pub preopened_dirs: Vec<(String, String)>,
    /// The guest's arguments, which `get-arguments` answers after the program name.
    pub program_args: Vec<String>,
    pub runtime: RuntimeKnobs,
}

#[derive(Clone, Copy)]
enum Opt {
    Dir,
    NoDir,
    Profile,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Dir, Self::NoDir, Self::Profile, Self::Help];

    const KNOBS: &[CompileKnobOpt] = &[
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

    const fn spec(self) -> args::OptSpec {
        match self {
            // Override the shared --dir description to also document the
            // implicit "preopen cwd by default" rule.
            Self::Dir => args::OptSpec {
                long: Some("dir"),
                short: None,
                value: Some("<path>"),
                desc: "Preopen directory for WASI filesystem access\nUse --dir host::guest to specify different guest path\nOverrides the default of preopening the current directory",
            },
            Self::NoDir => args::NO_DIR_SPEC,
            Self::Profile => args::OptSpec {
                long: Some("profile"),
                short: None,
                value: Some("<mode>"),
                desc: "Enable profiling:\n  guest[,path[,interval_ms]]  Cross-platform guest profiling\n                               (default: profile.json, 10ms)\n  jitdump   Linux perf jitdump (use with perf record -k mono)\n  perfmap   Linux perf map (use with perf record -k mono)",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado run [options] <file.wado>").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Compile and run a Wado CLI program (wasi:cli/command world)."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(
        buf,
        "{}",
        args::OptsHelp::default()
            .add(Opt::ALL, |o| o.spec())
            .add(Opt::KNOBS, |o| o.spec())
            .add(RuntimeKnobOpt::ALL, |o| o.spec())
            .add(HostKnobOpt::ALL, |o| o.spec())
            .add(args::ParamOpt::ALL, |o| o.spec())
            .render()
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "By default, the current directory is preopened as '.' for WASI filesystem access."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Profiling examples:").unwrap();
    writeln!(buf, "  wado run --profile guest prog.wado").unwrap();
    writeln!(
        buf,
        "    => writes profile.json, view at https://profiler.firefox.com/"
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "  wado run --profile guest,output.json,5 prog.wado").unwrap();
    writeln!(buf, "    => custom output path and 5ms sampling interval").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "  perf record -k mono wado run --profile jitdump prog.wado"
    )
    .unwrap();
    writeln!(
        buf,
        "  perf inject --jit --input perf.data --output perf.jit.data"
    )
    .unwrap();
    writeln!(buf, "  perf report --input perf.jit.data").unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<RunOptions, CliExit> {
    let usage = format_usage();
    let mut input: Option<String> = None;
    let mut profile = ProfileMode::None;
    let mut dirs = args::DirGrants::default();
    let mut program_args: Vec<String> = Vec::new();
    let mut runtime_knobs = RuntimeKnobs::default();
    let mut knobs = CompileKnobs::compile_and_run();
    let mut host = HostKnobs::default();

    // Whether a `--` has already ended this command's options. Only the one
    // that does that job is `wado run`'s; any other belongs to the guest.
    let mut options_ended = false;
    loop {
        options_ended |= ends_options(&mut parser);
        let Some(arg) = args::next_arg(&mut parser)? else {
            break;
        };
        if let Some(k) = args::match_opt(&arg, Opt::KNOBS, |k| k.spec()) {
            knobs.apply(k, &mut parser)?;
        } else if let Some(r) = args::match_opt(&arg, RuntimeKnobOpt::ALL, |r| r.spec()) {
            runtime_knobs.apply(r, &mut parser)?;
        } else if let Some(h) = args::match_opt(&arg, HostKnobOpt::ALL, |h| h.spec()) {
            host.apply(h);
        } else if let Some(p) = args::match_opt(&arg, args::ParamOpt::ALL, |p| p.spec()) {
            args::apply_param_opt(&mut knobs.params, p, &mut parser)?;
        } else if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Dir => dirs.add(&mut parser)?,
                Opt::NoDir => dirs.suppress_default(),
                Opt::Profile => {
                    let spec = args::require_string(&mut parser)?;
                    profile = runtime::parse_profile(&spec)?;
                }
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            input = Some(val.to_string_lossy().into_owned());
            program_args = args_after_input(&mut parser, options_ended);
            break;
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    Ok(RunOptions {
        input: manifest::resolve_input(input, manifest::EntryPointKind::Command, &usage)?,
        knobs,
        host,
        profile,
        preopened_dirs: dirs.finish(),
        program_args,
        runtime: runtime_knobs,
    })
}

async fn run_cli_component(
    wasm: &[u8],
    cranelift_opt: wasmtime::OptLevel,
    profile: &ProfileMode,
    preopened_dirs: &[(String, String)],
    argv: &[String],
    runtime_knobs: RuntimeKnobs,
) -> Result<()> {
    let engine = runtime::create_engine(cranelift_opt, profile, runtime_knobs.clone())?;
    let component = Component::new(&engine, wasm)?;
    let linker = runtime::create_linker(&component)?;
    let mut store = runtime::create_store(&engine, preopened_dirs, argv, runtime_knobs.process)?;

    let sampling = if let ProfileMode::Guest(guest) = profile {
        Some(Sampling::start(
            "wado",
            &engine,
            &component,
            &mut store,
            &guest.path,
            guest.interval,
        )?)
    } else {
        None
    };

    // `run` is exported through the `wasi:cli/run` instance, so bind via the
    // `Command` bindings; the async export is driven through `run_concurrent`.
    let instantiated =
        wasmtime_wasi::p3::bindings::Command::instantiate_async(&mut store, &component, &linker)
            .await;
    let outer = match instantiated {
        Ok(command) => {
            store
                .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
                .await
        }
        Err(e) => Err(e),
    };

    if let Some(fuel) = runtime::fuel_spent(&store) {
        eprintln!("fuel {fuel}");
    }

    let outcome = (|| match outer?? {
        Ok(()) => Ok(()),
        Err(()) => bail!("Component returned error"),
    })();

    Sampling::conclude(sampling, outcome)
}

pub async fn run(opts: RunOptions) -> Result<(), CliExit> {
    let cranelift_opt = opts.knobs.opt_level.to_wasmtime();
    // Leave `target_world` unset so the compiler picks the cli/command default.
    let flags = CompileFlags {
        knobs: opts.knobs,
        ..CompileFlags::default()
    };
    // `run` is a driver on the build tier (like `cargo run`): in a project it
    // builds the cli/command world through the shared build core (metadata
    // embedded, written to build/), then executes it; a bare file with no
    // project stays on the in-memory compile primitive.
    let wasm = Box::pin(build_for_driver(&opts.input, "wasi:cli/command", &flags)).await?;
    let argv: Vec<String> = std::iter::once(opts.input)
        .chain(opts.program_args)
        .collect();

    run_cli_component(
        &wasm,
        cranelift_opt,
        &opts.profile,
        &opts.preopened_dirs,
        &argv,
        opts.runtime,
    )
    .await
    .map_err(classify_run_error)
}

/// Propagate the code of a program that exits on purpose, and leave its own
/// diagnostics as the only output.
fn classify_run_error(e: anyhow::Error) -> CliExit {
    match exit_status(&e) {
        Some(code) => CliExit::silent_failure(code),
        None => CliExit::error(format!("Runtime error: {e:?}")),
    }
}
