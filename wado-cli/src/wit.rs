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
use crate::compile::{attach_manifest_and_component_deps, load_nearest_manifest};
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
    let scope = opts.scope;
    let (sem, world_imports) = if opts.lib {
        lib_semantics_and_opts(opts.input, &usage).await?
    } else {
        world_semantics_and_opts(opts.input, opts.world, &usage).await?
    };

    let text = wit_emit::emit_wit_text(&sem, &WitEmitOptions { scope }, &world_imports)
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
    world: Option<String>,
    usage: &str,
) -> Result<(Semantics, Vec<String>), CliExit> {
    let input = manifest::resolve_input(input, EntryPointKind::Command, usage)?;
    let path = Path::new(&input);
    let source = fs::read_to_string(path)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", path.display())))?;
    let base_path = path.parent().map(Path::to_path_buf).unwrap_or_default();

    // Mirror the compile host: manifest dependencies (offline, analysis tier)
    // and the kiln pipeline's generator redirects, so a `with { generator }`
    // consumer analyzes here as it compiles (issue #1646).
    let manifest_pair = load_nearest_manifest(path);
    let host = attach_manifest_and_component_deps(
        FilesystemCompilerHost::new(base_path.clone()),
        manifest_pair.as_ref(),
        &base_path,
        &source,
        false,
    )
    .await
    .map_err(CliExit::error)?;
    let import_host = attach_manifest_and_component_deps(
        FilesystemCompilerHost::silent(base_path.clone()),
        manifest_pair.as_ref(),
        &base_path,
        &source,
        false,
    )
    .await
    .map_err(CliExit::error)?;
    // Run the generators on the silent host: their warnings were not asked for
    // by `wado wit`, and a build failure still surfaces via `run_generators`'
    // error. The analysis below keeps the loud host so it reports diagnostics.
    let invocations = run_generators(path, &import_host, manifest_pair).await?;

    let mut sem = wado_compiler::semantics_for_world(
        &source,
        &host,
        Some(&input),
        world.as_deref(),
        invocations.clone(),
    )
    .await;
    if !sem.is_complete() {
        return Err(CliExit::silent_failure(1));
    }

    let world = world.unwrap_or_else(|| DEFAULT_WORLD.to_string());
    sem.set_wit_contract(wado_compiler::wit_emit::wit_contract(
        Some(&world),
        None,
        Some(&default_interface_name(&input)),
    ));

    let world_imports =
        resolve_world_imports(&import_host, &source, &input, &world, invocations).await;
    Ok((sem, world_imports))
}

/// Run the entry's kiln generators (cache-aware, so a prior build's outputs are
/// reused) and return the redirect index the analysis needs.
async fn run_generators(
    entry_file: &Path,
    host: &FilesystemCompilerHost,
    manifest_pair: Option<manifest::ProjectManifest>,
) -> Result<wado_compiler::kiln::InvocationIndex, CliExit> {
    crate::compile::maybe_run_pipeline(entry_file, host, false, manifest_pair)
        .await
        .map(|outcome| outcome.invocations)
        .map_err(|e| CliExit::error(format!("wado wit: running kiln generators: {e}")))
}

/// Analyze the `[package].lib` entry and build the emit options for the library
/// world — emitted as the anonymous `root`, exporting the default interface
/// `namespace:name/name` (see `manifest::LIB_WORLD_NAME`).
async fn lib_semantics_and_opts(
    input: Option<String>,
    usage: &str,
) -> Result<(Semantics, Vec<String>), CliExit> {
    let (project, target) = manifest::resolve_lib_project(input, usage)?;

    let entry_str = target.entry.to_string_lossy().into_owned();
    let source = fs::read_to_string(&target.entry)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", target.entry.display())))?;
    let base_path = target
        .entry
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();

    let host = attach_manifest_and_component_deps(
        FilesystemCompilerHost::new(base_path.clone()),
        Some(&project),
        &base_path,
        &source,
        false,
    )
    .await
    .map_err(CliExit::error)?;
    let import_host = attach_manifest_and_component_deps(
        FilesystemCompilerHost::silent(base_path.clone()),
        Some(&project),
        &base_path,
        &source,
        false,
    )
    .await
    .map_err(CliExit::error)?;
    // Generators run on the silent host (see the world path); the loud host
    // below still reports analysis diagnostics.
    let invocations = run_generators(&target.entry, &import_host, Some(project)).await?;

    let mut sem = wado_compiler::semantics_for_world(
        &source,
        &host,
        Some(&entry_str),
        None,
        invocations.clone(),
    )
    .await;
    if !sem.is_complete() {
        return Err(CliExit::silent_failure(1));
    }
    sem.set_wit_contract(wado_compiler::wit_emit::wit_contract(
        None,
        Some(&target.interface_fq),
        None,
    ));

    let world_imports = resolve_lib_world_imports(
        &import_host,
        &source,
        &entry_str,
        &target.interface_fq,
        invocations,
    )
    .await;
    Ok((sem, world_imports))
}

/// The faithful import set from a compiled WIR plan, empty when absent.
fn wir_imports(wir_package: Option<wado_compiler::wir::WirPackage>) -> Vec<String> {
    wir_package
        .map(|pkg| pkg.imported_cm_interfaces)
        .unwrap_or_default()
}

/// The imports the WIT should list, computed post-DCE. `Err` after a passing
/// `semantics` is anomalous (a later-pass failure), so it warns rather than
/// silently emitting import-less WIT.
fn imports_or_warn<T, E>(result: Result<T, E>, wir: impl FnOnce(T) -> Vec<String>) -> Vec<String> {
    let Ok(r) = result else {
        eprintln!("warning: could not resolve world imports; WIT may omit import lines");
        return Vec::new();
    };
    wir(r)
}

/// Analyze `source` against `world` (on the passed silent host, so diagnostics
/// are not re-emitted) and read the faithful import set from the WIR plan.
async fn resolve_world_imports(
    host: &FilesystemCompilerHost,
    source: &str,
    input: &str,
    world: &str,
    invocations: wado_compiler::kiln::InvocationIndex,
) -> Vec<String> {
    let result = wado_compiler::dump_with_host_and_world(
        source,
        host,
        Some(input),
        wado_compiler::OptLevel::O2,
        Some(world),
        None,
        None,
        None,
        &[],
        &wado_compiler::hashmap::IndexMap::default(),
        wado_compiler::param_resolution::ParamPolicy::default(),
        invocations,
    )
    .await;
    imports_or_warn(result, |r| wir_imports(r.wir_package))
}

/// Like [`resolve_world_imports`] but for the synthesized library world. Uses
/// the same `compile_with_options` path (and allocator) as `wado compile --lib`
/// so the import set matches what that build embeds. `lib_world` is the
/// default-interface FQ the codegen keys on (`namespace:name/name`).
async fn resolve_lib_world_imports(
    host: &FilesystemCompilerHost,
    source: &str,
    input: &str,
    lib_world: &str,
    invocations: wado_compiler::kiln::InvocationIndex,
) -> Vec<String> {
    let options = wado_compiler::CompilerOptions {
        opt_level: wado_compiler::OptLevel::O2,
        lib_world: Some(lib_world.to_string()),
        allocator: Some("freelist".to_string()),
        retain_wir: true,
        invocations,
        ..Default::default()
    };
    let result = wado_compiler::compile_with_options(source, host, Some(input), options).await;
    imports_or_warn(result, |r| wir_imports(r.wir_package))
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
