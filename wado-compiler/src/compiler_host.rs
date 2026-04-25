//! Compiler host abstraction for I/O operations
//!
//! This module provides the `CompilerHost` trait that abstracts source loading
//! and diagnostic output, enabling the compiler to run in different environments:
//! - CLI with filesystem access
//! - Browser with in-memory sources
//! - LSP with editor buffers
//!
//! See WEP: `CompilerHost` Abstraction for Compiler I/O

use std::future::Future;

use crate::token::Span;

/// Log level for filtering diagnostics and log messages
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum LogLevel {
    /// Show all messages including debug
    Debug,
    /// Show info, warnings, and errors
    #[default]
    Info,
    /// Show only warnings and errors
    Warn,
    /// Show only errors
    Error,
    /// Show nothing
    Off,
}

/// Severity level for diagnostics
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Fatal error (immediately stops compilation, e.g., too many errors)
    Fatal,
    /// Compilation error (prevents successful compilation)
    Error,
    /// Warning (compilation continues but may indicate issues)
    Warning,
    /// Informational message
    Info,
    /// Debug message
    Debug,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Severity::Fatal => write!(f, "fatal"),
            Severity::Error => write!(f, "error"),
            Severity::Warning => write!(f, "warning"),
            Severity::Info => write!(f, "info"),
            Severity::Debug => write!(f, "debug"),
        }
    }
}

/// Diagnostic code for categorizing messages
///
/// Named codes without payloads for clear categorization.
/// The actual details go in the `Diagnostic::message` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
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
    /// Variable used before it was definitely initialized
    UninitializedVariable,

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

    // Coherence errors
    /// Orphan rule violation
    OrphanRule,

    // Codegen errors
    /// Code generation failed
    CodegenError,
    /// Unsupported feature
    UnsupportedFeature,

    // Span tracking codes (for logging/profiling)
    /// Start of a span (phase, operation, etc.)
    SpanStart,
    /// End of a span (phase, operation, etc.)
    SpanEnd,

    // General logging
    /// Generic log message (info, debug, etc.)
    Log,

    // Kiln errors
    /// A generator's `Options` struct uses a shape not supported by Kiln.
    GeneratorOptionsUnsupported,
    /// A generator invocation's options value failed typed validation.
    GeneratorOptionsInvalid,
    /// Generator cache is stale and host cannot re-run generators (consume-only mode).
    KilnStaleCache,
    /// A Kiln generator package imports an interface the sandbox forbids.
    KilnGeneratorForbiddenImport,
    /// A `use ... from "<path>"` whose source is a non-`.wado` schema is missing
    /// the required `with { generator: { ... } }` clause.
    KilnMissingWith,
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Code::InvalidCharacter => "INVALID_CHARACTER",
            Code::UnterminatedString => "UNTERMINATED_STRING",
            Code::InvalidEscape => "INVALID_ESCAPE",
            Code::UnexpectedToken => "UNEXPECTED_TOKEN",
            Code::ExpectedToken => "EXPECTED_TOKEN",
            Code::InvalidSyntax => "INVALID_SYNTAX",
            Code::UndefinedVariable => "UNDEFINED_VARIABLE",
            Code::DuplicateDefinition => "DUPLICATE_DEFINITION",
            Code::ImmutableAssignment => "IMMUTABLE_ASSIGNMENT",
            Code::UninitializedVariable => "UNINITIALIZED_VARIABLE",
            Code::TypeMismatch => "TYPE_MISMATCH",
            Code::UnknownType => "UNKNOWN_TYPE",
            Code::InvalidCast => "INVALID_CAST",
            Code::ModuleNotFound => "MODULE_NOT_FOUND",
            Code::CircularDependency => "CIRCULAR_DEPENDENCY",
            Code::ModuleParseError => "MODULE_PARSE_ERROR",
            Code::FileReadError => "FILE_READ_ERROR",
            Code::NetworkError => "NETWORK_ERROR",
            Code::OrphanRule => "ORPHAN_RULE",
            Code::CodegenError => "CODEGEN_ERROR",
            Code::UnsupportedFeature => "UNSUPPORTED_FEATURE",
            Code::SpanStart => "SPAN_START",
            Code::SpanEnd => "SPAN_END",
            Code::Log => "LOG",
            Code::GeneratorOptionsUnsupported => "GENERATOR_OPTIONS_UNSUPPORTED",
            Code::GeneratorOptionsInvalid => "GENERATOR_OPTIONS_INVALID",
            Code::KilnStaleCache => "KILN_STALE_CACHE",
            Code::KilnGeneratorForbiddenImport => "KILN_GENERATOR_FORBIDDEN_IMPORT",
            Code::KilnMissingWith => "KILN_MISSING_WITH",
        };
        write!(f, "{name}")
    }
}

