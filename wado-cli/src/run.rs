use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::BufWriter;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use lexopt::Arg::Value;
use wasmtime::component::Component;
use wasmtime::{GuestProfiler, UpdateDeadline};

use crate::args::{self, CliExit};
use crate::build::build_for_driver;
use crate::compile::CompileFlags;
use crate::knobs::{CompileKnobOpt, CompileKnobs, RuntimeKnobOpt, RuntimeKnobs};
use crate::manifest;
use crate::runtime::{self, ProfileMode};
use crate::sync::lock;

pub struct RunOptions {
    pub input: String,
    pub knobs: CompileKnobs,
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
        CompileKnobOpt::InlineGrowth,
        CompileKnobOpt::OptIterations,
        CompileKnobOpt::LogLevel,
        CompileKnobOpt::Allocator,
        CompileKnobOpt::Feature,
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
    let mut knobs = CompileKnobs::default();

    // Whether a `--` has already ended this command's options. Only the one
    // that does that job is `wado run`'s; any other belongs to the guest.
    let mut options_ended = false;
    loop {
        options_ended |= parser
            .try_raw_args()
            .is_some_and(|raw| raw.peek() == Some(OsStr::new("--")));
        let Some(arg) = args::next_arg(&mut parser)? else {
            break;
        };
        if let Some(k) = args::match_opt(&arg, Opt::KNOBS, |k| k.spec()) {
            knobs.apply(k, &mut parser)?;
        } else if let Some(r) = args::match_opt(&arg, RuntimeKnobOpt::ALL, |r| r.spec()) {
            runtime_knobs.apply(r, &mut parser)?;
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
            // Everything after the input file goes to the guest, less the `--`
            // ending this command's options where none has ended them yet.
            if let Some(mut raw) = parser.try_raw_args() {
                if !options_ended {
                    raw.next_if(|first| first == "--");
                }
                for raw_arg in raw {
                    program_args.push(raw_arg.to_string_lossy().into_owned());
                }
            }
            break;
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    Ok(RunOptions {
        input: manifest::resolve_input(input, manifest::EntryPointKind::Command, &usage)?,
        knobs,
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
    let engine = runtime::create_engine(cranelift_opt, profile, runtime_knobs)?;
    let component = Component::new(&engine, wasm)?;
    let linker = runtime::create_linker(&component)?;
    let mut store = runtime::create_store(&engine, preopened_dirs, argv)?;

    let profiler = if let ProfileMode::Guest { interval_ms, .. } = profile {
        let interval = Duration::from_millis(*interval_ms);
        let profiler = GuestProfiler::new_component(
            &engine,
            "wado",
            interval,
            component.clone(),
            std::iter::empty::<(String, wasmtime::Module)>(),
        )?;
        let profiler = Arc::new(Mutex::new(Some(profiler)));

        let deadline = Arc::new(AtomicBool::new(false));
        let deadline_cb = deadline.clone();
        let profiler_for_cb = profiler.clone();
        store.epoch_deadline_callback(move |store_ctx| {
            if let Some(ref mut p) = *lock(&profiler_for_cb) {
                p.sample(&store_ctx, interval);
            }
            if deadline_cb.load(Ordering::Relaxed) {
                return Err(wasmtime::Error::msg(
                    "profile time budget reached (WADO_PROFILE_MAX_SECS)",
                ));
            }
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);

        let max_secs = std::env::var("WADO_PROFILE_MAX_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let deadline_thread = deadline;
        let engine_clone = engine.clone();
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            while !stop_clone.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                if let Some(secs) = max_secs
                    && start.elapsed().as_secs() >= secs
                {
                    deadline_thread.store(true, Ordering::Relaxed);
                }
                engine_clone.increment_epoch();
            }
        });

        Some((profiler, stop))
    } else {
        None
    };

    // `run` is exported through the `wasi:cli/run` instance, so bind via the
    // `Command` bindings; the async export is driven through `run_concurrent`.
    let command =
        wasmtime_wasi::p3::bindings::Command::instantiate_async(&mut store, &component, &linker)
            .await?;
    let outer = store
        .run_concurrent(async |accessor| command.wasi_cli_run().call_run(accessor).await)
        .await;

    if let Some((profiler_arc, stop)) = profiler {
        stop.store(true, Ordering::Relaxed);

        if let ProfileMode::Guest { path, .. } = profile {
            let guest_profiler = lock(&profiler_arc).take().unwrap();
            let file = std::fs::File::create(path)?;
            guest_profiler.finish(BufWriter::new(file))?;
            eprintln!("Profile written to {path}");
            eprintln!("View at https://profiler.firefox.com/");
        }
    }

    let result = outer??;
    result.map_err(|()| anyhow::anyhow!("Component returned error"))?;

    Ok(())
}

pub async fn run(opts: RunOptions) -> Result<(), CliExit> {
    let cranelift_opt = opts.knobs.opt_level.to_wasmtime();
    // Leave `target_world` unset so the compiler picks the cli/command default
    // (and bump allocator unless overridden).
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

/// wasmtime reports `wasi:cli/exit` as `Err(I32Exit(code))`, so a program that
/// exits on purpose is indistinguishable from a trap. Propagate the code and
/// leave the program's own diagnostics as the only output.
fn classify_run_error(e: anyhow::Error) -> CliExit {
    match e.downcast_ref::<wasmtime_wasi::I32Exit>() {
        Some(wasmtime_wasi::I32Exit(code)) => CliExit::silent_failure(*code),
        None => CliExit::error(format!("Runtime error: {e:?}")),
    }
}
