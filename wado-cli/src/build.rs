//! `wado build` — the project orchestrator. Reads `wado.toml`, builds each
//! declared world (`[package].lib` plus every `[world]` entry) through the
//! `compile` primitive with `[package]` metadata embedded, and writes each
//! component to `<root>/build/<world>.wasm`.
//!
//! Dependency resolution, locking, and caching belong here (see the
//! CLI-subcommands WEP, Command Tiers); `compile` never resolves. Wiring the
//! resolved dependency index in is a later phase — today the compile core reads
//! path deps from the nearest manifest itself.

use std::fmt::Write as _;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::args::{self, CliExit};
use crate::compile::{self, CompileFlags, CompileOptions};
use crate::knobs::{CompileKnobOpt, CompileKnobs, EmbedOpt, EmbedOptions};
use crate::manifest::{self, openable_dir};
use wado_host::sync::joined;

pub struct BuildOptions {
    /// `--world <fq>`: build only this hosted world. Mutually exclusive with `lib`.
    world: Option<String>,
    /// `--lib`: build only the library world. Mutually exclusive with `world`.
    lib: bool,
    /// `-o`: output path; valid only when a single world is selected.
    output: Option<String>,
    knobs: CompileKnobs,
    embed: EmbedOptions,
}

#[derive(Clone, Copy)]
enum Opt {
    World,
    Lib,
    Output,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::World, Self::Lib, Self::Output, Self::Help];

    const KNOBS: &[CompileKnobOpt] = &[
        CompileKnobOpt::OptLevel,
        CompileKnobOpt::InlineThreshold,
        CompileKnobOpt::InlineGrowth,
        CompileKnobOpt::OptIterations,
        CompileKnobOpt::OptThreads,
        CompileKnobOpt::LogLevel,
        CompileKnobOpt::NoValidate,
        CompileKnobOpt::NoCache,
        CompileKnobOpt::Allocator,
        CompileKnobOpt::Feature,
        CompileKnobOpt::NoLint,
        CompileKnobOpt::Lint,
        CompileKnobOpt::LintGenerated,
    ];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::World => args::OptSpec {
                long: Some("world"),
                short: None,
                value: Some("<fq>"),
                desc: "Build only this hosted world (Component Model world FQ)",
            },
            Self::Lib => args::OptSpec {
                long: Some("lib"),
                short: None,
                value: None,
                desc: "Build only the library world ([package].lib)",
            },
            Self::Output => args::OptSpec {
                long: None,
                short: Some('o'),
                value: Some("<file>"),
                desc: "Output path (only with a single --world / --lib; default: build/<world>.wasm)",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado build [options]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Build the project's declared worlds from wado.toml into build/<world>.wasm."
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
            .add(EmbedOpt::ALL, |o| o.spec())
            .add(args::ParamOpt::ALL, |o| o.spec())
            .render()
    )
    .unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<BuildOptions, CliExit> {
    let usage = format_usage();
    let mut world: Option<String> = None;
    let mut lib = false;
    let mut output: Option<String> = None;
    let mut knobs = CompileKnobs::default();
    let mut embed = EmbedOptions::default();

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(k) = args::match_opt(&arg, Opt::KNOBS, |k| k.spec()) {
            knobs.apply(k, &mut parser)?;
        } else if let Some(p) = args::match_opt(&arg, args::ParamOpt::ALL, |p| p.spec()) {
            args::apply_param_opt(&mut knobs.params, p, &mut parser)?;
        } else if let Some(e) = args::match_opt(&arg, EmbedOpt::ALL, |e| e.spec()) {
            embed.apply(e)?;
        } else if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::World => world = Some(args::require_string(&mut parser)?),
                Opt::Lib => lib = true,
                Opt::Output => output = Some(args::require_string(&mut parser)?),
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    if lib && world.is_some() {
        return Err(CliExit::error(
            "`--lib` and `--world` are mutually exclusive",
        ));
    }

    Ok(BuildOptions {
        world,
        lib,
        output,
        knobs,
        embed,
    })
}

