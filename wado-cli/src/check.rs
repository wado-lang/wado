//! `wado check` — CI-side integrity check for committed-source Kiln
//! workflows. Re-runs every Kiln invocation, byte-compares each output
//! against the on-disk file, and treats Kiln warnings as errors by
//! default.
//!
//! See [WEP: Kiln](../../docs/wep-2026-04-12-kiln.md), section "The
//! `wado check` command".

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use lexopt::Arg::Value;
use wado_compiler::Code;

use crate::args::{self, CliExit};
use crate::build;
use crate::compile::{attach_manifest_and_component_deps, load_nearest_manifest, prepare_kiln};
use crate::compiler_host::FilesystemCompilerHost;
use crate::dep_component::Acquisition;
use crate::kiln_driver::{CheckOutcome, PipelineError, check_pipeline};
use crate::knobs::{CompileKnobs, KnobOpt};
use crate::manifest;

#[derive(Debug)]
pub struct CheckOptions {
    /// The file to check. `None` checks every world `wado.toml` declares, the
    /// way `wado build` builds them.
    pub input: Option<String>,
    /// `false` (default) → Kiln warnings produce a non-zero exit. `true`
    /// → keep them as warnings (developer-friendly local triage).
    pub warn_only: bool,
    /// World to check against. `None` picks it from the manifest, falling back
    /// to the library world; `Some("test")` is the synthetic test world.
    pub target_world: Option<String>,
    pub knobs: CompileKnobs,
}

#[derive(Clone, Copy)]
enum Opt {
    Warn,
    World,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Warn, Self::World, Self::Help];

    const KNOBS: &[KnobOpt] = &[KnobOpt::LogLevel, KnobOpt::NoCache];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Warn => args::OptSpec {
                long: Some("warn"),
                short: None,
                value: None,
                desc: "Keep Kiln warnings as warnings instead of promoting them to errors",
            },
            // Not the shared `WORLD_SPEC`: `check` emits nothing, so its default
            // is the library world rather than `wasi:cli/command`.
            Self::World => args::OptSpec {
                long: Some("world"),
                short: None,
                value: Some("<name>"),
                desc: "Check against this world's entry-point contract\n\
                       (default: the world whose [world] entry names the file, \
                       else the library world)\nUse 'test' for the test world",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado check [options] [file.wado]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Verify Wado sources (and their Kiln generators) without emitting Wasm.\n\
         With no file, checks every world wado.toml declares — the targets\n\
         `wado build` builds — and stops after the analysis.",
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Re-runs every Kiln generator and compares the output against the on-disk\n\
         source. By default, any Kiln divergence (modified, regenerated, or stale\n\
         output) exits non-zero — suitable for CI gates on committed-source workflows.",
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(
        buf,
        "{}",
        args::OptsHelp::default()
            .add(Opt::ALL, |o| o.spec())
            .add(Opt::KNOBS, |o| o.spec())
            .render()
    )
    .unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<CheckOptions, CliExit> {
    let usage = format_usage();
    let mut input: Option<String> = None;
    let mut warn_only = false;
    let mut target_world: Option<String> = None;
    let mut knobs = CompileKnobs::default();
    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(k) = args::match_opt(&arg, Opt::KNOBS, |k| k.spec()) {
            knobs.apply(k, &mut parser)?;
        } else if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Warn => warn_only = true,
                Opt::World => target_world = Some(args::require_string(&mut parser)?),
                Opt::Help => {
                    return Err(CliExit::help(usage));
                }
            }
        } else if let Value(val) = arg {
            args::reject_multiple_inputs(&input)?;
            input = Some(val.to_string_lossy().to_string());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }
    Ok(CheckOptions {
        input,
        warn_only,
        target_world,
        knobs,
    })
}

pub async fn run(opts: CheckOptions) -> Result<(), CliExit> {
    let Some(input) = opts.input.clone() else {
        return check_declared_worlds(&opts).await;
    };
    let path = PathBuf::from(&input);
    let world = check_world(
        opts.target_world.as_deref(),
        &path,
        load_nearest_manifest(&path).as_ref(),
    );
    check_entry(&path, world, &opts).await
}

/// Check every world `wado.toml` declares, selected the way `wado build`
/// selects its targets. Same analysis as a single file, once per entry.
async fn check_declared_worlds(opts: &CheckOptions) -> Result<(), CliExit> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliExit::error(format!("cannot get current directory: {e}")))?;
    let project = manifest::discover(&cwd)
        .map_err(CliExit::error)?
        .ok_or_else(|| {
            CliExit::error(
                "no wado.toml found; name a file to check \
                 (`wado check <file.wado>`) or run from a project directory",
            )
        })?;
    manifest::emit_manifest_warnings(&project);

    let mut targets = build::declared_worlds(&project)?;
    if let Some(world_fq) = &opts.target_world {
        targets.retain(|t| t.target_world.as_deref() == Some(world_fq.as_str()));
        if targets.is_empty() {
            return Err(CliExit::error(format!(
                "wado.toml declares no [world].\"{world_fq}\" to check"
            )));
        }
    }
    if targets.is_empty() {
        return Err(CliExit::error(
            "no world to check; declare [package].lib or a [world] entry in wado.toml",
        ));
    }
    for target in targets {
        let world = match (&target.lib_world, &target.target_world) {
            (Some(fq), _) => CheckWorld::Lib(fq.clone()),
            (_, Some(fq)) => CheckWorld::Target(fq.clone()),
            _ => unreachable!("a build target names exactly one world"),
        };
        check_entry(&target.entry, world, opts).await?;
    }
    Ok(())
}

