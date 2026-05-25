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
use crate::compile::{self, CompileFlags, OptLevel};
use crate::manifest;
use crate::runtime::{self, ProfileMode};
use wado_compiler::LogLevel;

pub struct RunOptions {
    pub input: String,
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub profile: ProfileMode,
    /// `(host_path, guest_path)` pairs from `--dir host[::guest]`.
    pub preopened_dirs: Vec<(String, String)>,
    /// Arguments forwarded to the guest via `wasi:cli/environment.get-arguments`.
    pub program_args: Vec<String>,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
    pub allocator: Option<String>,
}

#[derive(Clone, Copy)]
enum Opt {
    Dir,
    NoDir,
    OptLevel,
    InlineThreshold,
    OptIterations,
    LogLevel,
    Allocator,
    Profile,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Dir,
        Self::NoDir,
        Self::OptLevel,
        Self::InlineThreshold,
        Self::OptIterations,
        Self::LogLevel,
        Self::Allocator,
        Self::Profile,
        Self::Help,
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
            Self::OptLevel => args::OPT_LEVEL_SPEC,
            Self::InlineThreshold => args::INLINE_THRESHOLD_SPEC,
            Self::OptIterations => args::OPT_ITERATIONS_SPEC,
            Self::LogLevel => args::LOG_LEVEL_SPEC,
            Self::Allocator => args::ALLOCATOR_SPEC,
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
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
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

pub fn print_usage() {
    eprint!("{}", format_usage());
}

/// Parse `--profile`. Shared by `run` and `serve`.
pub fn parse_profile(s: &str) -> Result<ProfileMode, CliExit> {
    if s == "jitdump" {
        return Ok(ProfileMode::JitDump);
    }
    if s == "perfmap" {
        return Ok(ProfileMode::PerfMap);
    }
    if s == "guest" {
        return Ok(ProfileMode::Guest {
            path: "profile.json".to_owned(),
            interval_ms: 10,
        });
    }
    if let Some(rest) = s.strip_prefix("guest,") {
        let parts: Vec<&str> = rest.splitn(2, ',').collect();
        let path = if parts[0].is_empty() {
            "profile.json".to_owned()
        } else {
            parts[0].to_owned()
        };
        let interval_ms = if parts.len() > 1 {
            parts[1]
                .parse::<u64>()
                .map_err(|_| CliExit::error(format!("invalid profiling interval '{}'", parts[1])))?
        } else {
            10
        };
        return Ok(ProfileMode::Guest { path, interval_ms });
    }
    Err(CliExit::error(format!(
        "unknown profile mode '{s}'. Use guest, jitdump, or perfmap"
    )))
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<RunOptions, CliExit> {
    let usage = format_usage();
    let mut input: Option<String> = None;
    let mut opt_level = OptLevel::default();
    let mut log_level = LogLevel::default();
    let mut profile = ProfileMode::None;
    let mut preopened_dirs: Vec<(String, String)> = Vec::new();
    let mut program_args: Vec<String> = Vec::new();
    let mut explicit_dirs = false;
    let mut no_dir = false;
    let mut inline_threshold: Option<usize> = None;
    let mut opt_iterations: Option<u32> = None;
    let mut allocator: Option<String> = None;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Dir => {
                    preopened_dirs.push(args::parse_dir_arg(&mut parser)?);
                    explicit_dirs = true;
                }
                Opt::NoDir => no_dir = true,
                Opt::OptLevel => opt_level = compile::parse_opt_level_arg(&mut parser)?,
                Opt::InlineThreshold => {
                    inline_threshold = Some(args::parse_inline_threshold_arg(
                        "--optimize-inline-threshold",
                        &mut parser,
                    )?);
                }
                Opt::OptIterations => {
                    opt_iterations = Some(args::parse_opt_iterations_arg(
                        "--optimize-iterations",
                        &mut parser,
                    )?);
                }
                Opt::LogLevel => log_level = args::parse_log_level_arg(&mut parser)?,
                Opt::Allocator => allocator = Some(args::require_string(&mut parser)?),
                Opt::Profile => {
                    let spec = args::require_string(&mut parser)?;
                    profile = parse_profile(&spec)?;
                }
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            input = Some(val.to_string_lossy().into_owned());
            // Everything after the input file (flags included) is forwarded to the guest.
            if let Some(raw) = parser.try_raw_args() {
                for raw_arg in raw {
                    program_args.push(raw_arg.to_string_lossy().into_owned());
                }
            }
            break;
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    // Default: preopen the current directory unless --dir or --no-dir was given.
    if !explicit_dirs && !no_dir {
        preopened_dirs.push((".".to_owned(), ".".to_owned()));
    }

    Ok(RunOptions {
        input: manifest::resolve_input(input, manifest::EntryPointKind::Command, &usage)?,
        opt_level,
        log_level,
        profile,
        preopened_dirs,
        program_args,
        inline_threshold,
        opt_iterations,
        allocator,
    })
}

async fn run_cli_component(
    wasm: &[u8],
    cranelift_opt: wasmtime::OptLevel,
    profile: &ProfileMode,
    preopened_dirs: &[(String, String)],
    program_args: &[String],
) -> Result<()> {
    let engine = runtime::create_engine(cranelift_opt, profile)?;
    let component = Component::new(&engine, wasm)?;
    let linker = runtime::create_linker(&engine)?;
    let mut store = runtime::create_store(&engine, preopened_dirs, program_args)?;

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

        let profiler_for_cb = profiler.clone();
        store.epoch_deadline_callback(move |store_ctx| {
            if let Some(ref mut p) = *profiler_for_cb.lock().unwrap() {
                p.sample(&store_ctx, interval);
            }
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let engine_clone = engine.clone();
        std::thread::spawn(move || {
            while !stop_clone.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                engine_clone.increment_epoch();
            }
        });

        Some((profiler, stop))
    } else {
        None
    };

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

    let (result,) = run_func.call_async(&mut store, ()).await?;

    if let Some((profiler_arc, stop)) = profiler {
        stop.store(true, Ordering::Relaxed);

        if let ProfileMode::Guest { path, .. } = profile {
            let guest_profiler = profiler_arc.lock().unwrap().take().unwrap();
            let file = std::fs::File::create(path)?;
            guest_profiler.finish(BufWriter::new(file))?;
            eprintln!("Profile written to {path}");
            eprintln!("View at https://profiler.firefox.com/");
        }
    }

    result.map_err(|()| anyhow::anyhow!("Component returned error"))?;

    Ok(())
}

pub async fn run(opts: RunOptions) -> Result<(), CliExit> {
    // Pass `target_world: None` so the compiler picks the cli/command default
    // (and bump allocator unless overridden).
    let flags = CompileFlags {
        opt_level: opts.opt_level,
        log_level: opts.log_level,
        target_world: None,
        skip_validation: false,
        inline_threshold: opts.inline_threshold,
        opt_iterations: opts.opt_iterations,
        allocator: opts.allocator,
    };
    let cranelift_opt = opts.opt_level.to_wasmtime();
    let wasm = compile::compile(&opts.input, &flags).await?;

    run_cli_component(
        &wasm,
        cranelift_opt,
        &opts.profile,
        &opts.preopened_dirs,
        &opts.program_args,
    )
    .await
    .map_err(|e| CliExit::error(format!("Runtime error: {e:?}")))
}
