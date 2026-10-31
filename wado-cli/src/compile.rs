use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use wado_manifest::{DependencySource, MANIFEST_FILENAME, MetadataSection, registry_url};

use lexopt::Arg::Value;
use wado_compiler::codegen_flags::Pgo;
use wado_compiler::pgo::{self, Profile};
use wado_compiler::world_registry::TEST_WORLD;
use wado_compiler::{CodegenFlags, CompilerHost as _, Diagnostic, Logger, Severity, wit_bundle};
use wado_lsp::host::discovery::normalize_path;
use wado_lsp::host::{PackageWalk, dependency_key};

use crate::args::{self, CliExit};
use crate::build_dep::{GENERATOR_WORLD_FQ, locked_generator_versions, spec_key};
use crate::compiler_host::FilesystemCompilerHost;
use crate::coverage_host::profile_file;
use crate::dep_component::{
    Acquisition, Resolution, fetch_component_dependencies, resolve_inline_component_dependencies,
    resolve_inline_git_dependencies,
};
use crate::fingerprint::Input;
use crate::git::materialize;
use crate::kiln_provider::{CliGeneratorProvider, RegistryContext, relative_to};
use crate::knobs::{CompileKnobOpt, CompileKnobs, EmbedOpt, EmbedOptions};
use crate::manifest;
use crate::manifest::{DiscoveryError, absolute, manifest_in, openable_dir, resolution_inputs};
use crate::metadata_embed::{clean_git_revision, embed_metadata_sections};
use crate::run_cache::RunCache;
use crate::wit::default_interface_name;
use wado_compiler::world_registry::WorldSurface;
use wado_host::kiln::driver::{
    Harvested, PipelineError, PipelineOutcome, ProviderError, emit_counting_errors,
    harvest_invocations, merge_across_packages, run_pipeline,
};
use wado_host::sync::joined;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputFormat {
    Wasm,
    Wat,
}

impl OutputFormat {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "wasm" => Some(Self::Wasm),
            "wat" => Some(Self::Wat),
            _ => None,
        }
    }

    fn from_extension(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(Self::from_str)
    }
}

pub struct CompileOptions {
    pub input: String,
    pub output: Option<String>,
    pub format: Option<OutputFormat>,
    pub wat_to_stdout: bool,
    /// The build knobs, shared with every other compiling subcommand.
    pub knobs: CompileKnobs,
    /// Whether the WIT and `[package]` metadata sections are embedded. The
    /// embedded WIT section is always the self-contained full closure (a
    /// `local`, registry-referencing section is not encodable; see WEP
    /// §"Text and section are different deliverables").
    pub embed: EmbedOptions,
    pub target_world: Option<String>,
    /// Library world FQ (`namespace:name/name@version`) for `--lib`. When set,
    /// the compiler synthesizes a library world from the entry module's
    /// `export fn`s and exports them in the package's default interface.
    pub lib_world: Option<String>,
    /// The entry came from a manifest (no path argument, or a directory
    /// argument), so the build is the package's declared artifact and carries
    /// its metadata. False for an explicit `.wado` file argument.
    pub manifest_driven: bool,
}

impl CompileOptions {
    /// A manifest-driven build of one world: metadata embedded, output at
    /// `output`. Exactly one of `lib_world` / `target_world` is `Some`, selecting
    /// the world as `--lib` / `--world` do. Shared by `wado build` and
    /// `wado publish`; callers override `knobs` / `embed` as needed before
    /// calling [`run`].
    #[must_use]
    pub fn for_world_build(
        input: String,
        output: PathBuf,
        lib_world: Option<String>,
        target_world: Option<String>,
    ) -> Self {
        Self {
            input,
            output: Some(output.to_string_lossy().into_owned()),
            format: Some(OutputFormat::Wasm),
            wat_to_stdout: false,
            knobs: CompileKnobs::default(),
            embed: EmbedOptions::default(),
            target_world,
            lib_world,
            manifest_driven: true,
        }
    }

    /// The Component Model world FQ this build targets: the `--lib` world, then
    /// `--world`, then the CLI default. Single source of truth for the WIT
    /// embed and the default output path.
    fn target_world_fq(&self) -> &str {
        self.lib_world
            .as_deref()
            .or(self.target_world.as_deref())
            .unwrap_or(DEFAULT_WORLD)
    }

    #[must_use]
    pub fn flags(&self) -> CompileFlags {
        CompileFlags {
            knobs: self.knobs.clone(),
            target_world: self.target_world.clone(),
            lib_world: self.lib_world.clone(),
            ..CompileFlags::default()
        }
    }
}

/// The world `wado compile` targets when neither `--world` nor `--lib` is given.
const DEFAULT_WORLD: &str = "wasi:cli/command";

/// The build-output directory at the manifest root, shared with kiln's
/// `build/kiln/...` layout (see `kiln_provider`).
const BUILD_DIR: &str = "build";

/// What one compile needs beyond the shared [`CompileKnobs`]: which world it
/// targets and what the caller wants retained from the result.
///
/// `target_world` stays `Option` so each subcommand can pin its own default
/// (`run` → cli/command, `serve` → http/service, `test` → test) while letting
/// `compile` accept `--world`.
#[derive(Clone, Debug, Default)]
pub struct CompileFlags {
    pub knobs: CompileKnobs,
    pub target_world: Option<String>,
    /// The tests the test world keeps. Forwarded to
    /// `CompilerOptions::test_selection`.
    pub test_selection: wado_compiler::TestSelection,
    /// Library world FQ for `--lib`. Forwarded to `CompilerOptions::lib_world`.
    pub lib_world: Option<String>,
    /// Retain the WIR module in the result. `wado compile` sets it when
    /// embedding WIT, to read the world surface without a second compile.
    pub retain_wir: bool,
    /// When `Some`, the compile retains the WIT subset
    /// (`CompileResult::wit_emit_snapshot`) for encoding the `component-type`
    /// section (issue #1654). Set by `wado compile` when embedding.
    pub embed_wit_contract: Option<wado_compiler::wit_emit::WitContract>,
    /// `wado test --coverage` or `--pgo`. Forwarded to
    /// `CompilerOptions::coverage`.
    pub coverage: Option<wado_compiler::coverage::CoverageScope>,
}

