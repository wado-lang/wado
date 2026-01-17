use std::fs;
use std::path::Path;
use std::process;

use lexopt::Arg::{Long, Value};

use crate::args::{next_arg, reject_multiple_inputs, require_input, unexpected_arg};
use crate::compiler_host::FilesystemCompilerHost;

pub struct DumpOptions {
    pub input: String,
    pub show_tokens: bool,
    pub show_ast: bool,
    pub show_desugar: bool,
    pub show_symbols: bool,
    pub show_modules: bool,
    pub show_tir: bool,
    pub show_lower: bool,
    pub show_optimize: bool,
    pub unparse: bool,
}

pub fn print_usage() {
    eprintln!("Usage: wado dump [options] <file.wado>");
    eprintln!();
    eprintln!("Dump compiler internal state for debugging.");
    eprintln!();
    eprintln!("Compilation Phases:");
    eprintln!("  --tokens     (Phase 1: Lexer) Show tokens");
    eprintln!("  --ast        (Phase 2: Parser) Show AST structure");
    eprintln!("  --desugar    (Phase 3: Desugar) Show desugared AST");
    eprintln!("  --modules    (Phase 4: Load) Show loaded modules");
    eprintln!("  --symbols    (Phase 5: Analyze) Show symbol table");
    eprintln!("  --tir        (Phase 6: Resolve) Show TIR (Typed IR)");
    eprintln!("  --lower      (Phase 7: Lower) Show lowered TIR");
    eprintln!("  --optimize   (Phase 8: Optimize) Show optimization hints");
    eprintln!("  --all        Show all phases (default if no phase specified)");
    eprintln!();
    eprintln!("Display Options:");
    eprintln!("  --unparse    Unparse to Wado source code (for ast/desugar/tir/lower)");
    eprintln!("               Default: Debug/tree format");
    eprintln!();
    eprintln!("Other:");
    eprintln!("  --help       Show this help message");
}

pub fn parse_args(mut parser: lexopt::Parser) -> DumpOptions {
    let mut input: Option<String> = None;
    let mut show_tokens = false;
    let mut show_ast = false;
    let mut show_desugar = false;
    let mut show_symbols = false;
    let mut show_modules = false;
    let mut show_tir = false;
    let mut show_lower = false;
    let mut show_optimize = false;
    let mut unparse = false;

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
                show_lower = true;
                show_optimize = true;
            }
            Value(val) => {
                reject_multiple_inputs(&input);
                input = Some(val.to_string_lossy().into_owned());
            }
            _ => unexpected_arg(arg, print_usage),
        }
    }

    let input = require_input(input, print_usage);

    // Default: show all
    if !show_tokens
        && !show_ast
        && !show_desugar
        && !show_symbols
        && !show_modules
        && !show_tir
        && !show_lower
        && !show_optimize
    {
        show_tokens = true;
        show_ast = true;
        show_desugar = true;
        show_symbols = true;
        show_modules = true;
        show_tir = true;
        show_lower = true;
        show_optimize = true;
    }

    DumpOptions {
        input,
        show_tokens,
        show_ast,
        show_desugar,
        show_symbols,
        show_modules,
        show_tir,
        show_lower,
        show_optimize,
        unparse,
    }
}

