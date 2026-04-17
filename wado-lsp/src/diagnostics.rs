use wado_compiler::{Code, Diagnostic as CompilerDiagnostic, Severity as CompilerSeverity};

/// LSP-compatible diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

/// Zero-based line/column position in a text document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// A range in a text document (start inclusive, end exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// A diagnostic message (error, warning, etc.) for a text document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Severity,
    pub code: String,
    pub message: String,
}

/// Convert a compiler diagnostic to an LSP-compatible diagnostic.
///
/// Returns `None` for diagnostics that should not be shown to the user
/// (span tracking, log messages, diagnostics without source location).
pub fn from_compiler_diagnostic(diag: &CompilerDiagnostic, _uri: &str) -> Option<Diagnostic> {
    // Skip internal span tracking and log messages
    match diag.code {
        Code::SpanStart | Code::SpanEnd | Code::Log => return None,
        _ => {}
    }

    let severity = match diag.severity {
        CompilerSeverity::Fatal | CompilerSeverity::Error => Severity::Error,
        CompilerSeverity::Warning => Severity::Warning,
        CompilerSeverity::Info => Severity::Information,
        CompilerSeverity::Debug => return None,
    };

    let span = diag.span.as_ref()?;

    // Compiler uses 1-based line/column; LSP uses 0-based.
    let start_line = span.line.saturating_sub(1) as u32;
    let start_char = span.column.saturating_sub(1) as u32;

    let end_line = span
        .end_line
        .map_or(start_line, |l| l.saturating_sub(1) as u32);
    let end_char = span.end_column.map_or(
        // No end_column available — highlight to end of start token.
        // Use start_char + 1 as a minimal highlight.
        start_char + 1,
        |c| c.saturating_sub(1) as u32,
    );

    Some(Diagnostic {
        range: Range {
            start: Position {
                line: start_line,
                character: start_char,
            },
            end: Position {
                line: end_line,
                character: end_char,
            },
        },
        severity,
        code: format!("{}", diag.code),
        message: diag.message.clone(),
    })
}

#[cfg(test)]
mod tests {
    use wado_compiler::DiagnosticSpan;

    use super::*;

    #[test]
    fn test_convert_error_diagnostic() {
        let compiler_diag = CompilerDiagnostic {
            severity: CompilerSeverity::Error,
            code: Code::TypeMismatch,
            message: "expected i32, found String".to_string(),
            span: Some(DiagnosticSpan {
                file: "test.wado".to_string(),
                line: 10,
                column: 5,
                end_line: Some(10),
                end_column: None,
            }),
        };

        let diag = from_compiler_diagnostic(&compiler_diag, "file:///test.wado").unwrap();
        assert_eq!(diag.severity, Severity::Error);
        assert_eq!(diag.code, "TYPE_MISMATCH");
        assert_eq!(diag.message, "expected i32, found String");
        assert_eq!(diag.range.start.line, 9); // 0-based
        assert_eq!(diag.range.start.character, 4); // 0-based
    }

    #[test]
    fn test_skip_span_tracking() {
        let compiler_diag = CompilerDiagnostic {
            severity: CompilerSeverity::Debug,
            code: Code::SpanStart,
            message: "parse".to_string(),
            span: None,
        };
        assert!(from_compiler_diagnostic(&compiler_diag, "file:///test.wado").is_none());
    }

    #[test]
    fn test_skip_no_span() {
        let compiler_diag = CompilerDiagnostic {
            severity: CompilerSeverity::Error,
            code: Code::CodegenError,
            message: "internal error".to_string(),
            span: None,
        };
        assert!(from_compiler_diagnostic(&compiler_diag, "file:///test.wado").is_none());
    }

    #[test]
    fn test_warning_severity() {
        let compiler_diag = CompilerDiagnostic {
            severity: CompilerSeverity::Warning,
            code: Code::UndefinedVariable,
            message: "unused variable".to_string(),
            span: Some(DiagnosticSpan {
                file: "test.wado".to_string(),
                line: 1,
                column: 1,
                end_line: Some(1),
                end_column: None,
            }),
        };
        let diag = from_compiler_diagnostic(&compiler_diag, "file:///test.wado").unwrap();
        assert_eq!(diag.severity, Severity::Warning);
    }

    #[test]
    fn end_column_is_used_when_provided() {
        // Compiler now always populates end_column (via Span::end_column) when
        // building a DiagnosticSpan from a Span. Verify the conversion uses it
        // exactly rather than falling back to start + 1.
        let compiler_diag = CompilerDiagnostic {
            severity: CompilerSeverity::Error,
            code: Code::TypeMismatch,
            message: "type mismatch".to_string(),
            span: Some(DiagnosticSpan {
                file: "test.wado".to_string(),
                line: 3,
                column: 5,
                end_line: Some(3),
                end_column: Some(15),
            }),
        };
        let diag = from_compiler_diagnostic(&compiler_diag, "file:///test.wado").unwrap();
        assert_eq!(
            diag.range,
            Range {
                start: Position {
                    line: 2,
                    character: 4
                },
                end: Position {
                    line: 2,
                    character: 14
                },
            }
        );
    }
}
