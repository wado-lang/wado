use serde::{Deserialize, Serialize};
use wado_compiler::{Code, Diagnostic as CompilerDiagnostic, Severity as CompilerSeverity};

use crate::text::PositionEncoding;

/// LSP-compatible diagnostic severity. Serializes as the 1..=4 integer
/// defined by the LSP wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "u32", try_from = "u32")]
pub enum Severity {
    Error = 1,
    Warning = 2,
    Information = 3,
    Hint = 4,
}

impl From<Severity> for u32 {
    fn from(s: Severity) -> Self {
        s as Self
    }
}

impl TryFrom<u32> for Severity {
    type Error = String;

    fn try_from(n: u32) -> Result<Self, <Self as TryFrom<u32>>::Error> {
        match n {
            1 => Ok(Self::Error),
            2 => Ok(Self::Warning),
            3 => Ok(Self::Information),
            4 => Ok(Self::Hint),
            _ => Err(format!("invalid Severity: {n}")),
        }
    }
}

/// Zero-based line/column position in a text document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// A range in a text document (start inclusive, end exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// A diagnostic message (error, warning, etc.) for a text document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub range: Range,
    pub severity: Severity,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub message: String,
}

/// Convert a compiler diagnostic to an LSP-compatible diagnostic.
///
/// Returns `None` for diagnostics that should not be shown to the user
/// (span tracking, log messages, diagnostics without source location).
///
/// `source` and `encoding` are used to re-express the compiler's
/// codepoint columns in the negotiated position encoding. Pass
/// `None` for diagnostics whose `span.file` is not the request
/// document — the result will still be valid for ASCII source but may
/// drift the column for non-ASCII codepoints (the spec's UTF-16
/// default). For the request document itself the caller should always
/// provide `Some(source)`.
pub fn from_compiler_diagnostic(
    diag: &CompilerDiagnostic,
    _uri: &str,
    source: Option<&str>,
    encoding: PositionEncoding,
) -> Option<Diagnostic> {
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

    // Compiler uses 1-based line/codepoint column; LSP uses 0-based.
    let start_line = span.line.saturating_sub(1) as u32;
    let end_line = span
        .end_line
        .map_or(start_line, |l| l.saturating_sub(1) as u32);
    let start_codepoint = span.column.saturating_sub(1) as u32;
    let end_codepoint = span
        .end_column
        .map_or(start_codepoint.saturating_add(1), |c| {
            c.saturating_sub(1) as u32
        });

    let (start_char, end_char) = match source {
        Some(src) => (
            codepoint_column_to_character(src, start_line, start_codepoint, encoding),
            codepoint_column_to_character(src, end_line, end_codepoint, encoding),
        ),
        None => (start_codepoint, end_codepoint),
    };

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
        source: Some("wado".to_string()),
        message: diag.message.clone(),
    })
}

/// Resolve a 0-based codepoint column on `line` of `source` into a
/// 0-based LSP `character` in `encoding`. Saturates to the end of the
/// line on overflow.
fn codepoint_column_to_character(
    source: &str,
    line: u32,
    codepoint_col: u32,
    encoding: PositionEncoding,
) -> u32 {
    let Some(line_text) = source.split_inclusive('\n').nth(line as usize) else {
        return codepoint_col;
    };
    let line_content = line_text
        .strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(line_text);
    let max = line_content.chars().count() as u32;
    let codepoint_col = codepoint_col.min(max);
    match encoding {
        PositionEncoding::Utf8 => line_content
            .chars()
            .take(codepoint_col as usize)
            .map(|c| c.len_utf8() as u32)
            .sum(),
        PositionEncoding::Utf16 => line_content
            .chars()
            .take(codepoint_col as usize)
            .map(|c| c.len_utf16() as u32)
            .sum(),
        PositionEncoding::Utf32 => codepoint_col,
    }
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

        let diag = from_compiler_diagnostic(
            &compiler_diag,
            "file:///test.wado",
            None,
            PositionEncoding::Utf16,
        )
        .unwrap();
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
        assert!(
            from_compiler_diagnostic(
                &compiler_diag,
                "file:///test.wado",
                None,
                PositionEncoding::Utf16
            )
            .is_none()
        );
    }

    #[test]
    fn test_skip_no_span() {
        let compiler_diag = CompilerDiagnostic {
            severity: CompilerSeverity::Error,
            code: Code::CodegenError,
            message: "internal error".to_string(),
            span: None,
        };
        assert!(
            from_compiler_diagnostic(
                &compiler_diag,
                "file:///test.wado",
                None,
                PositionEncoding::Utf16
            )
            .is_none()
        );
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
        let diag = from_compiler_diagnostic(
            &compiler_diag,
            "file:///test.wado",
            None,
            PositionEncoding::Utf16,
        )
        .unwrap();
        assert_eq!(diag.severity, Severity::Warning);
    }

    #[test]
    fn source_none_passes_codepoint_columns_through_verbatim() {
        // When the caller cannot supply the right module's source
        // (cross-file diagnostic, no on-hand text), `from_compiler_diagnostic`
        // must NOT re-encode against the wrong text. Codepoint columns
        // are emitted verbatim — correct under UTF-32 / ASCII, drifts
        // under UTF-16 but the alternative (re-encoding against a
        // different file's bytes) is worse.
        let compiler_diag = CompilerDiagnostic {
            severity: CompilerSeverity::Error,
            code: Code::TypeMismatch,
            message: "x".to_string(),
            span: Some(DiagnosticSpan {
                file: "imported.wado".to_string(),
                line: 1,
                column: 13, // codepoint col in imported.wado
                end_line: Some(1),
                end_column: Some(15),
            }),
        };
        let diag = from_compiler_diagnostic(
            &compiler_diag,
            "file:///entry.wado",
            None, // cross-file: no source on hand
            PositionEncoding::Utf16,
        )
        .unwrap();
        // Codepoint 13 (1-based) → LSP 0-based 12.
        assert_eq!(diag.range.start.character, 12);
        assert_eq!(diag.range.end.character, 14);
    }

    #[test]
    fn source_some_reencodes_to_utf16_for_non_ascii() {
        // When the diagnostic's span IS in the entry document, the
        // caller passes `Some(source)` and we re-express the codepoint
        // column in the requested encoding. For "// 🦀🦀" the
        // codepoint column 4 ("after '// 🦀'") sits at UTF-16 unit 5
        // because 🦀 is two UTF-16 units.
        let src = "// 🦀🦀\n";
        let compiler_diag = CompilerDiagnostic {
            severity: CompilerSeverity::Error,
            code: Code::TypeMismatch,
            message: "x".to_string(),
            span: Some(DiagnosticSpan {
                file: "entry.wado".to_string(),
                line: 1,
                column: 5,
                end_line: Some(1),
                end_column: Some(6),
            }),
        };
        let diag = from_compiler_diagnostic(
            &compiler_diag,
            "file:///entry.wado",
            Some(src),
            PositionEncoding::Utf16,
        )
        .unwrap();
        // Codepoint 5 → "// 🦀" → 3 ASCII + 1 codepoint (2 utf-16 units) = 5 utf-16 units.
        assert_eq!(diag.range.start.character, 5);
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
        let diag = from_compiler_diagnostic(
            &compiler_diag,
            "file:///test.wado",
            None,
            PositionEncoding::Utf16,
        )
        .unwrap();
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