#[derive(Clone, Copy)]
enum Opt {
    Output,
    Format,
    WatToStdout,
    World,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Output,
        Self::Format,
        Self::WatToStdout,
        Self::World,
        Self::Help,
    ];

    const KNOBS: &[CompileKnobOpt] = &[
        CompileKnobOpt::OptLevel,
        CompileKnobOpt::InlineThreshold,
        CompileKnobOpt::InlineGrowth,
        CompileKnobOpt::OptIterations,
        CompileKnobOpt::Specializations,
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
            Self::Output => args::OptSpec {
                long: None,
                short: Some('o'),
                value: Some("<file>"),
                desc: "Output file path (default: <input>.wasm)",
            },
            Self::Format => args::OptSpec {
                long: Some("format"),
                short: None,
                value: Some("<fmt>"),
                desc: "Output format: wasm, wat (default: guessed from -o extension)",
            },
            Self::WatToStdout => args::OptSpec {
                long: Some("wat-to-stdout"),
                short: None,
                value: None,
                desc: "Output WAT to stdout (shorthand for --format wat -o /dev/stdout)",
            },
            Self::World => args::WORLD_SPEC,
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado compile [options] <file.wado>").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Compile a Wado source file to WebAssembly.").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(
        buf,
        "{}",
        args::OptsHelp::default()
            .add(Opt::ALL, |o| o.spec())
            .add(Opt::KNOBS, |o| o.spec())
            .add(EmbedOpt::WIT_ONLY, |o| o.spec())
            .add(args::ParamOpt::ALL, |o| o.spec())
            .render()
    )
    .unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<CompileOptions, CliExit> {
    let usage = format_usage();
    let mut output: Option<String> = None;
    let mut format: Option<OutputFormat> = None;
    let mut input: Option<String> = None;
    let mut wat_to_stdout = false;
    let mut target_world: Option<String> = None;
    let mut knobs = CompileKnobs::default();
    let mut embed = EmbedOptions::default();
    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(k) = args::match_opt(&arg, Opt::KNOBS, |k| k.spec()) {
            knobs.apply(k, &mut parser)?;
        } else if let Some(p) = args::match_opt(&arg, args::ParamOpt::ALL, |p| p.spec()) {
            args::apply_param_opt(&mut knobs.params, p, &mut parser)?;
        } else if let Some(e) = args::match_opt(&arg, EmbedOpt::WIT_ONLY, |e| e.spec()) {
            embed.apply(e)?;
        } else if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Output => output = Some(args::require_string(&mut parser)?),
                Opt::Format => {
                    let fmt_str = args::require_string(&mut parser)?;
                    format = Some(OutputFormat::from_str(&fmt_str).ok_or_else(|| {
                        CliExit::error(format!("unknown format '{fmt_str}'. Use 'wasm' or 'wat'"))
                    })?);
                }
                Opt::WatToStdout => wat_to_stdout = true,
                Opt::World => target_world = Some(args::require_string(&mut parser)?),
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            args::reject_multiple_inputs(&input)?;
            input = Some(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    // `compile` is the file-scoped primitive: it takes one explicit `.wado`
    // file and never reads `wado.toml` to discover an entry or embed metadata.
    // Project builds (worlds, metadata, `build/<world>.wasm`) are `wado build`.
    let Some(input) = input else {
        return Err(CliExit::error_with_usage(
            "no input file specified; `wado compile` takes a single .wado file \
             (use `wado build` to build a project from wado.toml)",
            &usage,
        ));
    };
    if Path::new(&input).is_dir() {
        return Err(CliExit::error_with_usage(
            "`wado compile` takes a single .wado file, not a directory \
             (use `wado build` to build a project from wado.toml)",
            &usage,
        ));
    }

    Ok(CompileOptions {
        input,
        output,
        format,
        wat_to_stdout,
        knobs,
        embed,
        target_world,
        lib_world: None,
        manifest_driven: false,
    })
}

pub struct TryCompileError {
    pub is_todo_module: bool,
    pub message: Option<String>,
}

impl TryCompileError {
    /// A failure before the compiler ran, printed as it is recorded.
    fn reported(message: String) -> Self {
        eprintln!("{message}");
        Self {
            is_todo_module: false,
            message: Some(message),
        }
    }
}

/// Compile without bailing. Used by the test runner so `#![TODO]` modules
/// can be observed (via `CompileFailure::is_todo_module`) rather than
/// aborting the whole batch.
pub async fn try_compile(
    filename: &str,
    flags: &CompileFlags,
) -> Result<wado_compiler::CompileResult, wado_compiler::CompileFailure> {
    try_compile_with_run_cache(filename, flags, None)
        .await
        .map_err(|e| wado_compiler::CompileFailure {
            is_todo_module: e.is_todo_module,
        })
}

/// `try_compile` with an optional [`RunCache`]. `wado test` passes one shared
/// across every fixture, so the run resolves each generator once and reads one
/// view of the tree; `None` keeps the per-host cache for single-file builds.
pub async fn try_compile_with_run_cache(
    filename: &str,
    flags: &CompileFlags,
    run_cache: Option<Arc<RunCache>>,
) -> Result<wado_compiler::CompileResult, TryCompileError> {
    let path = Path::new(filename);

    let source = fs::read_to_string(path).map_err(|e| {
        TryCompileError::reported(format!("Error reading '{}': {e}", path.display()))
    })?;

    let base_path = path.parent().map(Path::to_path_buf).unwrap_or_default();
    // Load the nearest manifest once: it seeds both the host's `[dependencies]`
    // index and the Kiln pipeline's `[build-dependencies]` resolution.
    let manifest_pair =
        load_nearest_manifest(path).map_err(|e| TryCompileError::reported(e.to_string()))?;
    let base_host =
        FilesystemCompilerHost::with_log_level(base_path.clone(), flags.knobs.log_level);
    let base_host = match run_cache {
        Some(cache) => base_host.with_shared_run_cache(cache),
        None => base_host,
    };
    // The entry file never goes through `load_source`, so the watch would
    // otherwise miss the one file the caller named.
    base_host.run_cache().observe(path, None, source.as_bytes());
    if let Some(project) = &manifest_pair {
        for file in resolution_inputs(&project.root) {
            base_host.run_cache().depend_on_file(&file);
        }
    }
    let host = attach_manifest_and_component_deps(
        base_host,
        manifest_pair.as_ref(),
        &base_path,
        &[&source],
        Acquisition::Build,
    )
    .await
    .map_err(|e| {
        TryCompileError::reported(format!("Error fetching component dependencies: {e}"))
    })?;

    let pipeline_outcome = maybe_run_pipeline(path, &host, flags.knobs.no_cache, manifest_pair)
        .await
        .map_err(|e| TryCompileError::reported(e.to_string()))?;

    let knobs = &flags.knobs;
    let profile = entry_profile(path, knobs, flags.target_world.as_deref(), &host)?;
    let options = wado_compiler::CompilerOptions {
        opt_level: knobs.opt_level.to_compiler(),
        skip_validation: knobs.skip_validation,
        opt: knobs.opt,
        parallelism: knobs.optimize_threads(),
        log_level: Some(knobs.log_level),
        allocator: knobs.allocator.clone(),
        codegen_flags: knobs.codegen_flags.clone(),
        lints: knobs.lints,
        params: knobs.params.clone(),
        target_world: flags.target_world.clone(),
        invocations: pipeline_outcome.invocations,
        test_selection: flags.test_selection.clone(),
        lib_world: flags.lib_world.clone(),
        retain_wir: flags.retain_wir,
        embed_wit_contract: flags.embed_wit_contract.clone(),
        coverage: flags.coverage,
        profile,
        ..Default::default()
    };

    wado_compiler::compile_with_options(&source, &host, Some(filename), options)
        .await
        .map_err(|failure| TryCompileError {
            is_todo_module: failure.is_todo_module,
            message: render_error_diagnostics(&host.diagnostics()),
        })
}

/// The profile a compile of the entry `path` applies: the one `wado test --pgo`
/// records its workloads into, where `-f pgo` is in force. The compile depends
/// on that file, there or not, so a cached build follows it.
fn entry_profile(
    path: &Path,
    knobs: &CompileKnobs,
    target_world: Option<&str>,
    host: &FilesystemCompilerHost,
) -> Result<Option<Arc<Profile>>, TryCompileError> {
    let test_world = target_world == Some(TEST_WORLD);
    let parsed = CodegenFlags::parse(
        &knobs.codegen_flags,
        knobs.opt_level.to_compiler(),
        test_world,
    );
    // An unknown flag is the compile's to report.
    let Ok(CodegenFlags { pgo, .. }) = parsed else {
        return Ok(None);
    };
    if !pgo.applies() {
        return Ok(None);
    }
    let Some(file) = profile_file(path) else {
        return Ok(None);
    };
    host.run_cache().depend_on_file(&file);
    let text = match fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if pgo == Pgo::Requested {
                Logger::new(host, knobs.log_level).info(format!(
                    "-f pgo: no profile at {}, so none is applied",
                    file.display()
                ));
            }
            return Ok(None);
        }
        Err(e) => {
            return Err(TryCompileError::reported(format!(
                "Error reading '{}': {e}",
                file.display()
            )));
        }
    };
    pgo::parse(&text)
        .map(|profile| Some(Arc::new(profile)))
        .map_err(|e| TryCompileError::reported(format!("{}: {e}", file.display())))
}

/// The errors among `diagnostics`, one per line as `wado compile` prints them.
pub(crate) fn render_error_diagnostics(diagnostics: &[Diagnostic]) -> Option<String> {
    let rendered: Vec<String> = diagnostics
        .iter()
        .filter(|d| matches!(d.severity, Severity::Error | Severity::Fatal))
        .map(|d| match &d.span {
            Some(span) => format!("{}:{}:{}: {}", span.file, span.line, span.column, d.message),
            None => d.message.clone(),
        })
        .collect();
    (!rendered.is_empty()).then(|| rendered.join("\n"))
}

/// Compile a Wado source file and return the produced wasm bytes. On
/// failure, the host has already printed diagnostics — the returned
/// `CliExit::silent_failure` only carries the exit code.
pub async fn compile(filename: &str, flags: &CompileFlags) -> Result<Vec<u8>, CliExit> {
    try_compile(filename, flags)
        .await
        .map(|result| result.wasm)
        .map_err(|_| CliExit::silent_failure(1))
}

/// Compile `filename`, and say what the compile read where none of it is
/// untracked.
pub(crate) async fn compile_recording_reads(
    filename: &str,
    flags: &CompileFlags,
) -> Result<(wado_compiler::CompileResult, Option<Vec<Input>>), CliExit> {
    let scope = RunCache::new().scoped();
    let result = try_compile_with_run_cache(filename, flags, Some(Arc::clone(&scope)))
        .await
        .map_err(|_| CliExit::silent_failure(1))?;
    Ok((result, scope.reads()))
}

/// Seed `host` with the `[dependencies]` index derived from `manifest_pair`
/// (anchored at `base_path`), or return it unchanged when there is no
/// manifest. Shared by `compile`, `check`, and the `query` adapter so every
/// entry point attaches dependency resolution identically.
pub(crate) fn attach_manifest_deps(
    host: FilesystemCompilerHost,
    project: Option<&manifest::ProjectManifest>,
    base_path: &Path,
) -> FilesystemCompilerHost {
    match project {
        Some(project) => host.with_dependency_index(wado_lsp::host::dependency_index_from(
            &project.manifest,
            &project.root,
            base_path,
        )),
        None => host,
    }
}

