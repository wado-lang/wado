//! `wado wit` — emit the WIT text for a Wado program's component contract.
//!
//! Renders WIT from the subset one compile retains
//! (`CompileResult::wit_emit_snapshot`) plus its import plan, so the text
//! matches what `wado compile` embeds (issue #1654). See WEP
//! `wep-2026-05-02-wit-interoperability.md`.

use std::fs;
use std::path::{Path, PathBuf};

use lexopt::Arg::Value;
use wado_compiler::CompilerOptions;
use wado_compiler::wit_emit::{self, WitEmitOptions, WitEmitSnapshot, WitScope};

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
    let (snapshot, world_imports) = if opts.lib {
        lib_snapshot_and_imports(opts.input, &usage).await?
    } else {
        world_snapshot_and_imports(opts.input, opts.world, &usage).await?
    };

    let text =
        wit_emit::emit_wit_text_from(snapshot.input(), &WitEmitOptions { scope }, &world_imports)
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

/// Compile `input` against a fixed WASI world (or the default), returning its
/// WIT subset and import plan.
async fn world_snapshot_and_imports(
    input: Option<String>,
    world: Option<String>,
    usage: &str,
) -> Result<(WitEmitSnapshot, Vec<String>), CliExit> {
    let input = manifest::resolve_input(input, EntryPointKind::Command, usage)?;
    let path = Path::new(&input);
    let source = fs::read_to_string(path)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", path.display())))?;
    let base_path = path.parent().map(Path::to_path_buf).unwrap_or_default();

    let manifest_pair = load_nearest_manifest(path);
    let host = analysis_host(&base_path, manifest_pair.as_ref(), &source).await?;
    let invocations = run_generators(path, &host, manifest_pair).await?;

    let world_fq = world.clone().unwrap_or_else(|| DEFAULT_WORLD.to_string());
    let contract =
        wit_emit::wit_contract(Some(&world_fq), None, Some(&default_interface_name(&input)));
    let options = CompilerOptions {
        opt_level: wado_compiler::OptLevel::O2,
        target_world: world,
        invocations,
        retain_wir: true,
        unused_diagnostics: false,
        embed_wit_contract: Some(contract),
        ..Default::default()
    };
    compile_wit_snapshot(&source, &host, &input, options).await
}

/// The analysis host seeded with the same dependency index the main compile
/// uses.
async fn analysis_host(
    base_path: &Path,
    project: Option<&manifest::ProjectManifest>,
    source: &str,
) -> Result<FilesystemCompilerHost, CliExit> {
    attach_manifest_and_component_deps(
        FilesystemCompilerHost::new(base_path.to_path_buf()),
        project,
        base_path,
        source,
        false,
    )
    .await
    .map_err(CliExit::error)
}

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

/// Compile the `[package].lib` entry for the synthesized library world (the
/// anonymous `root`), returning its WIT subset and import plan.
async fn lib_snapshot_and_imports(
    input: Option<String>,
    usage: &str,
) -> Result<(WitEmitSnapshot, Vec<String>), CliExit> {
    let (project, target) = manifest::resolve_lib_project(input, usage)?;

    let entry_str = target.entry.to_string_lossy().into_owned();
    let source = fs::read_to_string(&target.entry)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", target.entry.display())))?;
    let base_path = target
        .entry
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();

    let host = analysis_host(&base_path, Some(&project), &source).await?;
    let invocations = run_generators(&target.entry, &host, Some(project)).await?;

    let contract = wit_emit::wit_contract(None, Some(&target.interface_fq), None);
    // Same `--lib` world + allocator as `wado compile --lib`, so the import set
    // and subset match what that build embeds.
    let options = CompilerOptions {
        opt_level: wado_compiler::OptLevel::O2,
        lib_world: Some(target.interface_fq.clone()),
        allocator: Some("freelist".to_string()),
        invocations,
        retain_wir: true,
        unused_diagnostics: false,
        embed_wit_contract: Some(contract),
        ..Default::default()
    };
    compile_wit_snapshot(&source, &host, &entry_str, options).await
}

/// Run the compile and pull out the retained WIT subset + import plan.
async fn compile_wit_snapshot(
    source: &str,
    host: &FilesystemCompilerHost,
    input: &str,
    options: CompilerOptions,
) -> Result<(WitEmitSnapshot, Vec<String>), CliExit> {
    let result = wado_compiler::compile_with_options(source, host, Some(input), options)
        .await
        // Diagnostics are already on the loud host; exit quietly.
        .map_err(|_| CliExit::silent_failure(1))?;
    // Unreachable while `embed_wit_contract` is set; surface it loudly rather
    // than a silent exit-1 if that invariant ever breaks.
    let snapshot = result.wit_emit_snapshot.ok_or_else(|| {
        CliExit::error(
            "wado wit: compiler did not retain the WIT subset (internal error)".to_string(),
        )
    })?;
    Ok((snapshot, wir_imports(result.wir_package)))
}

/// The faithful import set from a compiled WIR plan, empty when absent.
fn wir_imports(wir_package: Option<wado_compiler::wir::WirPackage>) -> Vec<String> {
    wir_package
        .map(|pkg| pkg.imported_cm_interfaces)
        .unwrap_or_default()
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
