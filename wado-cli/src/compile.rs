use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process;

use lexopt::Arg::Value;
use lexopt::Parser;
use wado_compiler::LogLevel;

use crate::args::{self, CliExit};
use crate::compiler_host::FilesystemCompilerHost;
use crate::kiln_driver::{PipelineError, PipelineOutcome};
use crate::kiln_provider::CliGeneratorProvider;
use crate::manifest;

/// Optimization level
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OptLevel {
    /// No optimizations. Used for debugging.
    O0,
    /// Development optimizations. All passes except DCE.
    /// Keeps dead code visible for debugging while improving runtime.
    /// Iterations: 2, Inline threshold: 10.
    O1,
    /// Production optimizations. Full passes including DCE (default).
    /// Iterations: 10, Inline threshold: 10.
    #[default]
    O2,
    /// Aggressive production optimizations. Full passes including DCE.
    /// Iterations: 100, Inline threshold: 20.
    O3,
    /// Size optimizations. O2 plus name section stripping.
    Os,
}

impl OptLevel {
    /// Convert to the matching wasmtime Cranelift `opt_level` so the runtime
    /// JIT pipeline tracks the Wado front-end optimization level. wasmtime
    /// has only three Cranelift settings (`None`/`Speed`/`SpeedAndSize`),
    /// so `O1`/`O2`/`O3` collapse to the same `Speed`. This mirrors what
    /// `wasmtime` CLI's own `-O` mapping does.
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
    pub lib: bool,
}

/// Compile-time options shared by every subcommand that produces a Wasm
/// component (`compile`, `run`, `serve`, `test`).
///
/// `target_world` and `allocator` are left as `Option`s because each
/// subcommand has a different default: `wado run` expects
/// `wasi:cli/command`, `wado serve` pins `wasi:http/service`, `wado test`
/// pins `test`, and `wado compile` lets the user override via `--world`.
/// Same for the allocator, which the compiler picks per-world when `None`.
#[derive(Clone, Debug, Default)]
pub struct CompileFlags {
    pub opt_level: OptLevel,
    pub log_level: LogLevel,
    pub target_world: Option<String>,
    pub skip_validation: bool,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
    pub allocator: Option<String>,
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
    Allocator,
    Lib,
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
        Self::Allocator,
        Self::Lib,
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
            Self::Allocator => args::ALLOCATOR_SPEC,
            Self::Lib => args::OptSpec {
                long: Some("lib"),
                short: None,
                value: None,
                desc: "Compile the library entry point from wado.toml ([package].lib)",
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
    buf
}

pub fn print_usage() {
    eprint!("{}", format_usage());
}

/// Parse command-line arguments for the `compile` subcommand.
///
/// # Errors
///
/// Returns an error if the arguments are invalid or required arguments are missing.
pub fn parse_args(mut parser: lexopt::Parser) -> Result<CompileOptions, CliExit> {
    let usage = format_usage();
    let mut output: Option<String> = None;
    let mut format: Option<OutputFormat> = None;
    let mut input: Option<String> = None;
    let mut opt_level = OptLevel::default();
    let mut wat_to_stdout = false;
    let mut log_level = LogLevel::default();
    let mut target_world: Option<String> = None;
    let mut skip_validation = false;
    let mut inline_threshold: Option<usize> = None;
    let mut opt_iterations: Option<u32> = None;
    let mut allocator: Option<String> = None;
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
                Opt::Allocator => {
                    allocator = Some(args::require_string(&mut parser)?);
                }
                Opt::Lib => lib = true,
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            args::reject_multiple_inputs(&input)?;
            input = Some(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    let entry_kind = if lib {
        manifest::EntryPointKind::Lib
    } else {
        manifest::EntryPointKind::Command
    };

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
        lib,
    })
}

/// Convert CLI `OptLevel` to compiler `OptLevel`
fn to_compiler_opt_level(level: OptLevel) -> wado_compiler::OptLevel {
    match level {
        OptLevel::O0 => wado_compiler::OptLevel::O0,
        OptLevel::O1 => wado_compiler::OptLevel::O1,
        OptLevel::O2 => wado_compiler::OptLevel::O2,
        OptLevel::O3 => wado_compiler::OptLevel::O3,
        OptLevel::Os => wado_compiler::OptLevel::Os,
    }
}

/// Parse the `-O<n>` value (with optional bare-`-O`). Shared by every
/// subcommand that exposes optimization-level control.
///
/// # Errors
///
/// Returns an error if the level token after `-O` is not recognised.
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

/// Try to compile a Wado source file, returning the compiler result without
/// exiting. Used by the test runner to handle `#![TODO]` modules gracefully.
///
/// # Errors
///
/// Propagates the compiler's own `CompileFailure` (with `is_todo_module`
/// set when the failure was an expected one for a `#![TODO]` module).
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
    let host = FilesystemCompilerHost::with_log_level(base_path, flags.log_level);

    let pipeline_outcome = match maybe_run_pipeline(path, &host).await {
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
        ..Default::default()
    };

    wado_compiler::compile_with_options(&source, &host, Some(filename), options).await
}

