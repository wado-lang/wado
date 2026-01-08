mod analyze;
mod ast;
mod codegen;
mod lexer;
mod parser;
mod resolver;
mod stdlib;
mod symbol;
mod token;

use std::env;
use std::fs;
use std::path::Path;
use std::process;

use analyze::Analyzer;
use codegen::Codegen;
use lexer::Lexer;
use parser::Parser;

fn print_usage() {
    eprintln!("Usage: wado-compiler [OPTIONS] <file.wado>");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --help  Show this help message");
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    // Parse arguments
    let mut filename: Option<&str> = None;

    for arg in &args[1..] {
        match arg.as_str() {
            "--help" | "-h" => {
                print_usage();
                process::exit(0);
            }
            arg if arg.starts_with('-') => {
                eprintln!("Error: unknown option '{arg}'");
                process::exit(1);
            }
            arg => {
                filename = Some(arg);
            }
        }
    }

    let filename = match filename {
        Some(f) => f,
        None => {
            eprintln!("Error: no input file specified");
            print_usage();
            process::exit(1);
        }
    };

    let source = match fs::read_to_string(filename) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("Error reading file '{filename}': {e}");
            process::exit(1);
        }
    };

    // Lexing
    let mut lexer = Lexer::new(&source);
    let tokens = match lexer.tokenize() {
        Ok(tokens) => tokens,
        Err(e) => {
            eprintln!(
                "Lexer error at line {}, column {}: {}",
                e.span.line, e.span.column, e.message
            );
            process::exit(1);
        }
    };

    // Parsing
    let mut parser = Parser::new(tokens);
    let module = match parser.parse() {
        Ok(module) => module,
        Err(e) => {
            eprintln!(
                "Parse error at line {}, column {}: {}",
                e.span.line, e.span.column, e.message
            );
            process::exit(1);
        }
    };

    // Semantic analysis (module resolution, symbol table)
    let mut analyzer = Analyzer::new();
    match analyzer.analyze(&module, &[]) {
        Ok(()) => {}
        Err(errors) => {
            for e in errors {
                eprintln!("Analysis error: {e}");
            }
            process::exit(1);
        }
    }

    // Code generation (Component Model)
    let symbols = analyzer.into_symbols();
    let mut codegen = Codegen::new(symbols);
    let wasm = codegen.generate_wasm(&module);

    // Output Wasm binary file
    let output_path = Path::new(filename).with_extension("wasm");
    match fs::write(&output_path, &wasm) {
        Ok(_) => {
            eprintln!("Generated: {}", output_path.display());
        }
        Err(e) => {
            eprintln!("Error writing output file: {e}");
            process::exit(1);
        }
    }

    // Also generate WAT for debugging
    let wat = codegen.generate_wat(&module);
    let wat_path = Path::new(filename).with_extension("wat");
    match fs::write(&wat_path, &wat) {
        Ok(_) => {
            eprintln!("Generated: {}", wat_path.display());
        }
        Err(e) => {
            eprintln!("Error writing WAT file: {e}");
            process::exit(1);
        }
    }
}
