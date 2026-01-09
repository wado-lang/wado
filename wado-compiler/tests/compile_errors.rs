//! Tests for compile error handling
//!
//! This module tests that compilation errors are properly reported with
//! correct error types, messages, and source locations.

use std::path::Path;
use wado_compiler::{CompileError, compile, compile_file};

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
use unknown::module::{foo};

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
use core::cli::{nonexistent_function};

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