/// A compiler diagnostic (error, warning, etc.)
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Severity level
    pub severity: Severity,
    /// Code categorizing the diagnostic
    pub code: Code,
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
    /// Create a `DiagnosticSpan` from a Span and optional filename
    pub fn from_span(span: &Span, filename: Option<&str>) -> Self {
        DiagnosticSpan {
            file: filename.unwrap_or_default().to_string(),
            line: span.line,
            column: span.column,
            end_line: Some(span.end_line),
            end_column: Some(span.end_column),
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

/// Minimal trait for emitting span start/end events during optimization.
///
/// This enables optimization passes to emit profiling spans without depending
/// on the full `Logger` type. The CLI host adds timestamps to measure duration.
pub trait SpanEmitter {
    fn span_start(&self, name: &str);
    fn span_end(&self, name: &str);
    fn debug(&self, message: &str);
}

/// No-op implementation for when profiling is not needed.
pub struct NullSpanEmitter;

impl SpanEmitter for NullSpanEmitter {
    fn span_start(&self, _name: &str) {}
    fn span_end(&self, _name: &str) {}
    fn debug(&self, _message: &str) {}
}

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
///     fn emit_diagnostic(&self, diagnostic: Diagnostic) {
///         // Handle diagnostic (print, collect, send to UI, etc.)
///     }
/// }
/// ```
pub trait CompilerHost: Send + Sync {
    /// Load a file as raw bytes.
    ///
    /// This is the single I/O primitive for the compiler. Both source files
    /// (`.wado`) and binary assets (`#include_bytes`) are loaded through this
    /// method. The compiler interprets the bytes as UTF-8 where appropriate.
    ///
    /// # Arguments
    /// * `path` - File path, which can be:
    ///   - Local path (e.g., "./lib.wado", "../utils.wado")
    ///   - Remote URL (e.g., "<https://example.com/lib.wado>", "<http://example.com/lib.wado>")
    ///
    /// NOTE: Standard library paths (core:*, wasi:*) are NOT passed to this method.
    /// They are handled directly by the compiler via embedded sources.
    ///
    /// # Returns
    /// The raw bytes of the file
    fn load_source(&self, path: &str) -> impl Future<Output = Result<Vec<u8>, SourceError>> + Send;

    /// Emit a diagnostic (error, warning, etc.)
    ///
    /// This method is called synchronously by the compiler whenever a diagnostic
    /// needs to be reported. Implementations can print to stderr, collect into
    /// a list, send to an LSP client, etc.
    fn emit_diagnostic(&self, diagnostic: Diagnostic);

    /// Execute a Kiln generator component and return its response.
    ///
    /// `component_wasm` is the compiled generator component. The host is
    /// responsible for instantiating it, linking `core:kiln/host` so that
    /// `read-file` and `emit-diagnostic` forward back into this same host,
    /// and returning the generator's response.
    ///
    /// The default implementation returns `Unsupported`, which drives
    /// "consume-only" mode (the compiler reuses the most recent cached
    /// outputs on disk and warns on drift). Hosts that have a Wasm runtime
    /// available (e.g. `wado-cli` via wasmtime) override this method.
    ///
    /// See WEP 2026-04-12 (Kiln) for the full protocol.
    fn run_generator(
        &self,
        _component_wasm: &[u8],
        _request: GeneratorRequest,
    ) -> impl Future<Output = Result<GeneratorResponse, GeneratorRunnerError>> + Send {
        async move { Err(GeneratorRunnerError::Unsupported) }
    }
}

/// Request handed to a Kiln generator by the compiler.
///
/// Mirrors `core:kiln/types::raw-request` 1:1 but does not depend on
/// wasmtime; hosts lift/lower at their own boundary.
#[derive(Debug, Clone)]
pub struct GeneratorRequest {
    /// The schema file named by `from` in the invocation.
    pub primary: GeneratorInputFile,
    /// Supplementary schema files.
    pub inputs: Vec<GeneratorInputFile>,
    /// Canonical JSON encoding of the user's `options = { ... }`.
    /// The generator-side compiler-synthesized adapter decodes this
    /// into the typed `Options` struct before user code runs.
    pub options: String,
}

/// One schema file passed to a Kiln generator.
#[derive(Debug, Clone)]
pub struct GeneratorInputFile {
    pub path: String,
    /// UTF-8 content of the file. Kiln schemas are text (proto,
    /// graphql, g4, wit, ...), so string is the natural wire form.
    pub content: String,
}

/// Response returned by a Kiln generator.
#[derive(Debug, Clone)]
pub struct GeneratorResponse {
    /// Generated Wado source files.
    pub files: Vec<GeneratorOutputFile>,
    /// Every `host::read-file` call the generator made during this run, in
    /// call order. The compiler feeds this list into the cache key so that
    /// transitive schema reads invalidate the cache when they change.
    pub reads: Vec<GeneratorReadRecord>,
}

/// One file produced by a Kiln generator.
#[derive(Debug, Clone)]
pub struct GeneratorOutputFile {
    /// Path relative to the invocation's output directory.
    pub path: String,
    /// UTF-8 source contents.
    pub content: String,
    /// Whether this file is the invocation's entry module.
    pub is_entry: bool,
}

/// A single recorded `host::read-file` call.
#[derive(Debug, Clone)]
pub struct GeneratorReadRecord {
    pub path: String,
    /// SHA-256 of the file contents, as raw bytes.
    pub content_hash: [u8; 32],
}

/// Generator-side error, mirroring `core:kiln/types::error`.
#[derive(Debug, Clone)]
pub enum GeneratorError {
    /// The generator rejected the schema it was given.
    InvalidSchema(String),
    /// The schema used a feature the generator does not (yet) handle.
    Unsupported(String),
    /// Catch-all generator-produced error.
    Other(String),
}

impl std::fmt::Display for GeneratorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeneratorError::InvalidSchema(msg) => write!(f, "invalid schema: {msg}"),
            GeneratorError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            GeneratorError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for GeneratorError {}

/// Outcome of calling `CompilerHost::run_generator`.
#[derive(Debug, Clone)]
pub enum GeneratorRunnerError {
    /// The host has no Wasm runtime available — trigger consume-only mode.
    Unsupported,
    /// The generator returned a typed error.
    Generator(GeneratorError),
    /// The host failed (instantiation, import linking, fuel, timeout, …).
    Host(String),
}

impl std::fmt::Display for GeneratorRunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeneratorRunnerError::Unsupported => {
                write!(f, "generator execution is not supported by this host")
            }
            GeneratorRunnerError::Generator(e) => write!(f, "generator error: {e}"),
            GeneratorRunnerError::Host(msg) => write!(f, "generator host error: {msg}"),
        }
    }
}

