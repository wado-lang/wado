//! Logger module for structured compiler diagnostics
//!
//! Provides a `Logger` wrapper around `CompilerHost` that serves as the single
//! error reporting channel for the compilation pipeline. All compilation errors
//! are emitted immediately to the `CompilerHost` via the logger, enabling:
//!
//! - Real-time error reporting (for IDE/LSP)
//! - Phase timing via host-side timestamps (compiler stays syscall-free)
//! - Unified error counting with automatic bail at `MAX_ERRORS`
//!
//! # Architecture
//!
//! ```text
//! Phase (Analyzer, Elaborator, ...) → Logger → CompilerHost → CLI/IDE/LSP
//!                                     ↑
//!                              error counting + bail
//! ```
//!
//! Each phase calls `logger.error_in(module, typed_error)?` which:
//! 1. Converts the typed error to a `Diagnostic` via `Into<Diagnostic>`
//! 2. Stamps the module's file onto the span when it carries none
//! 3. Emits the diagnostic to `CompilerHost::emit_diagnostic()`
//! 4. Increments the error count
//! 5. Returns `Err(Bail)` if the error limit is reached

use std::cell::Cell;

use crate::compiler_host::{
    Code, CompilerHost, Diagnostic, DiagnosticSpan, LogLevel, Severity, SpanEmitter,
};
use crate::module_source::ModuleSource;

/// Maximum number of errors before compilation is aborted
pub const MAX_ERRORS: usize = 100;

/// Signal that compilation has been aborted (too many errors or fatal error).
///
/// Propagated via `Result<T, Bail>` and the `?` operator through internal
/// compiler methods. Caught at phase boundaries in the pipeline.
///
/// `Bail` carries no error information — all error details have already been
/// emitted to the `CompilerHost` via the `Logger` before `Bail` is returned.
#[derive(Debug, Clone, Copy)]
pub struct Bail;

impl std::fmt::Display for Bail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compilation failed")
    }
}

impl std::error::Error for Bail {}

/// Logger for the compilation pipeline
///
/// Wraps a `CompilerHost` and provides the unified error reporting channel.
/// All compilation errors and informational messages go through this logger.
///
/// # Error Counting
///
/// The logger tracks how many errors have been emitted. When the count reaches
/// `MAX_ERRORS`, `error()` returns `Err(Bail)` to stop compilation.
///
/// # File Attribution
///
/// A diagnostic's file is stamped from the module the caller holds
/// (`error_in` / `error_at`), never from ambient logger state — so no phase
/// can forget to set it. `error(err)` is for diagnostics that already carry
/// their own location.
///
/// # Example
///
/// ```ignore
/// let logger = Logger::new(&host, LogLevel::Debug);
///
/// // Report compilation errors attributed to a module (counted, may bail)
/// logger.error_in(&module_source, TypeError::TypeMismatch { expected, found, span })?;
///
/// // Fatal errors always bail
/// logger.fatal(TypeError::TypeMismatch { ... })?; // always Err(Bail)
///
/// // Informational logging (fire-and-forget, filtered by level)
/// logger.debug("entering function");
/// logger.info("processing 10 items");
///
/// // Track phase timing with RAII guard
/// {
///     let _span = logger.span("parse");
///     // ... parsing happens here ...
/// } // SpanEnd is emitted when _span is dropped
/// ```
pub struct Logger<'a, H: CompilerHost> {
    host: &'a H,
    level: LogLevel,
    error_count: Cell<usize>,
}

impl<'a, H: CompilerHost> Logger<'a, H> {
    /// Create a new logger with the given host and log level
    pub fn new(host: &'a H, level: LogLevel) -> Self {
        Self {
            host,
            level,
            error_count: Cell::new(0),
        }
    }

