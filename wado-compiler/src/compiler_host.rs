//! Compiler host abstraction for I/O operations
//!
//! This module provides the `CompilerHost` trait that abstracts source loading
//! and diagnostic output, enabling the compiler to run in different environments:
//! - CLI with filesystem access
//! - Browser with in-memory sources
//! - LSP with editor buffers
//!
//! See WEP: CompilerHost Abstraction for Compiler I/O

use std::future::Future;

use crate::token::Span;

/// Severity level for diagnostics
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Compilation error (prevents successful compilation)
    Error,
    /// Warning (compilation continues but may indicate issues)
    Warning,
    /// Informational message
    Info,
    /// Hint for improvement
    Hint,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Error => write!(f, "error"),
            Severity::Warning => write!(f, "warning"),
            Severity::Info => write!(f, "info"),
            Severity::Hint => write!(f, "hint"),
        }
    }
}

/// Error code for diagnostics
///
/// Named error codes without payloads for clear categorization.
/// The actual error details go in the `Diagnostic::message` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    // Lexer errors
    /// Invalid character in source
    InvalidCharacter,
    /// Unterminated string literal
    UnterminatedString,
    /// Invalid escape sequence
    InvalidEscape,

    // Parser errors
    /// Unexpected token encountered
    UnexpectedToken,
    /// Expected a specific token
    ExpectedToken,
    /// Invalid syntax
    InvalidSyntax,

    // Binding errors
    /// Variable not found in scope
    UndefinedVariable,
    /// Duplicate definition
    DuplicateDefinition,
    /// Cannot assign to immutable variable
    ImmutableAssignment,

    // Type errors
    /// Type mismatch
    TypeMismatch,
    /// Unknown type name
    UnknownType,
    /// Invalid type cast
    InvalidCast,

    // Module errors
    /// Module not found
    ModuleNotFound,
    /// Circular dependency detected
    CircularDependency,
    /// Failed to parse module
    ModuleParseError,

    // I/O errors
    /// File read error
    FileReadError,
    /// Network error
    NetworkError,

    // Codegen errors
    /// Code generation failed
    CodegenError,
    /// Unsupported feature
    UnsupportedFeature,
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ErrorCode::InvalidCharacter => "INVALID_CHARACTER",
            ErrorCode::UnterminatedString => "UNTERMINATED_STRING",
            ErrorCode::InvalidEscape => "INVALID_ESCAPE",
            ErrorCode::UnexpectedToken => "UNEXPECTED_TOKEN",
            ErrorCode::ExpectedToken => "EXPECTED_TOKEN",
            ErrorCode::InvalidSyntax => "INVALID_SYNTAX",
            ErrorCode::UndefinedVariable => "UNDEFINED_VARIABLE",
            ErrorCode::DuplicateDefinition => "DUPLICATE_DEFINITION",
            ErrorCode::ImmutableAssignment => "IMMUTABLE_ASSIGNMENT",
            ErrorCode::TypeMismatch => "TYPE_MISMATCH",
            ErrorCode::UnknownType => "UNKNOWN_TYPE",
            ErrorCode::InvalidCast => "INVALID_CAST",
            ErrorCode::ModuleNotFound => "MODULE_NOT_FOUND",
            ErrorCode::CircularDependency => "CIRCULAR_DEPENDENCY",
            ErrorCode::ModuleParseError => "MODULE_PARSE_ERROR",
            ErrorCode::FileReadError => "FILE_READ_ERROR",
            ErrorCode::NetworkError => "NETWORK_ERROR",
            ErrorCode::CodegenError => "CODEGEN_ERROR",
            ErrorCode::UnsupportedFeature => "UNSUPPORTED_FEATURE",
        };
        write!(f, "{name}")
    }
}

/// A compiler diagnostic (error, warning, etc.)
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Severity level
    pub severity: Severity,
    /// Error code categorizing the diagnostic
    pub code: ErrorCode,
    /// Human-readable message
    pub message: String,
    /// Source location (if available)
    pub span: Option<DiagnosticSpan>,
}

/// Source location for a diagnostic
#[derive(Debug, Clone)]
pub struct DiagnosticSpan {
    /// File path or module name
    pub file: String,
    /// Line number (1-based)
    pub line: usize,
    /// Column number (1-based)
    pub column: usize,
    /// Optional end position for ranges
    pub end_line: Option<usize>,
    pub end_column: Option<usize>,
}

