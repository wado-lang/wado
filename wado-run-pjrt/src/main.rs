//! `wado run-pjrt`: compile a Wado program and run it with a PJRT plugin's
//! device, the only public way to reach a TPU. See `package-loam/devices.md`.

mod ffi;
mod host;
mod plugin;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use wado_host::options::{CliExit, OptSpec, require_value};
use wado_host::pinned_download::Pinned;
use wado_host::runner::{Invocation, RunOptions, command_line, program_for, usage};

/// The variable that names the plugin `--plugin` does not.
const PLUGIN_VARIABLE: &str = "WADO_PJRT_PLUGIN";

const PLUGIN_SPEC: OptSpec = OptSpec {
    long: Some("plugin"),
    short: None,
    value: Some("<path>"),
    desc: "The PJRT plugin to load (default: $WADO_PJRT_PLUGIN,\nelse XLA's CPU plugin, fetched once)",
};

fn help() -> String {
    usage(
        "run-pjrt",
        "Run a Wado program on a PJRT plugin's device.",
        &[PLUGIN_SPEC],
    )
}

/// What the command line asks for, and the plugin it names.
fn parse<I>(argv: I) -> Result<(Invocation, Option<PathBuf>), CliExit>
where
    I: IntoIterator<Item = OsString>,
{
    let mut plugin = None;
    let invocation = RunOptions::parse(argv, &help(), |name, parser| {
        if PLUGIN_SPEC.long != Some(name) {
            return Ok(false);
        }
        plugin = Some(PathBuf::from(require_value(parser)?));
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

/// XLA's CPU plugin, the default where one is pinned for the platform.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const CPU_PLUGIN: Option<Pinned> = Some(Pinned {
    name: "XLA's CPU PJRT plugin",
    url: "https://github.com/zml/pjrt-artifacts/releases/download/v18.0.0/pjrt-cpu_linux-amd64.tar.gz",
    archive: wado_host::pinned_download::Archive::TarGz {
        sha256: "6fbaa0cd1ff4d84fa7000ce5acc00e817bdf74c85cef1cc42c7cccc376c56431",
    },
    members: &[wado_host::pinned_download::Member {
        path: "libpjrt_cpu.so",
        sha256: "edc5ecc1b13eceeba4b0c312a05efbfdd9bd0732040990024beacb64cc9c4569",
    }],
});
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
const CPU_PLUGIN: Option<Pinned> = None;

/// The plugin `--plugin` or the variable names, else the pinned CPU plugin.
async fn plugin_path(named: Option<PathBuf>) -> Result<PathBuf, CliExit> {
    if let Some(path) = named.or_else(|| std::env::var_os(PLUGIN_VARIABLE).map(PathBuf::from)) {
        return Ok(path);
    }
    let Some(pinned) = CPU_PLUGIN else {
        return Err(CliExit::error(format!(
            "no PJRT plugin is pinned for this platform: pass --plugin or set {PLUGIN_VARIABLE}"
        )));
    };
    let dir = pinned.dir().await.map_err(|e| {
        CliExit::error(format!(
            "{e:#}\nPass --plugin or set {PLUGIN_VARIABLE} to use a plugin at hand"
        ))
    })?;
    Ok(dir.join(pinned.members[0].file_name()))
}

async fn run(options: RunOptions, plugin: Option<PathBuf>) -> Result<(), CliExit> {
    let plugin = plugin_path(plugin).await?;
    // The plugin first: a missing one is better said before the compile.
    let plugin = plugin::Plugin::open(&plugin)?;
    let program = program_for(&options)?;
    host::run(&program, &options, plugin).await
}