    /// Access the underlying `CompilerHost` for callers that need to
    /// relay pre-built diagnostics without contributing to the error count
    /// (e.g. soft-warn paths like the kiln `Options` descriptor extractor).
    pub fn host(&self) -> &'a H {
        self.host
    }

    // === Compilation error methods (counted, may bail) ===

    /// Count the diagnostic, emit it, and signal `Bail` at `MAX_ERRORS`.
    fn emit_error(&self, diag: Diagnostic) -> Result<(), Bail> {
        let count = self.error_count.get() + 1;
        self.error_count.set(count);
        self.host.emit_diagnostic(diag);
        if count >= MAX_ERRORS {
            Err(Bail)
        } else {
            Ok(())
        }
    }

    /// Stamp `file` onto a diagnostic's span when the span carries no file of
    /// its own, so a caller that knows its module wins over a bare `Span`.
    fn with_file(mut diag: Diagnostic, file: &str) -> Diagnostic {
        if let Some(span) = diag.span.as_mut()
            && span.file.is_empty()
        {
            span.file = file.to_string();
        }
        diag
    }

    /// Report a compilation error attributed to `module`'s source file.
    pub fn error_in(&self, module: &ModuleSource, err: impl Into<Diagnostic>) -> Result<(), Bail> {
        self.error_at(&module.source_path(), err)
    }

    /// Like [`Self::error_in`], for callers that hold a display filename
    /// rather than a [`ModuleSource`].
    pub fn error_at(&self, file: &str, err: impl Into<Diagnostic>) -> Result<(), Bail> {
        self.emit_error(Self::with_file(err.into(), file))
    }

    /// Report an error whose diagnostic already carries its own location
    /// (parser/lexer errors) or has none.
    pub fn error(&self, err: impl Into<Diagnostic>) -> Result<(), Bail> {
        self.emit_error(err.into())
    }

    /// Report a fatal compilation error. Always returns `Err(Bail)`.
    ///
    /// Use for errors that make it impossible to continue the current phase.
    pub fn fatal(&self, err: impl Into<Diagnostic>) -> Result<(), Bail> {
        self.error_count.set(self.error_count.get() + 1);
        self.host.emit_diagnostic(err.into());
        Err(Bail)
    }

    /// Report multiple errors from an external source.
    /// Returns `Err(Bail)` if the error limit is reached.
    pub fn extend(
        &self,
        errors: impl IntoIterator<Item = impl Into<Diagnostic>>,
    ) -> Result<(), Bail> {
        for err in errors {
            self.error(err)?;
        }
        Ok(())
    }

    /// Check if any errors have been reported
    pub fn has_errors(&self) -> bool {
        self.error_count.get() > 0
    }

    /// Get the number of errors reported
    pub fn error_count(&self) -> usize {
        self.error_count.get()
    }

    /// Return `Ok(value)` if no errors have been reported, `Err(Bail)` otherwise.
    pub fn ok_or_bail<T>(&self, value: T) -> Result<T, Bail> {
        if self.has_errors() {
            Err(Bail)
        } else {
            Ok(value)
        }
    }

    // === Informational logging (fire-and-forget, filtered by level) ===

    /// Whether a diagnostic of `severity` would be emitted at the current level.
    /// Lets callers skip building a diagnostic that would be filtered out.
    pub fn would_log(&self, severity: Severity) -> bool {
        self.should_log(severity)
    }

    /// Check if the given severity should be logged at the current level
    fn should_log(&self, severity: Severity) -> bool {
        match self.level {
            // Fatal is always logged regardless of level
            LogLevel::Off => severity == Severity::Fatal,
            LogLevel::Error => matches!(severity, Severity::Fatal | Severity::Error),
            LogLevel::Warn => {
                matches!(
                    severity,
                    Severity::Fatal | Severity::Error | Severity::Warning
                )
            }
            LogLevel::Info => {
                matches!(
                    severity,
                    Severity::Fatal | Severity::Error | Severity::Warning | Severity::Info
                )
            }
            LogLevel::Debug => true,
        }
    }

    /// Log a warning (informational, not counted as error)
    pub fn warn(&self, code: Code, message: impl Into<String>) {
        if self.should_log(Severity::Warning) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Warning,
                code,
                message: message.into(),
                span: None,
            });
        }
    }

    /// Log a warning carrying a source span.
    ///
    /// Used by span-bearing lints (unused diagnostics). The caller builds the
    /// `DiagnosticSpan` with its own file (`DiagnosticSpan::from_span(span,
    /// Some(file))`); no file is stamped here.
    pub fn warn_at(&self, code: Code, message: impl Into<String>, span: DiagnosticSpan) {
        if self.should_log(Severity::Warning) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Warning,
                code,
                message: message.into(),
                span: Some(span),
            });
        }
    }

    /// Log an info message
    pub fn info(&self, message: impl Into<String>) {
        if self.should_log(Severity::Info) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Info,
                code: Code::Log,
                message: message.into(),
                span: None,
            });
        }
    }

    /// Emit an optimizer remark at info level, carrying a source span.
    ///
    /// Remarks report residual costs that survived optimization (see WEP
    /// `wep-2026-06-03-optimizer-remarks.md`). They are gated at `Info`
    /// severity; since the CLI is quiet by default (`warn`), seeing them takes
    /// `--log-level info`. The span lets the source location be rendered, and
    /// the `remark:` prefix is added here so call sites pass only the bare
    /// message.
    pub fn remark(&self, message: impl Into<String>, span: DiagnosticSpan) {
        if self.should_log(Severity::Info) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Info,
                code: Code::Remark,
                message: format!("remark: {}", message.into()),
                span: Some(span),
            });
        }
    }

    /// Log a debug message
    pub fn debug(&self, message: impl Into<String>) {
        if self.should_log(Severity::Debug) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Debug,
                code: Code::Log,
                message: message.into(),
                span: None,
            });
        }
    }

    // === Span tracking ===

    /// Start a span for tracking
    ///
    /// Returns a `SpanGuard` that emits `SpanEnd` when dropped.
    /// The CLI or host implementation can add timestamps to track timing.
    pub fn span(&self, name: &str) -> SpanGuard<'_, 'a, H> {
        self.span_start(name);

        SpanGuard {
            logger: self,
            name: name.to_string(),
        }
    }

    /// Emit a span start marker
    ///
    /// Use this when RAII pattern (`span()`) doesn't work due to borrow conflicts.
    /// Must be paired with a corresponding `span_end()` call.
    pub fn span_start(&self, name: &str) {
        if self.should_log(Severity::Debug) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Debug,
                code: Code::SpanStart,
                message: name.to_string(),
                span: None,
            });
        }
    }

    /// Emit a span end marker
    ///
    /// Use this when RAII pattern (`span()`) doesn't work due to borrow conflicts.
    /// Must be paired with a corresponding `span_start()` call.
    pub fn span_end(&self, name: &str) {
        if self.should_log(Severity::Debug) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Debug,
                code: Code::SpanEnd,
                message: name.to_string(),
                span: None,
            });
        }
    }
}