impl DiagnosticSpan {
    /// Create a DiagnosticSpan from a Span and optional filename
    pub fn from_span(span: &Span, filename: Option<&str>) -> Self {
        DiagnosticSpan {
            file: filename.unwrap_or_default().to_string(),
            line: span.line,
            column: span.column,
            end_line: Some(span.end_line),
            end_column: None, // Span doesn't have end_column
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(span) = &self.span {
            write!(
                f,
                "{}:{}:{}: {}: {}",
                span.file, span.line, span.column, self.severity, self.message
            )
        } else {
            write!(f, "{}: {}", self.severity, self.message)
        }
    }
}

/// Error returned when source loading fails
#[derive(Debug, Clone)]
pub enum SourceError {
    /// Module/file was not found
    NotFound { path: String },
    /// I/O error reading the source
    IoError { path: String, message: String },
    /// Network error (for future HTTP support)
    NetworkError { url: String, message: String },
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::NotFound { path } => write!(f, "module not found: {path}"),
            SourceError::IoError { path, message } => {
                write!(f, "error reading '{path}': {message}")
            }
            SourceError::NetworkError { url, message } => {
                write!(f, "network error fetching '{url}': {message}")
            }
        }
    }
}

impl std::error::Error for SourceError {}

/// Abstraction for compiler I/O operations
///
/// This trait enables the compiler to work in different environments:
/// - `FilesystemCompilerHost` for CLI usage (implemented in wado-cli)
/// - `InMemoryCompilerHost` for testing
/// - `BrowserCompilerHost` for browser/playground usage
/// - `LspCompilerHost` for LSP integration
///
/// # Standard Library Handling
///
/// Standard library paths (`core:*`, `wasi:*`) are NOT passed to `load_source`.
/// They are handled directly by the compiler via embedded sources (`include_str!`).
///
/// # Example
///
/// ```ignore
/// struct MyHost { /* ... */ }
///
/// impl CompilerHost for MyHost {
///     async fn load_source(&self, path: &str) -> Result<String, SourceError> {
///         // Load source from custom storage
///     }
///
///     async fn emit_diagnostic(&self, diagnostic: Diagnostic) {
///         // Handle diagnostic (print, collect, send to UI, etc.)
///     }
/// }
/// ```
pub trait CompilerHost: Send + Sync {
    /// Load source code for a user module
    ///
    /// # Arguments
    /// * `path` - Normalized module path (e.g., "./lib.wado", "../utils.wado")
    ///   NOTE: Standard library paths (core:*, wasi:*) are NOT passed to this method.
    ///   They are handled directly by the compiler via embedded sources.
    ///
    /// # Returns
    /// The complete source code including `__DATA__` section if present
    fn load_source(&self, path: &str) -> impl Future<Output = Result<String, SourceError>> + Send;

    /// Emit a diagnostic (error, warning, etc.)
    ///
    /// This method is called by the compiler whenever a diagnostic needs to be reported.
    /// Implementations can print to stderr, collect into a list, send to an LSP client, etc.
    fn emit_diagnostic(&self, diagnostic: Diagnostic) -> impl Future<Output = ()> + Send;
}

/// A simple in-memory compiler host for testing
///
/// This host stores sources in a HashMap and collects diagnostics in a Vec.
#[derive(Debug, Default)]
pub struct InMemoryCompilerHost {
    /// Source files by path
    sources: std::collections::HashMap<String, String>,
    /// Collected diagnostics
    diagnostics: std::sync::Mutex<Vec<Diagnostic>>,
}

impl InMemoryCompilerHost {
    /// Create a new empty in-memory host
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a source file
    pub fn add_source(&mut self, path: impl Into<String>, source: impl Into<String>) {
        self.sources.insert(path.into(), source.into());
    }

    /// Get all collected diagnostics
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }

    /// Check if any errors were reported
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    /// Clear all diagnostics
    pub fn clear_diagnostics(&self) {
        self.diagnostics.lock().unwrap().clear();
    }
}

impl CompilerHost for InMemoryCompilerHost {
    async fn load_source(&self, path: &str) -> Result<String, SourceError> {
        self.sources
            .get(path)
            .cloned()
            .ok_or_else(|| SourceError::NotFound {
                path: path.to_string(),
            })
    }

    async fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_in_memory_host() {
        let mut host = InMemoryCompilerHost::new();
        host.add_source("./test.wado", "fn run() {}");

        // Test load_source
        let result = host.load_source("./test.wado").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "fn run() {}");

        // Test not found
        let result = host.load_source("./missing.wado").await;
        assert!(matches!(result, Err(SourceError::NotFound { .. })));
    }

    #[test]
    fn test_diagnostic_display() {
        let diag = Diagnostic {
            severity: Severity::Error,
            code: ErrorCode::UnexpectedToken,
            message: "expected ';' but found '}'".to_string(),
            span: Some(DiagnosticSpan {
                file: "test.wado".to_string(),
                line: 10,
                column: 5,
                end_line: None,
                end_column: None,
            }),
        };

        let display = format!("{diag}");
        assert!(display.contains("test.wado:10:5"));
        assert!(display.contains("error"));
        assert!(display.contains("expected ';'"));
    }
}
