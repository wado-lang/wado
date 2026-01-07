mod ast;
mod codegen;
mod lexer;
mod parser;
mod token;

use std::env;
use std::fs;
use std::path::Path;
use std::process;

use codegen::Codegen;
use lexer::Lexer;
use parser::Parser;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: wado-compiler <file.wado>");
        process::exit(1);
    }

    let filename = &args[1];

    let source = match fs::read_to_string(filename) {
        Ok(content) => content,
        Err(e) => {
            eprintln!("Error reading file '{}': {}", filename, e);
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

    // Code generation
    let mut codegen = Codegen::new();
    let wat = codegen.generate(&module);

    // Output WAT file
    let output_path = Path::new(filename).with_extension("wat");
    match fs::write(&output_path, &wat) {
        Ok(_) => {
            eprintln!("Generated: {}", output_path.display());
        }
        Err(e) => {
            eprintln!("Error writing output file: {}", e);
            process::exit(1);
        }
    }

    // Also print to stdout
    print!("{}", wat);
}