/// RAII guard for span tracking
///
/// When dropped, emits a `SpanEnd` diagnostic with the span name.
/// The CLI or host implementation can use timestamps to measure duration.
pub struct SpanGuard<'l, 'a, H: CompilerHost> {
    logger: &'l Logger<'a, H>,
    name: String,
}

impl<H: CompilerHost> Drop for SpanGuard<'_, '_, H> {
    fn drop(&mut self) {
        self.logger.span_end(&self.name);
    }
}

impl<H: CompilerHost> SpanEmitter for Logger<'_, H> {
    fn span_start(&self, name: &str) {
        Logger::span_start(self, name);
    }
    fn span_end(&self, name: &str) {
        Logger::span_end(self, name);
    }
    fn debug(&self, message: &str) {
        Logger::debug(self, message);
    }
}

/// A [`Logger`] bound to one module, so code threading a single `logger`
/// value (recursive validators) need not also thread a [`ModuleSource`].
/// Obtain one with [`Logger::in_module`].
pub struct ModuleDiag<'l, 'a, H: CompilerHost> {
    logger: &'l Logger<'a, H>,
    module: &'l ModuleSource,
}

impl<H: CompilerHost> ModuleDiag<'_, '_, H> {
    /// Report a compilation error attributed to the bound module's file.
    pub fn error(&self, err: impl Into<Diagnostic>) -> Result<(), Bail> {
        self.logger.error_in(self.module, err)
    }
}

