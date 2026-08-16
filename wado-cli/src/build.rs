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
use std::path::{Path, PathBuf};

use crate::args::{self, CliExit};
use crate::compile::{self, CompileFlags, CompileOptions};
use crate::knobs::{CompileKnobs, EmbedOpt, EmbedOptions, KnobOpt};
use crate::manifest;

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

    const KNOBS: &[KnobOpt] = &[
        KnobOpt::OptLevel,
        KnobOpt::InlineThreshold,
        KnobOpt::OptIterations,
        KnobOpt::LogLevel,
        KnobOpt::NoValidate,
        KnobOpt::NoCache,
        KnobOpt::Allocator,
        KnobOpt::Feature,
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
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::KNOBS, |o| o.spec())).unwrap();
    write!(
        buf,
        "{}",
        args::format_opts_help(EmbedOpt::ALL, |o| o.spec())
    )
    .unwrap();
    write!(
        buf,
        "{}",
        args::format_opts_help(args::ParamOpt::ALL, |o| o.spec())
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
            knobs.params.apply(p, &mut parser)?;
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
fn declared_worlds(project: &manifest::ProjectManifest) -> Result<Vec<BuildTarget>, CliExit> {
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
        targets.retain(|t| t.target_world.as_deref() == Some(world_fq.as_str()));
        if targets.is_empty() {
            return Err(CliExit::error(format!(
                "wado.toml declares no [world].\"{world_fq}\" to build"
            )));
        }
    }
    Ok(targets)
}

pub async fn run(opts: BuildOptions) -> Result<(), CliExit> {
    let cwd = std::env::current_dir()
        .map_err(|e| CliExit::error(format!("cannot get current directory: {e}")))?;
    let project = manifest::discover(&cwd)
        .map_err(CliExit::error)?
        .ok_or_else(|| {
            CliExit::error(
                "no wado.toml found; run from a project directory \
                 (use `wado compile <file>` to compile a single file)",
            )
        })?;
    manifest::emit_manifest_warnings(&project);

    let targets = select_targets(declared_worlds(&project)?, &opts)?;
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

    for target in targets {
        let output = match &opts.output {
            Some(path) => PathBuf::from(path),
            None => target.output.clone(),
        };
        build_world_component(&target, &output, &opts.knobs, opts.embed).await?;
    }
    Ok(())
}

/// The single project-build path: compile one world with `[package]` metadata
/// embedded, write it to `output`, and return its bytes. Shared by `wado build`
/// and the run/serve drivers so a run artifact matches a built one.
pub async fn build_world_component(
    target: &BuildTarget,
    output: &Path,
    knobs: &CompileKnobs,
    embed: EmbedOptions,
) -> Result<Vec<u8>, CliExit> {
    let mut opts = CompileOptions::for_world_build(
        target.entry.to_string_lossy().into_owned(),
        output.to_path_buf(),
        target.lib_world.clone(),
        target.target_world.clone(),
    );
    opts.knobs = knobs.clone();
    opts.embed = embed;
    // Use the bytes we just compiled rather than reading `output` back: a
    // concurrent build (e.g. parallel `serve` drivers sharing this project's
    // `build/<world>.wasm`) could leave the file torn or holding another
    // world's module.
    compile::run_returning_bytes(opts).await
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
    match compile::load_nearest_manifest(Path::new(entry)) {
        Some(project) => {
            let segment = compile::world_path_segment(target_world);
            let output = compile::build_output_path(&project.root, &segment);
            let target = BuildTarget {
                entry: PathBuf::from(entry),
                output: output.clone(),
                lib_world: None,
                target_world: Some(target_world.to_string()),
            };
            build_world_component(&target, &output, &flags.knobs, EmbedOptions::default()).await
        }
        None => compile::compile(entry, flags).await,
    }
}