async fn check_entry(path: &Path, world: CheckWorld, opts: &CheckOptions) -> Result<(), CliExit> {
    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let source = std::fs::read_to_string(path)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", path.display())))?;
    let manifest_pair = load_nearest_manifest(path);
    let (target_world, lib_world) = world.options();
    let host = attach_manifest_and_component_deps(
        FilesystemCompilerHost::with_log_level(base_path.clone(), opts.knobs.log_level),
        manifest_pair.as_ref(),
        &base_path,
        &source,
        Acquisition::Build,
    )
    .await
    .map_err(CliExit::error)?;

    // Same setup `wado compile` runs its generators through — only the pipeline
    // below differs: `check` dry-runs and byte-compares instead of writing.
    let kiln = prepare_kiln(path, &host, opts.knobs.no_cache, manifest_pair)
        .await
        .map_err(silent_or_reported)?;
    let outcome = match kiln {
        None => CheckOutcome::default(),
        Some(mut kiln) => {
            let mut outcome = check_pipeline(
                &kiln.manifest,
                &kiln.manifest_root,
                &kiln.host,
                &kiln.provider,
                std::mem::take(&mut kiln.invocations),
            )
            .await
            .map_err(|e| CliExit::error(FormatPipelineError(&e)))?;
            kiln.remap_conflicts(&mut outcome.invocations, &host)
                .map_err(silent_or_reported)?;
            outcome
        }
    };

    let kiln_drift = !outcome.stale.is_empty() || !outcome.missing.is_empty();

    // Drive the rest of the compile pipeline so type/resolve errors also gate
    // `wado check`. At `O0`, since the component is discarded: the optimization
    // loop reports nothing, and on a large program it is most of the run. The
    // phases around it stay, so every diagnostic a build produces still lands.
    let compiler_options = wado_compiler::CompilerOptions {
        log_level: Some(opts.knobs.log_level),
        target_world,
        lib_world,
        opt_level: wado_compiler::OptLevel::O0,
        analysis_only: true,
        invocations: outcome.invocations.clone(),
        ..Default::default()
    };
    let entry_name = path.to_string_lossy().into_owned();
    let compile_result =
        wado_compiler::compile_with_options(&source, &host, Some(&entry_name), compiler_options)
            .await;

    let has_compile_errors = host.has_errors() || compile_result.is_err();
    let has_kiln_warnings = host
        .diagnostics()
        .into_iter()
        .any(|d| is_kiln_diagnostic(&d.code));

    if has_compile_errors {
        return Err(CliExit::silent_failure(1));
    }
    if !opts.warn_only && (kiln_drift || has_kiln_warnings) {
        return Err(CliExit::error(
            "wado check: Kiln integrity check failed — \
             one or more generators produced output that differs from on-disk source. \
             Pass --warn to keep warnings as warnings.",
        ));
    }
    Ok(())
}

/// The world `wado check` verifies the entry against.
enum CheckWorld {
    /// A well-known world and its entry-point contract.
    Target(String),
    /// The library world, named by this FQ: every `export fn` is a world export
    /// and none is required.
    Lib(String),
}

impl CheckWorld {
    /// `(target_world, lib_world)` — the pair [`wado_compiler::CompilerOptions`]
    /// wants, of which exactly one is `Some`.
    fn options(self) -> (Option<String>, Option<String>) {
        match self {
            Self::Target(world) => (Some(world), None),
            Self::Lib(fq) => (None, Some(fq)),
        }
    }
}

/// The library FQ a check falls back to when `[package]` names no namespace.
/// Never emitted — `check` discards the component it builds.
const CHECK_LIB_WORLD: &str = "wado:check/check@0.0.0";

/// `--world` first, then the world whose `[world]` entry names this file, and
/// otherwise the library world: a module that is no world's entry is a library,
/// and demanding `export fn run` of one made `check` unusable on it (issue #2059).
fn check_world(
    requested: Option<&str>,
    path: &Path,
    project: Option<&manifest::ProjectManifest>,
) -> CheckWorld {
    if let Some(world) = requested {
        return CheckWorld::Target(world.to_string());
    }
    if let Some(world) = project.and_then(|p| manifest::world_declaring(p, path)) {
        return CheckWorld::Target(world);
    }
    let fq = project
        .and_then(|p| p.manifest.package.as_ref())
        .and_then(|pkg| manifest::lib_world_fq(pkg).ok())
        .unwrap_or_else(|| CHECK_LIB_WORLD.to_string());
    CheckWorld::Lib(fq)
}

/// A malformed inline clause and a redirect conflict have already been reported
/// as diagnostics, so they exit silently; anything else needs its own message.
fn silent_or_reported(e: PipelineError) -> CliExit {
    match e {
        PipelineError::InlineClause(_) | PipelineError::RedirectConflict(_) => {
            CliExit::silent_failure(1)
        }
        other => CliExit::error(FormatPipelineError(&other)),
    }
}

fn is_kiln_diagnostic(code: &Code) -> bool {
    matches!(
        code,
        Code::KilnStaleCache
            | Code::KilnGeneratorForbiddenImport
            | Code::KilnMissingWith
            | Code::KilnGeneratedModified
            | Code::KilnGeneratedRegenerated
            | Code::KilnGeneratedStaleOnDisk
    )
}

struct FormatPipelineError<'a>(&'a PipelineError);

impl std::fmt::Display for FormatPipelineError<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wado check: {}", self.0)
    }
}