/// Like [`attach_manifest_deps`], plus resolving registry dependencies as
/// prebuilt components so `use { X } from "ns:pkg"` resolves across the CM
/// boundary. Two sources feed the component map: a manifest `[dependencies]`
/// table entry (lock-pinned) and an inline
/// `use … from "ns:pkg@ver" with { registry }` clause parsed from one of
/// `sources`, each a whole file.
///
/// `tier` chooses how far acquisition may go: [`Acquisition::Build`] pulls
/// whatever is missing and resolves an unpinned version live, while
/// [`Acquisition::Analysis`] pulls only what a pin names and degrades a failure
/// to an `unresolved` hint. A warm, pinned project touches no network either way.
pub(crate) async fn attach_manifest_and_component_deps(
    base_host: FilesystemCompilerHost,
    project: Option<&manifest::ProjectManifest>,
    base_path: &Path,
    sources: &[&str],
    tier: Acquisition,
) -> Result<FilesystemCompilerHost, String> {
    let index = manifest_and_component_index(project, base_path, sources, tier).await?;
    Ok(base_host.with_dependency_index(index).acquiring(tier))
}

/// Build the [`DependencyIndex`] for `project` (path deps + offline registry
/// resolution via the shared `dependency_index_from`), then acquire the cold
/// entries per `tier`. Decoupled from the host so each caller applies it to its
/// own (`compile` verbose, `query` silent).
async fn manifest_and_component_index(
    project: Option<&manifest::ProjectManifest>,
    base_path: &Path,
    sources: &[&str],
    tier: Acquisition,
) -> Result<wado_compiler::DependencyIndex, String> {
    // Acquire for every package of the build, the consumer's and each source
    // dependency's at any depth, inline ones included, before indexing it, so
    // the offline pass finds their git worktrees warm.
    let mut index = match project {
        Some(project) => acquired_index(&project.manifest, &project.root, base_path, tier).await?,
        None => wado_compiler::DependencyIndex::default(),
    };

    let manifest = project.map(|p| &p.manifest);
    for source in sources {
        let inline = resolve_inline_component_dependencies(source, manifest, tier).await?;
        for (name, path) in inline.resolved {
            bind_once(&mut index.components, name, path)?;
        }
        index.unresolved.extend(inline.unresolved);

        // Inline git sources (`use … from "<name>" with { git }`) are source
        // deps: their materialized worktree entry lands in `resolved`, compiled
        // in.
        let inline_git = resolve_inline_git_dependencies(source, tier).await?;
        for (name, path) in inline_git.resolved {
            index.unresolved.swap_remove(&name);
            let entry = PathBuf::from(path);
            bind_once(&mut index.resolved, name, dependency_key(&entry, base_path))?;
            acquire_and_index(&mut index, vec![entry], base_path, tier).await?;
        }
        for (name, reason) in inline_git.unresolved {
            index.unresolved.insert(name, reason);
        }
    }
    Ok(index)
}

/// The index of the package at `root`, whose manifest is `manifest`, keyed from
/// `base_path`, with what it and each source dependency it reaches depend on
/// acquired per `tier`.
async fn acquired_index(
    manifest: &wado_manifest::Manifest,
    root: &Path,
    base_path: &Path,
    tier: Acquisition,
) -> Result<wado_compiler::DependencyIndex, String> {
    let own = acquire_package(manifest, root, tier).await?;
    let (mut index, entries) = wado_lsp::host::package_index(manifest, root, base_path);
    adopt_components(&mut index, own);
    acquire_and_index(&mut index, entries, base_path, tier).await?;
    Ok(index)
}

/// Acquire what each package a walk through `host` reached, and no manifest
/// declares, depends on, as the declared ones were before the walk. True when
/// that resolved a dependency the walk found unresolved, so the walk saw less
/// than is there.
async fn acquire_undeclared_packages(host: &FilesystemCompilerHost) -> Result<bool, String> {
    let Some((tier, unacquired)) = host.unacquired_packages() else {
        return Ok(false);
    };
    let mut resolved_more = false;
    for (read, offline) in unacquired {
        let root = Path::new(&read.root);
        // A manifest that cannot be read names nothing to acquire, and its
        // offline index already says why.
        let acquired = if let Ok(Some(manifest)) = manifest_in(root) {
            acquired_index(&manifest, root, &read.base, tier).await?
        } else {
            host.adopt_acquired(read, offline);
            continue;
        };
        resolved_more |= resolves_more(&offline, &acquired);
        host.adopt_acquired(read, acquired);
    }
    Ok(resolved_more)
}

/// Whether `acquired` resolves a dependency `offline` does not. Acquisition
/// only adds resolutions, so the count says it, whatever a newly reached
/// package leaves unresolved of its own.
fn resolves_more(
    offline: &wado_compiler::DependencyIndex,
    acquired: &wado_compiler::DependencyIndex,
) -> bool {
    resolved_count(acquired) > resolved_count(offline)
}

/// How many dependencies `index` and the packages it reaches resolve.
fn resolved_count(index: &wado_compiler::DependencyIndex) -> usize {
    let own = index.resolved.len() + index.components.len();
    own + index.packages.values().map(resolved_count).sum::<usize>()
}

/// Add to `index.packages` the package of each entry module in `entries`, and
/// of each source dependency those reach, at any depth, each acquired before
/// its own dependencies are listed, since a git one only resolves once
/// materialized.
async fn acquire_and_index(
    index: &mut wado_compiler::DependencyIndex,
    entries: Vec<PathBuf>,
    base_path: &Path,
    tier: Acquisition,
) -> Result<(), String> {
    let mut walk = PackageWalk::new(entries, base_path);
    while let Some(package) = walk.next(index) {
        let fetched = acquire_package(&package.manifest, &package.root, tier).await?;
        adopt_components(walk.index(index, package), fetched);
    }
    Ok(())
}

/// Acquire per `tier` what the package at `root`, whose manifest is
/// `manifest`, depends on: its locked git worktrees materialized, so the offline
/// git arm resolves them without a separate `wado fetch`, and its registry
/// components pulled. The analysis tier leaves a clone that failed for that arm
/// to report.
async fn acquire_package(
    manifest: &wado_manifest::Manifest,
    root: &Path,
    tier: Acquisition,
) -> Result<Resolution, String> {
    let materialized = materialize_git_dependencies(manifest, root).await;
    if tier == Acquisition::Build {
        materialized?;
    }
    let has_registry_dep = manifest
        .dependencies
        .values()
        .any(|d| matches!(d.source, DependencySource::Registry { .. }));
    if !has_registry_dep {
        return Ok(Resolution::default());
    }
    fetch_component_dependencies(manifest, root, tier).await
}

/// Upgrade the registry dependencies the offline pass left cold in `index`
/// with the components `fetched` pulled, clearing their stale hints.
fn adopt_components(index: &mut wado_compiler::DependencyIndex, fetched: Resolution) {
    for (name, path) in fetched.resolved {
        index.unresolved.swap_remove(&name);
        index.components.insert(name, path);
    }
    index.unresolved.extend(fetched.unresolved);
}

/// Bind the specifier `name` to `path` in `map`, where every file of the build
/// must agree on what one specifier names.
fn bind_once(
    map: &mut wado_compiler::hashmap::IndexMap<String, String>,
    name: String,
    path: String,
) -> Result<(), String> {
    if let Some(bound) = map.get(&name)
        && *bound != path
    {
        return Err(format!(
            "dependency {name:?} names two sources: {bound} and {path}; \
             one specifier resolves to one package"
        ));
    }
    map.insert(name, path);
    Ok(())
}

/// Materialize every locked git dependency's worktree so the build path reads a
/// git dep straight from the warm cache — no separate `wado fetch`. Lock-pinned:
/// a git dep with no `wado.lock` entry is left for the offline arm to report
/// (pointing at `wado update`); a warm worktree is a fast no-op inside
/// [`crate::git::materialize`].
///
/// Only `[dependencies]` are materialized, mirroring `dependency_index_from`,
/// which indexes only `[dependencies]`. `[dev-dependencies]` of any source are
/// not importable at compile/test time yet — a general follow-up tracked in the
/// dependency-management plan, not a git-specific gap.
async fn materialize_git_dependencies(
    manifest: &wado_manifest::Manifest,
    manifest_root: &Path,
) -> Result<(), String> {
    let locked = wado_lsp::host::discovery::locked_git_packages(manifest_root);
    for (name, dep) in &manifest.dependencies {
        let DependencySource::Git { url, .. } = &dep.source else {
            continue;
        };
        let Some((version, sha)) = locked.get(&format!("git+{url}/{name}")) else {
            continue;
        };
        let (url, version, sha) = (url.clone(), version.clone(), sha.clone());
        joined(tokio::task::spawn_blocking(move || materialize(&url, &version, &sha)).await)
            .map_err(|e| format!("materializing git dependency {name:?}: {e}"))?;
    }
    Ok(())
}

/// Everything one entry's Kiln generators need to run: the invocations, the
/// manifest they are anchored in, the provider that resolves their generators,
/// and a host rebased on the manifest root.
pub(crate) struct KilnSetup {
    pub manifest_root: PathBuf,
    pub invocations: Vec<wado_compiler::kiln::Invocation>,
    pub provider: CliGeneratorProvider,
    /// Kiln paths (`from`, `inputs`, `output_dir`) are anchored at the manifest
    /// root, so schemas must be loaded relative to it — not the entry file's
    /// directory, where the main compile host is based.
    pub host: FilesystemCompilerHost,
    /// Loader identities the harvested index is remapped onto.
    identities: wado_compiler::hashmap::IndexMap<String, String>,
    no_cache: bool,
}

