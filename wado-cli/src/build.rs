//! `wado build` — the project orchestrator. Reads `wado.toml`, builds each
//! declared world (`[package].lib` plus every `[world]` entry) through the
//! `compile` primitive with `[package]` metadata embedded, and writes each
//! component to `<root>/build/<world>.wasm`. With `-o -` it writes instead the
//! one component a driver would run to stdout, for a `wado run-*` subcommand to
//! host.
//!
//! Dependency resolution, locking, and caching belong here (see the
//! CLI-subcommands WEP, Command Tiers); `compile` never resolves. Wiring the
//! resolved dependency index in is a later phase — today the compile core reads
//! path deps from the nearest manifest itself.

use std::fmt::Write as _;
use std::io::Write as _;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lexopt::Arg::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use wado_lsp::host::discovery::Disk;
use wado_manifest::MetadataSection;

use crate::args::{self, CliExit};
use crate::compile::{self, Artifact, CompileFlags, CompileOptions};
use crate::fingerprint::Input;
use crate::input_cache::{InputCache, Slot};
use crate::kiln_provider::hex32;
use crate::knobs::{
    CompileKnobOpt, CompileKnobs, EmbedOpt, EmbedOptions, EmbedPolicy, HostKnobOpt, HostKnobs,
};
use crate::manifest::{self, EntryPointKind, openable_dir};
use crate::metadata_embed::embed_metadata_sections;
use wado_host::atomic_write::write_atomic;
use wado_host::runner::ENTRY_SECTION;
use wado_host::sync::joined;

/// The `-o` that writes the component to stdout.
const STDOUT: &str = "-";

pub struct BuildOptions {
    /// The one file to build, in place of the manifest's worlds.
    input: Option<String>,
    /// `--world <fq>`: build only this hosted world. Mutually exclusive with `lib`.
    world: Option<String>,
    /// `--lib`: build only the library world. Mutually exclusive with `world`.
    lib: bool,
    /// `-o`: output path; valid only when a single world is selected.
    output: Option<String>,
    /// `-o -`: write what a driver of this kind would run to stdout, in place of
    /// building the manifest's worlds.
    driver: Option<EntryPointKind>,
    /// `--embed-entry`: name the entry in the component.
    embed_entry: bool,
    knobs: CompileKnobs,
    pub host: HostKnobs,
    embed: EmbedOptions,
}

