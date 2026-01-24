//! Tests for compile error handling
//!
//! This module tests that compilation errors are properly reported with
//! correct error types, messages, and source locations.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use wado_compiler::{CompileError, CompilerHost, Diagnostic, OptLevel, SourceError};

// ============================================================================
// Test Compiler Hosts
// ============================================================================

/// In-memory compiler host for source-only tests
struct InMemoryTestHost;

impl CompilerHost for InMemoryTestHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<String, SourceError>> + Send {
        let path = path.to_string();
        async move { Err(SourceError::NotFound { path }) }
    }

    fn emit_diagnostic(&self, _diagnostic: Diagnostic) {}
}

/// Filesystem compiler host for file-based tests
struct FilesystemTestHost {
    base_path: PathBuf,
    diagnostics: Mutex<Vec<Diagnostic>>,
}

impl FilesystemTestHost {
    fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            diagnostics: Mutex::new(Vec::new()),
        }
    }
}

impl CompilerHost for FilesystemTestHost {
    fn load_source(
        &self,
        path: &str,
    ) -> impl std::future::Future<Output = Result<String, SourceError>> + Send {
        let full_path = self.base_path.join(path);
        async move {
            std::fs::read_to_string(&full_path).map_err(|e| SourceError::IoError {
                path: full_path.to_string_lossy().to_string(),
                message: e.to_string(),
            })
        }
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

/// Create a tokio runtime for blocking on async code
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Compile source string using in-memory host
fn compile(source: &str) -> Result<wado_compiler::CompileResult, CompileError> {
    let host = InMemoryTestHost;
    runtime().block_on(wado_compiler::compile_with_host(
        source,
        &host,
        None,
        OptLevel::default(),
    ))
}

/// Compile a file using filesystem host
fn compile_file(path: &Path) -> Result<wado_compiler::CompileResult, CompileError> {
    let source = std::fs::read_to_string(path).map_err(|e| CompileError::Io {
        path: path.to_string_lossy().to_string(),
        message: e.to_string(),
    })?;

    let base_path = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let host = FilesystemTestHost::new(base_path);

    runtime().block_on(wado_compiler::compile_with_host(
        &source,
        &host,
        Some(&path.to_string_lossy()),
        OptLevel::default(),
    ))
}

// ============================================================================
// I/O Errors
// ============================================================================

#[test]
fn test_io_error_file_not_found() {
    let result = compile_file(Path::new("nonexistent_file.wado"));
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Io { path, message } => {
            assert_eq!(path, "nonexistent_file.wado");
            assert!(message.contains("No such file") || message.contains("not found"));
        }
        other => panic!("Expected Io error, got: {other}"),
    }
}

#[test]
fn test_io_error_directory_instead_of_file() {
    let result = compile_file(Path::new("."));
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Io { path, message } => {
            assert_eq!(path, ".");
            assert!(
                message.contains("directory") || message.contains("Is a directory"),
                "Unexpected message: {message}"
            );
        }
        other => panic!("Expected Io error, got: {other}"),
    }
}

// ============================================================================
// Lexer Errors
// ============================================================================

#[test]
fn test_lexer_error_unterminated_string() {
    let source = r#"
fn main() {
    let x = "unterminated string;
}
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Lexer {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("unterminated") || message.contains("string"),
                "Unexpected message: {message}"
            );
            assert_eq!(line, 3);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Lexer error, got: {other}"),
    }
}

#[test]
fn test_lexer_error_invalid_character() {
    let source = "fn main() { let x = @invalid; }";

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Lexer {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("unexpected") || message.contains("invalid"),
                "Unexpected message: {message}"
            );
            assert_eq!(line, 1);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Lexer error, got: {other}"),
    }
}

// ============================================================================
// Parser Errors
// ============================================================================

#[test]
fn test_parser_error_missing_function_body() {
    let source = r#"
fn main()
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("expected") || message.contains("{"),
                "Unexpected message: {message}"
            );
            assert!(line > 0);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Parser error, got: {other}"),
    }
}

#[test]
fn test_parser_error_unexpected_token() {
    let source = r#"
fn main() {
    let = 42;
}
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("expected") || message.contains("identifier"),
                "Unexpected message: {message}"
            );
            assert_eq!(line, 3);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Parser error, got: {other}"),
    }
}

#[test]
fn test_parser_error_missing_semicolon() {
    let source = r#"
fn main() {
    let x = 1
    let y = 2;
}
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("expected") || message.contains(";"),
                "Unexpected message: {message}"
            );
            assert!(line >= 3);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Parser error, got: {other}"),
    }
}

#[test]
fn test_parser_error_invalid_use_statement() {
    let source = r#"
use;
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("expected") || message.contains("identifier"),
                "Unexpected message: {message}"
            );
            assert_eq!(line, 2);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Parser error, got: {other}"),
    }
}

// ============================================================================
// Analyzer Errors
// ============================================================================

