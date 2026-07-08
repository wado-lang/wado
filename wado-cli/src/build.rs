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

use wado_compiler::LogLevel;

use crate::args::{self, CliExit};
use crate::compile::{self, CompileFlags, CompileOptions, OptLevel};
use crate::manifest;

pub struct BuildOptions {
    /// `--world <fq>`: build only this hosted world. Mutually exclusive with `lib`.
    world: Option<String>,
    /// `--lib`: build only the library world. Mutually exclusive with `world`.
    lib: bool,
    /// `-o`: output path; valid only when a single world is selected.
    output: Option<String>,
    opt_level: OptLevel,
    log_level: LogLevel,
    skip_validation: bool,
    no_cache: bool,
    inline_threshold: Option<usize>,
    opt_iterations: Option<u32>,
    allocator: Option<String>,
    codegen_flags: Vec<String>,
    param_overrides: wado_compiler::hashmap::IndexMap<String, String>,
    param_policy: wado_compiler::param_resolution::ParamPolicy,
    no_embed_wit: bool,
    embed_wit: bool,
    no_embed_metadata: bool,
    embed_metadata: bool,
}

#[derive(Clone, Copy)]
enum Opt {
    World,
    Lib,
    Output,
    OptLevel,
    InlineThreshold,
    OptIterations,
    LogLevel,
    NoValidate,
    NoCache,
    Allocator,
    Feature,
    NoEmbedWit,
    EmbedWit,
    NoEmbedMetadata,
    EmbedMetadata,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::World,
        Self::Lib,
        Self::Output,
        Self::OptLevel,
        Self::InlineThreshold,
        Self::OptIterations,
        Self::LogLevel,
        Self::NoValidate,
        Self::NoCache,
        Self::Allocator,
        Self::Feature,
        Self::NoEmbedWit,
        Self::EmbedWit,
        Self::NoEmbedMetadata,
        Self::EmbedMetadata,
        Self::Help,
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
            Self::OptLevel => args::OPT_LEVEL_SPEC,
            Self::InlineThreshold => args::INLINE_THRESHOLD_SPEC,
            Self::OptIterations => args::OPT_ITERATIONS_SPEC,
            Self::LogLevel => args::LOG_LEVEL_SPEC,
            Self::NoValidate => args::NO_VALIDATE_SPEC,
            Self::NoCache => args::NO_CACHE_SPEC,
            Self::Allocator => args::ALLOCATOR_SPEC,
            Self::Feature => args::FEATURE_SPEC,
            Self::NoEmbedWit => args::OptSpec {
                long: Some("no-embed-wit"),
                short: None,
                value: None,
                desc: "Do not embed the WIT `component-type` section in the output",
            },
            Self::EmbedWit => args::OptSpec {
                long: Some("embed-wit"),
                short: None,
                value: None,
                desc: "Force embedding the WIT section on (e.g. under -Os, where it is off by default)",
            },
            Self::NoEmbedMetadata => args::OptSpec {
                long: Some("no-embed-metadata"),
                short: None,
                value: None,
                desc: "Do not embed the [package] metadata sections in the output",
            },
            Self::EmbedMetadata => args::OptSpec {
                long: Some("embed-metadata"),
                short: None,
                value: None,
                desc: "Force embedding the [package] metadata on (e.g. under -Os, where it is off by default)",
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
    let mut opt_level = OptLevel::default();
    let mut log_level = args::DEFAULT_LOG_LEVEL;
    let mut skip_validation = false;
    let mut no_cache = false;
    let mut inline_threshold: Option<usize> = None;
    let mut opt_iterations: Option<u32> = None;
    let mut allocator: Option<String> = None;
    let mut codegen_flags: Vec<String> = Vec::new();
    let mut no_embed_wit = false;
    let mut embed_wit = false;
    let mut no_embed_metadata = false;
    let mut embed_metadata = false;
    let mut param_args = args::ParamArgs::default();

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(p) = args::match_opt(&arg, args::ParamOpt::ALL, |p| p.spec()) {
            param_args.apply(p, &mut parser)?;
        } else if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::World => world = Some(args::require_string(&mut parser)?),
                Opt::Lib => lib = true,
                Opt::Output => output = Some(args::require_string(&mut parser)?),
                Opt::OptLevel => opt_level = compile::parse_opt_level_arg(&mut parser)?,
                Opt::InlineThreshold => {
                    inline_threshold = Some(args::parse_inline_threshold_arg(
                        "--optimize-inline-threshold",
                        &mut parser,
                    )?);
                }
                Opt::OptIterations => {
                    opt_iterations = Some(args::parse_opt_iterations_arg(
                        "--optimize-iterations",
                        &mut parser,
                    )?);
                }
                Opt::LogLevel => log_level = args::parse_log_level_arg(&mut parser)?,
                Opt::NoValidate => skip_validation = true,
                Opt::NoCache => no_cache = true,
                Opt::Allocator => allocator = Some(args::require_string(&mut parser)?),
                Opt::Feature => codegen_flags.push(args::require_string(&mut parser)?),
                Opt::NoEmbedWit => no_embed_wit = true,
                Opt::EmbedWit => embed_wit = true,
                Opt::NoEmbedMetadata => no_embed_metadata = true,
                Opt::EmbedMetadata => embed_metadata = true,
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
    if no_embed_wit && embed_wit {
        return Err(CliExit::error(
            "`--no-embed-wit` and `--embed-wit` are mutually exclusive",
        ));
    }
    if no_embed_metadata && embed_metadata {
        return Err(CliExit::error(
            "`--no-embed-metadata` and `--embed-metadata` are mutually exclusive",
        ));
    }

    Ok(BuildOptions {
        world,
        lib,
        output,
        opt_level,
        log_level,
        skip_validation,
        no_cache,
        inline_threshold,
        opt_iterations,
        allocator,
        codegen_flags,
        param_overrides: param_args.overrides,
        param_policy: param_args.policy,
        no_embed_wit,
        embed_wit,
        no_embed_metadata,
        embed_metadata,
    })
}

/// One world to build: its entry module, the `build/<segment>.wasm` output, and
/// the world selector (`--lib` FQ or `--world` FQ) passed to the compile core.
struct BuildTarget {
    entry: PathBuf,
    output: PathBuf,
    lib_world: Option<String>,
    target_world: Option<String>,
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

