use std::fs;
use std::io::Write;
use std::path::Path;
use std::process;

use lexopt::Arg::{Long, Short, Value};
use wado_compiler::OptLevel;

use crate::args::{next_arg, require_inputs, unexpected_arg};
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
    pub unparse: bool,
    pub opt_level: OptLevel,
    /// Output template for bulk generation, e.g., "path/to/{name}.lowered.wado"
    /// {name} is replaced with the input file's basename without extension
    pub output_template: Option<String>,
}

pub fn print_usage() {
    eprintln!("Usage: wado dump [options] <file.wado> [file2.wado ...]");
    eprintln!();
    eprintln!("Dump compiler internal state for debugging.");
    eprintln!("Supports multiple input files for batch processing.");
    eprintln!();
    eprintln!("Compilation Phases:");
    eprintln!("  --tokens       (Phase 1: Lexer) Show tokens");
    eprintln!("  --ast          (Phase 2: Parser) Show AST structure");
    eprintln!("  --desugar      (Phase 3: Desugar) Show desugared AST");
    eprintln!("  --modules      (Phase 4: Load) Show loaded modules");
    eprintln!("  --symbols      (Phase 5: Analyze) Show symbol table");
    eprintln!("  --tir          (Phase 6: Resolve) Show TIR (Typed IR)");
    eprintln!("  --monomorphize (Phase 7: Monomorphize) Show monomorphized TIR");
    eprintln!("  --lower        (Phase 8: Lower) Show lowered TIR");
    eprintln!("  --optimize     (Phase 9: Optimize) Show optimization hints");
    eprintln!("  --all        Show all phases (default if no phase specified)");
    eprintln!();
    eprintln!("Display Options:");
    eprintln!("  --unparse    Unparse to Wado source code (for ast/desugar/tir/lower/optimize)");
    eprintln!("               Default: Debug/tree format");
    eprintln!();
    eprintln!("Optimization Level (for --optimize phase):");
    eprintln!("  -O0          No optimizations");
    eprintln!("  -O1          Development optimizations (all passes except DCE)");
    eprintln!("  -O2          Production optimizations (default)");
    eprintln!("  -O3          Aggressive optimizations");
    eprintln!("  -Os          Size optimizations (O2 + strip names)");
    eprintln!();
    eprintln!("Output:");
    eprintln!("  -o <template>  Output template for bulk file generation");
    eprintln!("                 {{name}} is replaced with input filename (without extension)");
    eprintln!("                 Example: -o 'out/{{name}}.lowered.wado'");
    eprintln!();
    eprintln!("Other:");
    eprintln!("  --help       Show this help message");
}

pub fn parse_args(mut parser: lexopt::Parser) -> DumpOptions {
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
    let mut unparse = false;
    let mut opt_level = OptLevel::O2;
    let mut output_template: Option<String> = None;

    while let Some(arg) = next_arg(&mut parser) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Long("tokens") => show_tokens = true,
            Long("ast") => show_ast = true,
            Long("desugar") => show_desugar = true,
            Long("symbols") => show_symbols = true,
            Long("modules") => show_modules = true,
            Long("tir") => show_tir = true,
            Long("monomorphize") => show_monomorphize = true,
            Long("lower") => show_lower = true,
            Long("optimize") => show_optimize = true,
            Long("unparse") => unparse = true,
            Long("all") => {
                show_tokens = true;
                show_ast = true;
                show_desugar = true;
                show_symbols = true;
                show_modules = true;
                show_tir = true;
                show_monomorphize = true;
                show_lower = true;
                show_optimize = true;
            }
            Short('O') => {
                let level = parser.value().unwrap_or_else(|_| {
                    eprintln!("Error: -O requires a level (0, 1, 2, 3, or s)");
                    process::exit(1);
                });
                let level_str = level.to_string_lossy();
                opt_level = match level_str.as_ref() {
                    "0" => OptLevel::O0,
                    "1" => OptLevel::O1,
                    "2" => OptLevel::O2,
                    "3" => OptLevel::O3,
                    "s" => OptLevel::Os,
                    _ => {
                        eprintln!("Error: Unknown optimization level: -O{level_str}");
                        eprintln!("Valid levels: -O0, -O1, -O2, -O3, -Os");
                        process::exit(1);
                    }
                };
            }
            Short('o') | Long("output") => {
                let template = parser.value().unwrap_or_else(|_| {
                    eprintln!("Error: -o requires an output template");
                    process::exit(1);
                });
                output_template = Some(template.to_string_lossy().into_owned());
            }
            Value(val) => {
                inputs.push(val.to_string_lossy().into_owned());
            }
            _ => unexpected_arg(arg, print_usage),
        }
    }

    let inputs = require_inputs(inputs, print_usage);

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
    }

    DumpOptions {
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
        unparse,
        opt_level,
        output_template,
    }
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
    let parallelism = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(4);
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

    // Dump using async API
    let result = wado_compiler::dump_with_host(&source, &host, Some(input), opt_level)
        .await
        .map_err(|e| e.to_string())?;

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

    // Dump using async API
    let result =
        match wado_compiler::dump_with_host(&source, &host, Some(input), opts.opt_level).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e}");
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
}
