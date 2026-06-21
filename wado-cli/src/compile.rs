use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use wado_manifest::DependencySource;

use lexopt::Arg::Value;
use lexopt::Parser;
use wado_compiler::LogLevel;

use crate::args::{self, CliExit};
use crate::compiler_host::{FilesystemCompilerHost, KilnComponentCache};
use crate::kiln_driver::{PipelineError, PipelineOutcome};
use crate::kiln_provider::CliGeneratorProvider;
use crate::manifest;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OptLevel {
    O0,
    /// All passes except DCE. Iterations: 2, inline threshold: 10.
    O1,
    /// Production: all passes, including DCE. Iterations: 10, inline threshold: 10.
    #[default]
    O2,
    /// Aggressive. Iterations: 100, inline threshold: 20.
    O3,
    /// `O2` plus name-section stripping.
    Os,
}

impl OptLevel {
    /// wasmtime exposes only `None`/`Speed`/`SpeedAndSize`, so `O1`/`O2`/
    /// `O3` collapse to `Speed` — mirroring the `wasmtime` CLI's own `-O`
    /// mapping.
    #[must_use]
    pub const fn to_wasmtime(self) -> wasmtime::OptLevel {
        match self {
            OptLevel::O0 => wasmtime::OptLevel::None,
            OptLevel::O1 | OptLevel::O2 | OptLevel::O3 => wasmtime::OptLevel::Speed,
            OptLevel::Os => wasmtime::OptLevel::SpeedAndSize,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputFormat {
    Wasm,
    Wat,
}

impl OutputFormat {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "wasm" => Some(OutputFormat::Wasm),
            "wat" => Some(OutputFormat::Wat),
            _ => None,
        }
    }

    fn from_extension(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(|ext| match ext {
                "wasm" => Some(OutputFormat::Wasm),
                "wat" => Some(OutputFormat::Wat),
                _ => None,
            })
    }
}

pub struct CompileOptions {
    pub input: String,
    pub output: Option<String>,
    pub format: Option<OutputFormat>,
    pub opt_level: OptLevel,
    pub wat_to_stdout: bool,
    pub log_level: LogLevel,
    pub target_world: Option<String>,
    pub skip_validation: bool,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
    pub allocator: Option<String>,
    pub no_cache: bool,
    pub codegen_flags: Vec<String>,
    /// Library world FQ (`namespace:name/name@version`) for `--lib`. When set,
    /// the compiler synthesizes a library world from the entry module's
    /// `export fn`s and exports each one as a Component Model function.
    pub lib_world: Option<String>,
}

/// Compile-time options shared by `compile`/`run`/`serve`/`test`.
///
/// `target_world` and `allocator` stay `Option` so each subcommand can
/// pin its own default (`run` → cli/command, `serve` → http/service,
/// `test` → test) while letting `compile` accept `--world`.
#[derive(Clone, Debug, Default)]
pub struct CompileFlags {
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub target_world: Option<String>,
    pub skip_validation: bool,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
    pub allocator: Option<String>,
    /// When true, ignore all build caches: every Kiln invocation re-runs
    /// its generator, and the generator wasm itself is recompiled from
    /// source instead of reused from `build/kiln/generators/`. Cache
    /// *writes* still happen, so a follow-up run without `--no-cache`
    /// benefits from a warm cache again.
    pub no_cache: bool,
    /// `--test-name` substring filters for the test world (empty elsewhere).
    /// Forwarded to `CompilerOptions::test_name_filters`.
    pub test_name_filters: Vec<String>,
    /// Generic codegen feature flags from `-f <flag>` (e.g. `["array-copy"]`).
    /// Forwarded verbatim to `CompilerOptions::codegen_flags`; the compiler
    /// validates them.
    pub codegen_flags: Vec<String>,
    /// Library world FQ for `--lib`. Forwarded to `CompilerOptions::lib_world`.
    pub lib_world: Option<String>,
}

impl CompileOptions {
    #[must_use]
    pub fn flags(&self) -> CompileFlags {
        CompileFlags {
            opt_level: self.opt_level,
            log_level: self.log_level,
            target_world: self.target_world.clone(),
            skip_validation: self.skip_validation,
            inline_threshold: self.inline_threshold,
            opt_iterations: self.opt_iterations,
            allocator: self.allocator.clone(),
            no_cache: self.no_cache,
            test_name_filters: Vec::new(),
            codegen_flags: self.codegen_flags.clone(),
            lib_world: self.lib_world.clone(),
        }
    }
}

#[derive(Clone, Copy)]
enum Opt {
    Output,
    Format,
    WatToStdout,
    World,
    Lib,
    OptLevel,
    InlineThreshold,
    OptIterations,
    LogLevel,
    NoValidate,
    NoCache,
    Allocator,
    Feature,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Output,
        Self::Format,
        Self::WatToStdout,
        Self::World,
        Self::Lib,
        Self::OptLevel,
        Self::InlineThreshold,
        Self::OptIterations,
        Self::LogLevel,
        Self::NoValidate,
        Self::NoCache,
        Self::Allocator,
        Self::Feature,
        Self::Help,
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
            Self::Lib => args::OptSpec {
                long: Some("lib"),
                short: None,
                value: None,
                desc: "Compile as a Component Model library, exporting every `export fn`\n(entry from [package].lib; requires [package].namespace)",
            },
            Self::OptLevel => args::OPT_LEVEL_SPEC,
            Self::InlineThreshold => args::INLINE_THRESHOLD_SPEC,
            Self::OptIterations => args::OPT_ITERATIONS_SPEC,
            Self::LogLevel => args::LOG_LEVEL_SPEC,
            Self::NoValidate => args::NO_VALIDATE_SPEC,
            Self::NoCache => args::NO_CACHE_SPEC,
            Self::Allocator => args::ALLOCATOR_SPEC,
            Self::Feature => args::FEATURE_SPEC,
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
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
}

pub fn print_usage() {
    eprint!("{}", format_usage());
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<CompileOptions, CliExit> {
    let usage = format_usage();
    let mut output: Option<String> = None;
    let mut format: Option<OutputFormat> = None;
    let mut input: Option<String> = None;
    let mut opt_level = OptLevel::default();
    let mut wat_to_stdout = false;
    let mut log_level = args::DEFAULT_LOG_LEVEL;
    let mut target_world: Option<String> = None;
    let mut skip_validation = false;
    let mut inline_threshold: Option<usize> = None;
    let mut opt_iterations: Option<u32> = None;
    let mut allocator: Option<String> = None;
    let mut codegen_flags: Vec<String> = Vec::new();
    let mut no_cache = false;
    let mut lib = false;
    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
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
                Opt::Lib => lib = true,
                Opt::OptLevel => opt_level = parse_opt_level_arg(&mut parser)?,
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
                Opt::Allocator => {
                    allocator = Some(args::require_string(&mut parser)?);
                }
                Opt::Feature => codegen_flags.push(args::require_string(&mut parser)?),
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            args::reject_multiple_inputs(&input)?;
            input = Some(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    if lib && target_world.is_some() {
        return Err(CliExit::error(
            "`--lib` and `--world` are mutually exclusive",
        ));
    }

    let (input, lib_world) = if lib {
        let (entry, world_fq) = manifest::resolve_lib_input(input, &usage)?;
        // The library defaults to the freelist allocator (long-lived host),
        // still overridable by an explicit `--allocator`.
        if allocator.is_none() {
            allocator = Some("freelist".to_string());
        }
        (entry, Some(world_fq))
    } else {
        let entry = manifest::resolve_input(input, manifest::EntryPointKind::Command, &usage)?;
        (entry, None)
    };

    Ok(CompileOptions {
        input,
        output,
        format,
        opt_level,
        wat_to_stdout,
        log_level,
        target_world,
        skip_validation,
        inline_threshold,
        opt_iterations,
        allocator,
        no_cache,
        codegen_flags,
        lib_world,
    })
}

fn to_compiler_opt_level(level: OptLevel) -> wado_compiler::OptLevel {
    match level {
        OptLevel::O0 => wado_compiler::OptLevel::O0,
        OptLevel::O1 => wado_compiler::OptLevel::O1,
        OptLevel::O2 => wado_compiler::OptLevel::O2,
        OptLevel::O3 => wado_compiler::OptLevel::O3,
        OptLevel::Os => wado_compiler::OptLevel::Os,
    }
}

/// Parse `-O<n>` (with optional bare-`-O`). Shared by every subcommand
/// that exposes optimization-level control.
pub fn parse_opt_level_arg(parser: &mut Parser) -> Result<OptLevel, CliExit> {
    let val = parser.optional_value();
    let level_str = val
        .as_ref()
        .map(|v| v.to_string_lossy())
        .unwrap_or_default();
    match level_str.as_ref() {
        "" | "0" | "g" => Ok(OptLevel::O0),
        "1" => Ok(OptLevel::O1),
        "2" => Ok(OptLevel::O2),
        "3" => Ok(OptLevel::O3),
        "s" => Ok(OptLevel::Os),
        _ => Err(CliExit::error(format!(
            "unknown optimization level '-O{level_str}'. Use -O0, -O1, -O2, -O3, -Os, or -Og"
        ))),
    }
}

/// Compile without bailing. Used by the test runner so `#![TODO]` modules
/// can be observed (via `CompileFailure::is_todo_module`) rather than
/// aborting the whole batch.
pub async fn try_compile(
    filename: &str,
    flags: &CompileFlags,
) -> Result<wado_compiler::CompileResult, wado_compiler::CompileFailure> {
    try_compile_with_kiln_cache(filename, flags, None).await
}

/// `try_compile` with an optional [`KilnComponentCache`]. `wado test`
/// passes one shared across every fixture so a generator is AOT-compiled
/// once per run; `None` keeps the per-host cache for single-file builds.
pub async fn try_compile_with_kiln_cache(
    filename: &str,
    flags: &CompileFlags,
    kiln_cache: Option<Arc<KilnComponentCache>>,
) -> Result<wado_compiler::CompileResult, wado_compiler::CompileFailure> {
    let path = Path::new(filename);

    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading '{}': {e}", path.display());
            return Err(wado_compiler::CompileFailure {
                is_todo_module: false,
            });
        }
    };

    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    // Load the nearest manifest once: it seeds both the host's `[dependencies]`
    // index and the Kiln pipeline's `[build-dependencies]` resolution.
    let manifest_pair = load_nearest_manifest(path);
    let base_host = FilesystemCompilerHost::with_log_level(base_path.clone(), flags.log_level);
    let base_host = match kiln_cache {
        Some(cache) => base_host.with_shared_kiln_cache(cache),
        None => base_host,
    };
    let host = attach_manifest_deps(base_host, manifest_pair.as_ref(), &base_path);

    let pipeline_outcome =
        match maybe_run_pipeline(path, &host, flags.no_cache, manifest_pair).await {
            Ok(outcome) => outcome,
            Err(e) => {
                eprintln!("{e}");
                return Err(wado_compiler::CompileFailure {
                    is_todo_module: false,
                });
            }
        };

    let options = wado_compiler::CompilerOptions {
        opt_level: to_compiler_opt_level(flags.opt_level),
        target_world: flags.target_world.clone(),
        skip_validation: flags.skip_validation,
        inline_threshold: flags.inline_threshold,
        opt_iterations: flags.opt_iterations,
        log_level: Some(flags.log_level),
        allocator: flags.allocator.clone(),
        invocations: pipeline_outcome.invocations,
        test_name_filters: flags.test_name_filters.clone(),
        codegen_flags: flags.codegen_flags.clone(),
        lib_world: flags.lib_world.clone(),
        ..Default::default()
    };

    wado_compiler::compile_with_options(&source, &host, Some(filename), options).await
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

/// Seed `host` with the `[dependencies]` index derived from `manifest_pair`
/// (anchored at `base_path`), or return it unchanged when there is no
/// manifest. Shared by `compile`, `check`, and the `query` adapter so every
/// entry point attaches dependency resolution identically.
pub(crate) fn attach_manifest_deps(
    host: FilesystemCompilerHost,
    manifest_pair: Option<&(wado_manifest::Manifest, std::path::PathBuf)>,
    base_path: &Path,
) -> FilesystemCompilerHost {
    match manifest_pair {
        Some((manifest, root)) => host.with_dependency_index(
            wado_lsp::host::dependency_index_from(manifest, root, base_path),
        ),
        None => host,
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
    manifest_pair: Option<(wado_manifest::Manifest, std::path::PathBuf)>,
) -> Result<PipelineOutcome, PipelineError> {
    let manifest_root_for_inline = manifest_pair.as_ref().map(|(_, root)| root.clone());
    let probe_manifest_root = manifest_root_for_inline.clone().unwrap_or_else(|| {
        entry_file
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    });
    let (inline, identities) =
        collect_inline_invocations_for_entry_with_identities(entry_file, &probe_manifest_root);

    let (manifest, manifest_root) = match manifest_pair {
        Some(pair) => pair,
        None if inline.is_empty() => return Ok(PipelineOutcome::default()),
        None => (empty_manifest(), probe_manifest_root),
    };

    if inline.is_empty() {
        return Ok(PipelineOutcome::default());
    }
    let mut inline = inline;
    rewrite_build_dep_modules(&mut inline, &manifest, &manifest_root);
    rewrite_local_dir_modules(&mut inline, &manifest_root);
    let provider = CliGeneratorProvider::new(manifest_root.clone()).with_no_cache(no_cache);
    let mut outcome = crate::kiln_driver::run_pipeline(
        &manifest,
        &manifest_root,
        host,
        &provider,
        inline,
        no_cache,
    )
    .await?;
    // The index is keyed by `decl_site.module` (the harvest key, a full path);
    // rewrite those keys to the loader identities so a redirect from a
    // transitively-imported module matches.
    remap_index_decl_files(&mut outcome.invocations, &identities);
    Ok(outcome)
}

/// Rewrite each inline invocation whose `module` is a bare
/// `[build-dependencies]` name (`module: "gale"`) into a concrete
/// `LocalPath` pointing at the dependency package's
/// `[world]."core:kiln/generator"` entry. This resolves build-dep
/// generators once, off the already-loaded manifest, so the rest of the
/// pipeline (cache key, generator identity, provider) sees a path-addressed
/// module. Unresolvable names are left as `BuildDep` for the provider to
/// report.
pub(crate) fn rewrite_build_dep_modules(
    inline: &mut [wado_compiler::kiln::Invocation],
    manifest: &wado_manifest::Manifest,
    manifest_root: &Path,
) {
    use wado_compiler::kiln::GeneratorModule;
    for inv in inline.iter_mut() {
        let GeneratorModule::BuildDep(name) = &inv.module else {
            continue;
        };
        if let Some(local) = build_dep_generator_local_path(name, manifest, manifest_root) {
            inv.module = GeneratorModule::LocalPath(local);
        }
    }
}

/// Rewrite each inline invocation whose `module` is a `LocalPath` pointing
/// at a *directory* (a generator package) into a `LocalPath` pointing at
/// that package's `[world]."core:kiln/generator"` entry file. This collapses
/// `module: "../pkg"` onto the same path-addressed identity — and therefore
/// the same cache key — as `module: "../pkg/src/generator.wado"` and the
/// `[build-dependencies]` name form, all of which resolve to one entry file.
/// Resolved off the filesystem before the pipeline runs, mirroring
/// [`rewrite_build_dep_modules`]. A directory without a resolvable generator
/// world entry is left untouched for the provider to report.
pub(crate) fn rewrite_local_dir_modules(
    inline: &mut [wado_compiler::kiln::Invocation],
    manifest_root: &Path,
) {
    use wado_compiler::kiln::{GeneratorModule, InvocationPath};
    for inv in inline.iter_mut() {
        let GeneratorModule::LocalPath(path) = &inv.module else {
            continue;
        };
        let abs = manifest_root.join(path.as_str());
        if !abs.is_dir() {
            continue;
        }
        if let Some(entry) = package_generator_entry(&abs) {
            inv.module = GeneratorModule::LocalPath(InvocationPath::normalize(&format!(
                "{}/{entry}",
                path.as_str()
            )));
        }
    }
}

/// The generator entry of a path `[build-dependencies]` package, as a
/// manifest-root-relative [`InvocationPath`]: `<dep-path>/<generator world
/// entry>`. `None` when the dependency is absent, not a path dep, or declares
/// no `core:kiln/generator` world.
fn build_dep_generator_local_path(
    name: &str,
    manifest: &wado_manifest::Manifest,
    manifest_root: &Path,
) -> Option<wado_compiler::kiln::InvocationPath> {
    let dep = manifest.build_dependencies.get(name)?;
    let DependencySource::Path { path, .. } = &dep.source else {
        return None;
    };
    let entry = package_generator_entry(&manifest_root.join(path))?;
    Some(wado_compiler::kiln::InvocationPath::normalize(&format!(
        "{path}/{entry}"
    )))
}

/// Resolve a generator *package directory* (absolute) to its
/// `[world]."core:kiln/generator"` entry, as a path relative to that
/// directory. `None` when the directory has no readable `wado.toml` or the
/// manifest declares no such world entry. The single source of truth for
/// mapping a package to its generator entry, shared by the
/// `[build-dependencies]`-name and directory-`module:` resolution paths so
/// both spellings land on the same entry file.
fn package_generator_entry(pkg_dir: &Path) -> Option<String> {
    let manifest_text = fs::read_to_string(pkg_dir.join("wado.toml")).ok()?;
    let manifest: wado_manifest::Manifest = manifest_text.parse().ok()?;
    Some(manifest.world_entry("core:kiln/generator")?.to_string())
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
        workspace: None,
        test: wado_manifest::TestSettings::default(),
        format: wado_manifest::FormatSettings::default(),
    }
}

pub fn collect_inline_invocations_for_entry(
    entry_file: &Path,
    manifest_root: &Path,
) -> Vec<wado_compiler::kiln::Invocation> {
    collect_inline_invocations_for_entry_with_identities(entry_file, manifest_root).0
}

/// Harvest inline Kiln invocations from `entry_file` *and its transitive local
/// `.wado` imports*, plus the map needed to fix up the redirect index.
///
/// Kiln generation runs as a pre-pass before the loader, so a
/// `with { generator: ... }` clause in an imported module (not just the entry)
/// must be discovered here; otherwise that module's grammar `use` falls through
/// to the Wado lexer.
///
/// Two distinct module identities are at play, and a non-entry module needs a
/// different one for each:
///
/// - The **harvest key** keys `decl_site.module`, which Kiln resolves the
///   `module` / `output_dir` paths against (relative to the manifest root). It
///   must be the module's full path, exactly as the entry's is — so it is built
///   by `resolve_module_path` chaining from the entry's full path.
/// - The **loader identity** is the `ModuleSource::Local.path` the loader feeds
///   into `InvocationIndex::redirect`. The loader resolves the entry's own
///   imports through its `EntryPoint` branch (`normalize_module_path`) and
///   deeper imports through its `Local` branch (`resolve_module_path`), so the
///   identity is base-path-relative (`./eval.wado`), not the full path.
///
/// The returned map is `harvest_key → loader_identity`; the caller rewrites the
/// generated index's `decl_file` keys through it (see `remap_index_decl_files`).
/// For the entry the two coincide (its `EntryPoint.filename` is the full path),
/// so it maps to itself.
pub(crate) fn collect_inline_invocations_for_entry_with_identities(
    entry_file: &Path,
    manifest_root: &Path,
) -> (
    Vec<wado_compiler::kiln::Invocation>,
    std::collections::HashMap<String, String>,
) {
    use std::collections::VecDeque;
    use wado_compiler::name::{normalize_module_path, resolve_module_path};

    let mut modules =
        wado_compiler::hashmap::IndexMap::<String, wado_compiler::ast::Module>::default();
    let mut identities: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    // (harvest_key, loader_identity, filesystem_path, is_entry)
    let mut queue: VecDeque<(String, String, std::path::PathBuf, bool)> = VecDeque::new();
    // The entry's harvest key must stay byte-identical to its
    // `EntryPoint.filename` (interned verbatim by the loader), so the redirect
    // it keys is found at resolve time — do not normalize it here.
    // `resolve_module_path` (used for children below) is panic-free on its own,
    // so a path with URI-unsafe characters no longer crashes (#1417).
    let entry_key = entry_file.to_string_lossy().to_string();
    queue.push_back((entry_key.clone(), entry_key, entry_file.to_path_buf(), true));

    while let Some((key, loader_id, fs_path, is_entry)) = queue.pop_front() {
        if modules.contains_key(&key) {
            continue;
        }
        let Ok(source) = fs::read_to_string(&fs_path) else {
            continue;
        };
        // `parse` is resilient; if a module has recovered lex or parse errors
        // the AST is partial. Refuse to harvest from a partial tree so a
        // mid-edit source can't trigger surprising codegen side effects —
        // matches the prior fail-on-parse-error behaviour.
        let Ok(parsed) = wado_compiler::parse(&source).into_fail_fast() else {
            continue;
        };
        let ast = parsed.ast;

        let parent_dir = fs_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        for item in &ast.items {
            let wado_compiler::ast::Item::Use(use_decl) = item else {
                continue;
            };
            let src = use_decl.source.as_str();
            // Only follow local `.wado` modules. Non-`.wado` schemas (`.g4`,
            // …) are the Kiln targets themselves, and `core:` / `wasi:` /
            // bare dependency imports are not consumer modules to scan.
            if !(src.starts_with("./") || src.starts_with("../"))
                || !src.to_ascii_lowercase().ends_with(".wado")
            {
                continue;
            }
            let child_key = resolve_module_path(&key, src);
            let child_loader_id = if is_entry {
                normalize_module_path(src)
            } else {
                resolve_module_path(&loader_id, src)
            };
            if !modules.contains_key(&child_key) {
                queue.push_back((child_key, child_loader_id, parent_dir.join(src), false));
            }
        }
        identities.insert(key.clone(), loader_id);
        modules.insert(key, ast);
    }

    let descriptors = wado_compiler::hashmap::IndexMap::default();
    let manifest_root_str = manifest_root.to_string_lossy();
    let invocations = wado_compiler::kiln::collect_inline_invocations(
        modules.iter().map(|(k, v)| (k.as_str(), v)),
        &descriptors,
        &manifest_root_str,
    )
    .unwrap_or_default();
    (invocations, identities)
}

/// Rewrite the redirect index's `decl_file` keys from the harvest key (a
/// module's full path) to the loader identity the loader will present at
/// resolve time. Entries whose `decl_file` is not in `identities` (the entry
/// itself, or anything already in loader form) pass through unchanged.
pub(crate) fn remap_index_decl_files(
    index: &mut wado_compiler::kiln::InvocationIndex,
    identities: &std::collections::HashMap<String, String>,
) {
    let rewritten: Vec<(String, String, String)> = index
        .entries()
        .map(|(decl, from, uri)| {
            let decl = identities.get(decl).map_or(decl, String::as_str);
            (decl.to_string(), from.to_string(), uri.to_string())
        })
        .collect();
    let mut fresh = wado_compiler::kiln::InvocationIndex::new();
    for (decl, from, uri) in rewritten {
        fresh.insert(&decl, &from, &uri);
    }
    *index = fresh;
}

/// Walk up from `entry_file` looking for the nearest `wado.toml`. Returns
/// `None` (treated as "no Kiln config") on missing or malformed manifest.
pub fn load_nearest_manifest(
    entry_file: &Path,
) -> Option<(wado_manifest::Manifest, std::path::PathBuf)> {
    let mut dir = entry_file
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    if dir.as_os_str().is_empty() {
        dir = std::path::PathBuf::from(".");
    }
    loop {
        let candidate = dir.join("wado.toml");
        if candidate.is_file() {
            let text = fs::read_to_string(&candidate).ok()?;
            let manifest: wado_manifest::Manifest = text.parse().ok()?;
            return Some((manifest, dir));
        }
        if !dir.pop() {
            return None;
        }
    }
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

pub async fn run(opts: CompileOptions) -> Result<(), CliExit> {
    let flags = opts.flags();
    let wasm = compile(&opts.input, &flags).await?;

    if opts.wat_to_stdout {
        let wat = wasm_to_wat(&wasm)?;
        print!("{wat}");
        return Ok(());
    }

    // Format precedence: explicit `--format` > guessed from `-o` extension > wasm.
    let format = opts
        .format
        .or_else(|| {
            opts.output
                .as_ref()
                .and_then(|p| OutputFormat::from_extension(Path::new(p)))
        })
        .unwrap_or(OutputFormat::Wasm);

    let output_path = if let Some(path) = &opts.output {
        Path::new(path).to_path_buf()
    } else {
        let ext = match format {
            OutputFormat::Wasm => "wasm",
            OutputFormat::Wat => "wat",
        };
        Path::new(&opts.input).with_extension(ext)
    };

    let bytes = match format {
        OutputFormat::Wasm => wasm,
        OutputFormat::Wat => wasm_to_wat(&wasm)?.into_bytes(),
    };
    fs::write(&output_path, &bytes)
        .map_err(|e| CliExit::error(format!("writing output file: {e}")))?;
    eprintln!("Generated: {}", output_path.display());
    Ok(())
}

#[cfg(test)]
mod kiln_dir_module_tests {
    use super::*;
    use wado_compiler::kiln::{DeclSite, GeneratorModule, Invocation, InvocationPath};

    fn local_invocation(module_path: &str) -> Invocation {
        Invocation {
            decl_site: DeclSite {
                module: "consumer.wado".to_string(),
                synthetic_id: "kiln-test".to_string(),
            },
            module: GeneratorModule::LocalPath(InvocationPath::normalize(module_path)),
            from: InvocationPath::normalize("./grammar.g4"),
            inputs: Vec::new(),
            output_dir: InvocationPath::normalize("build"),
            options_canonical: Vec::new(),
            raw_options: None,
        }
    }

    fn write_pkg(root: &Path, name: &str, world_entry: Option<&str>) -> std::path::PathBuf {
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

    fn unique_tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("wado-kiln-dir-{tag}-{}", std::process::id()))
    }

    #[test]
    fn package_generator_entry_reads_world_entry() {
        let root = unique_tmp("entry");
        let _ = std::fs::remove_dir_all(&root);
        let pkg = write_pkg(&root, "gen-pkg", Some("src/generator.wado"));
        assert_eq!(
            package_generator_entry(&pkg).as_deref(),
            Some("src/generator.wado")
        );
        let plain = write_pkg(&root, "plain-pkg", None);
        assert_eq!(package_generator_entry(&plain), None);
        assert_eq!(package_generator_entry(&root.join("absent")), None);
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
        rewrite_local_dir_modules(&mut inline, &root);
        match &inline[0].module {
            GeneratorModule::LocalPath(p) => {
                assert_eq!(p.as_str(), "gen-pkg/src/generator.wado");
            }
            other => panic!("expected LocalPath, got {other:?}"),
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
        rewrite_local_dir_modules(&mut inline, &root);
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
        let (invs, identities) =
            collect_inline_invocations_for_entry_with_identities(&entry, &root);

        assert_eq!(invs.len(), 1, "should harvest the imported module's clause");
        let inv = &invs[0];
        assert!(
            inv.from.as_str().ends_with("Arith.g4"),
            "from: {}",
            inv.from.as_str()
        );
        assert!(
            inv.decl_site.module.ends_with("example/eval.wado"),
            "decl_site.module should be eval.wado's full path, got {}",
            inv.decl_site.module
        );
        assert_eq!(
            identities.get(&inv.decl_site.module).map(String::as_str),
            Some("./eval.wado"),
            "harvest key must map to the loader identity"
        );
        // The entry maps to itself (its `EntryPoint.filename` is the full path).
        let entry_key = entry.to_string_lossy().to_string();
        assert_eq!(
            identities.get(&entry_key).map(String::as_str),
            Some(entry_key.as_str())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn remap_index_decl_files_swaps_to_loader_identity() {
        use wado_compiler::kiln::InvocationIndex;

        let mut idx = InvocationIndex::new();
        idx.insert(
            "/abs/example/eval.wado",
            "./Arith.g4",
            "kiln:file:///g/arith.wado",
        );

        let mut identities = std::collections::HashMap::new();
        identities.insert(
            "/abs/example/eval.wado".to_string(),
            "./eval.wado".to_string(),
        );

        remap_index_decl_files(&mut idx, &identities);

        assert!(
            idx.redirect("/abs/example/eval.wado", "./Arith.g4")
                .is_none(),
            "the harvest-key decl_file must no longer redirect"
        );
        assert_eq!(
            idx.redirect("./eval.wado", "./Arith.g4"),
            Some("kiln:file:///g/arith.wado"),
            "the loader identity must redirect to the same URI"
        );
    }
}
