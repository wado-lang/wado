//! `wado check` — a build's diagnostics without its output. Runs the Kiln
//! pipeline as `wado compile` does and treats Kiln warnings as errors.
//!
//! See [WEP: Kiln](../../docs/wep-2026-04-12-kiln.md), section "The
//! `wado check` command".

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use lexopt::Arg::Value;
use wado_compiler::Code;
use wado_compiler::kiln::InvocationIndex;
use wado_compiler::path::normalize;

use crate::args::{self, CliExit};
use crate::build;
use crate::compile::{
    Settled, attach_manifest_and_component_deps, build_load, load_nearest_manifest,
    maybe_run_pipeline_for, pipeline_exit,
};
use crate::compiler_host::FilesystemCompilerHost;
use crate::dep_component::Acquisition;
use crate::discover;
use crate::knobs::{CompileKnobOpt, CompileKnobs};
use crate::manifest;
use crate::test::test_filters;

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

    const KNOBS: &[CompileKnobOpt] = &[
        CompileKnobOpt::LogLevel,
        CompileKnobOpt::NoCache,
        CompileKnobOpt::NoLint,
        CompileKnobOpt::Lint,
        CompileKnobOpt::LintGenerated,
    ];

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
    writeln!(buf, "Usage: wado check [options] [file.wado | dir]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Verify Wado sources (and their Kiln generators) without emitting Wasm.\n\
         Given a directory, or nothing, checks every world that directory's\n\
         wado.toml declares — the targets `wado build` builds.",
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Runs every Kiln generator as a build does, writing what it produces. By\n\
         default a Kiln warning exits non-zero, which is what gates CI; --warn\n\
         keeps warnings as warnings.",
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
        let project = build::project_here(NO_PROJECT)?;
        return check_declared_worlds(&project, &opts).await;
    };
    let path = PathBuf::from(&input);
    // A directory names the project to check, as it does for `test` and
    // `format`; `wado.toml` then supplies the entries.
    if path.is_dir() {
        let project = build::project_at(&path, NO_PROJECT)?;
        return check_declared_worlds(&project, &opts).await;
    }
    let project = load_nearest_manifest(&path).map_err(CliExit::error)?;
    let world = check_world(opts.target_world.as_deref(), &path, project.as_ref());
    check_entry(&path, world, project, true, &opts, &mut Settled::default())
        .await
        .map(drop)
}

const NO_PROJECT: &str = "no wado.toml found; name a file to check \
                          (`wado check <file.wado>`) or a directory under a project";

/// Check every world `wado.toml` declares, selected the way `wado build`
/// selects its targets. Same analysis as a single file, once per entry.
async fn check_declared_worlds(
    project: &manifest::ProjectManifest,
    opts: &CheckOptions,
) -> Result<(), CliExit> {
    let mut targets = build::declared_worlds(project)?;
    if let Some(world_fq) = &opts.target_world {
        build::retain_world(&mut targets, world_fq)?;
    }
    if targets.is_empty() {
        return Err(CliExit::error(
            "no world to check; declare [package].lib or a [world] entry in wado.toml",
        ));
    }
    // Unused is a question about the package, which one entry cannot answer:
    // its siblings and its tests use what it leaves alone. With `--world`
    // narrowing the entries, the package's production code is unknown, so each
    // entry answers alone.
    let whole_package = opts.target_world.is_none();
    let mut entry_runs = Vec::with_capacity(targets.len());
    let mut settled = Settled::default();
    for target in &targets {
        let world = match (&target.lib_world, &target.target_world) {
            (Some(fq), _) => CheckWorld::Lib(fq.clone()),
            (_, Some(fq)) => CheckWorld::Target(fq.clone()),
            _ => unreachable!("a build target names exactly one world"),
        };
        let entry_project = load_nearest_manifest(&target.entry).map_err(CliExit::error)?;
        // Each entry compiles from its own invocations, so its walk passes
        // over nothing another entry's settled.
        let mut walked = Settled::default();
        entry_runs.push(
            check_entry(
                &target.entry,
                world,
                entry_project,
                !whole_package,
                opts,
                &mut walked,
            )
            .await?,
        );
        settled.extend(walked);
    }
    if whole_package && opts.knobs.lints != wado_compiler::Lints::Off {
        check_package_unused(project, &targets, &entry_runs, settled, opts).await?;
    }
    Ok(())
}