/// One world to build: its entry module, the `build/<segment>.wasm` output, and
/// the world selector (`--lib` FQ or `--world` FQ) passed to the compile core.
#[derive(Clone)]
pub struct BuildTarget {
    pub entry: PathBuf,
    pub output: PathBuf,
    pub lib_world: Option<String>,
    pub target_world: Option<String>,
}

/// Every world the package declares: the library world (`[package].lib`) plus
/// each `[world]` entry. Unlike publish, `build` builds all of them regardless
/// of a world's `publish = false` — opting out of publishing does not opt out
/// of building.
pub fn declared_worlds(project: &manifest::ProjectManifest) -> Result<Vec<BuildTarget>, CliExit> {
    let pkg = project
        .manifest
        .package
        .as_ref()
        .ok_or_else(|| CliExit::error("no [package] in wado.toml; nothing to build"))?;
    let root = &project.root;
    let mut targets = Vec::new();
    if let Some(lib_rel) = pkg.lib.as_deref() {
        targets.push(BuildTarget {
            entry: root.join(lib_rel),
            output: compile::build_output_path(root, "lib"),
            lib_world: Some(manifest::lib_world_fq(pkg)?),
            target_world: None,
        });
    }
    for (world_fq, entry) in &project.manifest.world {
        let segment = compile::world_path_segment(world_fq);
        targets.push(BuildTarget {
            entry: root.join(&entry.entry),
            output: compile::build_output_path(root, &segment),
            lib_world: None,
            target_world: Some(world_fq.clone()),
        });
    }
    Ok(targets)
}

/// The project enclosing the current directory, its manifest warnings emitted.
/// `missing` words the error for a directory under no manifest.
pub fn project_here(missing: &'static str) -> Result<manifest::ProjectManifest, CliExit> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliExit::error(format!("cannot get current directory: {e}")))?;
    project_at(&cwd, missing)
}

/// The project enclosing `dir`, its manifest warnings emitted.
pub fn project_at(dir: &Path, missing: &'static str) -> Result<manifest::ProjectManifest, CliExit> {
    let project = manifest::discover(openable_dir(dir))
        .map_err(CliExit::error)?
        .ok_or_else(|| CliExit::error(missing))?;
    manifest::emit_manifest_warnings(&project);
    Ok(project)
}

/// Keep only the world `--world <fq>` names.
pub fn retain_world(targets: &mut Vec<BuildTarget>, world_fq: &str) -> Result<(), CliExit> {
    targets.retain(|t| t.target_world.as_deref() == Some(world_fq));
    if targets.is_empty() {
        return Err(CliExit::error(format!(
            "wado.toml declares no [world].\"{world_fq}\""
        )));
    }
    Ok(())
}

/// Keep only the world selected by `--lib` / `--world`, or all worlds when
/// neither is given.
fn select_targets(
    mut targets: Vec<BuildTarget>,
    opts: &BuildOptions,
) -> Result<Vec<BuildTarget>, CliExit> {
    if opts.lib {
        targets.retain(|t| t.lib_world.is_some());
        if targets.is_empty() {
            return Err(CliExit::error(
                "`--lib` was given but wado.toml declares no [package].lib",
            ));
        }
    } else if let Some(world_fq) = &opts.world {
        retain_world(&mut targets, world_fq)?;
    }
    Ok(targets)
}

pub async fn run(opts: BuildOptions) -> Result<(), CliExit> {
    let project = project_here(
        "no wado.toml found; run from a project directory \
         (use `wado compile <file>` to compile a single file)",
    )?;
    let mut targets = select_targets(declared_worlds(&project)?, &opts)?;
    if targets.is_empty() {
        return Err(CliExit::error(
            "no world to build; declare [package].lib or a [world] entry in wado.toml",
        ));
    }
    if opts.output.is_some() && targets.len() > 1 {
        return Err(CliExit::error(
            "`-o` builds a single world; select one with `--lib` or `--world <fq>`",
        ));
    }

    if let Some(path) = &opts.output {
        targets[0].output = PathBuf::from(path);
    }
    build_world_components(targets, &opts.knobs, opts.embed).await
}