/// What a Kiln pipeline inherits from the run it belongs to: the project it
/// resolves against, and the generator chain it continues.
#[derive(Debug, Clone)]
pub(crate) struct KilnRun {
    pub project: Option<manifest::ProjectManifest>,
    pub no_cache: bool,
    /// The generators being compiled, outermost first. A generator whose source
    /// reaches itself has no order to run in, so finding it here is the cycle.
    pub active: Arc<Mutex<Vec<String>>>,
}

impl KilnRun {
    pub(crate) fn entry(project: Option<manifest::ProjectManifest>, no_cache: bool) -> Self {
        Self {
            project,
            no_cache,
            active: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// The modules Kiln walks have harvested, by their path from the package root.
/// A walk passes over them: their clauses ran with the walk that reached them.
pub(crate) type Settled = wado_compiler::hashmap::IndexSet<String>;

/// Collect the inline `with { generator: { ... } }` clauses `entry_file`
/// declares and assemble everything the Kiln pipeline needs from them, past
/// the modules in `settled`, which then holds those the walk reached.
/// `Ok(None)` when the entry declares no generators.
///
/// Shared by [`maybe_run_pipeline`], `wado check` and [`run_nested_pipeline`],
/// so every tier resolves generators through one identical setup.
pub(crate) async fn prepare_kiln(
    entry_file: &Path,
    entry_identity: Option<&str>,
    host: &FilesystemCompilerHost,
    run: &KilnRun,
    settled: &mut Settled,
) -> Result<Option<KilnSetup>, PipelineError> {
    let KilnRun {
        project,
        no_cache,
        active,
    } = run.clone();
    let entry_dir = entry_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // Every invocation is anchored at the package root, so one schema keeps one
    // identity however the pipeline was reached. A file outside any package has
    // only its entry's directory to stand on.
    let manifest_root = project
        .as_ref()
        .map(|p| p.root.clone())
        .unwrap_or(entry_dir);
    let dependencies = host.dependency_index();
    // A walk acquiring an undeclared package's dependencies may reach modules
    // it could not before, so it walks again until it reaches nothing new.
    let unwalked = settled.clone();
    let Harvested {
        invocations,
        identities,
        packages,
        diagnostics: inline_diagnostics,
    } = loop {
        let harvested = collect_inline_invocations_for_entry_with_identities(
            entry_file,
            entry_identity,
            &manifest_root,
            host,
            &dependencies,
            settled,
        )
        .await;
        if !acquire_undeclared_packages(host)
            .await
            .map_err(PipelineError::Acquisition)?
        {
            break harvested;
        }
        settled.clone_from(&unwalked);
    };

    // Fail on a malformed clause here, with its own diagnostics, so the clear
    // error is not buried under the downstream failure of the unredirected
    // `use` falling through to the lexer.
    let inline_errors = emit_counting_errors(host, inline_diagnostics);
    if inline_errors > 0 {
        return Err(PipelineError::InlineClause(inline_errors));
    }

    if invocations.is_empty() {
        return Ok(None);
    }
    let manifest = project.map_or_else(empty_manifest, |p| p.manifest);
    let (theirs, mut ours): (Vec<_>, Vec<_>) = invocations
        .into_iter()
        .partition(|inv| packages.contains_key(&inv.decl_site().module));
    rewrite_build_dep_modules(&mut ours, &manifest, &manifest_root).map_err(manifest_error)?;
    let mut package_manifests = wado_compiler::hashmap::IndexMap::default();
    for mut invocation in theirs {
        let path = packages[&invocation.decl_site().module].as_deref();
        if !package_manifests.contains_key(&path) {
            let root = path.and_then(|path| Path::new(path).parent());
            let manifest = match root {
                Some(root) => manifest_in(root)
                    .map_err(manifest_error)?
                    .map(|m| (root.to_path_buf(), m)),
                None => None,
            };
            package_manifests.insert(path, manifest);
        }
        let package = package_manifests[&path].as_ref();
        let package = package.map(|(root, m)| (root.as_path(), m));
        adopt_dependency_invocation(&mut invocation, package, &manifest_root)?;
        ours.push(invocation);
    }
    let mut invocations = ours;
    rewrite_local_dir_modules(&mut invocations, &manifest_root).map_err(manifest_error)?;
    let invocations = merge_across_packages(invocations).map_err(|disagreements| {
        PipelineError::InlineClause(emit_counting_errors(host, disagreements))
    })?;
    // A generator's outputs are products of this run, not a tree moving under
    // it. Declared here but applied when the watch is asked, so the order
    // fixtures compile in does not matter.
    for invocation in &invocations {
        host.run_cache()
            .inputs()
            .mark_generated_dir(manifest_root.join(invocation.output_dir.as_str()));
    }
    let provider = CliGeneratorProvider::new(manifest_root.clone())
        .with_run_cache(host.run_cache())
        .with_no_cache(no_cache)
        .with_active(active)
        .with_run(run.clone())
        .with_log_level(host.log_level())
        .with_registry_context(RegistryContext {
            build_dependencies: manifest.build_dependencies,
            registries: manifest.registries,
            locked_versions: locked_generator_versions(&manifest_root),
        });
    let kiln_host = host.rebased(manifest_root.clone());
    Ok(Some(KilnSetup {
        manifest_root,
        invocations,
        provider,
        host: kiln_host,
        identities,
        no_cache,
    }))
}

impl KilnSetup {
    /// Run the harvested invocations, writing what they produce.
    pub(crate) async fn run(&mut self) -> Result<PipelineOutcome, PipelineError> {
        run_pipeline(
            &self.manifest_root,
            &self.host,
            &self.provider,
            std::mem::take(&mut self.invocations),
            self.no_cache,
        )
        .await
    }

    /// Remap a pipeline's harvested index onto the loader identities collected
    /// with the invocations, reporting any redirect that two declarations
    /// disagree on through `host`.
    pub(crate) fn remap_conflicts(
        &self,
        invocations: &mut wado_compiler::kiln::InvocationIndex,
        host: &FilesystemCompilerHost,
    ) -> Result<(), PipelineError> {
        let conflicts = remap_and_report_conflicts(invocations, &self.identities, host);
        if conflicts > 0 {
            return Err(PipelineError::RedirectConflict(conflicts));
        }
        Ok(())
    }
}

/// Collect inline `with { generator: { ... } }` clauses from `entry_file`
/// (and any sibling manifest's directory if one is found), then drive the
/// Kiln pipeline via [`run_pipeline`]. Returns `Ok(PipelineOutcome::default())`
/// when no inline clauses were collected.
///
/// Errors from [`run_pipeline`] are surfaced unchanged; the caller decides
/// whether to abort or continue (e.g. for consume-only mode, a stale-cache
/// warning is not fatal).
pub(crate) async fn maybe_run_pipeline(
    entry_file: &Path,
    host: &FilesystemCompilerHost,
    no_cache: bool,
    project: Option<manifest::ProjectManifest>,
) -> Result<PipelineOutcome, PipelineError> {
    Box::pin(maybe_run_pipeline_for(
        entry_file,
        None,
        host,
        no_cache,
        project,
        &mut Settled::default(),
    ))
    .await
}

/// [`maybe_run_pipeline`] for an entry the compile that follows names
/// `entry_identity` rather than by its path, past the modules in `settled`.
pub(crate) async fn maybe_run_pipeline_for(
    entry_file: &Path,
    entry_identity: Option<&str>,
    host: &FilesystemCompilerHost,
    no_cache: bool,
    project: Option<manifest::ProjectManifest>,
    settled: &mut Settled,
) -> Result<PipelineOutcome, PipelineError> {
    let run = KilnRun::entry(project, no_cache);
    let Some(mut kiln) = prepare_kiln(entry_file, entry_identity, host, &run, settled).await?
    else {
        return Ok(PipelineOutcome::default());
    };
    let mut outcome = kiln.run().await?;
    kiln.remap_conflicts(&mut outcome.invocations, host)?;
    Ok(outcome)
}

/// `base_host` with what a build of `path` loads through: the dependencies of
/// `project`, the manifest nearest it, and the Kiln invocations the entry's
/// generators produce. `subcommand` names the failure it reports. `settled`
/// gains the modules the Kiln walk reached.
pub(crate) async fn build_load(
    path: &Path,
    source: &str,
    project: Option<manifest::ProjectManifest>,
    base_host: FilesystemCompilerHost,
    no_cache: bool,
    subcommand: &str,
    settled: &mut Settled,
) -> Result<(FilesystemCompilerHost, wado_compiler::kiln::InvocationIndex), CliExit> {
    let base_path = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let host = attach_manifest_and_component_deps(
        base_host,
        project.as_ref(),
        &base_path,
        &[source],
        Acquisition::Build,
    )
    .await
    .map_err(CliExit::error)?;
    let outcome = Box::pin(maybe_run_pipeline_for(
        path, None, &host, no_cache, project, settled,
    ))
    .await
    .map_err(|e| pipeline_exit(e, subcommand))?;
    Ok((host, outcome.invocations))
}

fn manifest_error(error: DiscoveryError) -> PipelineError {
    PipelineError::Manifest(Box::new(error))
}

/// How `wado <subcommand>` exits on a Kiln pipeline failure.
pub(crate) fn pipeline_exit(error: PipelineError, subcommand: &str) -> CliExit {
    if error.is_reported() {
        CliExit::silent_failure(1)
    } else {
        CliExit::error(format!("wado {subcommand}: {error}"))
    }
}

/// Resolve the Kiln invocations a generator's own source carries, so the
/// compile that follows sees generated modules where it wrote schemas. Answers
/// an empty index when the generator declares none.
pub(crate) async fn run_nested_pipeline(
    entry_file: &Path,
    entry_identity: &str,
    host: &FilesystemCompilerHost,
    outer: &KilnRun,
) -> Result<wado_compiler::kiln::InvocationIndex, PipelineError> {
    // A generator shipped as a package names its dependencies in its own
    // manifest, so that is the project. The rest is the outer run's.
    let run = &KilnRun {
        project: load_nearest_manifest(entry_file).map_err(manifest_error)?,
        ..outer.clone()
    };
    // Its own root, so nothing the outer walk settled.
    let settled = &mut Settled::default();
    let Some(mut kiln) = prepare_kiln(entry_file, Some(entry_identity), host, run, settled).await?
    else {
        return Ok(wado_compiler::kiln::InvocationIndex::default());
    };
    let mut invocations = kiln.run().await?.invocations;
    kiln.remap_conflicts(&mut invocations, host)?;
    Ok(invocations)
}

/// Rewrite each inline invocation whose `module` is a `[build-dependencies]`
/// specifier (`module: "wado-lang:gale"`, `module: "lib:gale"`) that resolves to
/// a *path* dependency into a concrete `LocalPath` pointing at the dependency
/// package's `[world]."core:kiln/generator"` entry. This dispatches the
/// specifier on its declared source once, off the already-loaded manifest: a
/// path build-dependency compiles from source (like any path dep), so the rest
/// of the pipeline (cache key, generator identity, provider) sees a
/// path-addressed module; a registry build-dependency is left as `Spec` for the
/// provider to pull as a prebuilt component.
pub(crate) fn rewrite_build_dep_modules(
    inline: &mut [wado_compiler::kiln::Invocation],
    manifest: &wado_manifest::Manifest,
    manifest_root: &Path,
) -> Result<(), DiscoveryError> {
    use wado_compiler::kiln::GeneratorModule;
    for inv in inline.iter_mut() {
        let GeneratorModule::Spec(spec) = &inv.module else {
            continue;
        };
        let key = spec_key(&spec.spec);
        if let Some(local) =
            build_dep_generator_local_path(key, manifest, manifest_root, manifest_root)?
        {
            inv.module = GeneratorModule::LocalPath(local);
        }
    }
    Ok(())
}

/// Make an invocation a source dependency declares this run's own: its
/// generator resolves through the `[build-dependencies]` of `package`, the
/// dependency's root and manifest, never the consumer's, and it writes into the
/// consumer's tree, since a dependency's tree is not this run's to write.
fn adopt_dependency_invocation(
    invocation: &mut wado_compiler::kiln::Invocation,
    package: Option<(&Path, &wado_manifest::Manifest)>,
    manifest_root: &Path,
) -> Result<(), PipelineError> {
    use wado_compiler::kiln::{DeclSite, GeneratorModule, GeneratorSpec};
    let DeclSite {
        module,
        synthetic_id,
        ..
    } = invocation.decl_site().clone();
    invocation.write_into_consumer();
    let GeneratorModule::Spec(spec) = &invocation.module else {
        return Ok(());
    };
    let unsupported = |message: String| PipelineError::Provider {
        invocation: synthetic_id.clone(),
        source: ProviderError::Unsupported { message },
    };
    let declared_in = package.map_or_else(
        || format!("`{module}`, which no wado.toml governs"),
        |(root, _)| root.join(MANIFEST_FILENAME).display().to_string(),
    );
    let key = spec_key(&spec.spec);
    let declared = package.and_then(|(root, m)| Some((root, m, m.build_dependencies.get(key)?)));
    let Some((package_root, manifest, dep)) = declared else {
        // An inline source names the generator by itself (single-file mode).
        if spec.version.is_some() || spec.registry.is_some() {
            return Ok(());
        }
        return Err(unsupported(format!(
            "kiln: generator `{}` is not in the [build-dependencies] of {declared_in}",
            spec.spec
        )));
    };
    invocation.module = match &dep.source {
        DependencySource::Path { .. } => {
            let Some(local) =
                build_dep_generator_local_path(key, manifest, package_root, manifest_root)
                    .map_err(manifest_error)?
            else {
                return Err(unsupported(format!(
                    "kiln: build-dependency `{key}` of {declared_in} declares no \
                     `{GENERATOR_WORLD_FQ}` world"
                )));
            };
            GeneratorModule::LocalPath(local)
        }
        DependencySource::Registry {
            registry,
            package,
            version,
        } => GeneratorModule::Spec(GeneratorSpec {
            spec: package.clone(),
            version: Some(version.clone()),
            registry: Some(
                registry_url(&manifest.registries, registry.as_deref())
                    .cloned()
                    .ok_or_else(|| {
                        unsupported(format!(
                            "kiln: {declared_in} has no `[registries].{}` for `{key}`",
                            registry.as_deref().unwrap_or("default")
                        ))
                    })?,
            ),
        }),
        _ => {
            return Err(unsupported(format!(
                "kiln: build-dependency `{key}` of {declared_in} must be a registry or path \
                 entry; git and workspace sources are not supported"
            )));
        }
    };
    Ok(())
}

/// Rewrite each inline invocation whose `module` is a `LocalPath` pointing
/// at a *directory* (a generator package) into a `LocalPath` pointing at
/// that package's `[world]."core:kiln/generator"` entry file. This collapses
/// `module: "../pkg"` onto the same path-addressed identity — and therefore
/// the same cache key — as `module: "../pkg/src/generator.wado"` and a path
/// `[build-dependencies]` specifier, all of which resolve to one entry file.
/// Resolved off the filesystem before the pipeline runs, mirroring
/// [`rewrite_build_dep_modules`]. A directory without a resolvable generator
/// world entry is left untouched for the provider to report.
pub(crate) fn rewrite_local_dir_modules(
    inline: &mut [wado_compiler::kiln::Invocation],
    manifest_root: &Path,
) -> Result<(), DiscoveryError> {
    use wado_compiler::kiln::{GeneratorModule, InvocationPath};
    for inv in inline.iter_mut() {
        let GeneratorModule::LocalPath(path) = &inv.module else {
            continue;
        };
        let abs = manifest_root.join(path.as_str());
        if !abs.is_dir() {
            continue;
        }
        if let Some(entry) = package_generator_entry(&abs)? {
            inv.module = GeneratorModule::LocalPath(InvocationPath::normalize(&format!(
                "{}/{entry}",
                path.as_str()
            )));
        }
    }
    Ok(())
}

/// The generator entry of a path `[build-dependencies]` package of the
/// manifest at `package_root`, relative to `manifest_root`, so an absolute
/// `path` names the generator as a relative one does and the identity does not
/// carry the checkout's location. `None` when the dependency is absent, not a
/// path dep, or declares no `core:kiln/generator` world.
fn build_dep_generator_local_path(
    key: &str,
    manifest: &wado_manifest::Manifest,
    package_root: &Path,
    manifest_root: &Path,
) -> Result<Option<wado_compiler::kiln::InvocationPath>, DiscoveryError> {
    let Some(dep) = manifest.build_dependencies.get(key) else {
        return Ok(None);
    };
    let DependencySource::Path { path, .. } = &dep.source else {
        return Ok(None);
    };
    let package_dir = absolute(package_root).join(path);
    let Some(entry) = package_generator_entry(&package_dir)? else {
        return Ok(None);
    };
    let entry = normalize_path(&package_dir.join(entry));
    let rebased = relative_to(&absolute(manifest_root), &entry);
    Ok(Some(wado_compiler::kiln::InvocationPath::normalize(
        &rebased,
    )))
}

/// Resolve a generator *package directory* (absolute) to its
/// `[world]."core:kiln/generator"` entry, as a path relative to that
/// directory. `None` when the directory has no `wado.toml` or the manifest
/// declares no such world entry; an error when its `wado.toml` is invalid. The
/// single source of truth for mapping a package to its generator entry, shared
/// by the `[build-dependencies]`-name and directory-`module:` resolution paths
/// so both spellings land on the same entry file.
fn package_generator_entry(pkg_dir: &Path) -> Result<Option<String>, DiscoveryError> {
    let Some(manifest) = manifest_in(pkg_dir)? else {
        return Ok(None);
    };
    Ok(manifest
        .world_entry(GENERATOR_WORLD_FQ)
        .map(ToString::to_string))
}

/// Empty in-memory `wado.toml` manifest used as a fallback when the
/// compiled file has no nearby manifest. Shared with [`crate::check`].
#[must_use]
pub fn empty_manifest() -> wado_manifest::Manifest {
    wado_manifest::Manifest {
        package: None,
        world: indexmap::IndexMap::new(),
        registries: indexmap::IndexMap::new(),
        dependencies: indexmap::IndexMap::new(),
        dev_dependencies: indexmap::IndexMap::new(),
        build_dependencies: indexmap::IndexMap::new(),
        test: wado_manifest::TestSettings::default(),
        format: wado_manifest::FormatSettings::default(),
        unknown_sections: Vec::new(),
        inherited_unknown_fields: Vec::new(),
    }
}

/// Harvest inline Kiln invocations from `entry_file` and every Wado module it
/// reaches through `host`, its own and its source dependencies'.
///
/// Keys are spelled from `manifest_root`, so an invocation keeps one identity
/// whether the entry is compiled on its own behalf or as a generator.
async fn collect_inline_invocations_for_entry_with_identities(
    entry_file: &Path,
    entry_identity: Option<&str>,
    manifest_root: &Path,
    host: &FilesystemCompilerHost,
    dependencies: &wado_compiler::DependencyIndex,
    settled: &mut Settled,
) -> Harvested {
    // The identity must be byte-identical to the name the compile that follows
    // gives its entry, since that is what the loader interns the redirect under.
    let entry_identity = entry_identity
        .map(str::to_string)
        .unwrap_or_else(|| entry_file.to_string_lossy().to_string());
    let entry_key = relative_to(&absolute(manifest_root), &absolute(entry_file));
    // An unreadable entry harvests nothing; the compile that follows reports it.
    let Ok(entry_source) = fs::read_to_string(entry_file) else {
        return Harvested::default();
    };
    harvest_invocations(
        entry_file,
        &entry_source,
        &entry_identity,
        &entry_key,
        settled,
        dependencies,
        host,
    )
    .await
}

/// Remap `index` to loader identities via [`wado_compiler::kiln::remap_decl_files`], emit any
/// conflict diagnostics through `host`, and return the count of `Error`-severity
/// conflicts. Reached through [`KilnSetup::remap_conflicts`], which both
/// `compile` and `check` use.
fn remap_and_report_conflicts(
    index: &mut wado_compiler::kiln::InvocationIndex,
    identities: &wado_compiler::hashmap::IndexMap<String, String>,
    host: &FilesystemCompilerHost,
) -> usize {
    emit_counting_errors(
        host,
        wado_compiler::kiln::remap_decl_files(index, identities),
    )
}

/// Walk up from `entry_file` looking for the nearest `wado.toml`: `None` when
/// there is none, an error when the one found cannot be read or parsed.
pub fn load_nearest_manifest(
    entry_file: &Path,
) -> Result<Option<manifest::ProjectManifest>, DiscoveryError> {
    let dir = entry_file.parent().unwrap_or(Path::new("."));
    manifest::discover(openable_dir(dir))
}

fn wasm_to_wat(wasm: &[u8]) -> Result<String, CliExit> {
    let mut config = wasmprinter::Config::new();
    config.fold_instructions(true);
    let mut wat = String::new();
    config
        .print(wasm, &mut wasmprinter::PrintFmtWrite(&mut wat))
        .map_err(|e| CliExit::error(format!("generating WAT: {e}")))?;
    Ok(wat)
}

/// Fatal under `--embed-wit`, otherwise a warning that yields the un-embedded
/// (still valid, self-describing) component.
fn skip_embed(explicit: bool, wasm: Vec<u8>, msg: &str) -> Result<Vec<u8>, CliExit> {
    if explicit {
        return Err(CliExit::error(format!("--embed-wit: {msg}")));
    }
    eprintln!("warning: skipped embedding WIT component-type section: {msg}");
    Ok(wasm)
}

/// Append the WIT `component-type` section to `wasm`, encoded from the retained
/// WIT `snapshot` and the world `surface` from the retained WIR.
/// `explicit` is true for `--embed-wit`: it makes a failure fatal, where the
/// default-on path only warns and yields the un-embedded component.
fn embed_wit_section(
    snapshot: Option<wado_compiler::wit_emit::WitEmitSnapshot>,
    wasm: Vec<u8>,
    surface: WorldSurface,
    explicit: bool,
) -> Result<Vec<u8>, CliExit> {
    let Some(snapshot) = snapshot else {
        return skip_embed(explicit, wasm, "WIT analysis subset was not retained");
    };
    match wit_bundle::embed_component_type_from(&wasm, snapshot.input(), &surface) {
        Ok(embedded) => Ok(embedded),
        Err(e) => skip_embed(explicit, wasm, &e.to_string()),
    }
}

/// Embed the `[package]` metadata into the component when building the package's
/// declared artifact (manifest-driven mode) and `--no-embed-metadata` was not
/// given. Returns `wasm` unchanged otherwise — an explicit file argument, no
/// `wado.toml`, or a manifest without `[package]`. Under `-Os` (strip symbols
/// for minimal frontend delivery) the metadata is dropped as dead weight unless
/// `--embed-metadata` forces it on, matching the WIT section's policy. The git
/// `revision` is included only on a clean tree (omitted silently here; `wado
/// publish` warns).
///
/// `project` is the manifest already discovered by [`run`], reused here rather
/// than re-parsed.
fn embed_package_metadata(
    opts: &CompileOptions,
    project: Option<&manifest::ProjectManifest>,
    wasm: Vec<u8>,
) -> Vec<u8> {
    match project.filter(|_| opts.manifest_driven) {
        Some(project) => {
            embed_metadata_sections(wasm, &package_metadata(project, opts.embed, &opts.knobs))
        }
        None => wasm,
    }
}

/// The `[package]` metadata a build of `project` embeds: none without a
/// `[package]`, or where `embed` turns it off at this `-O` level. It reads the
/// git tree and the `license-file`, which no compile records, so a cached
/// component is compared against it.
pub(crate) fn package_metadata(
    project: &manifest::ProjectManifest,
    embed: EmbedOptions,
    knobs: &CompileKnobs,
) -> Vec<MetadataSection> {
    if embed.metadata.resolve(knobs.opt_level).is_none() {
        return Vec::new();
    }
    let Some(pkg) = project.manifest.package.as_ref() else {
        return Vec::new();
    };
    let revision = clean_git_revision(&project.root);
    let license_text = match read_license_text(project, pkg) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("warning: skipped embedding license text: {e}");
            None
        }
    };
    wado_manifest::metadata_sections(pkg, revision.as_deref(), license_text.as_deref())
}

/// Read the `license-file` text to embed, resolving the path against the
/// manifest that declared it (see [`manifest::license_file_base_dir`]). Returns
/// `Ok(None)` when the package has no `license-file`; an `Err` when the declared
/// file cannot be read (the caller downgrades this to a warning, matching the
/// other embed steps).
fn read_license_text(
    project: &manifest::ProjectManifest,
    pkg: &wado_manifest::Package,
) -> Result<Option<String>, String> {
    let Some(license_file) = pkg.license_file.as_deref() else {
        return Ok(None);
    };
    let path = manifest::license_file_base_dir(&project.root).join(license_file);
    fs::read_to_string(&path)
        .map(Some)
        .map_err(|e| format!("cannot read license-file {}: {e}", path.display()))
}

/// A finished build: the emitted component, and where it was written.
/// `--wat-to-stdout` writes no file, so `output_path` is `None` there.
pub struct Artifact {
    pub bytes: Vec<u8>,
    pub output_path: Option<PathBuf>,
    /// What the compile read, where none of it is untracked.
    pub(crate) reads: Option<Vec<Input>>,
}

/// Report a written file. A command that exists to produce one says so; a
/// `run` / `serve` driver, which only builds on the way through, stays quiet.
pub fn announce_artifact(output: impl AsRef<Path>) {
    eprintln!("Generated: {}", output.as_ref().display());
}

pub async fn run(opts: CompileOptions) -> Result<(), CliExit> {
    if let Some(output) = compile_to_artifact(opts).await?.output_path {
        announce_artifact(output);
    }
    Ok(())
}

/// Like [`run`], but silent and handing back the [`Artifact`]. A run / serve
/// driver uses its own compilation directly instead of reading the artifact
/// back from `build/`, where a concurrent build could leave it torn or serve a
/// different world's bytes.
pub async fn compile_to_artifact(opts: CompileOptions) -> Result<Artifact, CliExit> {
    // Output format is independent of the compiled bytes; resolve it first so we
    // know whether to retain WIR (only the wasm-output embedding path needs it).
    let format = opts
        .format
        .or_else(|| {
            opts.output
                .as_ref()
                .and_then(|p| OutputFormat::from_extension(Path::new(p)))
        })
        .unwrap_or(OutputFormat::Wasm);
    // WAT is a debug/inspection format, so it is left un-embedded.
    let embed = (!opts.wat_to_stdout && format == OutputFormat::Wasm)
        .then(|| opts.embed.wit.resolve(opts.knobs.opt_level))
        .flatten();

    // Up front, since the WIT contract names its interface after the package
    // before the compile; the output path and metadata embedding reuse it.
    let nearest = load_nearest_manifest(Path::new(&opts.input)).map_err(CliExit::error)?;

    let mut flags = opts.flags();
    flags.retain_wir = embed.is_some();
    // When embedding, retain the WIT subset so the section is encoded from this
    // compile, not a second frontend run (#1654).
    if embed.is_some() {
        flags.embed_wit_contract = Some(wado_compiler::wit_emit::wit_contract(
            opts.target_world.as_deref(),
            opts.lib_world.as_deref(),
            Some(&default_interface_name(&opts.input, nearest.as_ref())),
        ));
    }
    // Boxed so the manifest held across it keeps every caller's future small.
    let (result, reads) = Box::pin(compile_recording_reads(&opts.input, &flags)).await?;
    let wasm = result.wasm;

    if opts.wat_to_stdout {
        let wat = wasm_to_wat(&wasm)?;
        print!("{wat}");
        return Ok(Artifact {
            bytes: Vec::new(),
            output_path: None,
            reads: None,
        });
    }

    let project = nearest.filter(|_| opts.manifest_driven);

    let output_path = if let Some(path) = &opts.output {
        Path::new(path).to_path_buf()
    } else {
        default_output_path(&opts, project.as_ref(), format)
    };

    let bytes = match format {
        OutputFormat::Wat => wasm_to_wat(&wasm)?.into_bytes(),
        OutputFormat::Wasm => {
            // WIT first (if enabled), then the package metadata — both additive
            // custom sections on the component.
            let wasm = if let Some(explicit) = embed {
                let surface = result
                    .wir_package
                    .map(|p| p.world_surface)
                    .unwrap_or_default();
                embed_wit_section(result.wit_emit_snapshot, wasm, surface, explicit)?
            } else {
                wasm
            };
            embed_package_metadata(&opts, project.as_ref(), wasm)
        }
    };
    if let Some(parent) = output_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| CliExit::error(format!("creating output directory: {e}")))?;
    }
    fs::write(&output_path, &bytes)
        .map_err(|e| CliExit::error(format!("writing output file: {e}")))?;
    Ok(Artifact {
        bytes,
        output_path: Some(output_path),
        reads,
    })
}

