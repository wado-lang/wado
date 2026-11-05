//! `wado run-webgpu`: compile a Wado program and run it on a `wasi:webgpu` host.
//! See `docs/wep-2026-09-19-wasi-webgpu.md`.

mod args;
mod host;

use std::process::ExitCode;

use args::AdapterSelector;
use wado_host::runner::{Invocation, RunOptions, program_for};

#[tokio::main]
async fn main() -> ExitCode {
    let (options, gpu_adapter) = match args::parse(std::env::args_os().skip(1)) {
        Ok((Invocation::Run(options), gpu_adapter)) => (options, gpu_adapter),
        Ok((Invocation::Help, _)) => {
            print!("{}\n{}", args::help(), host::adapter_list());
            return ExitCode::SUCCESS;
        }
        Ok((Invocation::Version, _)) => {
            println!("wado-run-webgpu {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("wado-run-webgpu: {error}");
            return ExitCode::FAILURE;
        }
    };

    match run(options, gpu_adapter).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wado-run-webgpu: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(options: RunOptions, gpu_adapter: Option<AdapterSelector>) -> anyhow::Result<()> {
    // The GPU first: a machine without one has nothing to run on, and saying so
    // before the compile beats saying it after.
    let gpu = host::gpu(gpu_adapter.as_ref())?;
    let program = program_for(&options)?;
    host::run(program.path(), &options, gpu).await
}
