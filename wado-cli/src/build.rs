use std::fs;
use std::path::Path;
use std::process;

use wado_compiler::{Analyzer, Codegen, Lexer, Parser};

pub struct BuildOptions {
    pub input: String,
    pub output: Option<String>,
}

pub fn print_usage() {
    eprintln!("Usage: wado build [options] <file.wado>");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -o <file>   Output file path (default: <input>.wasm)");
    eprintln!("  --help, -h  Show this help message");
}

pub fn parse_args(args: &[String]) -> BuildOptions {
    let mut output: Option<String> = None;
    let mut input: Option<String> = None;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_usage();
                process::exit(0);
            }
            "-o" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: -o requires an argument");
                    process::exit(1);
                }
                output = Some(args[i + 1].clone());
                i += 2;
            }
            arg if arg.starts_with('-') => {
                eprintln!("Error: unknown option '{arg}'");
                print_usage();
                process::exit(1);
            }
            arg => {
                if input.is_some() {
                    eprintln!("Error: multiple input files not supported");
                    process::exit(1);
                }
                input = Some(arg.to_string());
                i += 1;
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

    BuildOptions { input, output }
}

/// Compile a Wado source file and return the Wasm binary
pub fn compile(filename: &str) -> Vec<u8> {
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

    // Semantic analysis
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

    // Code generation
    let symbols = analyzer.into_symbols();
    let mut codegen = Codegen::new(symbols);
    codegen.generate_wasm(&module)
}

pub fn run(opts: BuildOptions) {
    let wasm = compile(&opts.input);

    // Determine output path
    let output_path = match &opts.output {
        Some(path) => Path::new(path).to_path_buf(),
        None => Path::new(&opts.input).with_extension("wasm"),
    };

    // Output Wasm binary file
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
    let wat = wasmprinter::print_bytes(&wasm).unwrap_or_else(|e| {
        eprintln!("Error generating WAT: {e}");
        process::exit(1);
    });
    let wat_path = output_path.with_extension("wat");
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