/// The `<root>/build/<segment>.wasm` artifact path for a publish build, where
/// `<segment>` is `"lib"` or a [`world_path_segment`].
#[must_use]
pub fn build_output_path(root: &Path, segment: &str) -> PathBuf {
    build_dir(root).join(format!("{segment}.wasm"))
}

/// The build-output directory of the package rooted at `root`.
pub fn build_dir(root: &Path) -> PathBuf {
    root.join(BUILD_DIR)
}

/// The default output path when `-o` is absent. A manifest-driven build writes
/// `<manifest_root>/build/<world>.<ext>` — keeping artifacts out of the source
/// tree and giving each world its own file, mirroring kiln's `build/` layout.
/// A standalone file argument writes `<input>.<ext>` beside the source.
fn default_output_path(
    opts: &CompileOptions,
    project: Option<&manifest::ProjectManifest>,
    format: OutputFormat,
) -> PathBuf {
    let ext = match format {
        OutputFormat::Wasm => "wasm",
        OutputFormat::Wat => "wat",
    };
    match project {
        Some(project) => {
            // `--lib` builds the library world, which has no external FQ name.
            let world = if opts.lib_world.is_some() {
                "lib".to_string()
            } else {
                world_path_segment(opts.target_world_fq())
            };
            build_dir(&project.root).join(format!("{world}.{ext}"))
        }
        None => Path::new(&opts.input).with_extension(ext),
    }
}

