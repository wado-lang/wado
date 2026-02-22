use std::io::BufWriter;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use lexopt::Arg::{Long, Short, Value};
use wasmtime::component::Component;
use wasmtime::{GuestProfiler, UpdateDeadline};

use crate::args::{next_arg, require_input, require_string, unexpected_arg};
use crate::compile::{self, OptLevel};
use crate::runtime::{self, ProfileMode};
use wado_compiler::LogLevel;

pub struct RunOptions {
    pub input: String,
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub profile: ProfileMode,
    /// Preopened directories as `(host_path, guest_path)` pairs.
    /// Populated by `--dir host_path[::guest_path]`.
    pub preopened_dirs: Vec<(String, String)>,
    /// Arguments passed to the guest program via `wasi:cli/environment.get-arguments`.
    pub program_args: Vec<String>,
}

pub fn print_usage() {
    eprintln!("Usage: wado run [options] <file.wado>");
    eprintln!();
    eprintln!("Compile and run a Wado CLI program (wasi:cli/command world).");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --dir <path>       Preopen directory for WASI filesystem access.");
    eprintln!("                     Use --dir host::guest to specify different guest path.");
    eprintln!("                     Overrides the default of preopening the current directory.");
    eprintln!("  --no-dir           Do not preopen any directories (disables the default).");
    eprintln!("  -O<n>              Optimization level: -O0, -O1, -O2, -O3, -Os");
    eprintln!("  --log-level <l>    Log level: debug, info, warn, error, off (default: info)");
    eprintln!("  --profile <mode>   Enable profiling:");
    eprintln!("                       guest[,path[,interval_ms]]  Cross-platform guest profiling");
    eprintln!("                                                   (default: profile.json, 10ms)");
    eprintln!("                       jitdump   Linux perf jitdump (use with perf record -k mono)");
    eprintln!("                       perfmap   Linux perf map (use with perf record -k mono)");
    eprintln!("  --help             Show this help message");
    eprintln!();
    eprintln!("By default, the current directory is preopened as '.' for WASI filesystem access.");
    eprintln!();
    eprintln!("Profiling examples:");
    eprintln!("  wado run --profile guest prog.wado");
    eprintln!("    => writes profile.json, view at https://profiler.firefox.com/");
    eprintln!();
    eprintln!("  wado run --profile guest,output.json,5 prog.wado");
    eprintln!("    => custom output path and 5ms sampling interval");
    eprintln!();
    eprintln!("  perf record -k mono wado run --profile jitdump prog.wado");
    eprintln!("  perf inject --jit --input perf.data --output perf.jit.data");
    eprintln!("  perf report --input perf.jit.data");
}

fn parse_log_level(s: &str) -> Option<LogLevel> {
    match s.to_lowercase().as_str() {
        "debug" => Some(LogLevel::Debug),
        "info" => Some(LogLevel::Info),
        "warn" | "warning" => Some(LogLevel::Warn),
        "error" => Some(LogLevel::Error),
        "off" | "none" => Some(LogLevel::Off),
        _ => None,
    }
}

fn parse_profile(s: &str) -> ProfileMode {
    if s == "jitdump" {
        return ProfileMode::JitDump;
    }
    if s == "perfmap" {
        return ProfileMode::PerfMap;
    }
    if s == "guest" {
        return ProfileMode::Guest {
            path: "profile.json".to_owned(),
            interval_ms: 10,
        };
    }
    if let Some(rest) = s.strip_prefix("guest,") {
        let parts: Vec<&str> = rest.splitn(2, ',').collect();
        let path = if parts[0].is_empty() {
            "profile.json".to_owned()
        } else {
            parts[0].to_owned()
        };
        let interval_ms = if parts.len() > 1 {
            parts[1].parse::<u64>().unwrap_or_else(|_| {
                eprintln!("Error: invalid profiling interval '{}'", parts[1]);
                process::exit(1);
            })
        } else {
            10
        };
        return ProfileMode::Guest { path, interval_ms };
    }
    eprintln!("Error: unknown profile mode '{s}'. Use guest, jitdump, or perfmap");
    process::exit(1);
}

