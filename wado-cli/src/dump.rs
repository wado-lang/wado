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
    pub show_desugared: bool,
    pub show_symbols: bool,
    pub show_modules: bool,
    pub show_types: bool,
    pub show_tir: bool,
    pub show_tir_resolved: bool,
    pub show_tir_monomorphized: bool,
    pub show_tir_lowered: bool,
    pub show_wir: bool,
    pub inspect: bool,
    pub opt_level: OptLevel,
    pub inline_threshold: Option<usize>,
    pub opt_iterations: Option<u32>,
    /// Output template for bulk generation, e.g., "path/to/{name}.lowered.wado"
    /// {name} is replaced with the input file's basename without extension
    pub output_template: Option<String>,
}

#[derive(Clone, Copy)]
enum Opt {
    Tokens,
    Ast,
    Desugared,
    Modules,
    Symbols,
    Types,
    Tir,
    TirResolved,
    TirMonomorphized,
    TirLowered,
    Wir,
    Inspect,
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
        Self::Desugared,
        Self::Modules,
        Self::Symbols,
        Self::Types,
        Self::Tir,
        Self::TirResolved,
        Self::TirMonomorphized,
        Self::TirLowered,
        Self::Wir,
        Self::Inspect,
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
                desc: "Show lexer tokens",
            },
            Self::Ast => args::OptSpec {
                long: Some("ast"),
                short: None,
                value: None,
                desc: "Show parsed AST",
            },
            Self::Desugared => args::OptSpec {
                long: Some("desugared"),
                short: None,
                value: None,
                desc: "Show desugared AST",
            },
            Self::Modules => args::OptSpec {
                long: Some("modules"),
                short: None,
                value: None,
                desc: "Show loaded modules",
            },
            Self::Symbols => args::OptSpec {
                long: Some("symbols"),
                short: None,
                value: None,
                desc: "Show symbol table",
            },
            Self::Types => args::OptSpec {
                long: Some("types"),
                short: None,
                value: None,
                desc: "Show type table (all resolved types)",
            },
            Self::Tir => args::OptSpec {
                long: Some("tir"),
                short: None,
                value: None,
                desc: "Show final TIR (after optimization, affected by -Ox)",
            },
            Self::TirResolved => args::OptSpec {
                long: Some("tir-resolved"),
                short: None,
                value: None,
                desc: "Show TIR after type resolution (before lowering)",
            },
            Self::TirMonomorphized => args::OptSpec {
                long: Some("tir-monomorphized"),
                short: None,
                value: None,
                desc: "Show TIR after monomorphization",
            },
            Self::TirLowered => args::OptSpec {
                long: Some("tir-lowered"),
                short: None,
                value: None,
                desc: "Show TIR after lowering (before optimization)",
            },
            Self::Wir => args::OptSpec {
                long: Some("wir"),
                short: None,
                value: None,
                desc: "Show final WIR (after optimization, affected by -Ox) [default]",
            },
            Self::Inspect => args::OptSpec {
                long: Some("inspect"),
                short: None,
                value: None,
                desc: "Show internal Debug format instead of unparsed source",
            },
            Self::OptLevel => args::OptSpec {
                long: None,
                short: Some('O'),
                value: Some("<n>"),
                desc: "Optimization level (affects --tir and --wir output)",
            },
            Self::InlineThreshold => args::INLINE_THRESHOLD_SPEC,
            Self::OptIterations => args::OPT_ITERATIONS_SPEC,
            Self::Output => args::OptSpec {
                long: Some("output"),
                short: Some('o'),
                value: Some("<template>"),
                desc: "Output template for bulk file generation\n{name} is replaced with input filename (without extension)\nExample: -o 'out/{name}.wir.wado'",
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
    writeln!(
        buf,
        "Default: shows final WIR as unparsed source (equivalent to --wir)."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Phases:").unwrap();
    write!(
        buf,
        "{}",
        args::format_opts_help(
            &[
                Opt::Tokens,
                Opt::Ast,
                Opt::Desugared,
                Opt::Modules,
                Opt::Symbols,
                Opt::Types,
                Opt::TirResolved,
                Opt::TirMonomorphized,
                Opt::TirLowered,
                Opt::Tir,
                Opt::Wir,
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
        args::format_opts_help(&[Opt::Inspect], |o| o.spec())
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Optimization Level:").unwrap();
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
/// Parse command-line arguments for the `dump` subcommand.
///
/// # Errors
///
/// Returns an error if the arguments are invalid or required arguments are missing.
pub fn parse_args(mut parser: lexopt::Parser) -> Result<DumpOptions, CliExit> {
    let usage = format_usage();
    let mut inputs: Vec<String> = Vec::new();
    let mut show_tokens = false;
    let mut show_ast = false;
    let mut show_desugared = false;
    let mut show_symbols = false;
    let mut show_modules = false;
    let mut show_types = false;
    let mut show_tir = false;
    let mut show_tir_resolved = false;
    let mut show_tir_monomorphized = false;
    let mut show_tir_lowered = false;
    let mut show_wir = false;
    let mut inspect = false;
    let mut opt_level = OptLevel::O2;
    let mut inline_threshold: Option<usize> = None;
    let mut opt_iterations: Option<u32> = None;
    let mut output_template: Option<String> = None;
    let mut any_phase = false;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Tokens => {
                    show_tokens = true;
                    any_phase = true;
                }
                Opt::Ast => {
                    show_ast = true;
                    any_phase = true;
                }
                Opt::Desugared => {
                    show_desugared = true;
                    any_phase = true;
                }
                Opt::Symbols => {
                    show_symbols = true;
                    any_phase = true;
                }
                Opt::Modules => {
                    show_modules = true;
                    any_phase = true;
                }
                Opt::Types => {
                    show_types = true;
                    any_phase = true;
                }
                Opt::Tir => {
                    show_tir = true;
                    any_phase = true;
                }
                Opt::TirResolved => {
                    show_tir_resolved = true;
                    any_phase = true;
                }
                Opt::TirMonomorphized => {
                    show_tir_monomorphized = true;
                    any_phase = true;
                }
                Opt::TirLowered => {
                    show_tir_lowered = true;
                    any_phase = true;
                }
                Opt::Wir => {
                    show_wir = true;
                    any_phase = true;
                }
                Opt::Inspect => inspect = true,
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

    // Default: show WIR
    if !any_phase {
        show_wir = true;
    }

    Ok(DumpOptions {
        inputs,
        show_tokens,
        show_ast,
        show_desugared,
        show_symbols,
        show_modules,
        show_types,
        show_tir,
        show_tir_resolved,
        show_tir_monomorphized,
        show_tir_lowered,
        show_wir,
        inspect,
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

/// Size of the alternate signal stack installed in each compiler thread (8 MB).
///
/// Without an alternate signal stack, a stack overflow in a spawned thread
/// delivers SIGSEGV with no stack to run the signal handler on. The process
/// then terminates silently — no panic message, no error output. Installing
/// `sigaltstack` in each thread ensures the signal handler (and Rust's panic
/// hook) can run even when the main stack is exhausted.
#[cfg(unix)]
const ALT_STACK_SIZE: usize = 8 * 1024 * 1024;

/// Install an alternate signal stack for the current thread.
///
/// Must be called at the start of each spawned compiler thread so that
/// SIGSEGV from stack overflow produces a proper panic message instead of
/// silently killing the process.
#[cfg(unix)]
fn install_alt_stack() {
    // Allocate and intentionally leak: the OS reclaims thread memory on exit.
    let mem = vec![0u8; ALT_STACK_SIZE].into_boxed_slice();
    let ptr = Box::into_raw(mem);
    // SAFETY: ptr is a valid, exclusively-owned allocation of ALT_STACK_SIZE bytes.
    unsafe {
        let ss = libc::stack_t {
            ss_sp: ptr.cast::<libc::c_void>(),
            ss_flags: 0,
            ss_size: ALT_STACK_SIZE,
        };
        libc::sigaltstack(&ss, std::ptr::null_mut());
    }
}

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
            #[cfg(unix)]
            install_alt_stack();
            let _ = tx.send(f());
        })
        .expect("failed to spawn compiler thread");
    rx
}

/// Bulk file output mode - writes each input to a file based on template (parallel)
///
/// Each compilation runs on its own large-stack OS thread with an **isolated**
/// `current_thread` tokio runtime. This avoids the subtle hazard of multiple
/// OS threads calling `handle.block_on()` on the same shared multi-thread
/// runtime handle simultaneously. Concurrency is capped at `parallelism` to
/// bound peak memory and avoid SIGSEGV under memory pressure.
async fn run_bulk(opts: &DumpOptions, template: &str) {
    let start = std::time::Instant::now();

    // Each compilation uses ~16 MB stack + significant heap. Cap at half the
    // available CPUs (minimum 2) so peak memory stays predictable even right
    // after a heavyweight `cargo clippy` run.
    let parallelism = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(4)
        / 2;
    let parallelism = parallelism.max(2);
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(parallelism));

    let mut skip_count_early = 0u32;
    let mut tasks: Vec<tokio::task::JoinHandle<Result<String, String>>> = Vec::new();

    for input in &opts.inputs {
        // Skip files with expected compilation failures: compile_error or TODO tests.
        if let Ok(source) = fs::read_to_string(input) {
            let data = source.find("\n__DATA__\n").map_or("", |p| &source[p..]);
            if data.contains("\"TODO\"") || data.contains("\"compile_error\"") {
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
        let show_tir = opts.show_tir;
        let show_wir = opts.show_wir;
        let opt_level = opts.opt_level;
        let inline_threshold = opts.inline_threshold;
        let opt_iterations = opts.opt_iterations;
        let sem = semaphore.clone();

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

                // Ensure parent directory exists.
                if let Some(parent) = Path::new(&output_path).parent()
                    && !parent.as_os_str().is_empty()
                    && let Err(e) = fs::create_dir_all(parent)
                {
                    return Err(format!(
                        "Failed to create directory {}: {e}",
                        parent.display()
                    ));
                }

                // Build an isolated current_thread runtime for this compilation.
                // Using a per-compilation runtime instead of block_on on the shared
                // outer multi-thread handle keeps each compilation fully independent
                // and avoids any subtle interaction between concurrent callers.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build per-compilation tokio runtime");

                match rt.block_on(generate_output_params(
                    show_tir,
                    show_wir,
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

    for task in tasks {
        match task.await {
            Ok(Ok(output_path)) => {
                eprintln!("  Generated {output_path}");
                success_count += 1;
            }
            Ok(Err(e)) => {
                panic!(
                    "Golden fixture generation failed: {e}\n\
                     If this test is expected to fail at compile time, add \
                     \"compile_error\" or \"TODO\" to its __DATA__ section."
                );
            }
            Err(e) => {
                panic!("Golden fixture generation task panicked: {e}");
            }
        }
    }

    let skip_count = skip_count_early;
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!("Generated {success_count} files ({skip_count} skipped) in {elapsed:.2}s");
}

/// Generate output content for a single file, returning the content string.
///
/// Takes individual parameters instead of `&DumpOptions` so it can be called
/// from `tokio::spawn` (which requires `'static` futures).
async fn generate_output_params(
    show_tir: bool,
    show_wir: bool,
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

    if show_wir {
        if let Some(ref wir_module) = result.wir_module {
            writeln!(output, "// Golden file: WIR with -O2 optimization").unwrap();
            writeln!(output, "// Source: {input}").unwrap();
            writeln!(output, "// Generated by: make update-golden-fixtures").unwrap();
            writeln!(output).unwrap();

            let unparsed = wado_compiler::wir_unparse::unparse_wir(wir_module, Some(input));
            write!(output, "{unparsed}").unwrap();
        }
    } else if show_tir {
        if let Some(ref project) = result.optimized_project {
            for (module_source, module) in &project.tir_modules {
                if module_source.is_entry_point() {
                    let source_path = input;

                    writeln!(
                        output,
                        "// Golden file: Optimized TIR with -O2 optimization"
                    )
                    .unwrap();
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
        return Err("Bulk output only supports --wir or --tir mode".to_string());
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
        println!("=== Tokens ===");
        for (i, token) in result.tokens.iter().enumerate() {
            println!("  [{i}] {token:?}");
        }
        println!();
    }

    // AST section (Parser phase)
    if opts.show_ast {
        if opts.inspect {
            println!("=== AST (inspect) ===");
            for (i, item) in result.ast.items.iter().enumerate() {
                println!("  [{i}] {item:#?}");
            }
            if let Some(data) = result.ast.data_section() {
                println!();
                println!("--- Data Section ---");
                println!("{data}");
            }
        } else {
            println!("=== AST ===");
            let unparser = wado_compiler::unparse::Unparser::new(&result.comments);
            let unparsed = unparser.unparse(&result.ast);
            println!("{unparsed}");
        }
        println!();
    }

    // Desugared AST section
    if opts.show_desugared {
        if opts.inspect {
            println!("=== Desugared AST (inspect) ===");
            for (i, item) in result.desugared_ast.items.iter().enumerate() {
                println!("  [{i}] {item:#?}");
            }
            if let Some(data) = result.desugared_ast.data_section() {
                println!();
                println!("--- Data Section ---");
                println!("{data}");
            }
        } else {
            println!("=== Desugared AST ===");
            let unparser = wado_compiler::unparse::Unparser::new(&result.comments);
            let unparsed = unparser.unparse(&result.desugared_ast);
            println!("{unparsed}");
        }
        println!();
    }

    // Modules section
    if opts.show_modules {
        println!("=== Loaded Modules ===");
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

    // Symbols section
    if opts.show_symbols {
        println!("=== Symbol Table ===");
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

    // Types section (after type resolution)
    if opts.show_types {
        if let Some(ref tir_modules) = result.tir_modules {
            // Type table is shared across modules; grab from the first one
            if let Some((_, first_module)) = tir_modules.iter().next() {
                let type_table = first_module.type_table.borrow();
                if opts.inspect {
                    println!("=== Types (inspect) ===");
                    for (id, ty) in type_table.all_types() {
                        println!("  [{id}] {ty:#?}");
                    }
                } else {
                    println!("=== Types ===");
                    for (id, ty) in type_table.all_types() {
                        let name = type_table.type_name(*id);
                        let kind = match ty {
                            wado_compiler::tir::ResolvedType::Primitive(_) => "primitive",
                            wado_compiler::tir::ResolvedType::Unit => "unit",
                            wado_compiler::tir::ResolvedType::Never => "never",
                            wado_compiler::tir::ResolvedType::Struct { .. } => "struct",
                            wado_compiler::tir::ResolvedType::Enum { .. } => "enum",
                            wado_compiler::tir::ResolvedType::Variant { .. } => "variant",
                            wado_compiler::tir::ResolvedType::Newtype { .. } => "newtype",
                            wado_compiler::tir::ResolvedType::Resource { .. } => "resource",
                            wado_compiler::tir::ResolvedType::GenericResource { .. } => "resource",
                            wado_compiler::tir::ResolvedType::Ref(_) => "ref",
                            wado_compiler::tir::ResolvedType::MutRef(_) => "mut_ref",
                            wado_compiler::tir::ResolvedType::Function { .. } => "fn",
                            wado_compiler::tir::ResolvedType::Tuple(_) => "tuple",
                            wado_compiler::tir::ResolvedType::BuiltinArray(_) => "builtin_array",
                            wado_compiler::tir::ResolvedType::Reactive(_) => "reactive",
                            wado_compiler::tir::ResolvedType::TypeParam { .. } => "type_param",
                            wado_compiler::tir::ResolvedType::GenericInstance { .. } => {
                                "generic_instance"
                            }
                            wado_compiler::tir::ResolvedType::AssocTypeProjection { .. } => {
                                "assoc_type"
                            }
                            wado_compiler::tir::ResolvedType::Flags { .. } => "flags",
                            wado_compiler::tir::ResolvedType::Unknown => "unknown",
                            wado_compiler::tir::ResolvedType::Error => "error",
                        };
                        println!("  [{id}] {kind}: {name}");
                    }
                }
            }
        } else {
            println!("=== Types ===");
            println!("(Type resolution failed or not available)");
        }
        println!();
    }

    // TIR resolved section (after type resolution, before lowering)
    if opts.show_tir_resolved {
        if let Some(ref tir_modules) = result.tir_modules {
            if opts.inspect {
                println!("=== TIR Resolved (inspect) ===");
                for (module_source, module) in tir_modules {
                    let path_str = module_source.to_string();
                    println!("--- Module: {path_str} ---");
                    println!("{module:#?}");
                    println!();
                }
            } else {
                println!("=== TIR Resolved ===");
                for (module_source, module) in tir_modules {
                    let path_str = module_source.to_string();
                    println!("// --- Module: {path_str} ---");
                    let unparsed = wado_compiler::unparse::unparse_tir(module);
                    println!("{unparsed}");
                }
            }
        } else {
            println!("=== TIR Resolved ===");
            println!("(TIR resolution failed or not available)");
            println!();
        }
    }

    // TIR monomorphized section
    if opts.show_tir_monomorphized {
        if let Some(ref monomorphized_modules) = result.monomorphized_tir_modules {
            if opts.inspect {
                println!("=== TIR Monomorphized (inspect) ===");
                for (module_source, module) in monomorphized_modules {
                    let path_str = module_source.to_string();
                    println!("--- Module: {path_str} ---");
                    println!("{module:#?}");
                    println!();
                }
            } else {
                println!("=== TIR Monomorphized ===");
                for (module_source, module) in monomorphized_modules {
                    let path_str = module_source.to_string();
                    println!("// --- Module: {path_str} ---");
                    let unparsed = wado_compiler::unparse::unparse_tir(module);
                    println!("{unparsed}");
                }
            }
        } else {
            println!("=== TIR Monomorphized ===");
            println!("(Monomorphization failed or not available)");
            println!();
        }
    }

    // TIR lowered section (after lowering, before optimization)
    if opts.show_tir_lowered {
        if let Some(ref lowered_modules) = result.lowered_tir_modules {
            if opts.inspect {
                println!("=== TIR Lowered (inspect) ===");
                for (module_source, module) in lowered_modules {
                    let path_str = module_source.to_string();
                    println!("--- Module: {path_str} ---");
                    println!("{module:#?}");
                    println!();
                }
            } else {
                println!("=== TIR Lowered ===");
                for (module_source, module) in lowered_modules {
                    let path_str = module_source.to_string();
                    println!("// --- Module: {path_str} ---");
                    let unparsed = wado_compiler::unparse::unparse_tir(module);
                    println!("{unparsed}");
                }
            }
        } else {
            println!("=== TIR Lowered ===");
            println!("(TIR lowering failed or not available)");
            println!();
        }
    }

    // Final TIR section (after optimization)
    if opts.show_tir {
        if let Some(ref project) = result.optimized_project {
            if opts.inspect {
                println!("=== TIR (inspect) ===");
                println!("{project:#?}");
            } else {
                println!("=== TIR ===");
                for (module_source, module) in &project.tir_modules {
                    let path_str = module_source.to_string();
                    println!("// --- Module: {path_str} ---");
                    let unparsed = wado_compiler::unparse::unparse_tir(module);
                    println!("{unparsed}");
                }
            }
            println!();
        } else {
            println!("=== TIR ===");
            println!("(Optimization failed or not available)");
            println!();
        }
    }

    // Final WIR section (after optimization)
    if opts.show_wir {
        if let Some(ref wir_module) = result.wir_module {
            if opts.inspect {
                println!("=== WIR (inspect) ===");
                println!("{wir_module:#?}");
            } else {
                let unparsed = wado_compiler::wir_unparse::unparse_wir(wir_module, None);
                if unparsed.is_empty() {
                    println!("(empty module)");
                } else {
                    print!("{unparsed}");
                }
            }
            println!();
        } else {
            println!("=== WIR ===");
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
    } else if let Some(stripped) = source.strip_prefix("__DATA__\n") {
        stripped
    } else {
        return None;
    };
    let json: serde_json::Value = serde_json::from_str(data.trim()).ok()?;
    // Old format: explicit "world" key
    if let Some(world) = json.get("world").and_then(|v| v.as_str()) {
        return Some(world.to_string());
    }
    // New format: world name is a top-level key (wasi:* prefix or "test")
    if let Some(obj) = json.as_object() {
        for key in obj.keys() {
            if key.starts_with("wasi:") || key == "test" {
                return Some(key.clone());
            }
        }
    }
    None
}
