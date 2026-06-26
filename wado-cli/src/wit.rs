//! `wado wit` — emit the WIT text for a Wado program's component contract.
//!
//! Runs the frontend (`wado_compiler::semantics`) and stops; WIT is a
//! pre-codegen fact, so there is no optimization level. See WEP
//! `wep-2026-05-02-wit-interoperability.md`.

use std::fs;
use std::path::{Path, PathBuf};

use lexopt::Arg::Value;
use wado_compiler::wit_emit::{self, WitEmitOptions, WitScope};

use crate::args::{self, CliExit, OptSpec};
use crate::compiler_host::FilesystemCompilerHost;
use crate::manifest::{self, EntryPointKind};

const DEFAULT_WORLD: &str = "wasi:cli/command";

pub struct WitOptions {
    pub input: Option<String>,
    pub scope: WitScope,
    pub world: Option<String>,
    pub output: Option<String>,
}

#[derive(Clone, Copy)]
enum Opt {
    Scope,
    World,
    Output,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Scope, Self::World, Self::Output, Self::Help];

    const fn spec(self) -> OptSpec {
        match self {
            Self::Scope => OptSpec {
                long: Some("scope"),
                short: None,
                value: Some("<full|local>"),
                desc: "Inlining scope for referenced interfaces (default: full)",
            },
            Self::World => args::WORLD_SPEC,
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
                Opt::Output => output = Some(args::require_string(&mut parser)?),
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            inputs.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
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
        output,
    })
}

pub async fn run(opts: WitOptions) -> Result<(), CliExit> {
    let usage = format_usage();
    let input = manifest::resolve_input(opts.input, EntryPointKind::Command, &usage)?;
    let path = Path::new(&input);

    let source = fs::read_to_string(path)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", path.display())))?;

    let base_path = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let host = FilesystemCompilerHost::new(base_path);

    let sem = wado_compiler::semantics(&source, &host, Some(&input)).await;
    if !sem.is_complete() {
        // Diagnostics are already emitted by the host; signal silently.
        return Err(CliExit::silent_failure(1));
    }

    let world = opts.world.unwrap_or_else(|| DEFAULT_WORLD.to_string());

    // The faithful world import set is the WIR-level plan, available only after
    // DCE — so compile through optimize to read it. Diagnostics were already
    // surfaced by the `semantics` pass above; a quiet host avoids duplicates.
    let world_imports = resolve_world_imports(&source, &input, &world).await;

    let emit_opts = WitEmitOptions {
        scope: opts.scope,
        world_fq: world,
        default_interface_name: default_interface_name(&input),
        world_imports,
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

/// The default interface name: the manifest `[package].name` when the input
/// resolves through a project, otherwise the entry file stem.
pub(crate) fn default_interface_name(input: &str) -> String {
    if let Some((manifest, _root)) = crate::compile::load_nearest_manifest(Path::new(input))
        && let Some(package) = manifest.package
    {
        return package.name;
    }
    PathBuf::from(input)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_string())
}