/// Sanitize a Component Model world FQ name into a single path segment: drop the
/// `@version`, then replace every character outside `[A-Za-z0-9._-]` with `-`
/// (e.g. `wasi:cli/command` → `wasi-cli-command`).
pub fn world_path_segment(world_fq: &str) -> String {
    let base = world_fq.split('@').next().unwrap_or(world_fq);
    base.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod world_segment_tests {
    use super::world_path_segment;

    #[test]
    fn sanitizes_and_drops_version() {
        assert_eq!(world_path_segment("wasi:cli/command"), "wasi-cli-command");
        assert_eq!(world_path_segment("wasi:http/service"), "wasi-http-service");
        assert_eq!(world_path_segment("foo:bar/baz@1.2.3"), "foo-bar-baz");
    }
}

#[cfg(test)]
mod acquisition_tests {
    use super::resolves_more;
    use wado_compiler::DependencyIndex;

    /// Acquiring a package can resolve one dependency and reach another that
    /// is unresolved: the walk still has modules it did not see.
    #[test]
    fn a_resolved_dependency_counts_though_its_package_leaves_one_unresolved() {
        let mut offline = DependencyIndex::default();
        offline
            .unresolved
            .insert("lib:greet".to_string(), "not cached".to_string());
        let mut greet = DependencyIndex::default();
        greet
            .unresolved
            .insert("lib:far".to_string(), "not cached".to_string());
        let mut acquired = DependencyIndex::default();
        acquired
            .resolved
            .insert("lib:greet".to_string(), "greet/src/lib.wado".to_string());
        acquired
            .packages
            .insert("greet/src/lib.wado".to_string(), greet);
        assert!(resolves_more(&offline, &acquired));
        assert!(!resolves_more(&offline, &offline));
    }
}

#[cfg(test)]
mod kiln_dir_module_tests {
    use super::*;
    use wado_compiler::kiln::{DeclSite, GeneratorModule, Invocation, InvocationPath};

    fn local_invocation(module_path: &str) -> Invocation {
        Invocation {
            decl_sites: vec![DeclSite {
                module: "consumer.wado".to_string(),
                source: InvocationPath::normalize("./grammar.g4"),
                synthetic_id: "kiln-test".to_string(),
            }],
            module: GeneratorModule::LocalPath(InvocationPath::normalize(module_path)),
            invoked_as: module_path.to_string(),
            use_type: None,
            from: InvocationPath::normalize("grammar.g4"),
            integrity: None,
            inputs: Vec::new(),
            output_dir: InvocationPath::normalize("build"),
            options: wado_compiler::kiln::CanonicalOptions::default(),
            raw_options: None,
            options_span: wado_compiler::Span::default(),
        }
    }

    fn write_pkg(root: &Path, name: &str, world_entry: Option<&str>) -> PathBuf {
        let pkg = root.join(name);
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        let mut manifest = format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n");
        if let Some(entry) = world_entry {
            manifest.push_str(&format!(
                "\n[world]\n\"core:kiln/generator\" = \"{entry}\"\n"
            ));
        }
        std::fs::write(pkg.join("wado.toml"), manifest).unwrap();
        std::fs::write(pkg.join("src/generator.wado"), "// generator\n").unwrap();
        pkg
    }

    fn unique_tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wado-kiln-dir-{tag}-{}", std::process::id()))
    }

    /// The harvest, driven to completion: its reads are filesystem-immediate,
    /// so a test needs no runtime around them.
    fn harvest(
        entry: &Path,
        root: &Path,
    ) -> (
        Vec<wado_compiler::kiln::Invocation>,
        wado_compiler::hashmap::IndexMap<String, String>,
        Vec<wado_compiler::Diagnostic>,
    ) {
        let host = FilesystemCompilerHost::new(entry.parent().unwrap().to_path_buf());
        let harvested =
            futures::executor::block_on(collect_inline_invocations_for_entry_with_identities(
                entry,
                None,
                root,
                &host,
                &host.dependency_index(),
                &mut Settled::default(),
            ));
        (
            harvested.invocations,
            harvested.identities,
            harvested.diagnostics,
        )
    }

    #[test]
    fn package_generator_entry_reads_world_entry() {
        let root = unique_tmp("entry");
        let _ = std::fs::remove_dir_all(&root);
        let pkg = write_pkg(&root, "gen-pkg", Some("src/generator.wado"));
        assert_eq!(
            package_generator_entry(&pkg).unwrap().as_deref(),
            Some("src/generator.wado")
        );
        let plain = write_pkg(&root, "plain-pkg", None);
        assert_eq!(package_generator_entry(&plain).unwrap(), None);
        assert_eq!(package_generator_entry(&root.join("absent")).unwrap(), None);
        // An invalid manifest is reported, not read as one declaring no entry.
        let broken = write_pkg(&root, "broken-pkg", None);
        std::fs::write(broken.join("wado.toml"), "[package\n").unwrap();
        assert!(package_generator_entry(&broken).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rewrite_local_dir_modules_resolves_directory_to_entry_file() {
        let root = unique_tmp("rewrite");
        let _ = std::fs::remove_dir_all(&root);
        write_pkg(&root, "gen-pkg", Some("src/generator.wado"));

        // A directory module is rewritten to its package generator entry,
        // landing on the same path identity as a direct entry-file module.
        let mut inline = vec![local_invocation("./gen-pkg")];
        rewrite_local_dir_modules(&mut inline, &root).unwrap();
        match &inline[0].module {
            GeneratorModule::LocalPath(p) => {
                assert_eq!(p.as_str(), "gen-pkg/src/generator.wado");
            }
            other => panic!("expected LocalPath, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rewrite_build_dep_modules_rebases_an_absolute_path_onto_the_manifest_root() {
        let root = absolute(&unique_tmp("abs-dep"));
        let _ = std::fs::remove_dir_all(&root);
        let gen_pkg = write_pkg(&root, "gen-pkg", Some("src/generator.wado"));
        let app = write_pkg(&root, "app", None);
        let spellings = [gen_pkg.display().to_string(), "../gen-pkg".to_string()];
        for path in spellings {
            std::fs::write(
                app.join("wado.toml"),
                format!(
                    "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n\
                     [build-dependencies]\n\"lib:gen\" = {{ path = {path:?} }}\n"
                ),
            )
            .unwrap();
            let manifest = manifest_in(&app).unwrap().unwrap();
            let mut inline = vec![local_invocation("./unused.wado")];
            inline[0].module = GeneratorModule::Spec(wado_compiler::kiln::GeneratorSpec {
                spec: "lib:gen".to_string(),
                version: None,
                registry: None,
            });
            rewrite_build_dep_modules(&mut inline, &manifest, &app).unwrap();
            match &inline[0].module {
                GeneratorModule::LocalPath(p) => {
                    assert_eq!(p.as_str(), "../gen-pkg/src/generator.wado", "path {path}");
                }
                other => panic!("expected LocalPath, got {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rewrite_local_dir_modules_leaves_files_and_unresolvable_dirs() {
        let root = unique_tmp("leave");
        let _ = std::fs::remove_dir_all(&root);
        // A package directory without a generator world entry is left alone
        // (the provider reports the misconfiguration).
        write_pkg(&root, "plain-pkg", None);
        // A plain `.wado` file module must never be touched.
        std::fs::write(root.join("gen.wado"), "// gen\n").unwrap();

        let mut inline = vec![
            local_invocation("./plain-pkg"),
            local_invocation("./gen.wado"),
        ];
        rewrite_local_dir_modules(&mut inline, &root).unwrap();
        match &inline[0].module {
            GeneratorModule::LocalPath(p) => assert_eq!(p.as_str(), "plain-pkg"),
            other => panic!("expected untouched LocalPath, got {other:?}"),
        }
        match &inline[1].module {
            GeneratorModule::LocalPath(p) => assert_eq!(p.as_str(), "gen.wado"),
            other => panic!("expected untouched LocalPath, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn transitive_import_harvests_kiln_clause_and_maps_loader_identity() {
        // The Kiln `with` clause lives in an *imported* module, not the entry.
        // The harvester must still discover it, key it by its full path
        // (`decl_site.module`), and map that to the loader identity
        // (`./eval.wado`) the loader will present for the redirect.
        let root = unique_tmp("transitive");
        let _ = std::fs::remove_dir_all(&root);
        let example = root.join("example");
        std::fs::create_dir_all(&example).unwrap();
        std::fs::write(
            example.join("main.wado"),
            "use { evaluate } from \"./eval.wado\";\nexport fn run() { let _ = evaluate(); }\n",
        )
        .unwrap();
        std::fs::write(
            example.join("eval.wado"),
            "use arith from \"./Arith.g4\"\n    with { generator: { module: \"../src/generator.wado\" } };\n\
             pub fn evaluate() -> bool { return false; }\n",
        )
        .unwrap();

        let entry = example.join("main.wado");
        let (invs, identities, _diags) = harvest(&entry, &root);

        assert_eq!(invs.len(), 1, "should harvest the imported module's clause");
        let inv = &invs[0];
        assert!(
            inv.from.as_str().ends_with("Arith.g4"),
            "from: {}",
            inv.from.as_str()
        );
        assert!(
            inv.decl_site().module.ends_with("example/eval.wado"),
            "decl_site.module should be eval.wado's full path, got {}",
            inv.decl_site().module
        );
        assert_eq!(
            identities.get(&inv.decl_site().module).map(String::as_str),
            Some("./eval.wado"),
            "harvest key must map to the loader identity"
        );
        // The entry is keyed from the package root and maps to the name the
        // loader will present, its `EntryPoint.filename`.
        assert_eq!(
            identities.get("example/main.wado").map(String::as_str),
            Some(entry.to_string_lossy().as_ref())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // #1423: a module reached directly and via a `../`-chained sibling above the
    // entry dir must collapse to one canonical identity.
    #[test]
    fn escape_diamond_canonicalizes_to_one_loader_identity() {
        let root = unique_tmp("escape-diamond");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/gen")).unwrap();
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::fs::write(
            root.join("src/main.wado"),
            "use { p } from \"./gen/parser.wado\";\nuse { h } from \"../shared/helper.wado\";\nexport fn run() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/gen/parser.wado"),
            "use x from \"./G.g4\" with { generator: { module: \"../../tool/gen.wado\" } };\npub fn p() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("shared/helper.wado"),
            "use { p } from \"../src/gen/parser.wado\";\npub fn h() {}\n",
        )
        .unwrap();

        let entry = root.join("src/main.wado");
        let (invs, identities, _d) = harvest(&entry, &root);
        assert_eq!(invs.len(), 1);
        let parser_key = &invs[0].decl_site().module;
        assert_eq!(
            identities.get(parser_key).map(String::as_str),
            Some("./gen/parser.wado"),
            "escape-reentry must canonicalize to the direct spelling",
        );

        let mut idx = wado_compiler::kiln::InvocationIndex::new();
        idx.insert(parser_key, "./G.g4", "kiln:file:///g/parser.wado");
        let conflicts = wado_compiler::kiln::remap_decl_files(&mut idx, &identities);
        assert!(conflicts.is_empty());
        assert_eq!(
            idx.redirect("./gen/parser.wado", "./G.g4"),
            Some("kiln:file:///g/parser.wado"),
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // A `../`-chained back-reference to the entry folds onto `EntryPoint`, so
    // the entry keeps a single identity.
    #[test]
    fn entry_backref_folds_to_single_identity() {
        let root = unique_tmp("entry-backref");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::fs::write(
            root.join("src/main.wado"),
            "use g from \"./G.g4\" with { generator: { module: \"../tool/gen.wado\" } };\nuse { h } from \"../shared/helper.wado\";\nexport fn run() {}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("shared/helper.wado"),
            "use { run } from \"../src/main.wado\";\npub fn h() {}\n",
        )
        .unwrap();

        let entry = root.join("src/main.wado");
        let (_invs, identities, _d) = harvest(&entry, &root);
        assert_eq!(
            identities.get("src/main.wado").map(String::as_str),
            Some(entry.to_string_lossy().as_ref()),
            "the entry maps to its loader name only",
        );
        assert!(
            !identities.values().any(|v| v == "./main.wado"),
            "a back-reference must not introduce a relative entry identity: {identities:?}",
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // An in-directory diamond already collapses to one canonical identity.
    #[test]
    fn diamond_import_yields_single_canonical_identity() {
        let root = unique_tmp("diamond");
        let _ = std::fs::remove_dir_all(&root);
        let ex = root.join("example");
        std::fs::create_dir_all(ex.join("a")).unwrap();
        std::fs::create_dir_all(ex.join("b")).unwrap();
        // `b/bar.wado` re-imports `a/foo.wado` via `../a/foo.wado`.
        std::fs::write(
            ex.join("main.wado"),
            "use { f } from \"./a/foo.wado\";\nuse { g } from \"./b/bar.wado\";\nexport fn run() {}\n",
        )
        .unwrap();
        std::fs::write(
            ex.join("a/foo.wado"),
            "use x from \"./G.g4\" with { generator: { module: \"../../src/gen.wado\" } };\npub fn f() {}\n",
        )
        .unwrap();
        std::fs::write(
            ex.join("b/bar.wado"),
            "use { f } from \"../a/foo.wado\";\npub fn g() {}\n",
        )
        .unwrap();

        let entry = ex.join("main.wado");
        let (invs, identities, _d) = harvest(&entry, &root);

        assert_eq!(invs.len(), 1, "the generator clause is harvested once");
        let foo_key = &invs[0].decl_site().module;
        assert_eq!(
            identities.get(foo_key).map(String::as_str),
            Some("./a/foo.wado"),
            "diamond must collapse to one canonical loader identity",
        );

        let mut idx = wado_compiler::kiln::InvocationIndex::new();
        idx.insert(foo_key, "./G.g4", "kiln:file:///g/foo.wado");
        let conflicts = wado_compiler::kiln::remap_decl_files(&mut idx, &identities);
        assert!(conflicts.is_empty());
        assert_eq!(
            idx.redirect("./a/foo.wado", "./G.g4"),
            Some("kiln:file:///g/foo.wado"),
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
