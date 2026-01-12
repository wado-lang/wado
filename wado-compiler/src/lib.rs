pub mod analyze;
pub mod ast;
pub mod bundled;
pub mod codegen;
pub mod lexer;
pub mod parser;
pub mod resolver;
pub mod stdlib;
pub mod symbol;
pub mod token;
pub mod wasm_postprocess;

pub use analyze::Analyzer;
pub use codegen::Codegen;
pub use lexer::{LexError, Lexer};
pub use parser::{ParseError, Parser};
pub use token::Span;

use std::path::Path;

/// Result of compiling a Wado source file
#[derive(Debug)]
pub struct CompileResult {
    /// Compiled WebAssembly component bytes
    pub wasm: Vec<u8>,
    /// Parsed module AST (includes data section if present)
    pub module: ast::Module,
}

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
/// fn run() with Stdout {
///     println("Hello!");
/// }
/// "#).expect("compilation failed");
///
/// // Verify it produces valid Component Model wasm
/// assert!(wasm.len() > 8);
/// assert_eq!(&wasm[0..4], b"\0asm");
/// ```
pub fn compile(source: &str) -> Result<Vec<u8>, CompileError> {
    compile_impl(source, None, None).map(|r| r.wasm)
}

/// Compile Wado source code with a base path for relative imports.
pub fn compile_with_base_path(source: &str, base_path: &Path) -> Result<Vec<u8>, CompileError> {
    compile_impl(source, None, Some(base_path)).map(|r| r.wasm)
}

/// Compile a Wado source file to Component Model WebAssembly.
///
/// Like [`compile`], but reads source from a file and includes the filename
/// in error messages. Also supports relative imports from the file's directory.
/// Returns a [`CompileResult`] containing both the wasm bytes and the parsed module.
pub fn compile_file(path: &Path) -> Result<CompileResult, CompileError> {
    let source = std::fs::read_to_string(path).map_err(|e| CompileError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;

    // Get the directory containing the file for relative imports
    let base_path = path.parent().map(|p| p.to_path_buf());

    compile_impl(
        &source,
        Some(path.display().to_string()),
        base_path.as_deref(),
    )
}

fn compile_impl(
    source: &str,
    filename: Option<String>,
    base_path: Option<&Path>,
) -> Result<CompileResult, CompileError> {
    // Lexer
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().map_err(|e| CompileError::Lexer {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: filename.clone(),
    })?;
    let data_section = lexer.into_data_section();

    // Parser (with data section from lexer)
    let mut parser = Parser::with_data_section(tokens, data_section);
    let ast = parser.parse().map_err(|e| CompileError::Parser {
        message: e.message,
        line: e.span.line,
        column: e.span.column,
        filename: filename.clone(),
    })?;

    // Analyzer (with base path for local imports if provided)
    let mut analyzer = if let Some(base) = base_path {
        Analyzer::with_base_path(base)
    } else {
        Analyzer::new()
    };
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

    // Get loaded modules and symbols for codegen
    let (symbols, loaded_modules) = analyzer.into_parts();

    // Convert HashMap to Vec of references for codegen
    let loaded_modules_vec: Vec<(&Vec<String>, &crate::ast::Module)> =
        loaded_modules.iter().collect();

    // Codegen (pass source code for power-assert messages)
    let mut codegen = Codegen::new_with_source(source.to_string());
    let wasm = codegen.generate_wasm_with_modules(&ast, &loaded_modules_vec, &symbols);

    Ok(CompileResult { wasm, module: ast })
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