#[test]
fn test_analyzer_error_unknown_module() {
    let source = r#"
use {foo} from "unknown:module";

fn main() {
    foo();
}
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Analyzer { message, filename } => {
            assert!(
                message.contains("unknown") || message.contains("not found"),
                "Unexpected message: {message}"
            );
            assert!(filename.is_none());
        }
        other => panic!("Expected Analyzer error, got: {other}"),
    }
}

#[test]
fn test_analyzer_error_unknown_import() {
    let source = r#"
use {nonexistent_function} from "core:cli";

fn main() {
    nonexistent_function();
}
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Analyzer { message, filename } => {
            assert!(
                message.contains("nonexistent_function") || message.contains("not found"),
                "Unexpected message: {message}"
            );
            assert!(filename.is_none());
        }
        other => panic!("Expected Analyzer error, got: {other}"),
    }
}

// Note: The following analyzer checks are not yet implemented:
// - Undefined symbol in expressions (e.g., using undefined_variable)
// - Missing effect declarations (e.g., calling println without `with Stdout`)
// - Type checking
// These tests should be added when the analyzer is extended.

// ============================================================================
// Comparison Chaining Errors
// ============================================================================

#[test]
fn test_comparison_chain_error_not_equal_cannot_chain() {
    let source = r#"
fn run() {
    let a = 1 != 2 != 3;
}
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("!= operator cannot be chained"),
                "Unexpected message: {message}"
            );
            assert_eq!(line, 3);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Parser error, got: {other}"),
    }
}

#[test]
fn test_comparison_chain_error_mixed_ascending_descending() {
    let source = r#"
fn run() {
    let a = 1 < 2 > 3;
}
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("cannot mix ascending") || message.contains("descending"),
                "Unexpected message: {message}"
            );
            assert_eq!(line, 3);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Parser error, got: {other}"),
    }
}

#[test]
fn test_comparison_chain_error_mixed_descending_ascending() {
    let source = r#"
fn run() {
    let a = 3 > 2 < 1;
}
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("cannot mix ascending") || message.contains("descending"),
                "Unexpected message: {message}"
            );
            assert_eq!(line, 3);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Parser error, got: {other}"),
    }
}

#[test]
fn test_comparison_chain_error_equality_with_inequality() {
    let source = r#"
fn run() {
    let a = 1 == 2 < 3;
}
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("cannot mix ==") || message.contains("inequality"),
                "Unexpected message: {message}"
            );
            assert_eq!(line, 3);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Parser error, got: {other}"),
    }
}

#[test]
fn test_comparison_chain_error_inequality_with_equality() {
    let source = r#"
fn run() {
    let a = 1 < 2 == 3;
}
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("cannot mix ==") || message.contains("inequality"),
                "Unexpected message: {message}"
            );
            assert_eq!(line, 3);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Parser error, got: {other}"),
    }
}

#[test]
fn test_comparison_chain_error_not_equal_after_less_than() {
    let source = r#"
fn run() {
    let a = 1 < 2 != 3;
}
"#;

    let result = compile(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
        } => {
            assert!(
                message.contains("!= operator cannot be chained"),
                "Unexpected message: {message}"
            );
            assert_eq!(line, 3);
            assert!(column > 0);
            assert!(filename.is_none());
        }
        other => panic!("Expected Parser error, got: {other}"),
    }
}

// ============================================================================
// Error Display Format
// ============================================================================

#[test]
fn test_error_display_with_filename() {
    // Use compile_file with a file that has syntax errors
    // For this test, we'll check the Display impl manually
    let err = CompileError::Parser {
        message: "unexpected token".to_string(),
        line: 10,
        column: 5,
        filename: Some("test.wado".to_string()),
    };

    let display = format!("{err}");
    assert!(display.contains("test.wado"));
    assert!(display.contains("10"));
    assert!(display.contains("5"));
    assert!(display.contains("unexpected token"));
    assert!(display.contains("parse error"));
}

#[test]
fn test_error_display_without_filename() {
    let err = CompileError::Lexer {
        message: "invalid character".to_string(),
        line: 3,
        column: 12,
        filename: None,
    };

    let display = format!("{err}");
    assert!(display.contains("line 3"));
    assert!(display.contains("column 12"));
    assert!(display.contains("invalid character"));
    assert!(display.contains("lexer error"));
}

#[test]
fn test_error_display_io() {
    let err = CompileError::Io {
        path: "missing.wado".to_string(),
        message: "No such file or directory".to_string(),
    };

    let display = format!("{err}");
    assert!(display.contains("missing.wado"));
    assert!(display.contains("No such file or directory"));
}

#[test]
fn test_error_display_analyzer_with_filename() {
    let err = CompileError::Analyzer {
        message: "undefined variable 'x'".to_string(),
        filename: Some("main.wado".to_string()),
    };

    let display = format!("{err}");
    assert!(display.contains("main.wado"));
    assert!(display.contains("undefined variable 'x'"));
    assert!(display.contains("analysis error"));
}

#[test]
fn test_error_display_analyzer_without_filename() {
    let err = CompileError::Analyzer {
        message: "type mismatch".to_string(),
        filename: None,
    };

    let display = format!("{err}");
    assert!(display.contains("type mismatch"));
    assert!(display.contains("analysis error"));
}
