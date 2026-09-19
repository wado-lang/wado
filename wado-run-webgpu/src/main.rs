//! `wado run-webgpu`: compile a Wado program and run it on a `wasi:webgpu` host.
//! See `docs/wep-2026-09-19-wasi-webgpu.md`.

mod args;
mod compile;
mod host;

use std::process::ExitCode;

use args::Args;

#[tokio::main]
async fn main() -> ExitCode {
    let args = match Args::parse(std::env::args_os().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => return ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wado-run-webgpu: {error}");
            return ExitCode::FAILURE;
        }
    };

    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("wado-run-webgpu: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> anyhow::Result<()> {
    // The GPU first: a machine without one has nothing to run on, and saying so
    // before the compile beats saying it after.
    let gpu = host::gpu()?;
    let component = compile::component_for(&args)?;
    host::run(component.path(), &args, gpu).await
}
