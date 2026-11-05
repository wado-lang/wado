use std::fmt::Write as _;
use std::path::PathBuf;

use wado_host::guest::GuestState;
use wado_host::options::{CliExit, OptSpec};
use wado_host::runner::{self, CompileOpt, OwnOption, Parsed, RunEngine, run_options_help};

use crate::args::{self, require_string};
use crate::build::build_for_driver;
use crate::compile::CompileFlags;
use crate::knobs::{CompileKnobs, HostKnobs, RuntimeKnobs};
use crate::manifest;
use crate::runtime::{ProfileMode, WasiState};

pub struct RunOptions {
    pub input: String,
    pub knobs: CompileKnobs,
    pub host: HostKnobs,
    pub profile: ProfileMode,
    /// `(host_path, guest_path)` pairs from `--dir host[::guest]`.
    pub preopened_dirs: Vec<(String, String)>,
    /// The guest's arguments, which `get-arguments` answers after the program name.
    pub program_args: Vec<String>,
    pub runtime: RuntimeKnobs,
    /// `--emit-component`: where to write the component instead of running it.
    pub emit_component: Option<PathBuf>,
}

const EMIT_COMPONENT_SPEC: OptSpec = OptSpec {
    long: Some("emit-component"),
    short: None,
    value: Some("<path>"),
    desc: "Write the component to <path> and print the entry it was built from,\ninstead of running it. A `wado run-*` subcommand compiles this way",
};

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado run [options] <file.wado>").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Compile and run a Wado CLI program (wasi:cli/command world)."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", run_options_help(&[EMIT_COMPONENT_SPEC])).unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "By default, the current directory is preopened as '.' for WASI filesystem access."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Profiling examples:").unwrap();
    writeln!(buf, "  wado run --profile guest prog.wado").unwrap();
    writeln!(
        buf,
        "    => writes profile.json, view at https://profiler.firefox.com/"
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "  wado run --profile guest,output.json,5 prog.wado").unwrap();
    writeln!(buf, "    => custom output path and 5ms sampling interval").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "  perf record -k mono wado run --profile jitdump prog.wado"
    )
    .unwrap();
    writeln!(
        buf,
        "  perf inject --jit --input perf.data --output perf.jit.data"
    )
    .unwrap();
    writeln!(buf, "  perf report --input perf.jit.data").unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<RunOptions, CliExit> {
    let usage = format_usage();
    let mut knobs = CompileKnobs::compile_and_run();
    let mut host = HostKnobs::default();
    let mut emit_component = None;

    let parsed = runner::parse_run_command(
        &mut parser,
        &usage,
        |opt, parser| match opt {
            CompileOpt::Knob(knob) => knobs.apply(knob, parser),
            CompileOpt::Param(param) => args::apply_param_opt(&mut knobs.params, param, parser),
            CompileOpt::Host(knob) => {
                host.apply(knob);
                Ok(())
            }
        },
        |name, parser| {
            if EMIT_COMPONENT_SPEC.long == Some(name) {
                emit_component = Some(PathBuf::from(require_string(parser)?));
                return Ok(OwnOption::Taken);
            }
            Ok(OwnOption::Unknown)
        },
    )?;
    let Parsed::Run(command) = parsed else {
        return Err(CliExit::help(usage));
    };

    Ok(RunOptions {
        input: manifest::resolve_input(command.input, manifest::EntryPointKind::Command, &usage)?,
        knobs,
        host,
        profile: command.profile,
        preopened_dirs: command.preopened_dirs,
        program_args: command.program_args,
        runtime: command.runtime,
        emit_component,
    })
}

pub async fn run(opts: RunOptions) -> Result<(), CliExit> {
    let opt_level = opts.knobs.opt_level.to_wasmtime();
    // Leave `target_world` unset so the compiler picks the cli/command default.
    let flags = CompileFlags {
        knobs: opts.knobs,
        ..CompileFlags::default()
    };
    // `run` is a driver on the build tier (like `cargo run`): in a project it
    // builds the cli/command world through the shared build core (metadata
    // embedded, written to build/), then executes it; a bare file with no
    // project stays on the in-memory compile primitive.
    let wasm = Box::pin(build_for_driver(&opts.input, "wasi:cli/command", &flags)).await?;
    if let Some(path) = opts.emit_component {
        std::fs::write(&path, &wasm)
            .map_err(|e| CliExit::error(format!("{}: {e}", path.display())))?;
        println!("{}", opts.input);
        return Ok(());
    }

    let argv: Vec<String> = std::iter::once(opts.input)
        .chain(opts.program_args)
        .collect();
    let guest = GuestState::new(&opts.preopened_dirs, &argv, opts.runtime.process.clone())
        .map_err(|e| CliExit::error(format!("Runtime error: {e:?}")))?;
    let engine = RunEngine {
        opt_level,
        profile: &opts.profile,
        knobs: &opts.runtime,
    };
    runner::run("wado", &wasm, &engine, WasiState::from(guest), |_, _, _| {
        Ok(())
    })
    .await
}
