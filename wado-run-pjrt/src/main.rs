//! `wado run-pjrt`: compile a Wado program and run it with a PJRT plugin's
//! device, the only public way to reach a TPU. See `package-loam/devices.md`.

mod ffi;
mod host;
mod plugin;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use wado_host::runner::{Invocation, RunOptions, command_line, program_for, usage};

/// The variable that names the plugin `--plugin` does not.
const PLUGIN_VARIABLE: &str = "WADO_PJRT_PLUGIN";

fn help() -> String {
    usage(
        "run-pjrt",
        "Run a Wado program on a PJRT plugin's device.",
        "      \
      --plugin <path>    The PJRT plugin to load (default: $WADO_PJRT_PLUGIN)
",
    )
}

/// What the command line asks for, and the plugin it names.
fn parse<I>(argv: I) -> Result<(Invocation, Option<PathBuf>)>
where
    I: IntoIterator<Item = OsString>,
{
    let mut plugin = None;
    let invocation = RunOptions::parse(argv, &help(), |name, parser| {
        if name != "plugin" {
            return Ok(false);
        }
        plugin = Some(PathBuf::from(parser.value()?));
        Ok(true)
    })?;
    Ok((invocation, plugin))
}

#[tokio::main]
async fn main() -> ExitCode {
    command_line(
        "wado-run-pjrt",
        env!("CARGO_PKG_VERSION"),
        parse(std::env::args_os().skip(1)),
        help,
        run,
    )
    .await
}

async fn run(options: RunOptions, plugin: Option<PathBuf>) -> Result<()> {
    let Some(plugin) = plugin.or_else(|| std::env::var_os(PLUGIN_VARIABLE).map(PathBuf::from))
    else {
        bail!("no PJRT plugin: pass --plugin or set {PLUGIN_VARIABLE}");
    };
    // The plugin first: a missing one is better said before the compile.
    let client = plugin::Client::open(&plugin)?;
    let program = program_for(&options)?;
    host::run(program.path(), &options, client).await
}
