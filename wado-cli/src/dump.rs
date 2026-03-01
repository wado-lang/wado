use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process;

use lexopt::Arg::Value;
use wado_compiler::OptLevel;

use crate::args::{self, CliExit};
use crate::compiler_host::FilesystemCompilerHost;

pub struct DumpOptions {
    pub inputs: Vec<String>,
    pub show_tokens: bool,
    pub show_ast: bool,
    pub show_desugar: bool,
    pub show_symbols: bool,
    pub show_modules: bool,
    pub show_tir: bool,
    pub show_monomorphize: bool,
    pub show_lower: bool,
    pub show_optimize: bool,
    pub show_wir: bool,
    pub unparse: bool,
    pub opt_level: OptLevel,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
    /// Output template for bulk generation, e.g., "path/to/{name}.lowered.wado"
    /// {name} is replaced with the input file's basename without extension
    pub output_template: Option<String>,
}

#[derive(Clone, Copy)]
#[allow(clippy::enum_variant_names)]
enum Opt {
    Tokens,
    Ast,
    Desugar,
    Modules,
    Symbols,
    Tir,
    Monomorphize,
    Lower,
    Optimize,
    Wir,
    All,
    Unparse,
    OptLevel,
    InlineThreshold,
    OptIterations,
    Output,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Tokens,
        Self::Ast,
        Self::Desugar,
        Self::Modules,
        Self::Symbols,
        Self::Tir,
        Self::Monomorphize,
        Self::Lower,
        Self::Optimize,
        Self::Wir,
        Self::All,
        Self::Unparse,
        Self::OptLevel,
        Self::InlineThreshold,
        Self::OptIterations,
        Self::Output,
        Self::Help,
    ];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Tokens => args::OptSpec {
                long: Some("tokens"),
                short: None,
                value: None,
                desc: "(Phase 1: Lexer) Show tokens",
            },
            Self::Ast => args::OptSpec {
                long: Some("ast"),
                short: None,
                value: None,
                desc: "(Phase 2: Parser) Show AST structure",
            },
            Self::Desugar => args::OptSpec {
                long: Some("desugar"),
                short: None,
                value: None,
                desc: "(Phase 3: Desugar) Show desugared AST",
            },
            Self::Modules => args::OptSpec {
                long: Some("modules"),
                short: None,
                value: None,
                desc: "(Phase 4: Load) Show loaded modules",
            },
            Self::Symbols => args::OptSpec {
                long: Some("symbols"),
                short: None,
                value: None,
                desc: "(Phase 5: Analyze) Show symbol table",
            },
            Self::Tir => args::OptSpec {
                long: Some("tir"),
                short: None,
                value: None,
                desc: "(Phase 6: Resolve) Show TIR (Typed IR)",
            },
            Self::Monomorphize => args::OptSpec {
                long: Some("monomorphize"),
                short: None,
                value: None,
                desc: "(Phase 7: Monomorphize) Show monomorphized TIR",
            },
            Self::Lower => args::OptSpec {
                long: Some("lower"),
                short: None,
                value: None,
                desc: "(Phase 8: Lower) Show lowered TIR",
            },
            Self::Optimize => args::OptSpec {
                long: Some("optimize"),
                short: None,
                value: None,
                desc: "(Phase 9: Optimize) Show optimization hints",
            },
            Self::Wir => args::OptSpec {
                long: Some("wir"),
                short: None,
                value: None,
                desc: "(Phase 10: WIR) Show Wasm IR",
            },
            Self::All => args::OptSpec {
                long: Some("all"),
                short: None,
                value: None,
                desc: "Show all phases (default if no phase specified)",
            },
            Self::Unparse => args::OptSpec {
                long: Some("unparse"),
                short: None,
                value: None,
                desc: "Unparse to Wado source code (for ast/desugar/tir/lower/optimize)\nDefault: Debug/tree format",
            },
            Self::OptLevel => args::OptSpec {
                long: None,
                short: Some('O'),
                value: Some("<n>"),
                desc: "Optimization level (for --optimize phase)",
            },
            Self::InlineThreshold => args::INLINE_THRESHOLD_SPEC,
            Self::OptIterations => args::OPT_ITERATIONS_SPEC,
            Self::Output => args::OptSpec {
                long: Some("output"),
                short: Some('o'),
                value: Some("<template>"),
                desc: "Output template for bulk file generation\n{name} is replaced with input filename (without extension)\nExample: -o 'out/{name}.lowered.wado'",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(
        buf,
        "Usage: wado dump [options] <file.wado> [file2.wado ...]"
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Dump compiler internal state for debugging.").unwrap();
    writeln!(buf, "Supports multiple input files for batch processing.").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Compilation Phases:").unwrap();
    write!(
        buf,
        "{}",
        args::format_opts_help(
            &[
                Opt::Tokens,
                Opt::Ast,
                Opt::Desugar,
                Opt::Modules,
                Opt::Symbols,
                Opt::Tir,
                Opt::Monomorphize,
                Opt::Lower,
                Opt::Optimize,
                Opt::Wir,
                Opt::All,
            ],
            |o| o.spec(),
        )
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Display Options:").unwrap();
    write!(
        buf,
        "{}",
        args::format_opts_help(&[Opt::Unparse], |o| o.spec())
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Optimization Level (for --optimize phase):").unwrap();
    writeln!(buf, "  -O0          No optimizations").unwrap();
    writeln!(
        buf,
        "  -O1          Development optimizations (all passes except DCE)"
    )
    .unwrap();
    writeln!(buf, "  -O2          Production optimizations (default)").unwrap();
    writeln!(buf, "  -O3          Aggressive optimizations").unwrap();
    writeln!(buf, "  -Os          Size optimizations (O2 + strip names)").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Output:").unwrap();
    write!(
        buf,
        "{}",
        args::format_opts_help(&[Opt::Output], |o| o.spec())
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Other:").unwrap();
    write!(
        buf,
        "{}",
        args::format_opts_help(&[Opt::Help], |o| o.spec())
    )
    .unwrap();
    buf
}

pub fn print_usage() {
    eprint!("{}", format_usage());
}

#[allow(clippy::similar_names)] // show_tir and show_wir are intentional phase names
pub fn parse_args(mut parser: lexopt::Parser) -> Result<DumpOptions, CliExit> {
    let usage = format_usage();
    let mut inputs: Vec<String> = Vec::new();
    let mut show_tokens = false;
    let mut show_ast = false;
    let mut show_desugar = false;
    let mut show_symbols = false;
    let mut show_modules = false;
    let mut show_tir = false;
    let mut show_monomorphize = false;
    let mut show_lower = false;
    let mut show_optimize = false;
    let mut show_wir = false;
    let mut unparse = false;
    let mut opt_level = OptLevel::O2;
    let mut inline_threshold: Option<usize> = None;
    let mut opt_iterations: Option<u32> = None;
    let mut output_template: Option<String> = None;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Tokens => show_tokens = true,
                Opt::Ast => show_ast = true,
                Opt::Desugar => show_desugar = true,
                Opt::Symbols => show_symbols = true,
                Opt::Modules => show_modules = true,
                Opt::Tir => show_tir = true,
                Opt::Monomorphize => show_monomorphize = true,
                Opt::Lower => show_lower = true,
                Opt::Optimize => show_optimize = true,
                Opt::Wir => show_wir = true,
                Opt::All => {
                    show_tokens = true;
                    show_ast = true;
                    show_desugar = true;
                    show_symbols = true;
                    show_modules = true;
                    show_tir = true;
                    show_monomorphize = true;
                    show_lower = true;
                    show_optimize = true;
                    show_wir = true;
                }
                Opt::Unparse => unparse = true,
                Opt::OptLevel => {
                    let level = args::require_value(&mut parser)
                        .map_err(|_| CliExit::error("-O requires a level (0, 1, 2, 3, or s)"))?;
                    let level_str = level.to_string_lossy();
                    opt_level = match level_str.as_ref() {
                        "0" => OptLevel::O0,
                        "1" => OptLevel::O1,
                        "2" => OptLevel::O2,
                        "3" => OptLevel::O3,
                        "s" => OptLevel::Os,
                        _ => {
                            return Err(CliExit::error(format!(
                                "Unknown optimization level: -O{level_str}\nValid levels: -O0, -O1, -O2, -O3, -Os"
                            )));
                        }
                    };
                }
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
                Opt::Output => {
                    let template = args::require_value(&mut parser)
                        .map_err(|_| CliExit::error("-o requires an output template"))?;
                    output_template = Some(template.to_string_lossy().into_owned());
                }
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            inputs.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    let inputs = args::require_inputs(inputs, &usage)?;

    // Default: show all
    if !show_tokens
        && !show_ast
        && !show_desugar
        && !show_symbols
        && !show_modules
        && !show_tir
        && !show_monomorphize
        && !show_lower
        && !show_optimize
        && !show_wir
    {
        show_tokens = true;
        show_ast = true;
        show_desugar = true;
        show_symbols = true;
        show_modules = true;
        show_tir = true;
        show_monomorphize = true;
        show_lower = true;
        show_optimize = true;
        show_wir = true;
    }

    Ok(DumpOptions {
        inputs,
        show_tokens,
        show_ast,
        show_desugar,
        show_symbols,
        show_modules,
        show_tir,
        show_monomorphize,
        show_lower,
        show_optimize,
        show_wir,
        unparse,
        opt_level,
        inline_threshold,
        opt_iterations,
        output_template,
    })
}

pub async fn run(opts: DumpOptions) {
    // Bulk file output mode
    if let Some(ref template) = opts.output_template {
        run_bulk(&opts, template).await;
        return;
    }

    // Normal stdout mode
    let multiple_files = opts.inputs.len() > 1;

    for input in &opts.inputs {
        if multiple_files {
            println!("// ========== {input} ==========");
        }

        run_single(&opts, input).await;
    }
}

/// Stack size for compiler worker threads (16 MB).
///
/// The default tokio blocking thread stack (2 MB) is too small for the
/// recursive optimizer passes; processing many fixtures causes
/// stack overflow (SIGSEGV).
const COMPILER_STACK_SIZE: usize = 16 * 1024 * 1024;

/// Spawn a closure on a dedicated thread with a large stack, returning a
/// oneshot receiver for the result.
fn spawn_with_large_stack<F, T>(f: F) -> tokio::sync::oneshot::Receiver<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .stack_size(COMPILER_STACK_SIZE)
        .spawn(move || {
            let _ = tx.send(f());
        })
        .expect("failed to spawn compiler thread");
    rx
}

/// Bulk file output mode - writes each input to a file based on template (parallel)
async fn run_bulk(opts: &DumpOptions, template: &str) {
    let start = std::time::Instant::now();

    // Limit concurrency to avoid exhausting memory with many large-stack threads.
    // Each compilation uses ~16 MB stack + significant heap for the optimizer,
    // so we cap at half the available CPUs (minimum 2) to stay within memory.
    let parallelism = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(4)
        / 2;
    let parallelism = parallelism.max(2);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(parallelism));

    let mut skip_count_early = 0u32;
    let mut tasks: Vec<tokio::task::JoinHandle<Result<String, String>>> = Vec::new();

    for input in &opts.inputs {
        // Skip files with TODO or compile_error in __DATA__ section
        if let Ok(source) = fs::read_to_string(input)
            && let Some(data_start) = source.find("\n__DATA__\n")
        {
            let data = &source[data_start..];
            if data.contains("\"TODO\"") || data.contains("\"compile_error\"") {
                // Remove stale golden file if it exists
                let name = Path::new(input)
                    .file_stem()
                    .map_or("unknown", |s| s.to_str().unwrap_or("unknown"));
                let output_path = template.replace("{name}", name);
                let _ = fs::remove_file(&output_path);
                skip_count_early += 1;
                continue;
            }
        }

        let input = input.clone();
        let template = template.to_owned();
        let show_optimize = opts.show_optimize;
        let unparse = opts.unparse;
        let opt_level = opts.opt_level;
        let inline_threshold = opts.inline_threshold;
        let opt_iterations = opts.opt_iterations;
        let sem = semaphore.clone();

        let handle = tokio::runtime::Handle::current();

        tasks.push(tokio::spawn(async move {
            // Acquire a permit to limit the number of concurrent compiler threads.
            let _permit = sem.acquire().await.expect("semaphore closed");

            let rx = spawn_with_large_stack(move || {
                let path = Path::new(&input);
                let name = path.file_stem().map_or_else(
                    || "unknown".to_string(),
                    |s| s.to_string_lossy().into_owned(),
                );

                let output_path = template.replace("{name}", &name);

                // Ensure parent directory exists
                if let Some(parent) = Path::new(&output_path).parent()
                    && !parent.as_os_str().is_empty()
                    && let Err(e) = fs::create_dir_all(parent)
                {
                    return Err(format!(
                        "Failed to create directory {}: {e}",
                        parent.display()
                    ));
                }

                // Use block_on to run async dump_with_host on this thread
                match handle.block_on(generate_output_params(
                    show_optimize,
                    unparse,
                    opt_level,
                    inline_threshold,
                    opt_iterations,
                    &input,
                )) {
                    Ok(content) => {
                        if content.is_empty() {
                            let _ = fs::remove_file(&output_path);
                            Err(format!("Empty output for {output_path} (skipping)"))
                        } else {
                            match fs::write(&output_path, content) {
                                Ok(()) => Ok(output_path),
                                Err(e) => Err(format!("Failed to write {output_path}: {e}")),
                            }
                        }
                    }
                    Err(e) => {
                        let _ = fs::remove_file(&output_path);
                        Err(format!("Failed to generate {output_path}: {e}"))
                    }
                }
            });

            rx.await
                .unwrap_or_else(|_| Err("compiler thread panicked".to_string()))
        }));
    }

    let mut success_count = 0;
    let mut skip_count = 0;

    for task in tasks {
        match task.await {
            Ok(Ok(output_path)) => {
                eprintln!("  Generated {output_path}");
                success_count += 1;
            }
            Ok(Err(warning)) => {
                eprintln!("  WARNING: {warning}");
                skip_count += 1;
            }
            Err(e) => {
                eprintln!("  WARNING: Task panicked: {e}");
                skip_count += 1;
            }
        }
    }

    let skip_count = skip_count + skip_count_early;
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!("Generated {success_count} files ({skip_count} skipped) in {elapsed:.2}s");
}

/// Generate output content for a single file, returning the content string.
///
/// Takes individual parameters instead of `&DumpOptions` so it can be called
/// from `tokio::spawn` (which requires `'static` futures).
async fn generate_output_params(
    show_optimize: bool,
    unparse: bool,
    opt_level: OptLevel,
    inline_threshold: Option<usize>,
    opt_iterations: Option<u32>,
    input: &str,
) -> Result<String, String> {
    let path = Path::new(input);

    // Read source file
    let source =
        fs::read_to_string(path).map_err(|e| format!("Error reading '{}': {e}", path.display()))?;

    // Get base path for relative imports
    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemCompilerHost::new(base_path);

    // Extract target world from __DATA__ section if present
    let target_world = extract_world_from_data_section(&source);

    // Dump using async API with target world
    let result = wado_compiler::dump_with_host_and_world(
        &source,
        &host,
        Some(input),
        opt_level,
        target_world.as_deref(),
        inline_threshold,
        opt_iterations,
    )
    .await
    .map_err(|_bail| "compilation failed".to_string())?;

    let mut output = Vec::new();

    // For golden fixtures (--optimize --unparse), extract only the entry module
    if show_optimize && unparse {
        if let Some(ref project) = result.optimized_project {
            // Find the entry module (ModuleSource::EntryPoint)
            for (module_source, module) in &project.tir_modules {
                if module_source.is_entry_point() {
                    // Use the original input path for the Source header
                    let source_path = path.to_string_lossy();

                    writeln!(output, "// Golden file: Lowered TIR with -O2 optimization").unwrap();
                    writeln!(output, "// Source: {source_path}").unwrap();
                    writeln!(output, "// Generated by: make update-golden-fixtures").unwrap();
                    writeln!(output).unwrap();

                    let unparsed = wado_compiler::unparse::unparse_tir(module);
                    // Strip __DATA__ section (same as original awk script)
                    let content = if let Some(idx) = unparsed.find("\n__DATA__\n") {
                        &unparsed[..=idx] // Include the newline before __DATA__
                    } else if let Some(idx) = unparsed.find("__DATA__\n") {
                        &unparsed[..idx]
                    } else {
                        &unparsed
                    };
                    write!(output, "{content}").unwrap();
                    break;
                }
            }
        }
    } else {
        // For other phases, generate full output (not implemented for bulk yet)
        return Err("Bulk output only supports --optimize --unparse mode".to_string());
    }

    Ok(String::from_utf8(output).unwrap())
}

async fn run_single(opts: &DumpOptions, input: &str) {
    let path = Path::new(input);

    // Read source file
    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading '{}': {e}", path.display());
            process::exit(1);
        }
    };

    // Get base path for relative imports
    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemCompilerHost::new(base_path);

    // Extract target world from __DATA__ section if present
    let target_world = extract_world_from_data_section(&source);

    // Dump using async API with target world
    let result = match wado_compiler::dump_with_host_and_world(
        &source,
        &host,
        Some(input),
        opts.opt_level,
        target_world.as_deref(),
        opts.inline_threshold,
        opts.opt_iterations,
    )
    .await
    {
        Ok(r) => r,
        Err(_bail) => {
            // Errors already printed by host via emit_diagnostic
            process::exit(1);
        }
    };

    // Tokens section (Lexer phase)
    if opts.show_tokens {
        println!("=== Tokens (Lexer) ===");
        for (i, token) in result.tokens.iter().enumerate() {
            println!("  [{i}] {token:?}");
        }
        println!();
    }

    // AST section (Parser phase)
    if opts.show_ast {
        if opts.unparse {
            println!("=== AST (Parser, unparsed) ===");
            let unparser = wado_compiler::unparse::Unparser::new(&result.comments);
            let unparsed = unparser.unparse(&result.ast);
            println!("{unparsed}");
        } else {
            println!("=== AST (Parser) ===");
            for (i, item) in result.ast.items.iter().enumerate() {
                println!("  [{i}] {item:#?}");
            }
            if let Some(data) = result.ast.data_section() {
                println!();
                println!("--- Data Section ---");
                println!("{data}");
            }
        }
        println!();
    }

    // Desugared AST section (Desugar phase)
    if opts.show_desugar {
        if opts.unparse {
            println!("=== Desugared AST (Desugar, unparsed) ===");
            let unparser = wado_compiler::unparse::Unparser::new(&result.comments);
            let unparsed = unparser.unparse(&result.desugared_ast);
            println!("{unparsed}");
        } else {
            println!("=== Desugared AST (Desugar) ===");
            for (i, item) in result.desugared_ast.items.iter().enumerate() {
                println!("  [{i}] {item:#?}");
            }
            if let Some(data) = result.desugared_ast.data_section() {
                println!();
                println!("--- Data Section ---");
                println!("{data}");
            }
        }
        println!();
    }

    // Modules section (Load phase)
    if opts.show_modules {
        println!("=== Loaded Modules (Load) ===");
        for module_source in &result.loaded_modules {
            let path_str = module_source.to_string();
            let is_implicit = result.implicit_modules.contains(module_source);
            if is_implicit {
                println!("  {path_str} (implicit)");
            } else {
                println!("  {path_str}");
            }
        }
        println!();

        println!("=== Implicit Modules ===");
        for module_source in &result.implicit_modules {
            let path_str = module_source.to_string();
            println!("  {path_str}");
        }
        println!();
    }

    // Symbols section (Analyze phase)
    if opts.show_symbols {
        println!("=== Symbol Table (Analyze) ===");
        for symbol in result.symbols.all_symbols() {
            let module_path = symbol.module_source.to_string();
            let kind_str = match &symbol.kind {
                wado_compiler::symbol::SymbolKind::Function(f) => {
                    let effects = if f.effects.is_empty() {
                        String::new()
                    } else {
                        format!(" with {}", f.effects.join(", "))
                    };
                    let ret = f
                        .return_type
                        .as_ref()
                        .map(|t| format!(" -> {t}"))
                        .unwrap_or_default();
                    let wasi = f
                        .wasi_import
                        .as_ref()
                        .map(|w| format!(" [wasi: {}]", w.interface_path()))
                        .unwrap_or_default();
                    format!("fn({}){ret}{effects}{wasi}", f.params.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Effect(e) => {
                    let wasi = e
                        .wasi_import
                        .as_ref()
                        .map(|w| format!(" [wasi: {}]", w.interface_path()))
                        .unwrap_or_default();
                    format!("effect{{ {} }}{wasi}", e.methods.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Struct(s) => {
                    format!("struct{{ {} }}", s.fields.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Enum(e) => {
                    format!("enum{{ {} }}", e.cases.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Flags(f) => {
                    format!("flags{{ {} }}", f.members.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Variant(v) => {
                    format!("variant{{ {} }}", v.cases.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Newtype(t) => {
                    format!("type = {}", t.aliased_type)
                }
                wado_compiler::symbol::SymbolKind::Variable(v) => {
                    let mut flags = Vec::new();
                    if v.is_mut {
                        flags.push("mut");
                    }
                    if v.is_reactive {
                        flags.push("reactive");
                    }
                    if flags.is_empty() {
                        "var".to_string()
                    } else {
                        format!("var({})", flags.join(", "))
                    }
                }
                wado_compiler::symbol::SymbolKind::Resource(r) => {
                    let wasi = r
                        .wasi_import
                        .as_ref()
                        .map(|w| format!(" [wasi: {}]", w.interface_path()))
                        .unwrap_or_default();
                    format!("resource{{ {} }}{wasi}", r.methods.join(", "))
                }
                wado_compiler::symbol::SymbolKind::World(w) => {
                    let imports: Vec<_> = w.imports.iter().map(|i| i.effect_name.clone()).collect();
                    let exports: Vec<_> = w.exports.iter().map(|e| e.name.clone()).collect();
                    format!(
                        "world{{ imports: [{}], exports: [{}] }}",
                        imports.join(", "),
                        exports.join(", ")
                    )
                }
                wado_compiler::symbol::SymbolKind::Trait(t) => {
                    format!("trait{{ {} }}", t.methods.join(", "))
                }
                wado_compiler::symbol::SymbolKind::Global(g) => {
                    if g.is_mut {
                        "global mut".to_string()
                    } else {
                        "global".to_string()
                    }
                }
            };
            println!(
                "  [{}] {} :: {} = {}",
                symbol.id.0, module_path, symbol.name, kind_str
            );
        }
        println!();
    }

    // TIR section (Resolve phase)
    if opts.show_tir {
        if let Some(ref tir_modules) = result.tir_modules {
            if opts.unparse {
                println!("=== TIR (Resolve, unparsed) ===");
                for (module_source, module) in tir_modules {
                    let path_str = module_source.to_string();
                    println!("// --- Module: {path_str} ---");
                    let unparsed = wado_compiler::unparse::unparse_tir(module);
                    println!("{unparsed}");
                }
            } else {
                println!("=== TIR (Resolve) ===");
                for (module_source, module) in tir_modules {
                    let path_str = module_source.to_string();
                    println!("--- Module: {path_str} ---");
                    println!("{module:#?}");
                    println!();
                }
            }
        } else {
            println!("=== TIR (Resolve) ===");
            println!("(TIR resolution failed or not available)");
            println!();
        }
    }

    // Monomorphized TIR section (Monomorphize phase)
    if opts.show_monomorphize {
        if let Some(ref monomorphized_modules) = result.monomorphized_tir_modules {
            if opts.unparse {
                println!("=== Monomorphized TIR (Monomorphize, unparsed) ===");
                for (module_source, module) in monomorphized_modules {
                    let path_str = module_source.to_string();
                    println!("// --- Module: {path_str} ---");
                    let unparsed = wado_compiler::unparse::unparse_tir(module);
                    println!("{unparsed}");
                }
            } else {
                println!("=== Monomorphized TIR (Monomorphize) ===");
                for (module_source, module) in monomorphized_modules {
                    let path_str = module_source.to_string();
                    println!("--- Module: {path_str} ---");
                    println!("{module:#?}");
                    println!();
                }
            }
        } else {
            println!("=== Monomorphized TIR (Monomorphize) ===");
            println!("(Monomorphization failed or not available)");
            println!();
        }
    }

    // Lowered TIR section (Lower phase)
    if opts.show_lower {
        if let Some(ref lowered_modules) = result.lowered_tir_modules {
            if opts.unparse {
                println!("=== Lowered TIR (Lower, unparsed) ===");
                for (module_source, module) in lowered_modules {
                    let path_str = module_source.to_string();
                    println!("// --- Module: {path_str} ---");
                    let unparsed = wado_compiler::unparse::unparse_tir(module);
                    println!("{unparsed}");
                }
            } else {
                println!("=== Lowered TIR (Lower) ===");
                for (module_source, module) in lowered_modules {
                    let path_str = module_source.to_string();
                    println!("--- Module: {path_str} ---");
                    println!("{module:#?}");
                    println!();
                }
            }
        } else {
            println!("=== Lowered TIR (Lower) ===");
            println!("(TIR lowering failed or not available)");
            println!();
        }
    }

    // Optimized project section (Optimize phase)
    if opts.show_optimize {
        if let Some(ref project) = result.optimized_project {
            if opts.unparse {
                println!("=== Optimized TIR (Optimize, unparsed) ===");
                for (module_source, module) in &project.tir_modules {
                    let path_str = module_source.to_string();
                    println!("// --- Module: {path_str} ---");
                    let unparsed = wado_compiler::unparse::unparse_tir(module);
                    println!("{unparsed}");
                }
            } else {
                println!("=== Optimized Project (Optimize) ===");
                println!("{project:#?}");
            }
            println!();
        } else {
            println!("=== Optimized Project (Optimize) ===");
            println!("(Optimization failed or not available)");
            println!();
        }
    }

    // WIR section (Wasm IR phase)
    if opts.show_wir {
        if let Some(ref wir_module) = result.wir_module {
            if opts.unparse {
                println!("=== WIR (Wasm IR, unparsed) ===");
                let unparsed = wado_compiler::wir_unparse::unparse_wir(wir_module, None);
                if unparsed.is_empty() {
                    println!("(empty module)");
                } else {
                    print!("{unparsed}");
                }
            } else {
                println!("=== WIR (Wasm IR) ===");
                println!("{wir_module:#?}");
            }
            println!();
        } else {
            println!("=== WIR (Wasm IR) ===");
            println!("(WIR generation failed or not available)");
            println!();
        }
    }
}

/// Extract the target world from the `__DATA__` JSON section of a source file.
///
/// Supports two formats:
/// - Old: `{"world": "wasi:http/service", ...}`
/// - New: `{"wasi:http/service": {...}}` (world as top-level key containing `:` or `"test"`)
fn extract_world_from_data_section(source: &str) -> Option<String> {
    let marker = "\n__DATA__\n";
    let data = if let Some(pos) = source.find(marker) {
        &source[pos + marker.len()..]
    } else if source.starts_with("__DATA__\n") {
        &source["__DATA__\n".len()..]
    } else {
        return None;
    };
    let json: serde_json::Value = serde_json::from_str(data.trim()).ok()?;
    // Old format: explicit "world" key
    if let Some(world) = json.get("world").and_then(|v| v.as_str()) {
        return Some(world.to_string());
    }
    // New format: world name is a top-level key (contains ':' or is "test")
    if let Some(obj) = json.as_object() {
        for key in obj.keys() {
            if key.contains(':') || key == "test" {
                return Some(key.clone());
            }
        }
    }
    None
}