impl std::error::Error for GeneratorRunnerError {}

/// Severity mirrored from `core:kiln/host::diagnostic-level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeneratorDiagnosticLevel {
    Error,
    Warning,
    Info,
    Hint,
}

/// Span into a file known to the generator. Paths are relayed back to the
/// compiler's diagnostic renderer, which re-reads the file via `load_source`.
#[derive(Debug, Clone)]
pub struct GeneratorSourceSpan {
    pub path: String,
    pub byte_start: u32,
    pub byte_end: u32,
}

/// A diagnostic produced by a generator via `host::emit-diagnostic`.
#[derive(Debug, Clone)]
pub struct GeneratorDiagnostic {
    pub level: GeneratorDiagnosticLevel,
    pub span: Option<GeneratorSourceSpan>,
    pub message: String,
}

/// The Kiln WIT world, embedded so the compiler can treat its byte identity
/// as part of every cache key. The single source of truth; `wado-cli` reads
/// the same file via `wasmtime::component::bindgen!(path = "...")`.
pub const KILN_GENERATOR_WIT: &str = include_str!("../lib/core/kiln/generator.wit");

/// A simple in-memory compiler host for testing
///
/// This host stores sources in an `IndexMap` and collects diagnostics in a Vec.
#[derive(Debug, Default)]
pub struct InMemoryCompilerHost {
    /// Source files by path (stored as raw bytes)
    sources: crate::hashmap::IndexMap<String, Vec<u8>>,
    /// Collected diagnostics
    diagnostics: std::sync::Mutex<Vec<Diagnostic>>,
}

impl InMemoryCompilerHost {
    /// Create a new empty in-memory host
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a source file (text)
    pub fn add_source(&mut self, path: impl Into<String>, source: impl Into<String>) {
        self.sources.insert(path.into(), source.into().into_bytes());
    }

    /// Add a binary file
    pub fn add_bytes(&mut self, path: impl Into<String>, bytes: Vec<u8>) {
        self.sources.insert(path.into(), bytes);
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
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        self.sources
            .get(path)
            .cloned()
            .ok_or_else(|| SourceError::NotFound {
                path: path.to_string(),
            })
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_memory_host() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut host = InMemoryCompilerHost::new();
                host.add_source("./test.wado", "fn run() {}");

                // Test load_source returns bytes
                let result = host.load_source("./test.wado").await;
                assert!(result.is_ok());
                assert_eq!(result.unwrap(), b"fn run() {}");

                // Test not found
                let result = host.load_source("./missing.wado").await;
                assert!(matches!(result, Err(SourceError::NotFound { .. })));
            });
    }

    #[test]
    fn default_run_generator_is_unsupported() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let host = InMemoryCompilerHost::new();
                let req = GeneratorRequest {
                    primary: GeneratorInputFile {
                        path: "schema.proto".to_string(),
                        content: "syntax = \"proto3\";".to_string(),
                    },
                    inputs: vec![],
                    options: String::new(),
                };
                let result = host.run_generator(b"\0asm", req).await;
                assert!(matches!(result, Err(GeneratorRunnerError::Unsupported)));
            });
    }

    #[test]
    fn kiln_generator_wit_is_embedded() {
        assert!(KILN_GENERATOR_WIT.contains("package core:kiln"));
        assert!(KILN_GENERATOR_WIT.contains("world generator"));
        assert!(KILN_GENERATOR_WIT.contains("export generate"));
    }

    #[test]
    fn test_diagnostic_display() {
        let diag = Diagnostic {
            severity: Severity::Error,
            code: Code::UnexpectedToken,
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
