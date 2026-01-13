use std::path::Path;
use std::process;

use lexopt::prelude::*;

pub struct DumpOptions {
    pub input: String,
    pub show_ast: bool,
    pub show_symbols: bool,
    pub show_modules: bool,
}

pub fn print_usage() {
    eprintln!("Usage: wado dump [options] <file.wado>");
    eprintln!();
    eprintln!("Dump compiler internal state for debugging.");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --ast        Show the AST (abstract syntax tree)");
    eprintln!("  --symbols    Show the symbol table");
    eprintln!("  --modules    Show loaded modules");
    eprintln!("  --all        Show all information (default if no options)");
    eprintln!("  --help       Show this help message");
}

pub fn parse_args(mut parser: lexopt::Parser) -> DumpOptions {
    let mut input: Option<String> = None;
    let mut show_ast = false;
    let mut show_symbols = false;
    let mut show_modules = false;

    while let Some(arg) = parser.next().unwrap_or_else(|e| {
        eprintln!("Error: {e}");
        process::exit(1);
    }) {
        match arg {
            Long("help") => {
                print_usage();
                process::exit(0);
            }
            Long("ast") => {
                show_ast = true;
            }
            Long("symbols") => {
                show_symbols = true;
            }
            Long("modules") => {
                show_modules = true;
            }
            Long("all") => {
                show_ast = true;
                show_symbols = true;
                show_modules = true;
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
    if !show_ast && !show_symbols && !show_modules {
        show_ast = true;
        show_symbols = true;
        show_modules = true;
    }

    DumpOptions {
        input,
        show_ast,
        show_symbols,
        show_modules,
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

    // Modules section
    if opts.show_modules {
        println!("=== Loaded Modules ===");
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

    // Symbols section
    if opts.show_symbols {
        println!("=== Symbol Table ===");
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

    // AST section
    if opts.show_ast {
        println!("=== AST ===");
        for (i, item) in result.ast.items.iter().enumerate() {
            println!("  [{}] {:?}", i, item);
        }
        if let Some(data) = result.ast.data_section() {
            println!();
            println!("=== Data Section ===");
            println!("{}", data);
        }
    }
}