    let core = BuildCore {
        opt_level: opts.opt_level,
        log_level: opts.log_level,
        skip_validation: opts.skip_validation,
        no_cache: opts.no_cache,
        inline_threshold: opts.inline_threshold,
        opt_iterations: opts.opt_iterations,
        allocator: opts.allocator.clone(),
        codegen_flags: opts.codegen_flags.clone(),
        param_overrides: opts.param_overrides.clone(),
        param_policy: opts.param_policy,
        no_embed_wit: opts.no_embed_wit,
        embed_wit: opts.embed_wit,
        no_embed_metadata: opts.no_embed_metadata,
        embed_metadata: opts.embed_metadata,
    };
    for target in targets {
        let output = match &opts.output {
            Some(path) => PathBuf::from(path),
            None => target.output.clone(),
        };
        build_world_component(
            &target.entry,
            &output,
            target.lib_world,
            target.target_world,
            &core,
        )
        .await?;
    }
    Ok(())
}

/// Build-time knobs the drivers (`build` / `run` / `serve`) forward to the
/// compile core for one world. Separated from the world identity (entry /
/// output / world selector) so the same core is reused across worlds.
pub struct BuildCore {
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub skip_validation: bool,
    pub no_cache: bool,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
    pub allocator: Option<String>,
    pub codegen_flags: Vec<String>,
    pub param_overrides: wado_compiler::hashmap::IndexMap<String, String>,
    pub param_policy: wado_compiler::param_resolution::ParamPolicy,
    pub no_embed_wit: bool,
    pub embed_wit: bool,
    pub no_embed_metadata: bool,
    pub embed_metadata: bool,
}

impl BuildCore {
    /// A driver core from a `run` / `serve` [`CompileFlags`]. Metadata and WIT
    /// embed at their defaults (on), matching `wado build`, so a run/serve
    /// artifact is identical to a built one.
    #[must_use]
    pub fn from_flags(flags: &CompileFlags) -> Self {
        Self {
            opt_level: flags.opt_level,
            log_level: flags.log_level,
            skip_validation: flags.skip_validation,
            no_cache: flags.no_cache,
            inline_threshold: flags.inline_threshold,
            opt_iterations: flags.opt_iterations,
            allocator: flags.allocator.clone(),
            codegen_flags: flags.codegen_flags.clone(),
            param_overrides: flags.param_overrides.clone(),
            param_policy: flags.param_policy,
            no_embed_wit: false,
            embed_wit: false,
            no_embed_metadata: false,
            embed_metadata: false,
        }
    }
}

/// The single project-build path: compile one world with `[package]` metadata
/// embedded, write it to `output`, and return its bytes. Shared by `wado build`
/// and the run/serve drivers so a run artifact matches a built one.
pub async fn build_world_component(
    entry: &Path,
    output: &Path,
    lib_world: Option<String>,
    target_world: Option<String>,
    core: &BuildCore,
) -> Result<Vec<u8>, CliExit> {
    let mut opts = CompileOptions::for_world_build(
        entry.to_string_lossy().into_owned(),
        output.to_path_buf(),
        lib_world,
        target_world,
    );
    opts.opt_level = core.opt_level;
    opts.log_level = core.log_level;
    opts.skip_validation = core.skip_validation;
    opts.no_cache = core.no_cache;
    opts.inline_threshold = core.inline_threshold;
    opts.opt_iterations = core.opt_iterations;
    opts.allocator.clone_from(&core.allocator);
    opts.codegen_flags.clone_from(&core.codegen_flags);
    opts.param_overrides = core.param_overrides.clone();
    opts.param_policy = core.param_policy;
    opts.no_embed_wit = core.no_embed_wit;
    opts.embed_wit = core.embed_wit;
    opts.no_embed_metadata = core.no_embed_metadata;
    opts.embed_metadata = core.embed_metadata;
    // Use the bytes we just compiled rather than reading `output` back: a
    // concurrent build (e.g. parallel `serve` drivers sharing this project's
    // `build/<world>.wasm`) could leave the file torn or holding another
    // world's module.
    compile::run_returning_bytes(opts).await
}

/// Produce the runnable component for a driver (`run` / `serve`). In a project
/// (a nearby `wado.toml`), build the world through the shared core — metadata
/// embedded, written to `build/<segment>.wasm`, matching `wado build`. With no
/// project, fall back to the standalone compile primitive (in-memory, no
/// artifact on disk). `flags` supplies the build knobs and the fallback world.
pub async fn build_for_driver(
    entry: &str,
    target_world: &str,
    world_segment: &str,
    flags: &CompileFlags,
) -> Result<Vec<u8>, CliExit> {
    match compile::load_nearest_manifest(Path::new(entry)) {
        Some(project) => {
            let output = compile::build_output_path(&project.root, world_segment);
            let core = BuildCore::from_flags(flags);
            build_world_component(
                Path::new(entry),
                &output,
                None,
                Some(target_world.to_string()),
                &core,
            )
            .await
        }
        None => compile::compile(entry, flags).await,
    }
}
