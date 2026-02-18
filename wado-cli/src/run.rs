use std::process;

use anyhow::Result;
use lexopt::Arg::{Long, Short, Value};
use wasmtime::component::Component;

use crate::args::{
    next_arg, reject_multiple_inputs, require_input, require_string, unexpected_arg,
};
use crate::compile::{self, OptLevel};
use crate::runtime;
use wado_compiler::LogLevel;

pub struct RunOptions {
    pub input: String,
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
}

pub fn print_usage() {
    eprintln!("Usage: wado run [options] <file.wado>");
    eprintln!();
    eprintln!("Compile and run a Wado CLI program (wasi:cli/command world).");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -O<n>            Optimization level: -O0, -O1, -O2, -O3, -Os");
    eprintln!("  --log-level <l>  Log level: debug, info, warn, error, off (default: info)");
    eprintln!("  --help           Show this help message");
}

/// Parse log level from string
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

pub fn parse_args(mut parser: lexopt::Parser) -> RunOptions {
    let mut input: Option<String> = None;
    let mut opt_level = OptLevel::default();
    let mut log_level = LogLevel::default();

    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
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
                reject_multiple_inputs(&input);
                input = Some(val.to_string_lossy().into_owned());
            }
            _ => unexpected_arg(arg, print_usage),
        }
    }

    RunOptions {
        input: require_input(input, print_usage),
        opt_level,
        log_level,
    }
}

async fn run_cli_component(wasm: &[u8]) -> Result<()> {
    let engine = runtime::create_engine(wasmtime::OptLevel::Speed)?;
    let component = Component::new(&engine, wasm)?;
    let linker = runtime::create_linker(&engine)?;
    let mut store = runtime::create_store(&engine);

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

    let (result,) = run_func.call_async(&mut store, ()).await?;
    result.map_err(|()| anyhow::anyhow!("Component returned error"))?;

    Ok(())
}

pub async fn run(opts: RunOptions) {
    let wasm = compile::compile_with_opts(&opts.input, opts.opt_level, opts.log_level).await;

    if let Err(e) = run_cli_component(&wasm).await {
        eprintln!("Runtime error: {e}");
        process::exit(1);
    }
}
