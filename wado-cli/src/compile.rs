use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use wado_manifest::DependencySource;

use lexopt::Arg::Value;
use lexopt::Parser;
use wado_compiler::LogLevel;
use wado_compiler::wit_bundle;
use wado_compiler::wit_emit::{WitEmitOptions, WitScope};

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
    /// `-D NAME=value` compile-time parameter overrides.
    pub param_overrides: wado_compiler::hashmap::IndexMap<String, String>,
    /// `--param-*` policy levels.
    pub param_policy: wado_compiler::param_resolution::ParamPolicy,
    /// `--no-embed-wit`: opt out of embedding the `component-type` WIT section.
    pub no_embed_wit: bool,
    /// `--embed-wit`: force embedding on, overriding the `-Os` default-off.
    /// Takes no value — the embedded section is always the self-contained full
    /// closure (a `local`, registry-referencing section is not encodable; see
    /// WEP §"Phase 2 finding"). Mutually exclusive with `--no-embed-wit`.
    pub embed_wit: bool,
    /// `--no-embed-metadata`: opt out of embedding the `[package]` metadata.
    pub no_embed_metadata: bool,
    /// `--embed-metadata`: force embedding on, overriding the `-Os` default-off.
    /// Mutually exclusive with `--no-embed-metadata`.
    pub embed_metadata: bool,
    /// The entry came from a manifest (no path argument, or a directory
    /// argument), so the build is the package's declared artifact and carries
    /// its metadata. False for an explicit `.wado` file argument.
    pub manifest_driven: bool,
}