pub async fn run(opts: DumpOptions) {
    let path = Path::new(&opts.input);

    // Read source file
    let source = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading '{}': {e}", path.display());
            process::exit(1);
        }
    };

    // Get base path for relative imports
    let base_path = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let host = FilesystemCompilerHost::new(base_path);

    // Dump using async API
    let result = match wado_compiler::dump_with_host(&source, &host, Some(&opts.input)).await {
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
            println!("  [{}] {:?}", i, token);
        }
        println!();
    }

    // AST section (Parser phase)
    if opts.show_ast {
        if opts.unparse {
            println!("=== AST (Parser, unparsed) ===");
            let unparser = wado_compiler::unparse::Unparser::new(&result.comments);
            let unparsed = unparser.unparse(&result.ast);
            println!("{}", unparsed);
        } else {
            println!("=== AST (Parser) ===");
            for (i, item) in result.ast.items.iter().enumerate() {
                println!("  [{}] {:#?}", i, item);
            }
            if let Some(data) = result.ast.data_section() {
                println!();
                println!("--- Data Section ---");
                println!("{}", data);
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
            println!("{}", unparsed);
        } else {
            println!("=== Desugared AST (Desugar) ===");
            for (i, item) in result.desugared_ast.items.iter().enumerate() {
                println!("  [{}] {:#?}", i, item);
            }
            if let Some(data) = result.desugared_ast.data_section() {
                println!();
                println!("--- Data Section ---");
                println!("{}", data);
            }
        }
        println!();
    }

    // Modules section (Load phase)
    if opts.show_modules {
        println!("=== Loaded Modules (Load) ===");
        for module_path in &result.loaded_modules {
            let path_str: String = module_path.join("::");
            let is_implicit = result.implicit_modules.contains(module_path);
            if is_implicit {
                println!("  {} (implicit)", path_str);
            } else {
                println!("  {}", path_str);
            }
        }
        println!();

        println!("=== Implicit Modules ===");
        for module_path in &result.implicit_modules {
            let path_str: String = module_path.join("::");
            println!("  {}", path_str);
        }
        println!();
    }

    // Symbols section (Analyze phase)
    if opts.show_symbols {
        println!("=== Symbol Table (Analyze) ===");
        for symbol in result.symbols.all_symbols() {
            let module_path = if symbol.module_path.is_empty() {
                "(local)".to_string()
            } else {
                symbol.module_path.join("::")
            };
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
                    format!("enum{{ {} }}", e.variants.join(", "))
                }
                wado_compiler::symbol::SymbolKind::TypeAlias(t) => {
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
            };
            println!(
                "  [{}] {} :: {} = {}",
                symbol.id, module_path, symbol.name, kind_str
            );
        }
        println!();
    }

    // TIR section (Resolve phase)
    if opts.show_tir {
        if let Some(ref tir_modules) = result.tir_modules {
            if opts.unparse {
                println!("=== TIR (Resolve, unparsed) ===");
                for (path, module) in tir_modules {
                    let path_str: String = path.join("::");
                    println!("// --- Module: {} ---", path_str);
                    let unparsed = wado_compiler::unparse::unparse_tir(module);
                    println!("{}", unparsed);
                }
            } else {
                println!("=== TIR (Resolve) ===");
                for (path, module) in tir_modules {
                    let path_str: String = path.join("::");
                    println!("--- Module: {} ---", path_str);
                    println!("{:#?}", module);
                    println!();
                }
            }
        } else {
            println!("=== TIR (Resolve) ===");
            println!("(TIR resolution failed or not available)");
            println!();
        }
    }

    // Lowered TIR section (Lower phase)
    if opts.show_lower {
        if let Some(ref lowered_modules) = result.lowered_tir_modules {
            if opts.unparse {
                println!("=== Lowered TIR (Lower, unparsed) ===");
                for (path, module) in lowered_modules {
                    let path_str: String = path.join("::");
                    println!("// --- Module: {} ---", path_str);
                    let unparsed = wado_compiler::unparse::unparse_tir(module);
                    println!("{}", unparsed);
                }
            } else {
                println!("=== Lowered TIR (Lower) ===");
                for (path, module) in lowered_modules {
                    let path_str: String = path.join("::");
                    println!("--- Module: {} ---", path_str);
                    println!("{:#?}", module);
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
            println!("=== Optimized Project (Optimize) ===");
            println!("{:#?}", project);
            println!();
        } else {
            println!("=== Optimized Project (Optimize) ===");
            println!("(Optimization failed or not available)");
            println!();
        }
    }
}
