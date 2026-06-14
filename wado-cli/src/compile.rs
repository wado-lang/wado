use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use wado_manifest::DependencySource;

use lexopt::Arg::Value;
use lexopt::Parser;
use wado_compiler::LogLevel;

use crate::args::{self, CliExit};
use crate::compiler_host::FilesystemCompilerHost;
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
        }
    }
}

#[derive(Clone, Copy)]
enum Opt {
    Output,
    Format,
    WatToStdout,
    World,
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

    // `--lib` is abolished pending a world model that fits libraries; every
    // `wado compile` resolves the command entry point.
    let entry_kind = manifest::EntryPointKind::Command;

    Ok(CompileOptions {
        input: manifest::resolve_input(input, entry_kind, &usage)?,
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
    let dep_index = manifest_pair
        .as_ref()
        .map(|(manifest, root)| wado_lsp::host::dependency_index_from(manifest, root, &base_path));
    let mut host = FilesystemCompilerHost::with_log_level(base_path, flags.log_level);
    if let Some(index) = dep_index {
        host = host.with_dependency_index(index);
    }

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

/// Collect inline `with { generator: { ... } }` clauses from `entry_file`
/// (and any sibling manifest's directory if one is found), then drive the
/// Kiln pipeline via [`run_pipeline`]. Returns `Ok(PipelineOutcome::default())`
/// when no inline clauses were collected.
///
/// Errors from [`run_pipeline`] are surfaced unchanged; the caller decides
/// whether to abort or continue (e.g. for consume-only mode, a stale-cache
/// warning is not fatal).
async fn maybe_run_pipeline(
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
    let inline = collect_inline_invocations_for_entry(entry_file, &probe_manifest_root);

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
    crate::kiln_driver::run_pipeline(&manifest, &manifest_root, host, &provider, inline, no_cache)
        .await
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
    let Ok(source) = fs::read_to_string(entry_file) else {
        return Vec::new();
    };
    // `parse` is resilient; if the entry has recovered lex or parse errors
    // the AST is partial. Refuse to harvest kiln invocations from a partial
    // tree so a mid-edit source can't trigger surprising codegen side
    // effects — matches the prior fail-on-parse-error behaviour.
    let Ok(parsed) = wado_compiler::parse(&source).into_fail_fast() else {
        return Vec::new();
    };
    let mut modules =
        wado_compiler::hashmap::IndexMap::<String, wado_compiler::ast::Module>::default();
    // Key by the full path so `decl_site.module` here matches the
    // `decl_file` the loader feeds into `InvocationIndex::redirect`;
    // otherwise the inline redirect misses.
    let entry_name = entry_file.to_string_lossy().to_string();
    modules.insert(entry_name, parsed.ast);
    let descriptors = wado_compiler::hashmap::IndexMap::default();
    let manifest_root_str = manifest_root.to_string_lossy();
    wado_compiler::kiln::collect_inline_invocations(
        modules.iter().map(|(k, v)| (k.as_str(), v)),
        &descriptors,
        &manifest_root_str,
    )
    .unwrap_or_default()
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
                assert_eq!(p.as_str(), "gen-pkg/src/generator.wado")
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
}