impl<'a, H: CompilerHost> Logger<'a, H> {
    /// Bind this logger to `module`, so diagnostics emitted through the
    /// returned [`ModuleDiag`] are attributed to `module`'s file.
    pub fn in_module<'l>(&'l self, module: &'l ModuleSource) -> ModuleDiag<'l, 'a, H> {
        ModuleDiag {
            logger: self,
            module,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler_host::InMemoryCompilerHost;

    #[test]
    fn test_logger_levels() {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Warn);

        // Debug and info should be filtered out
        logger.debug("debug message");
        logger.info("info message");
        logger.warn(Code::UnsupportedFeature, "warning message");
        // Use a direct Diagnostic for error (not a typed error)
        let _ = logger.error(Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: "error message".to_string(),
            span: None,
        });

        let diags = host.diagnostics();
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].severity, Severity::Warning);
        assert_eq!(diags[1].severity, Severity::Error);
    }

    #[test]
    fn test_span_guard() {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Debug);

        {
            let _span = logger.span("test_phase");
            // Span is active
        }
        // Span should be closed

        let diags = host.diagnostics();
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].code, Code::SpanStart);
        assert_eq!(diags[0].message, "test_phase");
        assert_eq!(diags[1].code, Code::SpanEnd);
        assert_eq!(diags[1].message, "test_phase");
    }

    #[test]
    fn test_log_level_off() {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Off);

        // Warnings and info/debug are filtered
        logger.warn(Code::UnsupportedFeature, "warning");
        logger.info("info");
        logger.debug("debug");

        let diags = host.diagnostics();
        assert!(diags.is_empty());
    }

    #[test]
    fn test_error_counting_and_bail() {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Error);

        // Push MAX_ERRORS - 1 errors (should all succeed)
        for i in 0..MAX_ERRORS - 1 {
            let result = logger.error(Diagnostic {
                severity: Severity::Error,
                code: Code::TypeMismatch,
                message: format!("error {i}"),
                span: None,
            });
            assert!(result.is_ok());
        }

        // The MAX_ERRORS-th error should trigger Bail
        let result = logger.error(Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: "final error".to_string(),
            span: None,
        });
        assert!(result.is_err());
        assert_eq!(logger.error_count(), MAX_ERRORS);
    }

    #[test]
    fn test_fatal_always_bails() {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Error);

        let result = logger.fatal(Diagnostic {
            severity: Severity::Fatal,
            code: Code::TypeMismatch,
            message: "fatal error".to_string(),
            span: None,
        });
        assert!(result.is_err());
        assert_eq!(logger.error_count(), 1);
    }

    #[test]
    fn test_ok_or_bail() {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Error);

        // No errors → Ok
        assert!(logger.ok_or_bail(42).is_ok());

        // With errors → Err
        let _ = logger.error(Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: "error".to_string(),
            span: None,
        });
        assert!(logger.ok_or_bail(42).is_err());
    }

    #[test]
    fn test_error_at_stamps_empty_span_file() {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Error);

        let _ = logger.error_at(
            "test.wado",
            Diagnostic {
                severity: Severity::Error,
                code: Code::TypeMismatch,
                message: "error".to_string(),
                span: Some(crate::compiler_host::DiagnosticSpan {
                    file: String::new(),
                    line: 10,
                    column: 5,
                    end_line: None,
                    end_column: None,
                }),
            },
        );

        let diags = host.diagnostics();
        assert_eq!(diags[0].span.as_ref().unwrap().file, "test.wado");
    }

    #[test]
    fn test_error_at_does_not_override_existing_file() {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Error);

        let _ = logger.error_at(
            "outer.wado",
            Diagnostic {
                severity: Severity::Error,
                code: Code::TypeMismatch,
                message: "error".to_string(),
                span: Some(crate::compiler_host::DiagnosticSpan {
                    file: "inner.wado".to_string(),
                    line: 10,
                    column: 5,
                    end_line: None,
                    end_column: None,
                }),
            },
        );

        let diags = host.diagnostics();
        assert_eq!(diags[0].span.as_ref().unwrap().file, "inner.wado");
    }

    #[test]
    fn test_fatal_always_logged_even_at_off() {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Off);

        let _ = logger.fatal(Diagnostic {
            severity: Severity::Fatal,
            code: Code::TypeMismatch,
            message: "fatal error".to_string(),
            span: None,
        });

        let diags = host.diagnostics();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Fatal);
    }
}
