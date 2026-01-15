use std::path::Path;
use std::process;

use lexopt::prelude::*;

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
}

pub fn print_usage() {
    eprintln!("Usage: wado dump [options] <file.wado>");
    eprintln!();
    eprintln!("Dump compiler internal state for debugging.");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --tokens     Show tokens from lexer");
    eprintln!("  --ast        Show AST from parser (unparsed to Wado source)");
    eprintln!("  --desugar    Show desugared AST (unparsed to Wado source)");
    eprintln!("  --symbols    Show symbol table from analyzer");
    eprintln!("  --modules    Show loaded modules");
    eprintln!("  --tir        Show TIR (Typed IR) from resolver");
    eprintln!("  --lower      Show lowered TIR");
    eprintln!("  --optimize   Show optimization hints");
    eprintln!("  --all        Show all information (default if no options)");
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

    while let Some(arg) = parser.next().unwrap_or_else(|e| {
        eprintln!("Error: {e}");
        process::exit(1);
    }) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Long("tokens") => {
                show_tokens = true;
            }
            Long("ast") => {
                show_ast = true;
            }
            Long("desugar") => {
                show_desugar = true;
            }
            Long("symbols") => {
                show_symbols = true;
            }
            Long("modules") => {
                show_modules = true;
            }
            Long("tir") => {
                show_tir = true;
            }
            Long("lower") => {
                show_lower = true;
            }
            Long("optimize") => {
                show_optimize = true;
            }
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
                if input.is_some() {
                    eprintln!("Error: multiple input files not supported");
                    process::exit(1);
                }
                input = Some(val.to_string_lossy().into_owned());
            }
            _ => {
                eprintln!("Error: {}", arg.unexpected());
                print_usage();
                process::exit(1);
            }
        }
    }

    let input = match input {
        Some(f) => f,
        None => {
            eprintln!("Error: no input file specified");
            print_usage();
            process::exit(1);
        }
    };

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
    }
}

pub fn run(opts: DumpOptions) {
    let result = match wado_compiler::dump_file(Path::new(&opts.input)) {
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
        println!("=== AST (Parser, unparsed) ===");
        let unparser = wado_compiler::unparse::Unparser::new(&result.comments);
        let unparsed = unparser.unparse(&result.ast);
        println!("{}", unparsed);
        println!();
    }

    // Desugared AST section (Desugar phase)
    if opts.show_desugar {
        println!("=== Desugared AST (Desugar, unparsed) ===");
        let unparser = wado_compiler::unparse::Unparser::new(&result.comments);
        let unparsed = unparser.unparse(&result.desugared_ast);
        println!("{}", unparsed);
        println!();
    }

    // Modules section (Load phase)
    if opts.show_modules {
        println!("=== Loaded Modules (Load) ===");
        for module_path in &result.loaded_modules {
            let path_str = module_path.join("::");
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
            println!("  {}", module_path.join("::"));
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
            println!("=== TIR (Resolve) ===");
            for (path, module) in tir_modules {
                println!("--- Module: {} ---", path.join("::"));
                println!("{:#?}", module);
                println!();
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
            println!("=== Lowered TIR (Lower) ===");
            for (path, module) in lowered_modules {
                println!("--- Module: {} ---", path.join("::"));
                println!("{:#?}", module);
                println!();
            }
        } else {
            println!("=== Lowered TIR (Lower) ===");
            println!("(TIR lowering failed or not available)");
            println!();
        }
    }

    // Optimization hints section (Optimize phase)
    if opts.show_optimize {
        if let Some(ref hints) = result.opt_hints {
            println!("=== Optimization Hints (Optimize) ===");
            println!("{:#?}", hints);
            println!();
        } else {
            println!("=== Optimization Hints (Optimize) ===");
            println!("(Optimization analysis failed or not available)");
            println!();
        }
    }
}
