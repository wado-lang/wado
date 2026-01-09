pub mod analyze;
pub mod ast;
pub mod codegen;
pub mod lexer;
pub mod parser;
pub mod resolver;
pub mod stdlib;
pub mod symbol;
pub mod token;

pub use analyze::Analyzer;
pub use codegen::Codegen;
pub use lexer::{LexError, Lexer};
pub use parser::{ParseError, Parser};
pub use token::Span;

use std::path::Path;

/// Compile Wado source code to Component Model WebAssembly bytes.
///
/// This is a convenience function that runs the full compilation pipeline:
/// lexer -> parser -> analyzer -> codegen
///
/// # Example
/// ```
/// let wasm = wado_compiler::compile(r#"
/// use {println, Stdout} from "core:cli";
///
/// fn main() with Stdout {
///     println("Hello!");
/// }
/// "#).expect("compilation failed");
///
/// // Verify it produces valid Component Model wasm
/// assert!(wasm.len() > 8);
/// assert_eq!(&wasm[0..4], b"\0asm");
/// ```
pub fn compile(source: &str) -> Result<Vec<u8>, CompileError> {
    compile_impl(source, None)
}

/// Compile a Wado source file to Component Model WebAssembly bytes.
///
/// Like [`compile`], but reads source from a file and includes the filename
/// in error messages.
pub fn compile_file(path: &Path) -> Result<Vec<u8>, CompileError> {
    let source = std::fs::read_to_string(path).map_err(|e| CompileError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    compile_impl(&source, Some(path.display().to_string()))
}

fn compile_impl(source: &str, filename: Option<String>) -> Result<Vec<u8>, CompileError> {
    // Lexer
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| CompileError::Lexer {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: filename.clone(),
    })?;

    // Parser
    let mut parser = Parser::new(tokens);
    let ast = parser.parse().map_err(|e| CompileError::Parser {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: filename.clone(),
    })?;

    // Analyzer
    let mut analyzer = Analyzer::new();
    analyzer.analyze(&ast, &[]).map_err(|errors| {
        // Take the first error for now
        let msg = errors
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        CompileError::Analyzer {
            message: msg,
            filename: filename.clone(),
        }
    })?;
    let _symbols = analyzer.into_symbols();

    // Codegen
    let mut codegen = Codegen::new();
    let wasm = codegen.generate_wasm(&ast);

    Ok(wasm)
}

/// Compilation error with structured location info
#[derive(Debug)]
pub enum CompileError {
    /// I/O error reading source file
    Io { path: String, message: String },
    /// Lexer error with location
    Lexer {
        message: String,
        line: usize,
        column: usize,
        filename: Option<String>,
    },
    /// Parser error with location
    Parser {
        message: String,
        line: usize,
        column: usize,
        filename: Option<String>,
    },
    /// Semantic analysis error
    Analyzer {
        message: String,
        filename: Option<String>,
    },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::Io { path, message } => {
                write!(f, "Error reading '{path}': {message}")
            }
            CompileError::Lexer {
                message,
                line,
                column,
                filename,
            } => {
                if let Some(file) = filename {
                    write!(f, "{file}:{line}:{column}: lexer error: {message}")
                } else {
                    write!(f, "line {line}, column {column}: lexer error: {message}")
                }
            }
            CompileError::Parser {
                message,
                line,
                column,
                filename,
            } => {
                if let Some(file) = filename {
                    write!(f, "{file}:{line}:{column}: parse error: {message}")
                } else {
                    write!(f, "line {line}, column {column}: parse error: {message}")
                }
            }
            CompileError::Analyzer { message, filename } => {
                if let Some(file) = filename {
                    write!(f, "{file}: analysis error: {message}")
                } else {
                    write!(f, "analysis error: {message}")
                }
            }
        }
    }
}

impl std::error::Error for CompileError {}
