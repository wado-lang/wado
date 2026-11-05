//! `wado run-webgpu`: compile a Wado program and run it on a `wasi:webgpu` host.
//! See `docs/wep-2026-09-19-wasi-webgpu.md`.

mod args;
mod host;

use std::process::ExitCode;

use args::AdapterSelector;
use wado_host::options::CliExit;
use wado_host::runner::{RunOptions, command_line, program_for};

#[tokio::main]
async fn main() -> ExitCode {
    command_line(
        "wado-run-webgpu",
        env!("CARGO_PKG_VERSION"),
        args::parse(std::env::args_os().skip(1)),
        || format!("{}\n{}", args::help(), host::adapter_list()),
        run,
    )
    .await
}

async fn run(options: RunOptions, gpu_adapter: Option<AdapterSelector>) -> Result<(), CliExit> {
    // The GPU first: a machine without one has nothing to run on, and saying so
    // before the compile beats saying it after.
    let gpu = host::gpu(gpu_adapter.as_ref())?;
    let program = program_for(&options)?;
    host::run(&program, &options, gpu).await
}
