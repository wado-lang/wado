//! Tests for compile error handling
//!
//! This module tests that compilation errors are properly reported with
//! correct error types, messages, and source locations.

use std::path::Path;
use wado_compiler::{CompileError, OptLevel};

#[test]
fn test_io_error_file_not_found() {
    let result = crate::common::compile_file(Path::new("nonexistent_file.wado"));
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
    let result = crate::common::compile_file(Path::new("."));
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

#[test]
fn test_lexer_error_unterminated_string() {
    let source = r#"
fn main() {
    let x = "unterminated string;
}
"#;

    let result = crate::common::compile_source(source);
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

    let result = crate::common::compile_source(source);
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

#[test]
fn test_parser_error_missing_function_body() {
    let source = r"
fn main()
";

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
            ..
        } => {
            assert!(
                message.contains("expected") || message.contains('{'),
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
    let source = r"
fn main() {
    let = 42;
}
";

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
            ..
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
    let source = r"
fn main() {
    let x = 1
    let y = 2;
}
";

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
            ..
        } => {
            assert!(
                message.contains("expected") || message.contains(';'),
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
    let source = r"
use;
";

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
            ..
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

#[test]
fn test_analyzer_error_unknown_module() {
    let source = r#"
use {foo} from "unknown:module";

fn main() {
    foo();
}
"#;

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Analyzer {
            message, filename, ..
        } => {
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

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Analyzer {
            message, filename, ..
        } => {
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

#[test]
fn test_comparison_chain_error_not_equal_cannot_chain() {
    let source = r"
fn run() {
    let a = 1 != 2 != 3;
}
";

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
            ..
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
    let source = r"
fn run() {
    let a = 1 < 2 > 3;
}
";

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
            ..
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
    let source = r"
fn run() {
    let a = 3 > 2 < 1;
}
";

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
            ..
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
    let source = r"
fn run() {
    let a = 1 == 2 < 3;
}
";

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
            ..
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
    let source = r"
fn run() {
    let a = 1 < 2 == 3;
}
";

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
            ..
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
    let source = r"
fn run() {
    let a = 1 < 2 != 3;
}
";

    let result = crate::common::compile_source(source);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        CompileError::Parser {
            message,
            line,
            column,
            filename,
            ..
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
fn test_error_display_with_filename() {
    let err = CompileError::Parser {
        message: "unexpected token".to_string(),
        line: 10,
        column: 5,
        filename: Some("test.wado".to_string()),
        is_todo_module: false,
    };

    let display = format!("{err}");
    assert!(display.contains("test.wado"));
    assert!(display.contains("10"));
    assert!(display.contains('5'));
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
        line: 5,
        column: 10,
        filename: Some("main.wado".to_string()),
    };

    let display = format!("{err}");
    assert!(display.contains("main.wado:5:10"));
    assert!(display.contains("undefined variable 'x'"));
    assert!(display.contains("analysis error"));
}

#[test]
fn test_error_display_analyzer_without_filename() {
    let err = CompileError::Analyzer {
        message: "type mismatch".to_string(),
        line: 0,
        column: 0,
        filename: None,
    };

    let display = format!("{err}");
    assert!(display.contains("type mismatch"));
    assert!(display.contains("analysis error"));
}

/// Orphan / coherence / sealed-`Reflect` errors must point at the user's file,
/// not a stdlib module (`core:libm.wat` before the fix). Regression for #1596.
fn analyzer_filename(source: &str) -> String {
    let path = Path::new("orphan_phase_diag.wado");
    let err = crate::common::compile_source_with_opts(path, source, OptLevel::default())
        .expect_err("expected a compile error");
    match err {
        CompileError::Analyzer { filename, .. } => {
            filename.expect("diagnostic must carry a source file")
        }
        other => panic!("expected Analyzer error, got: {other}"),
    }
}

#[test]
fn test_coherence_error_attributed_to_user_file() {
    let filename = analyzer_filename(
        "impl i32 {\n    fn my_foo(&self) -> i32 { return 1; }\n}\n\nexport fn run() {}\n",
    );
    assert_eq!(filename, "orphan_phase_diag.wado");
}

#[test]
fn test_orphan_error_attributed_to_user_file() {
    let filename = analyzer_filename(
        "impl Eq for String {\n    fn eq(&self, other: &Self) -> bool { return true; }\n}\n\nexport fn run() {}\n",
    );
    assert_eq!(filename, "orphan_phase_diag.wado");
}

#[test]
fn test_orphan_error_attributed_to_the_submodule_that_defines_it() {
    // The impl lives in the imported submodule, so the file must be that
    // submodule — never the entry — which pins per-module attribution (#1596).
    let path = Path::new("tests/fixtures/orphan_xmod_entry.wado");
    let err = crate::common::compile_file(path).expect_err("expected a compile error");
    match err {
        CompileError::Analyzer {
            filename, message, ..
        } => {
            assert!(message.contains("orphan rule"), "unexpected: {message}");
            let filename = filename.expect("diagnostic must carry a source file");
            assert!(
                filename.ends_with("orphan_xmod_lib.wado"),
                "expected the submodule that defines the impl, got: {filename}"
            );
        }
        other => panic!("expected Analyzer error, got: {other}"),
    }
}

#[test]
fn test_sealed_reflect_error_attributed_to_user_file() {
    let filename = analyzer_filename(
        "struct Point { x: i32, y: i32 }\n\nimpl ReflectStruct for Point {\n    type FieldTypes = [i32, i32];\n    fn type_name() -> String { return \"forged\"; }\n}\n\nexport fn run() {}\n",
    );
    assert_eq!(filename, "orphan_phase_diag.wado");
}
