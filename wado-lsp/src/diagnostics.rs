use serde::{Deserialize, Serialize};
use wado_compiler::{Code, Diagnostic as CompilerDiagnostic, Severity as CompilerSeverity};

use crate::macros::lsp_repr_u32_enum;
use crate::text::{LineIndex, PositionEncoding};

lsp_repr_u32_enum!(
    /// LSP-compatible diagnostic severity. Serializes as the 1..=4 integer
    /// defined by the LSP wire format.
    pub enum Severity {
        Error = 1,
        Warning = 2,
        Information = 3,
        Hint = 4,
    }
);

lsp_repr_u32_enum!(
    /// LSP `DiagnosticTag`: `Unnecessary` fades the range, `Deprecated`
    /// strikes it through.
    pub enum DiagnosticTag {
        Unnecessary = 1,
        Deprecated = 2,
    }
);

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
    /// Diagnostic tags; unused / dead-code lints carry `Unnecessary`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<DiagnosticTag>,
}

/// Anchor for a diagnostic the compiler reported without a source location.
const DOCUMENT_START: Range = Range {
    start: Position {
        line: 0,
        character: 0,
    },
    end: Position {
        line: 0,
        character: 0,
    },
};

/// Convert a [`DiagnosticSpan`](wado_compiler::DiagnosticSpan) to an LSP
/// [`Range`], re-expressing the compiler's 1-based codepoint columns in
/// `encoding`. `lines` indexes the text the span points into; `None` passes
/// the codepoint columns through (correct for ASCII / UTF-32).
fn span_to_range(
    span: &wado_compiler::DiagnosticSpan,
    lines: Option<&LineIndex>,
    encoding: PositionEncoding,
) -> Range {
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

    let (start_char, end_char) = match lines {
        Some(lines) => (
            lines.to_character(start_line, start_codepoint, encoding),
            lines.to_character(end_line, end_codepoint, encoding),
        ),
        None => (start_codepoint, end_codepoint),
    };

    Range {
        start: Position {
            line: start_line,
            character: start_char,
        },
        end: Position {
            line: end_line,
            character: end_char,
        },
    }
}

/// Convert a compiler diagnostic to an LSP-compatible diagnostic.
///
/// Returns `None` for diagnostics that should not be shown to the user
/// (span tracking, log messages, `Debug`-severity output).
///
/// A diagnostic with no span is anchored at the start of the document
/// rather than dropped. The loader reports its hard failures — a missing
/// or unreadable import (`ModuleNotFound`) above all — without a span, and
/// those are exactly the failures that also blank out every position query
/// for the document. Dropping them left the editor showing a file with no
/// errors, no hover, and no navigation, with nothing on screen to explain
/// why.
///
/// `lines` and `encoding` are used to re-express the compiler's
/// codepoint columns in the negotiated position encoding. Pass
/// `None` for diagnostics whose `span.file` is not the request
/// document — the result will still be valid for ASCII source but may
/// drift the column for non-ASCII codepoints (the spec's UTF-16
/// default). For the request document itself the caller should always
/// provide the index. It is built once per `textDocument/publishDiagnostics`
/// and shared across every diagnostic in it — see [`LineIndex`].
pub(crate) fn from_compiler_diagnostic(
    diag: &CompilerDiagnostic,
    _uri: &str,
    lines: Option<&LineIndex>,
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

    let tags = if diag.code.is_unused_lint() {
        vec![DiagnosticTag::Unnecessary]
    } else {
        Vec::new()
    };

    let range = match diag.span.as_ref() {
        Some(span) => span_to_range(span, lines, encoding),
        None => DOCUMENT_START,
    };

    Some(Diagnostic {
        range,
        severity,
        code: format!("{}", diag.code),
        source: Some("wado".to_string()),
        message: diag.message.clone(),
        tags,
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
    fn dead_code_diagnostic_carries_unnecessary_tag() {
        let compiler_diag = CompilerDiagnostic {
            severity: CompilerSeverity::Warning,
            code: Code::DeadFunction,
            message: "function `helper` is never used".to_string(),
            span: Some(DiagnosticSpan {
                file: "test.wado".to_string(),
                line: 1,
                column: 4,
                end_line: Some(1),
                end_column: Some(10),
            }),
        };
        let diag = from_compiler_diagnostic(
            &compiler_diag,
            "file:///test.wado",
            None,
            PositionEncoding::Utf16,
        )
        .unwrap();
        assert_eq!(diag.tags, vec![DiagnosticTag::Unnecessary]);
        let json = serde_json::to_string(&diag).unwrap();
        assert!(json.contains("\"tags\":[1]"), "got {json}");
    }

    #[test]
    fn error_diagnostic_has_no_tags() {
        let compiler_diag = CompilerDiagnostic {
            severity: CompilerSeverity::Error,
            code: Code::TypeMismatch,
            message: "type mismatch".to_string(),
            span: Some(DiagnosticSpan {
                file: "test.wado".to_string(),
                line: 1,
                column: 1,
                end_line: Some(1),
                end_column: Some(2),
            }),
        };
        let diag = from_compiler_diagnostic(
            &compiler_diag,
            "file:///test.wado",
            None,
            PositionEncoding::Utf16,
        )
        .unwrap();
        assert!(diag.tags.is_empty());
        let json = serde_json::to_string(&diag).unwrap();
        assert!(
            !json.contains("tags"),
            "empty tags must be omitted, got {json}"
        );
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
    fn span_less_diagnostic_anchors_at_document_start() {
        // The loader reports `ModuleNotFound` without a span, and that same
        // failure empties the `Semantics` so every position query goes quiet.
        // Dropping it left the user with a silently dead file.
        let compiler_diag = CompilerDiagnostic {
            severity: CompilerSeverity::Error,
            code: Code::ModuleNotFound,
            message: "module not found: ./missing.wado".to_string(),
            span: None,
        };
        let diag = from_compiler_diagnostic(
            &compiler_diag,
            "file:///test.wado",
            None,
            PositionEncoding::Utf16,
        )
        .expect("a span-less error must still reach the editor");
        assert_eq!(diag.range, DOCUMENT_START);
        assert_eq!(diag.severity, Severity::Error);
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
            Some(&LineIndex::new(src)),
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