/// Build every target concurrently, announcing each artifact and printing each
/// failure as it lands, so one broken world does not hide another. The
/// builds share the cores, so unless `--optimize-threads` says otherwise each
/// optimizes on its share of them.
pub async fn build_world_components(
    targets: Vec<BuildTarget>,
    knobs: &CompileKnobs,
    embed: EmbedOptions,
) -> Result<(), CliExit> {
    assert!(!targets.is_empty(), "every caller has a world to build");
    let cores = std::thread::available_parallelism().map_or(1, NonZero::get);
    let mut knobs = knobs.clone();
    knobs
        .opt_threads
        .get_or_insert((cores / targets.len()).max(1));
    let slots = Arc::new(Semaphore::new(cores));
    let mut builds = JoinSet::<Result<(), CliExit>>::new();
    for target in targets {
        let slot = Arc::clone(&slots)
            .acquire_owned()
            .await
            .expect("the build semaphore is never closed");
        let knobs = knobs.clone();
        // A compile blocks its thread for its whole run, so each one gets a
        // blocking thread and a runtime of its own, as `wado test`'s do.
        builds.spawn_blocking(move || {
            let _slot = slot;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| CliExit::error(format!("unable to create a build runtime: {e}")))?
                .block_on(build_world_component(&target, &knobs, embed))?;
            compile::announce_artifact(&target.output);
            Ok(())
        });
    }
    let mut exit_code = None;
    while let Some(build) = builds.join_next().await {
        if let Err(failure) = joined(build) {
            eprint!("{}", failure.message);
            exit_code.get_or_insert(failure.exit_code);
        }
    }
    exit_code.map_or(Ok(()), |code| Err(CliExit::silent_failure(code)))
}

/// The single project-build path: compile one world with `[package]` metadata
/// embedded, write it to `target.output`, and return its bytes. Shared by `wado
/// build` and the run/serve drivers so a run artifact matches a built one. It
/// announces nothing; `wado build` does that for the files it asked for.
pub async fn build_world_component(
    target: &BuildTarget,
    knobs: &CompileKnobs,
    embed: EmbedOptions,
) -> Result<Vec<u8>, CliExit> {
    let mut opts = CompileOptions::for_world_build(
        target.entry.to_string_lossy().into_owned(),
        target.output.clone(),
        target.lib_world.clone(),
        target.target_world.clone(),
    );
    opts.knobs = knobs.clone();
    opts.embed = embed;
    // Use the bytes we just compiled rather than reading `output` back: a
    // concurrent build (e.g. parallel `serve` drivers sharing this project's
    // `build/<world>.wasm`) could leave the file torn or holding another
    // world's module.
    Ok(compile::compile_to_artifact(opts).await?.bytes)
}

/// Produce the runnable component for a driver (`run` / `serve`). In a project
/// (a nearby `wado.toml`), build the world through the shared core — metadata
/// embedded, written to `build/<world segment>.wasm`, matching `wado build`.
/// With no project, fall back to the standalone compile primitive (in-memory,
/// no artifact on disk). `flags` supplies the build knobs and the fallback
/// world. Metadata and WIT embed at their defaults, matching `wado build`, so a
/// run/serve artifact is identical to a built one.
pub async fn build_for_driver(
    entry: &str,
    target_world: &str,
    flags: &CompileFlags,
) -> Result<Vec<u8>, CliExit> {
    match compile::load_nearest_manifest(Path::new(entry)).map_err(CliExit::error)? {
        Some(project) => {
            let segment = compile::world_path_segment(target_world);
            let target = BuildTarget {
                entry: PathBuf::from(entry),
                output: compile::build_output_path(&project.root, &segment),
                lib_world: None,
                target_world: Some(target_world.to_string()),
            };
            build_world_component(&target, &flags.knobs, EmbedOptions::default()).await
        }
        None => compile::compile(entry, flags).await,
    }
}