pub fn parse_args(mut parser: lexopt::Parser) -> RunOptions {
    let mut input: Option<String> = None;
    let mut opt_level = OptLevel::default();
    let mut log_level = LogLevel::default();
    let mut profile = ProfileMode::None;
    let mut preopened_dirs: Vec<(String, String)> = Vec::new();
    let mut program_args: Vec<String> = Vec::new();
    let mut explicit_dirs = false;
    let mut no_dir = false;

    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Long("dir") => {
                let dir_spec = require_string(&mut parser);
                // Support "host::guest" or just "host" (guest defaults to host path).
                let (host, guest) = if let Some((h, g)) = dir_spec.split_once("::") {
                    (h.to_owned(), g.to_owned())
                } else {
                    (dir_spec.clone(), dir_spec)
                };
                preopened_dirs.push((host, guest));
                explicit_dirs = true;
            }
            Long("no-dir") => {
                no_dir = true;
            }
            Long("profile") => {
                let spec = require_string(&mut parser);
                profile = parse_profile(&spec);
            }
            Short('O') => {
                let val = parser.optional_value();
                let level_str = val
                    .as_ref()
                    .map(|v| v.to_string_lossy())
                    .unwrap_or_default();
                opt_level = match level_str.as_ref() {
                    "" | "0" | "g" => OptLevel::O0,
                    "1" => OptLevel::O1,
                    "2" => OptLevel::O2,
                    "3" => OptLevel::O3,
                    "s" => OptLevel::Os,
                    _ => {
                        eprintln!(
                            "Error: unknown optimization level '-O{level_str}'. Use -O0, -O1, -O2, -O3, -Os, or -Og"
                        );
                        process::exit(1);
                    }
                };
            }
            Long("log-level") => {
                let level_str = require_string(&mut parser);
                if let Some(level) = parse_log_level(&level_str) {
                    log_level = level;
                } else {
                    eprintln!(
                        "Error: unknown log level '{level_str}'. Use debug, info, warn, error, or off"
                    );
                    process::exit(1);
                }
            }
            Value(val) => {
                let s = val.to_string_lossy().into_owned();
                if input.is_none() {
                    input = Some(s);
                } else {
                    // Arguments after the source file are passed to the program.
                    program_args.push(s);
                }
            }
            _ => unexpected_arg(arg, print_usage),
        }
    }

    // Default: preopen the current directory unless --dir or --no-dir was given.
    if !explicit_dirs && !no_dir {
        preopened_dirs.push((".".to_owned(), ".".to_owned()));
    }

    RunOptions {
        input: require_input(input, print_usage),
        opt_level,
        log_level,
        profile,
        preopened_dirs,
        program_args,
    }
}

async fn run_cli_component(
    wasm: &[u8],
    profile: &ProfileMode,
    preopened_dirs: &[(String, String)],
    program_args: &[String],
) -> Result<()> {
    let engine = runtime::create_engine(wasmtime::OptLevel::Speed, profile)?;
    let component = Component::new(&engine, wasm)?;
    let linker = runtime::create_linker(&engine)?;
    let mut store = runtime::create_store(&engine, preopened_dirs, program_args)?;

    // Set up guest profiler if requested.
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

        // Register epoch deadline callback for sampling.
        let profiler_for_cb = profiler.clone();
        store.epoch_deadline_callback(move |store_ctx| {
            if let Some(ref mut p) = *profiler_for_cb.lock().unwrap() {
                p.sample(&store_ctx, interval);
            }
            Ok(UpdateDeadline::Continue(1))
        });
        store.set_epoch_deadline(1);

        // Start epoch-bumping thread.
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

    // Finish guest profiling.
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

pub async fn run(opts: RunOptions) {
    let wasm = compile::compile_with_opts(&opts.input, opts.opt_level, opts.log_level).await;

    if let Err(e) =
        run_cli_component(&wasm, &opts.profile, &opts.preopened_dirs, &opts.program_args).await
    {
        eprintln!("Runtime error: {e}");
        process::exit(1);
    }
}