#[derive(Clone, Copy)]
enum Opt {
    World,
    Lib,
    Output,
    EmbedEntry,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::World,
        Self::Lib,
        Self::Output,
        Self::EmbedEntry,
        Self::Help,
    ];

    const KNOBS: &[CompileKnobOpt] = &[
        CompileKnobOpt::OptLevel,
        CompileKnobOpt::InlineThreshold,
        CompileKnobOpt::InlineHotCallsiteThreshold,
        CompileKnobOpt::InlineColdCallsiteThreshold,
        CompileKnobOpt::InlineGrowth,
        CompileKnobOpt::OptIterations,
        CompileKnobOpt::Specializations,
        CompileKnobOpt::OptThreads,
        CompileKnobOpt::PredictableBranchThreshold,
        CompileKnobOpt::ProfileSummaryCutoffHot,
        CompileKnobOpt::ProfileSummaryCutoffCold,
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
                desc: "Output path (only with a single --world / --lib; default: build/<world>.wasm)\n`-` writes the component `wado run` or `wado serve` would run to stdout",
            },
            Self::EmbedEntry => args::OptSpec {
                long: Some("embed-entry"),
                short: None,
                value: None,
                desc: "Name the entry in the component, in the `wado:entry` custom section\n(only with -o -)",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado build [options] [<file.wado>]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Build the project's declared worlds from wado.toml into build/<world>.wasm."
    )
    .unwrap();
    writeln!(
        buf,
        "A named input, which needs -o -, builds that file as `wado run` would."
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
            .add(HostKnobOpt::ALL, |o| o.spec())
            .add(EmbedOpt::ALL, |o| o.spec())
            .add(args::ParamOpt::ALL, |o| o.spec())
            .render()
    )
    .unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<BuildOptions, CliExit> {
    let usage = format_usage();
    let mut input: Option<String> = None;
    let mut world: Option<String> = None;
    let mut lib = false;
    let mut output: Option<String> = None;
    let mut embed_entry = false;
    let mut knobs = CompileKnobs::default();
    let mut host = HostKnobs::default();
    let mut embed = EmbedOptions::default();

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(k) = args::match_opt(&arg, Opt::KNOBS, |k| k.spec()) {
            knobs.apply(k, &mut parser)?;
        } else if let Some(h) = args::match_opt(&arg, HostKnobOpt::ALL, |h| h.spec()) {
            host.apply(h);
        } else if let Some(p) = args::match_opt(&arg, args::ParamOpt::ALL, |p| p.spec()) {
            args::apply_param_opt(&mut knobs.params, p, &mut parser)?;
        } else if let Some(e) = args::match_opt(&arg, EmbedOpt::ALL, |e| e.spec()) {
            embed.apply(e)?;
        } else if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::World => world = Some(args::require_string(&mut parser)?),
                Opt::Lib => lib = true,
                Opt::Output => output = Some(args::require_string(&mut parser)?),
                Opt::EmbedEntry => embed_entry = true,
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            args::reject_multiple_inputs(&input)?;
            input = Some(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    if lib && world.is_some() {
        return Err(CliExit::error(
            "`--lib` and `--world` are mutually exclusive",
        ));
    }
    let to_stdout = output.as_deref() == Some(STDOUT);
    if input.is_some() && !to_stdout {
        return Err(CliExit::error(
            "a named input needs `-o -`; `wado compile` writes one file to a path",
        ));
    }
    if embed_entry && !to_stdout {
        return Err(CliExit::error("`--embed-entry` takes `-o -`"));
    }
    let driver = to_stdout
        .then(|| driver_kind(world.as_deref(), lib, embed))
        .transpose()?;

    Ok(BuildOptions {
        input,
        world,
        lib,
        output,
        driver,
        embed_entry,
        knobs,
        host,
        embed,
    })
}

/// The driver whose component `-o -` writes, which takes only what that driver
/// takes.
fn driver_kind(
    world: Option<&str>,
    lib: bool,
    embed: EmbedOptions,
) -> Result<EntryPointKind, CliExit> {
    if lib {
        return Err(CliExit::error(
            "`--lib` does not take `-o -`: no driver runs a library",
        ));
    }
    if embed.wit != EmbedPolicy::Auto || embed.metadata != EmbedPolicy::Auto {
        return Err(CliExit::error(
            "`--embed-wit` and `--embed-metadata` do not take `-o -`: \
             a driver's component embeds at the defaults",
        ));
    }
    let Some(world) = world else {
        return Ok(EntryPointKind::Command);
    };
    [EntryPointKind::Command, EntryPointKind::Service]
        .into_iter()
        .find(|kind| kind.world_fq() == Some(world))
        .ok_or_else(|| {
            CliExit::error(format!(
                "no driver runs the world `{world}`; `-o -` takes \
                 wasi:cli/command or wasi:http/service"
            ))
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
    let project = manifest::discover(openable_dir(dir), &Disk)
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
    if let Some(kind) = opts.driver {
        return Box::pin(emit_driver_component(opts, kind)).await;
    }
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

/// Write the component a driver of `kind` would run to stdout, which is how a
/// `wado run-*` subcommand compiles. The entry section goes on here, past the
/// cache, so what `wado run` keeps and runs is unchanged by it.
async fn emit_driver_component(opts: BuildOptions, kind: EntryPointKind) -> Result<(), CliExit> {
    // No usage: the person reading this asked a runner, not `wado build`.
    let entry = manifest::resolve_input(opts.input, kind, "")?;
    let wasm = Box::pin(build_for_driver(&entry, kind, opts.knobs)).await?;
    let wasm = if opts.embed_entry {
        let section = MetadataSection {
            name: ENTRY_SECTION.to_owned(),
            value: entry,
        };
        embed_metadata_sections(wasm, &[section])
    } else {
        wasm
    };
    // `Stdout` holds what follows the last newline, and a failure writing that
    // at exit goes unreported, so a truncated component would read as written.
    let mut stdout = std::io::stdout();
    stdout
        .write_all(&wasm)
        .and_then(|()| stdout.flush())
        .map_err(|e| CliExit::error(format!("writing the component to stdout: {e}")))
}

/// Build every target concurrently, printing each failure as it lands, so one
/// broken world does not hide another. The
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
/// build` and the run/serve drivers so a run artifact matches a built one.
pub async fn build_world_component(
    target: &BuildTarget,
    knobs: &CompileKnobs,
    embed: EmbedOptions,
) -> Result<Artifact, CliExit> {
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
    compile::compile_to_artifact(opts).await
}

/// Produce the runnable component for a driver (`run` / `serve`). In a project
/// (a nearby `wado.toml`), build the world through the shared core — metadata
/// embedded, written to `build/<world segment>.wasm`, matching `wado build`.
/// With no project, fall back to the standalone compile primitive (in-memory,
/// no artifact on disk). Metadata and WIT embed at their defaults, matching
/// `wado build`, so a run/serve artifact is identical to a built one.
///
/// A component built before, from sources unchanged since and with the same
/// metadata to embed, is taken from the cache instead, and nothing is written.
pub async fn build_for_driver(
    entry: &str,
    kind: EntryPointKind,
    knobs: CompileKnobs,
) -> Result<Vec<u8>, CliExit> {
    let target_world = kind.world_fq().expect("a driver runs a hosted world");
    let flags = CompileFlags {
        // The compiler picks the cli/command world by default.
        target_world: (kind != EntryPointKind::Command).then(|| target_world.to_owned()),
        knobs,
        ..CompileFlags::default()
    };
    let entry_path = Path::new(entry);
    let project = compile::load_nearest_manifest(entry_path, &Disk).map_err(CliExit::error)?;
    let metadata = project.as_ref().map_or_else(String::new, |project| {
        let sections = compile::package_metadata(project, EmbedOptions::default(), &flags.knobs);
        sha256_hex(format!("{sections:?}").as_bytes())
    });
    let cache = InputCache::new("run", &flags, target_world);
    let slot = cache.slot(entry_path);
    if let Some(wasm) = slot
        .as_ref()
        .and_then(|slot| cached_component(slot, &metadata))
    {
        return Ok(wasm);
    }
    let (wasm, reads) = if let Some(project) = project {
        let segment = compile::world_path_segment(target_world);
        let target = BuildTarget {
            entry: PathBuf::from(entry),
            output: compile::build_output_path(&project.root, &segment),
            lib_world: None,
            target_world: Some(target_world.to_string()),
        };
        let artifact =
            build_world_component(&target, &flags.knobs, EmbedOptions::default()).await?;
        (artifact.bytes, artifact.reads)
    } else {
        let (result, reads) = compile::compile_recording_reads(entry, &flags).await?;
        (result.wasm, reads)
    };
    if let (Some(reads), Some(slot)) = (reads, &slot) {
        cache_component(slot, reads, &wasm, metadata);
    }
    Ok(wasm)
}

/// A component kept beside its entry, which names it by its SHA-256, so a
/// torn or replaced file is a miss. Its own copy rather than `build/`'s, which
/// `wado build` and concurrent drivers overwrite.
fn cached_component(slot: &Slot<'_>, metadata: &str) -> Option<Vec<u8>> {
    let (sha256, embedded): (String, String) = slot.lookup()?;
    if embedded != metadata {
        return None;
    }
    let wasm = std::fs::read(slot.path("wasm")).ok()?;
    (sha256_hex(&wasm) == sha256).then_some(wasm)
}

fn cache_component(slot: &Slot<'_>, reads: Vec<Input>, wasm: &[u8], metadata: String) {
    // Losing the cache is never an error: the next run compiles again.
    if write_atomic(&slot.path("wasm"), wasm).is_ok() {
        slot.store(reads, (sha256_hex(wasm), metadata));
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex32(&Sha256::digest(bytes).into())
}

#[cfg(test)]
mod tests {
    use wado_host::runner::RUN_KNOBS;

    use super::Opt;

    #[test]
    fn build_takes_every_compile_knob_a_runner_passes_it() {
        let taken: Vec<String> = Opt::KNOBS.iter().map(|k| k.spec().flag()).collect();
        for knob in RUN_KNOBS {
            let flag = knob.spec().flag();
            assert!(taken.contains(&flag), "`wado build` refuses {flag}");
        }
    }
}