/// Compile a Wado source file and return the produced wasm bytes. Aborts
/// the process on any failure (diagnostics already printed via the host).
pub async fn compile(filename: &str, flags: &CompileFlags) -> Vec<u8> {
    match try_compile(filename, flags).await {
        Ok(result) => result.wasm,
        Err(_) => process::exit(1),
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
async fn maybe_run_pipeline(
    entry_file: &Path,
    host: &FilesystemCompilerHost,
) -> Result<PipelineOutcome, PipelineError> {
    let manifest_pair = load_nearest_manifest(entry_file);

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
    let provider = CliGeneratorProvider::new(manifest_root.clone());
    crate::kiln_driver::run_pipeline(&manifest, &manifest_root, host, &provider, inline).await
}

/// Parse the entry file to collect inline Kiln invocations from
/// `use ... with { generator: { ... } }` clauses. Returns an empty vector when
/// the file cannot be parsed — downstream compilation will surface the parse
/// error, so we don't need to report it twice.
/// Construct an empty in-memory `wado.toml` manifest used as a fallback
/// when the compiled file has no nearby manifest. Shared with
/// [`crate::check`].
#[must_use]
pub fn empty_manifest() -> wado_manifest::Manifest {
    wado_manifest::Manifest {
        package: None,
        registries: indexmap::IndexMap::new(),
        dependencies: indexmap::IndexMap::new(),
        dev_dependencies: indexmap::IndexMap::new(),
        build_dependencies: indexmap::IndexMap::new(),
        workspace: None,
        test: wado_manifest::TestSettings::default(),
    }
}

pub fn collect_inline_invocations_for_entry(
    entry_file: &Path,
    manifest_root: &Path,
) -> Vec<wado_compiler::kiln::Invocation> {
    let Ok(source) = fs::read_to_string(entry_file) else {
        return Vec::new();
    };
    let Ok(parsed) = wado_compiler::parse(&source) else {
        return Vec::new();
    };
    let mut modules =
        wado_compiler::hashmap::IndexMap::<String, wado_compiler::ast::Module>::default();
    // Key the module by the same string the loader uses as the entry's
    // `ModuleSource::EntryPoint { filename }` — the full path — so the
    // The `decl_site.module` recorded here matches the `decl_file`
    // the loader feeds into `InvocationIndex::redirect` at resolve time.
    // Without this, the inline redirect misses entirely.
    let entry_name = entry_file.to_string_lossy().to_string();
    modules.insert(entry_name, parsed.ast);
    let descriptors = wado_compiler::hashmap::IndexMap::default();
    let manifest_root_str = manifest_root.to_string_lossy();
    wado_compiler::kiln::collect_inline_invocations(&modules, &descriptors, &manifest_root_str)
        .unwrap_or_default()
}

/// Walk up from `entry_file` looking for the nearest `wado.toml`. Returns the
/// parsed manifest plus the directory that contains it (the Kiln pipeline's
/// `manifest_root`). Silently returns `None` when no manifest is found or the
/// manifest cannot be parsed — the caller treats this as "no Kiln config" and
/// continues.
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

/// Convert Wasm binary to WAT text format (folded style)
fn wasm_to_wat(wasm: &[u8]) -> String {
    let mut config = wasmprinter::Config::new();
    config.fold_instructions(true);
    let mut wat = String::new();
    config
        .print(wasm, &mut wasmprinter::PrintFmtWrite(&mut wat))
        .unwrap_or_else(|e| {
            eprintln!("Error generating WAT: {e}");
            process::exit(1);
        });
    wat
}

pub async fn run(opts: CompileOptions) {
    let flags = opts.flags();
    let wasm = compile(&opts.input, &flags).await;

    // Handle --wat-to-stdout: output WAT to stdout and return
    if opts.wat_to_stdout {
        let wat = wasm_to_wat(&wasm);
        print!("{wat}");
        return;
    }

    // Determine format: explicit > guessed from -o extension > default (wasm)
    let format = opts
        .format
        .or_else(|| {
            opts.output
                .as_ref()
                .and_then(|p| OutputFormat::from_extension(Path::new(p)))
        })
        .unwrap_or(OutputFormat::Wasm);

    // Determine output path, using format to pick extension if no -o specified
    let output_path = if let Some(path) = &opts.output {
        Path::new(path).to_path_buf()
    } else {
        let ext = match format {
            OutputFormat::Wasm => "wasm",
            OutputFormat::Wat => "wat",
        };
        Path::new(&opts.input).with_extension(ext)
    };

    match format {
        OutputFormat::Wasm => match fs::write(&output_path, &wasm) {
            Ok(()) => {
                eprintln!("Generated: {}", output_path.display());
            }
            Err(e) => {
                eprintln!("Error writing output file: {e}");
                process::exit(1);
            }
        },
        OutputFormat::Wat => {
            let wat = wasm_to_wat(&wasm);
            match fs::write(&output_path, &wat) {
                Ok(()) => {
                    eprintln!("Generated: {}", output_path.display());
                }
                Err(e) => {
                    eprintln!("Error writing output file: {e}");
                    process::exit(1);
                }
            }
        }
    }
}
