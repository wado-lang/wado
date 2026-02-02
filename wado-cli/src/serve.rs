use std::process::{self, Command, Stdio};

use anyhow::Result;
use lexopt::Arg::{Long, Short, Value};
use tempfile::NamedTempFile;

use crate::args::{
    next_arg, reject_multiple_inputs, require_input, require_string, unexpected_arg,
};
use crate::compile::{self, OptLevel};
use wado_compiler::LogLevel;

pub struct ServeOptions {
    pub input: String,
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub addr: String,
}

pub fn print_usage() {
    eprintln!("Usage: wado serve [options] <file.wado>");
    eprintln!();
    eprintln!("Compile and serve a Wado HTTP service using wasmtime serve.");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --addr <addr>    Address to listen on (default: 0.0.0.0:8080)");
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

pub fn parse_args(mut parser: lexopt::Parser) -> ServeOptions {
    let mut input: Option<String> = None;
    let mut opt_level = OptLevel::default();
    let mut log_level = LogLevel::default();
    let mut addr = "0.0.0.0:8080".to_string();

    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Long("addr") => {
                addr = require_string(&mut parser);
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

    ServeOptions {
        input: require_input(input, print_usage),
        opt_level,
        log_level,
        addr,
    }
}

fn run_http_service(wasm: &[u8], addr: &str) -> Result<()> {
    use std::io::Write;

    // Write wasm to a temp file
    let mut temp_file = NamedTempFile::new()?;
    temp_file.write_all(wasm)?;
    temp_file.flush()?;

    let temp_path = temp_file.path();
    eprintln!("Starting HTTP server with wasmtime serve...");
    eprintln!("Listening on: http://{addr}/");

    // Run wasmtime serve with P3 support
    let status = Command::new("wasmtime")
        .arg("serve")
        .arg("--addr")
        .arg(addr)
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

pub async fn run(opts: ServeOptions) {
    let wasm = compile::compile_with_full_opts(
        &opts.input,
        opts.opt_level,
        opts.log_level,
        Some("wasi:http/service".to_string()),
    )
    .await;

    if let Err(e) = run_http_service(&wasm, &opts.addr) {
        eprintln!("Runtime error: {e}");
        process::exit(1);
    }
}
