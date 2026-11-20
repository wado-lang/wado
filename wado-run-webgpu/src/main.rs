//! `wado run-webgpu`: compile a Wado program and run it on a `wasi:webgpu` host.
//! See `docs/wep-2026-09-19-wasi-webgpu.md`.

mod args;
mod host;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod software;

use std::process::ExitCode;

use args::AdapterSelector;
use wado_host::options::CliExit;
use wado_host::runner::{RunOptions, command_line, program_for};

fn main() -> ExitCode {
    let parsed = args::parse(std::env::args_os().skip(1));
    // `--help` lists the adapters a run gets, but fetches none to list.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if let Ok((invocation, _)) = &parsed {
        use software::Fetch;
        use wado_host::runner::Invocation;
        let fetch = match invocation {
            Invocation::Run(_) => Some(Fetch::Allowed),
            Invocation::Help => Some(Fetch::CachedOnly),
            Invocation::Version => None,
        };
        if let Some(fetch) = fetch
            && let Err(e) = software::provide_when_absent(fetch, host::has_adapter)
        {
            eprintln!("error: no GPU adapter here, and none could be fetched: {e:#}");
            return ExitCode::FAILURE;
        }
    }
    tokio::runtime::Runtime::new()
        .expect("the runtime starts")
        .block_on(command_line(
            "wado-run-webgpu",
            env!("CARGO_PKG_VERSION"),
            parsed,
            || format!("{}\n{}", args::help(), host::adapter_list()),
            run,
        ))
}

async fn run(options: RunOptions, gpu_adapter: Option<AdapterSelector>) -> Result<(), CliExit> {
    // The GPU first: a machine without one has nothing to run on, and saying so
    // before the compile beats saying it after.
    let gpu = host::gpu(gpu_adapter.as_ref())?;
    let program = program_for(&options)?;
    host::run(&program, &options, gpu).await
}
