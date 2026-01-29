use std::process::{self, Command, Stdio};

use anyhow::Result;
use lexopt::Arg::{Long, Value};
use tempfile::NamedTempFile;
use wasmtime::component::Component;

use crate::args::{next_arg, reject_multiple_inputs, require_input, require_string, unexpected_arg};
use crate::compile::{self, OptLevel};
use crate::runtime;
use wado_compiler::LogLevel;

pub struct RunOptions {
    pub input: String,
    /// Target world for the component (e.g., "wasi:http/service")
    /// Defaults to "wasi:cli/command" if not specified
    pub world: Option<String>,
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
}

pub fn print_usage() {
    eprintln!("Usage: wado run [options] <file.wado>");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --world <world>  Target world (default: wasi:cli/command)");
    eprintln!("                   Examples: wasi:http/service, wasi:http/middleware");
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
    use lexopt::Arg::Short;

    let mut input: Option<String> = None;
    let mut world: Option<String> = None;
    let mut opt_level = OptLevel::default();
    let mut log_level = LogLevel::default();

    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Long("world") => {
                world = Some(require_string(&mut parser));
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
        world,
        opt_level,
        log_level,
    }
}

async fn run_cli_component(wasm: &[u8]) -> Result<()> {
    let engine = runtime::create_engine()?;
    let component = Component::new(&engine, wasm)?;
    let linker = runtime::create_linker(&engine)?;
    let mut store = runtime::create_store(&engine);

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run_func = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;

    let (result,) = run_func.call_async(&mut store, ()).await?;
    result.map_err(|()| anyhow::anyhow!("Component returned error"))?;

    Ok(())
}

fn run_http_service(wasm: &[u8]) -> Result<()> {
    use std::io::Write;

    // Write wasm to a temp file
    let mut temp_file = NamedTempFile::new()?;
    temp_file.write_all(wasm)?;
    temp_file.flush()?;

    let temp_path = temp_file.path();
    eprintln!("Starting HTTP server with wasmtime serve...");
    eprintln!("Test with: curl http://localhost:8080/");

    // Run wasmtime serve with P3 support
    let status = Command::new("wasmtime")
        .arg("serve")
        .arg("-W")
        .arg("all-proposals=y")
        .arg("-W")
        .arg("stack-switching=n")
        .arg("-S")
        .arg("p3=y")
        .arg("-S")
        .arg("http=y")
        .arg(temp_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        anyhow::bail!("wasmtime serve exited with status: {status}");
    }

    Ok(())
}

pub async fn run(opts: RunOptions) {
    let wasm = compile::compile_with_full_opts(
        &opts.input,
        opts.opt_level,
        opts.log_level,
        opts.world.clone(),
    )
    .await;

    // Determine which runtime to use based on target world
    let is_http_service = opts
        .world
        .as_ref()
        .is_some_and(|w| w == "wasi:http/service" || w == "wasi:http/middleware");

    let result = if is_http_service {
        run_http_service(&wasm)
    } else {
        run_cli_component(&wasm).await
    };

    if let Err(e) = result {
        eprintln!("Runtime error: {e}");
        process::exit(1);
    }
}