impl CompileOptions {
    /// Resolve whether to embed the `component-type` WIT section. Returns
    /// `Some(explicit)`, where `explicit` is true when the user passed
    /// `--embed-wit` (so an embedding failure is fatal rather than a warning),
    /// or `None` to skip. Embedding is default-on except under `-Os` (frontend
    /// delivery, where the WIT metadata never reaches a CM host). See WEP
    /// `wep-2026-05-02-wit-interoperability.md` §"Embedding policy".
    fn embed_decision(&self) -> Option<bool> {
        resolve_embed_decision(self.no_embed_wit, self.embed_wit, self.opt_level)
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
}

/// The world `wado compile` targets when neither `--world` nor `--lib` is given.
const DEFAULT_WORLD: &str = "wasi:cli/command";

/// The build-output directory at the manifest root, shared with kiln's
/// `build/kiln/...` layout (see `kiln_provider`).
const BUILD_DIR: &str = "build";

/// Pure embedding-policy resolution, factored out of [`CompileOptions`] for
/// testing. See [`CompileOptions::embed_decision`].
fn resolve_embed_decision(
    no_embed_wit: bool,
    embed_wit: bool,
    opt_level: OptLevel,
) -> Option<bool> {
    if no_embed_wit {
        return None;
    }
    if embed_wit {
        return Some(true);
    }
    if opt_level == OptLevel::Os {
        return None;
    }
    Some(false)
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
    /// `-D NAME=value` compile-time parameter overrides. Forwarded to
    /// `CompilerOptions::param_overrides`.
    pub param_overrides: wado_compiler::hashmap::IndexMap<String, String>,
    /// `--param-*` policy levels. Forwarded to `CompilerOptions::param_policy`.
    pub param_policy: wado_compiler::param_resolution::ParamPolicy,
    /// Retain the WIR module in the result. `wado compile` sets it when
    /// embedding WIT, to read the faithful import plan
    /// (`imported_cm_interfaces`) without a second compile.
    pub retain_wir: bool,
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
            param_overrides: self.param_overrides.clone(),
            param_policy: self.param_policy,
            retain_wir: false,
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
    NoEmbedWit,
    EmbedWit,
    NoEmbedMetadata,
    EmbedMetadata,
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
        Self::NoEmbedWit,
        Self::EmbedWit,
        Self::NoEmbedMetadata,
        Self::EmbedMetadata,
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
    writeln!(buf, "Usage: wado compile [options] <file.wado>").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Compile a Wado source file to WebAssembly.").unwrap();
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
                Opt::NoEmbedWit => no_embed_wit = true,
                Opt::EmbedWit => embed_wit = true,
                Opt::NoEmbedMetadata => no_embed_metadata = true,
                Opt::EmbedMetadata => embed_metadata = true,
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

    // No path argument or a directory argument resolves the entry through a
    // manifest; an explicit `.wado` file is a standalone target. Gates metadata
    // embedding (WEP `wep-2026-02-14-package-manifest.md`).
    let manifest_driven = input.as_deref().is_none_or(|s| Path::new(s).is_dir());

    // Metadata embedding only happens for manifest-driven builds, so these flags
    // would be silently ignored on a standalone file — reject the combination.
    if (no_embed_metadata || embed_metadata) && !manifest_driven {
        return Err(CliExit::error(
            "`--embed-metadata` / `--no-embed-metadata` apply only to manifest-driven builds \
             (no argument or a directory argument), not an explicit file",
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
        param_overrides: param_args.overrides,
        param_policy: param_args.policy,
        no_embed_wit,
        embed_wit,
        no_embed_metadata,
        embed_metadata,
        manifest_driven,
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
        param_overrides: flags.param_overrides.clone(),
        param_policy: flags.param_policy,
        retain_wir: flags.retain_wir,
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
    let manifest_root_for_inline = project.as_ref().map(|p| p.root.clone());
    let probe_manifest_root = manifest_root_for_inline.clone().unwrap_or_else(|| {
        entry_file
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    });
    let (inline, identities, inline_diagnostics) =
        collect_inline_invocations_for_entry_with_identities(entry_file, &probe_manifest_root);

    // Fail on a malformed clause here, with its own diagnostics, so the clear
    // error is not buried under the downstream failure of the unredirected
    // `use` falling through to the lexer.
    let inline_errors = inline_diagnostics
        .iter()
        .filter(|d| d.severity == wado_compiler::Severity::Error)
        .count();
    for d in inline_diagnostics {
        wado_compiler::CompilerHost::emit_diagnostic(host, d);
    }
    if inline_errors > 0 {
        return Err(crate::kiln_driver::PipelineError::InlineClause(
            inline_errors,
        ));
    }

    let (manifest, manifest_root) = match project {
        Some(p) => (p.manifest, p.root),
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
    // Kiln paths (`from`, `inputs`, `output_dir`) are anchored at the manifest
    // root, so schemas must be loaded relative to it — not the entry file's
    // directory, where the main compile host is based.
    let kiln_host = host.rebased(manifest_root.clone());
    let mut outcome = crate::kiln_driver::run_pipeline(
        &manifest,
        &manifest_root,
        &kiln_host,
        &provider,
        inline,
        no_cache,
    )
    .await?;
    // The harvested index is keyed by full path; remap to loader identities.
    let conflict_errors = remap_and_report_conflicts(&mut outcome.invocations, &identities, host);
    if conflict_errors > 0 {
        return Err(crate::kiln_driver::PipelineError::RedirectConflict(
            conflict_errors,
        ));
    }
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
    let manifest = crate::manifest::resolve_manifest(pkg_dir, &manifest_text).ok()?;
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
        unknown_sections: Vec::new(),
        inherited_unknown_fields: Vec::new(),
    }
}

/// Harvest inline Kiln invocations from `entry_file` *and its transitive local
/// `.wado` imports*, plus the map needed to fix up the redirect index.
///
/// Kiln generation runs as a pre-pass before the loader, so a
/// `with { generator: ... }` clause in an imported module (not just the entry)
/// must be discovered here; otherwise that module's grammar `use` falls through
/// to the Wado lexer.
///
/// Returns the invocations plus a `harvest_key → loader_identity` map: the
/// harvest keys `decl_site.module` by full path, but the loader presents a
/// base-relative identity at resolve time, so the caller rewrites the index's
/// keys through this map (see `remap_index_decl_files`). The loader identity is
/// canonical (see [`wado_compiler::name::canonical_local_path`]), so the map is
/// single-valued.
pub(crate) fn collect_inline_invocations_for_entry_with_identities(
    entry_file: &Path,
    manifest_root: &Path,
) -> (
    Vec<wado_compiler::kiln::Invocation>,
    wado_compiler::hashmap::IndexMap<String, String>,
    Vec<wado_compiler::Diagnostic>,
) {
    use std::collections::VecDeque;
    use wado_compiler::name::{
        canonical_local_path, normalize_module_path, resolve_local_identity, resolve_module_path,
    };

    let mut modules =
        wado_compiler::hashmap::IndexMap::<String, wado_compiler::ast::Module>::default();
    let mut identities: wado_compiler::hashmap::IndexMap<String, String> =
        wado_compiler::hashmap::IndexMap::default();

    // (harvest_key, loader_identity, filesystem_path, is_entry)
    let mut queue: VecDeque<(String, String, std::path::PathBuf, bool)> = VecDeque::new();
    // The entry's harvest key must stay byte-identical to its
    // `EntryPoint.filename` (interned verbatim by the loader), so the redirect
    // it keys is found at resolve time — do not normalize it here.
    // `resolve_module_path` (used for children below) is panic-free on its own,
    // so a path with URI-unsafe characters no longer crashes (#1417).
    let entry_key = entry_file.to_string_lossy().to_string();
    let entry_dir = wado_compiler::name::module_parent_dir(&entry_key).to_string();
    identities.insert(entry_key.clone(), entry_key.clone());
    queue.push_back((
        entry_key.clone(),
        entry_key.clone(),
        entry_file.to_path_buf(),
        true,
    ));

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
            // Must match the loader's identity derivation (entry vs deeper).
            let child_loader_id = if is_entry {
                canonical_local_path(&entry_dir, &normalize_module_path(src))
            } else {
                resolve_local_identity(&entry_dir, &loader_id, src)
            };
            // Don't overwrite the entry's pinned identity (back-refs fold to it).
            if child_key != entry_key {
                identities.insert(child_key.clone(), child_loader_id.clone());
            }
            if !modules.contains_key(&child_key) {
                queue.push_back((child_key, child_loader_id, parent_dir.join(src), false));
            }
        }
        modules.insert(key, ast);
    }

    let descriptors = wado_compiler::hashmap::IndexMap::default();
    let manifest_root_str = manifest_root.to_string_lossy();
    let (invocations, diagnostics) = match wado_compiler::kiln::collect_inline_invocations(
        modules.iter().map(|(k, v)| (k.as_str(), v)),
        &descriptors,
        &manifest_root_str,
    ) {
        Ok(invs) => (invs, Vec::new()),
        // Hand the diagnostics back for the caller to surface; a malformed
        // clause produces no invocations.
        Err(diags) => (Vec::new(), diags),
    };
    (invocations, identities, diagnostics)
}

/// Rewrite the index's `decl_file` keys from harvest keys to loader identities
/// (keys absent from `identities` pass through). Returns a diagnostic for each
/// *conflict* — two keys resolving to the same `(loader_identity, from)` but
/// different targets — instead of silently dropping a redirect (last-write-wins);
/// matching targets are a harmless duplicate. Unreachable with canonical
/// identities; guards against a future identity-scheme regression.
#[must_use]
pub(crate) fn remap_index_decl_files(
    index: &mut wado_compiler::kiln::InvocationIndex,
    identities: &wado_compiler::hashmap::IndexMap<String, String>,
) -> Vec<wado_compiler::Diagnostic> {
    let rewritten: Vec<(String, String, String)> = index
        .entries()
        .map(|(decl, from, uri)| {
            let decl = identities.get(decl).map_or(decl, String::as_str);
            (decl.to_string(), from.to_string(), uri.to_string())
        })
        .collect();

    let mut diagnostics = Vec::new();
    let mut fresh = wado_compiler::kiln::InvocationIndex::new();
    for (decl, from, uri) in rewritten {
        // `redirect` normalizes `from` identically to `insert`, so this sees
        // the key `insert` would write.
        if let Some(existing) = fresh.redirect(&decl, &from)
            && existing != uri
        {
            diagnostics.push(wado_compiler::Diagnostic {
                severity: wado_compiler::Severity::Error,
                code: wado_compiler::Code::KilnRedirectConflict,
                message: format!(
                    "kiln: two generator invocations for `use ... from {from:?}` in `{decl}` \
                     redirect to different generated modules (`{existing}` and `{uri}`); \
                     a module cannot resolve one import to two generated entries",
                ),
                span: None,
            });
            continue;
        }
        fresh.insert(&decl, &from, &uri);
    }
    *index = fresh;
    diagnostics
}

/// Remap `index` to loader identities via [`remap_index_decl_files`], emit any
/// conflict diagnostics through `host`, and return the count of `Error`-severity
/// conflicts. Shared by `compile` and `check`, which differ only in how a
/// non-zero count maps to their own error type.
pub(crate) fn remap_and_report_conflicts(
    index: &mut wado_compiler::kiln::InvocationIndex,
    identities: &wado_compiler::hashmap::IndexMap<String, String>,
    host: &FilesystemCompilerHost,
) -> usize {
    let conflicts = remap_index_decl_files(index, identities);
    let errors = conflicts
        .iter()
        .filter(|d| d.severity == wado_compiler::Severity::Error)
        .count();
    for d in conflicts {
        wado_compiler::CompilerHost::emit_diagnostic(host, d);
    }
    errors
}

/// Walk up from `entry_file` looking for the nearest `wado.toml`. Returns
/// `None` (treated as "no Kiln config") on missing or malformed manifest.
pub fn load_nearest_manifest(entry_file: &Path) -> Option<manifest::ProjectManifest> {
    let mut dir = entry_file
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    if dir.as_os_str().is_empty() {
        dir = std::path::PathBuf::from(".");
    }
    crate::manifest::discover(&dir).ok().flatten()
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

/// Append the WIT `component-type` section to `wasm`, using `world_imports`
/// (the faithful plan read from the main compile's retained WIR, so no second
/// optimize pass). `explicit` is true for `--embed-wit`: it makes a failure
/// fatal, where the default-on path only warns and yields the un-embedded
/// (still valid, self-describing) component.
async fn embed_wit_section(
    opts: &CompileOptions,
    project: Option<&manifest::ProjectManifest>,
    wasm: Vec<u8>,
    world_imports: Vec<String>,
    explicit: bool,
) -> Result<Vec<u8>, CliExit> {
    let input = &opts.input;
    let path = Path::new(input);
    let Ok(source) = fs::read_to_string(path) else {
        // The file compiled moments ago; an unreadable source now is unexpected.
        return Ok(wasm);
    };

    let world = opts.target_world_fq().to_string();

    let base_path = path.parent().map(Path::to_path_buf).unwrap_or_default();
    // Attach the manifest `[dependencies]` so a multi-package project's
    // re-analysis resolves the same imports the main compile did; a quiet host
    // avoids re-emitting diagnostics.
    let host = attach_manifest_deps(
        FilesystemCompilerHost::silent(base_path.clone()),
        project,
        &base_path,
    );
    let sem = wado_compiler::semantics(&source, &host, Some(input)).await;
    if !sem.is_complete() {
        let msg = "WIT analysis did not complete";
        if explicit {
            return Err(CliExit::error(format!("--embed-wit: {msg}")));
        }
        eprintln!("warning: skipped embedding WIT component-type section: {msg}");
        return Ok(wasm);
    }

    let emit_opts = WitEmitOptions {
        scope: WitScope::Full,
        world_fq: world,
        default_interface_name: crate::wit::default_interface_name(input),
        world_imports,
    };

    match wit_bundle::embed_component_type(&wasm, &sem, &emit_opts) {
        Ok(embedded) => Ok(embedded),
        Err(e) if explicit => Err(CliExit::error(format!("--embed-wit: {e}"))),
        Err(e) => {
            eprintln!("warning: skipped embedding WIT component-type section: {e}");
            Ok(wasm)
        }
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
    if opts.no_embed_metadata || !opts.manifest_driven {
        return wasm;
    }
    if opts.opt_level == OptLevel::Os && !opts.embed_metadata {
        return wasm;
    }
    let Some(project) = project else {
        return wasm;
    };
    let Some(pkg) = project.manifest.package.as_ref() else {
        return wasm;
    };
    let revision = crate::metadata_embed::clean_git_revision(&project.root);
    let sections = wado_manifest::metadata_sections(pkg, revision.as_deref());
    crate::metadata_embed::embed_metadata_sections(wasm, &sections)
}

pub async fn run(opts: CompileOptions) -> Result<(), CliExit> {
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
        .then(|| opts.embed_decision())
        .flatten();

    let mut flags = opts.flags();
    flags.retain_wir = embed.is_some();
    let result = try_compile(&opts.input, &flags)
        .await
        .map_err(|_| CliExit::silent_failure(1))?;
    let wasm = result.wasm;

    if opts.wat_to_stdout {
        let wat = wasm_to_wat(&wasm)?;
        print!("{wat}");
        return Ok(());
    }

    // Discover the package manifest once, reused for the default output path and
    // the wasm-output embedding paths (WIT + metadata) so nothing re-parses it.
    let project = opts
        .manifest_driven
        .then(|| load_nearest_manifest(Path::new(&opts.input)))
        .flatten();

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
                let world_imports = result
                    .wir_package
                    .map(|p| p.imported_cm_interfaces)
                    .unwrap_or_default();
                embed_wit_section(&opts, project.as_ref(), wasm, world_imports, explicit).await?
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
    eprintln!("Generated: {}", output_path.display());
    Ok(())
}

/// The default output path when `-o` is absent. A manifest-driven build writes
/// `<manifest_root>/build/<world>.<ext>` — keeping artifacts out of the source
/// tree and giving each world its own file, mirroring kiln's `build/` layout.
/// A standalone file argument writes `<input>.<ext>` beside the source.
fn default_output_path(
    opts: &CompileOptions,
    project: Option<&manifest::ProjectManifest>,
    format: OutputFormat,
) -> std::path::PathBuf {
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
            project.root.join(BUILD_DIR).join(format!("{world}.{ext}"))
        }
        None => Path::new(&opts.input).with_extension(ext),
    }
}

/// Sanitize a Component Model world FQ name into a single path segment: drop the
/// `@version`, then replace every character outside `[A-Za-z0-9._-]` with `-`
/// (e.g. `wasi:cli/command` → `wasi-cli-command`).
fn world_path_segment(world_fq: &str) -> String {
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
mod embed_policy_tests {
    use super::{OptLevel, resolve_embed_decision};

    #[test]
    fn default_on_except_os() {
        // Default (no flags): embed, non-explicit.
        assert_eq!(
            resolve_embed_decision(false, false, OptLevel::O2),
            Some(false)
        );
        // `-Os`: default off (frontend delivery; WIT is dead weight).
        assert_eq!(resolve_embed_decision(false, false, OptLevel::Os), None);
    }

    #[test]
    fn no_embed_wit_opts_out_everywhere() {
        assert_eq!(resolve_embed_decision(true, false, OptLevel::O2), None);
        assert_eq!(resolve_embed_decision(true, false, OptLevel::Os), None);
    }

    #[test]
    fn explicit_embed_wit_forces_on_even_under_os() {
        // `--embed-wit` marks the decision explicit, so failures become fatal.
        assert_eq!(
            resolve_embed_decision(false, true, OptLevel::O2),
            Some(true)
        );
        assert_eq!(
            resolve_embed_decision(false, true, OptLevel::Os),
            Some(true)
        );
    }
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
            from: InvocationPath::normalize("grammar.g4"),
            source: InvocationPath::normalize("./grammar.g4"),
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
        let (invs, identities, _diags) =
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
        let (invs, identities, _d) =
            collect_inline_invocations_for_entry_with_identities(&entry, &root);
        assert_eq!(invs.len(), 1);
        let parser_key = &invs[0].decl_site.module;
        assert_eq!(
            identities.get(parser_key).map(String::as_str),
            Some("./gen/parser.wado"),
            "escape-reentry must canonicalize to the direct spelling",
        );

        let mut idx = wado_compiler::kiln::InvocationIndex::new();
        idx.insert(parser_key, "./G.g4", "kiln:file:///g/parser.wado");
        let conflicts = remap_index_decl_files(&mut idx, &identities);
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
        let entry_key = entry.to_string_lossy().to_string();
        let (_invs, identities, _d) =
            collect_inline_invocations_for_entry_with_identities(&entry, &root);
        assert_eq!(
            identities.get(&entry_key).map(String::as_str),
            Some(entry_key.as_str()),
            "the entry maps to itself only",
        );
        assert!(
            !identities.values().any(|v| v == "./main.wado"),
            "a back-reference must not introduce a relative entry identity: {identities:?}",
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

        let mut identities = wado_compiler::hashmap::IndexMap::default();
        identities.insert(
            "/abs/example/eval.wado".to_string(),
            "./eval.wado".to_string(),
        );

        let conflicts = remap_index_decl_files(&mut idx, &identities);
        assert!(conflicts.is_empty(), "no conflict for a single mapping");

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

    // Two keys collapsing onto one `(identity, from)` with different targets
    // must be reported, not silently resolved last-write-wins (#1423).
    #[test]
    fn remap_index_decl_files_reports_conflict_on_identity_collision() {
        use wado_compiler::kiln::InvocationIndex;

        let mut idx = InvocationIndex::new();
        idx.insert("/abs/a.wado", "./G.g4", "kiln:file:///g/from_a.wado");
        idx.insert("/abs/b.wado", "./G.g4", "kiln:file:///g/from_b.wado");

        let mut identities = wado_compiler::hashmap::IndexMap::default();
        identities.insert("/abs/a.wado".to_string(), "./shared.wado".to_string());
        identities.insert("/abs/b.wado".to_string(), "./shared.wado".to_string());

        let conflicts = remap_index_decl_files(&mut idx, &identities);
        assert_eq!(conflicts.len(), 1, "the collision must be reported");
        assert_eq!(conflicts[0].code, wado_compiler::Code::KilnRedirectConflict);
        assert_eq!(conflicts[0].severity, wado_compiler::Severity::Error);
        // The first redirect survives; the conflicting second is dropped.
        assert_eq!(
            idx.redirect("./shared.wado", "./G.g4"),
            Some("kiln:file:///g/from_a.wado"),
        );
    }

    // Same identity *and* target is a benign duplicate: no diagnostic.
    #[test]
    fn remap_index_decl_files_allows_duplicate_identical_target() {
        use wado_compiler::kiln::InvocationIndex;

        let mut idx = InvocationIndex::new();
        idx.insert("/abs/a.wado", "./G.g4", "kiln:file:///g/shared.wado");
        idx.insert("/abs/b.wado", "./G.g4", "kiln:file:///g/shared.wado");

        let mut identities = wado_compiler::hashmap::IndexMap::default();
        identities.insert("/abs/a.wado".to_string(), "./shared.wado".to_string());
        identities.insert("/abs/b.wado".to_string(), "./shared.wado".to_string());

        let conflicts = remap_index_decl_files(&mut idx, &identities);
        assert!(conflicts.is_empty(), "identical target is not a conflict");
        assert_eq!(
            idx.redirect("./shared.wado", "./G.g4"),
            Some("kiln:file:///g/shared.wado"),
        );
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
        let (invs, identities, _d) =
            collect_inline_invocations_for_entry_with_identities(&entry, &root);

        assert_eq!(invs.len(), 1, "the generator clause is harvested once");
        let foo_key = &invs[0].decl_site.module;
        assert_eq!(
            identities.get(foo_key).map(String::as_str),
            Some("./a/foo.wado"),
            "diamond must collapse to one canonical loader identity",
        );

        let mut idx = wado_compiler::kiln::InvocationIndex::new();
        idx.insert(foo_key, "./G.g4", "kiln:file:///g/foo.wado");
        let conflicts = remap_index_decl_files(&mut idx, &identities);
        assert!(conflicts.is_empty());
        assert_eq!(
            idx.redirect("./a/foo.wado", "./G.g4"),
            Some("kiln:file:///g/foo.wado"),
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