/// Report the unused items of the package rooted at `project`, decided over
/// every member at once: what `targets` reach is production code, the rest of
/// the members are its tests. `entry_runs` holds what each target's own check
/// generated, and `settled` the modules those checks walked, so a generator
/// clause runs and warns once, in the first walk that reaches its module.
async fn check_package_unused(
    project: &manifest::ProjectManifest,
    targets: &[build::BuildTarget],
    entry_runs: &[InvocationIndex],
    mut settled: Settled,
    opts: &CheckOptions,
) -> Result<(), CliExit> {
    let root = &project.root;
    // The spelling a member is matched by, so `src/../lib.wado` names
    // `lib.wado`. A manifest entry outside the root is no member.
    let relative = |path: &Path| {
        Some(normalize(&path.strip_prefix(root).ok()?.to_string_lossy()))
            .filter(|rel| !rel.starts_with("../"))
    };
    // The members are the files `wado test` runs, and the entries, which a
    // `[test].exclude` may leave out of those.
    let mut files = discover::package_files(root, &|pkg| test_filters(pkg, &[]))?;
    let mut members: Vec<String> = files
        .iter()
        .map(|f| relative(f).expect("a package's members lie under its root"))
        .collect();
    // Each entry's first check, by the spelling its member is matched by.
    let mut runs: IndexMap<String, (&build::BuildTarget, &InvocationIndex)> = IndexMap::new();
    for (target, run) in targets.iter().zip(entry_runs) {
        if let Some(entry) = relative(&target.entry) {
            runs.entry(entry).or_insert((target, run));
        }
    }
    let entries: Vec<String> = runs.keys().cloned().collect();
    for entry in &entries {
        if !members.contains(entry) {
            files.push(root.join(entry));
            members.push(entry.clone());
        }
    }
    let mut read = Vec::with_capacity(files.len());
    for (file, member) in files.into_iter().zip(members) {
        let source = std::fs::read_to_string(&file)
            .map_err(|e| CliExit::error(format!("{}: {e}", file.display())))?;
        // A generated module belongs to the module importing it, never a member
        // of its own: a Kiln output under the root would load once as each.
        if entries.contains(&member) || !wado_compiler::is_generated_source(&source) {
            read.push((file, member, source));
        }
    }
    let package = wado_compiler::PackageSources {
        root: root.to_string_lossy().into_owned(),
        members: read.iter().map(|(_, member, _)| member.clone()).collect(),
        entries,
    };
    let host = attach_manifest_and_component_deps(
        FilesystemCompilerHost::with_log_level(root.clone(), opts.knobs.log_level),
        Some(project),
        root,
        &read
            .iter()
            .map(|(_, _, source)| source.as_str())
            .collect::<Vec<_>>(),
        Acquisition::Build,
    )
    .await
    .map_err(CliExit::error)?;
    let mut invocations = InvocationIndex::default();
    for (file, member, _) in &read {
        // `check_entry` names its entry by the path as given.
        if let Some((target, index)) = runs.get(member) {
            let run_name = target.entry.to_string_lossy();
            package.adopt_member_invocations(member, &run_name, index, &mut invocations);
            continue;
        }
        let member_identity = package.member_identity(member);
        // The walk names what the member imports from the member's directory,
        // so the host placing those modules in packages must stand there too.
        let member_dir = file.parent().expect("a member names a file");
        let outcome = maybe_run_pipeline_for(
            file,
            Some(&member_identity),
            &host.rebased(member_dir.to_path_buf()),
            opts.knobs.no_cache,
            Some(project.clone()),
            &mut settled,
        )
        .await
        .map_err(|e| pipeline_exit(e, "check"))?;
        package.adopt_member_invocations(
            member,
            &member_identity,
            &outcome.invocations,
            &mut invocations,
        );
    }
    let lints = wado_compiler::package_unused_diagnostics(
        &host,
        &package,
        invocations,
        opts.knobs.lints,
        opts.knobs.log_level,
    )
    .await;
    for lint in lints.into_iter().flatten() {
        wado_compiler::CompilerHost::emit_diagnostic(&host, lint);
    }
    check_outcome(&host, false, opts)
}

/// Check `path` against `world`, under `manifest_pair`, the manifest nearest it.
/// `unused` reports the items this entry alone leaves unused. Answers what the
/// entry's generators recorded; `settled` gains the modules its walk reached.
async fn check_entry(
    path: &Path,
    world: CheckWorld,
    manifest_pair: Option<manifest::ProjectManifest>,
    unused: bool,
    opts: &CheckOptions,
    settled: &mut Settled,
) -> Result<InvocationIndex, CliExit> {
    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let source = std::fs::read_to_string(path)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", path.display())))?;
    let (target_world, lib_world) = world.options();
    // The pipeline a build runs, writes included. A route of its own would be a
    // second behaviour to hold in step with this one.
    let (host, invocations) = Box::pin(build_load(
        path,
        &source,
        manifest_pair,
        FilesystemCompilerHost::with_log_level(base_path, opts.knobs.log_level),
        opts.knobs.no_cache,
        "check",
        settled,
    ))
    .await?;

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
        lints: opts.knobs.lints,
        unused_diagnostics: unused,
        invocations: invocations.clone(),
        ..Default::default()
    };
    let entry_name = path.to_string_lossy().into_owned();
    let compile_result =
        wado_compiler::compile_with_options(&source, &host, Some(&entry_name), compiler_options)
            .await;

    check_outcome(&host, compile_result.is_err(), opts)?;
    Ok(invocations)
}

/// How a check ends once `host` holds what it reported: an error fails it,
/// `failed` among them, and so does a Kiln warning unless `--warn` was given.
fn check_outcome(
    host: &FilesystemCompilerHost,
    failed: bool,
    opts: &CheckOptions,
) -> Result<(), CliExit> {
    if failed || host.has_errors() {
        return Err(CliExit::silent_failure(1));
    }
    let has_kiln_warnings = host
        .diagnostics()
        .into_iter()
        .any(|d| is_kiln_diagnostic(&d.code));
    if !opts.warn_only && has_kiln_warnings {
        return Err(CliExit::error(
            "wado check: Kiln integrity check failed — \
             see the warnings above. Pass --warn to keep warnings as warnings.",
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

/// Not `KilnGeneratedRegenerated`: the check writes what it generates, so an
/// overwrite is what an edited input is supposed to produce.
fn is_kiln_diagnostic(code: &Code) -> bool {
    matches!(
        code,
        Code::KilnStaleCache
            | Code::KilnGeneratorForbiddenImport
            | Code::KilnNoGeneratedModule
            | Code::KilnGeneratedModified
    )
}
