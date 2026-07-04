//! `wado wit` — emit the WIT text for a Wado program's component contract.
//!
//! Runs the frontend (`wado_compiler::semantics`) and stops; WIT is a
//! pre-codegen fact, so there is no optimization level. See WEP
//! `wep-2026-05-02-wit-interoperability.md`.

use std::fs;
use std::path::{Path, PathBuf};

use lexopt::Arg::Value;
use wado_compiler::semantics::Semantics;
use wado_compiler::wit_emit::{self, WitEmitOptions, WitScope};

use crate::args::{self, CliExit, OptSpec};
use crate::compiler_host::FilesystemCompilerHost;
use crate::manifest::{self, EntryPointKind};

const DEFAULT_WORLD: &str = "wasi:cli/command";

pub struct WitOptions {
    pub input: Option<String>,
    pub scope: WitScope,
    pub world: Option<String>,
    pub lib: bool,
    pub output: Option<String>,
}

#[derive(Clone, Copy)]
enum Opt {
    Scope,
    World,
    Lib,
    Output,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Scope,
        Self::World,
        Self::Lib,
        Self::Output,
        Self::Help,
    ];

    const fn spec(self) -> OptSpec {
        match self {
            Self::Scope => OptSpec {
                long: Some("scope"),
                short: None,
                value: Some("<full|local>"),
                desc: "Inlining scope for referenced interfaces (default: full)",
            },
            Self::World => args::WORLD_SPEC,
            Self::Lib => OptSpec {
                long: Some("lib"),
                short: None,
                value: None,
                desc: "Emit the library world (from [package].lib); excludes --world",
            },
            Self::Output => OptSpec {
                long: Some("output"),
                short: Some('o'),
                value: Some("<file>"),
                desc: "Write WIT to a file instead of stdout",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    use std::fmt::Write as _;
    let mut buf = String::new();
    writeln!(buf, "Usage: wado wit [options] [file.wado | dir]").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
}

pub fn print_usage() {
    eprint!("{}", format_usage());
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<WitOptions, CliExit> {
    let usage = format_usage();
    let mut inputs: Vec<String> = Vec::new();
    let mut scope = WitScope::Full;
    let mut world: Option<String> = None;
    let mut lib = false;
    let mut output: Option<String> = None;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Scope => {
                    let value = args::require_string(&mut parser)?;
                    scope = match value.as_str() {
                        "full" => WitScope::Full,
                        "local" => WitScope::Local,
                        other => {
                            return Err(CliExit::error(format!(
                                "unknown --scope value '{other}' (expected 'full' or 'local')"
                            )));
                        }
                    };
                }
                Opt::World => world = Some(args::require_string(&mut parser)?),
                Opt::Lib => lib = true,
                Opt::Output => output = Some(args::require_string(&mut parser)?),
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            inputs.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    if lib && world.is_some() {
        return Err(CliExit::error_with_usage(
            "`--lib` and `--world` are mutually exclusive",
            &usage,
        ));
    }
    if inputs.len() > 1 {
        return Err(CliExit::error_with_usage(
            "wado wit takes a single file or directory",
            &usage,
        ));
    }

    Ok(WitOptions {
        input: inputs.into_iter().next(),
        scope,
        world,
        lib,
        output,
    })
}

pub async fn run(opts: WitOptions) -> Result<(), CliExit> {
    let usage = format_usage();
    let (sem, emit_opts) = if opts.lib {
        lib_semantics_and_opts(opts.input, opts.scope, &usage).await?
    } else {
        world_semantics_and_opts(opts.input, opts.scope, opts.world, &usage).await?
    };

    let text = wit_emit::emit_wit_text(&sem, &emit_opts)
        .map_err(|e| CliExit::error(format!("wado wit: {e}")))?;

    match opts.output {
        Some(file) => {
            fs::write(&file, &text)
                .map_err(|e| CliExit::error(format!("writing '{file}': {e}")))?;
            eprintln!("Generated: {file}");
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// Analyze `input` against a fixed WASI world (or the default) and build the
/// emit options for it.
async fn world_semantics_and_opts(
    input: Option<String>,
    scope: WitScope,
    world: Option<String>,
    usage: &str,
) -> Result<(Semantics, WitEmitOptions), CliExit> {
    let input = manifest::resolve_input(input, EntryPointKind::Command, usage)?;
    let path = Path::new(&input);
    let source = fs::read_to_string(path)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", path.display())))?;
    let base_path = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let host = FilesystemCompilerHost::new(base_path);

    let sem =
        wado_compiler::semantics_for_world(&source, &host, Some(&input), world.as_deref()).await;
    if !sem.is_complete() {
        return Err(CliExit::silent_failure(1));
    }

    let world = world.unwrap_or_else(|| DEFAULT_WORLD.to_string());
    let world_imports = resolve_world_imports(&source, &input, &world).await;
    let emit_opts = WitEmitOptions {
        scope,
        world_fq: world,
        default_interface_name: default_interface_name(&input),
        world_imports,
    };
    Ok((sem, emit_opts))
}

/// Analyze the `[package].lib` entry and build the emit options for the library
/// world — emitted as the anonymous `root`, exporting the default interface
/// `namespace:name/name` (see `manifest::LIB_WORLD_NAME`).
async fn lib_semantics_and_opts(
    input: Option<String>,
    scope: WitScope,
    usage: &str,
) -> Result<(Semantics, WitEmitOptions), CliExit> {
    let (project, entry) = manifest::resolve_lib_project(input, usage)?;
    let pkg = project
        .manifest
        .package
        .as_ref()
        .expect("resolve_lib_project guarantees a [package]");
    let world_fq = manifest::lib_emit_world_fq(pkg)?;
    let interface_world = manifest::lib_world_fq(pkg)?;
    let default_interface = pkg.name.clone();

    let entry_str = entry.to_string_lossy().into_owned();
    let source = fs::read_to_string(&entry)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", entry.display())))?;
    let base_path = entry.parent().map(Path::to_path_buf).unwrap_or_default();
    let host = FilesystemCompilerHost::new(base_path);

    let sem = wado_compiler::semantics_for_world(&source, &host, Some(&entry_str), None).await;
    if !sem.is_complete() {
        return Err(CliExit::silent_failure(1));
    }

    let world_imports = resolve_lib_world_imports(&source, &entry_str, &interface_world).await;
    let emit_opts = WitEmitOptions {
        scope,
        world_fq,
        default_interface_name: default_interface,
        world_imports,
    };
    Ok((sem, emit_opts))
}

/// Compile `source` through optimize (on a silent host, so diagnostics are not
/// re-emitted) and read the faithful import set from the WIR-level plan
/// (`NirPackage::imported_cm_interfaces`). Returns empty on any failure; the
/// caller has already validated the program with `semantics`.
async fn resolve_world_imports(source: &str, input: &str, world: &str) -> Vec<String> {
    let base_path = Path::new(input)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemCompilerHost::silent(base_path);
    match wado_compiler::dump_with_host_and_world(
        source,
        &host,
        Some(input),
        wado_compiler::OptLevel::O2,
        Some(world),
        None,
        None,
        &[],
        &wado_compiler::hashmap::IndexMap::default(),
        wado_compiler::param_resolution::ParamPolicy::default(),
    )
    .await
    {
        Ok(result) => result
            .wir_package
            .map(|pkg| pkg.imported_cm_interfaces)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Compile the library entry with its synthesized world (via `lib_world`) and
/// read the faithful import set from the retained WIR plan. `lib_world` is the
/// default-interface FQ the codegen keys on (`namespace:name/name`), not the
/// emitted `root` world name. Returns empty on any failure; the caller has
/// already validated the program with `semantics`.
async fn resolve_lib_world_imports(source: &str, input: &str, lib_world: &str) -> Vec<String> {
    let base_path = Path::new(input)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemCompilerHost::silent(base_path);
    let options = wado_compiler::CompilerOptions {
        opt_level: wado_compiler::OptLevel::O2,
        lib_world: Some(lib_world.to_string()),
        retain_wir: true,
        log_level: Some(wado_compiler::LogLevel::Error),
        ..Default::default()
    };
    match wado_compiler::compile_with_options(source, &host, Some(input), options).await {
        Ok(result) => result
            .wir_package
            .map(|pkg| pkg.imported_cm_interfaces)
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// The default interface name: the manifest `[package].name` when the input
/// resolves through a project, otherwise the entry file stem.
pub(crate) fn default_interface_name(input: &str) -> String {
    if let Some(project) = crate::compile::load_nearest_manifest(Path::new(input))
        && let Some(package) = project.manifest.package
    {
        return package.name;
    }
    PathBuf::from(input)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_string())
}
