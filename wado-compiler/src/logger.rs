//! Logger module for structured compiler logging
//!
//! Provides a `Logger` wrapper around `CompilerHost` for convenient logging
//! with severity levels and span tracking.
//!
//! Also provides `ErrorLog<E>` for collecting typed compilation errors with
//! a count limit. When errors exceed `MAX_ERRORS`, compilation is aborted
//! via the `Bail` signal type.
//!
//! Note: Time tracking is intentionally omitted from the compiler to keep it
//! syscall-free. The CLI or host implementation should add timestamps if needed.

use crate::compiler_host::{Code, CompilerHost, Diagnostic, LogLevel, Severity};

/// Maximum number of errors before compilation is aborted
pub const MAX_ERRORS: usize = 100;

/// Signal that compilation has been aborted (too many errors or fatal error).
///
/// Propagated via `Result<T, Bail>` and the `?` operator through internal
/// compiler methods. Caught at phase boundaries and converted to the
/// appropriate error return type.
#[derive(Debug, Clone, Copy)]
pub struct Bail;

/// Error log that collects typed compilation errors with a count limit.
///
/// Replaces `Vec<E>` for error accumulation in compiler phases.
/// Tracks the error count and signals `Bail` when the limit is reached.
///
/// # Example
///
/// ```ignore
/// let mut errors = ErrorLog::new();
///
/// // Log errors - returns Ok(()) normally, Err(Bail) at limit
/// errors.error(TypeError::TypeMismatch { ... })?;
///
/// // Fatal errors always bail immediately
/// errors.fatal(TypeError::TypeMismatch { ... })?; // always Err(Bail)
///
/// // Check results
/// let tir_module = errors.into_result(tir_module)?;
/// ```
pub struct ErrorLog<E> {
    errors: Vec<E>,
}

impl<E> ErrorLog<E> {
    /// Create a new empty error log
    pub fn new() -> Self {
        Self { errors: Vec::new() }
    }

    /// Log an error. Returns `Err(Bail)` if the error count reaches `MAX_ERRORS`.
    pub fn error(&mut self, err: E) -> Result<(), Bail> {
        self.errors.push(err);
        if self.errors.len() >= MAX_ERRORS {
            Err(Bail)
        } else {
            Ok(())
        }
    }

    /// Log a fatal error that immediately stops compilation.
    /// Always returns `Err(Bail)`.
    pub fn fatal(&mut self, err: E) -> Result<(), Bail> {
        self.errors.push(err);
        Err(Bail)
    }

    /// Check if any errors have been logged
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Get the number of errors logged
    pub fn error_count(&self) -> usize {
        self.errors.len()
    }

    /// Take all collected errors, leaving the log empty
    pub fn take(&mut self) -> Vec<E> {
        std::mem::take(&mut self.errors)
    }

    /// Extend with errors from another source. Returns `Err(Bail)` if the limit is reached.
    pub fn extend(&mut self, errors: Vec<E>) -> Result<(), Bail> {
        for err in errors {
            self.error(err)?;
        }
        Ok(())
    }

    /// Return `Ok(value)` if no errors, `Err(errors)` otherwise.
    pub fn into_result<T>(&mut self, value: T) -> Result<T, Vec<E>> {
        if self.errors.is_empty() {
            Ok(value)
        } else {
            Err(self.take())
        }
    }

    /// Return `Ok(())` if no errors, `Err(errors)` otherwise.
    pub fn finish(&mut self) -> Result<(), Vec<E>> {
        self.into_result(())
    }
}

impl<E> Default for ErrorLog<E> {
    fn default() -> Self {
        Self::new()
    }
}

/// Logger for compiler phases
///
/// Wraps a `CompilerHost` and provides convenience methods for logging
/// at different severity levels. Also supports phase span tracking with
/// RAII guards.
///
/// # Example
///
/// ```ignore
/// let logger = Logger::new(&host, LogLevel::Debug);
///
/// // Log messages at different levels
/// logger.debug("entering function");
/// logger.info("processing 10 items");
/// logger.warn("deprecated feature used");
/// logger.error(Code::TypeMismatch, "expected i32, found String", None);
/// logger.fatal(Code::TypeMismatch, "too many errors");
///
/// // Track phase timing with RAII guard
/// {
///     let _span = logger.span("parse");
///     // ... parsing happens here ...
/// } // PhaseEnd is emitted when _span is dropped
/// ```
pub struct Logger<'a, H: CompilerHost> {
    host: &'a H,
    level: LogLevel,
}

impl<'a, H: CompilerHost> Logger<'a, H> {
    /// Create a new logger with the given host and log level
    pub fn new(host: &'a H, level: LogLevel) -> Self {
        Self { host, level }
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

    /// Log a fatal error with a diagnostic code.
    /// Fatal errors indicate unrecoverable conditions (e.g., too many errors).
    pub fn fatal(&self, code: Code, message: impl Into<String>) {
        if self.should_log(Severity::Fatal) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Fatal,
                code,
                message: message.into(),
                span: None,
            });
        }
    }

    /// Log an error with a diagnostic code
    pub fn error(&self, code: Code, message: impl Into<String>) {
        if self.should_log(Severity::Error) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Error,
                code,
                message: message.into(),
                span: None,
            });
        }
    }

    /// Log a warning with a diagnostic code
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

    /// Log a debug message
    pub fn debug(&self, message: impl Into<String>) {
        if self.should_log(Severity::Debug) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Debug, // Use Debug for debug level
                code: Code::Log,
                message: message.into(),
                span: None,
            });
        }
    }

    /// Start a span for tracking
    ///
    /// Returns a `SpanGuard` that emits `SpanEnd` when dropped.
    /// The CLI or host implementation can add timestamps to track timing.
    ///
    /// # Example
    ///
    /// ```ignore
    /// {
    ///     let _span = logger.span("parse");
    ///     // ... parsing ...
    /// } // SpanEnd emitted here
    /// ```
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
        self.host.emit_diagnostic(Diagnostic {
            severity: Severity::Debug,
            code: Code::SpanStart,
            message: name.to_string(),
            span: None,
        });
    }

    /// Emit a span end marker
    ///
    /// Use this when RAII pattern (`span()`) doesn't work due to borrow conflicts.
    /// Must be paired with a corresponding `span_start()` call.
    pub fn span_end(&self, name: &str) {
        self.host.emit_diagnostic(Diagnostic {
            severity: Severity::Debug,
            code: Code::SpanEnd,
            message: name.to_string(),
            span: None,
        });
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
        logger.error(Code::TypeMismatch, "error message");

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

        logger.error(Code::TypeMismatch, "error");
        logger.warn(Code::UnsupportedFeature, "warning");
        logger.info("info");
        logger.debug("debug");

        let diags = host.diagnostics();
        assert!(diags.is_empty());
    }

    #[test]
    fn test_error_log_basic() {
        let mut log: ErrorLog<String> = ErrorLog::new();
        assert!(!log.has_errors());
        assert_eq!(log.error_count(), 0);

        log.error("first error".to_string()).unwrap();
        assert!(log.has_errors());
        assert_eq!(log.error_count(), 1);

        log.error("second error".to_string()).unwrap();
        assert_eq!(log.error_count(), 2);

        let errors = log.take();
        assert_eq!(errors.len(), 2);
        assert!(!log.has_errors());
    }

    #[test]
    fn test_error_log_bail_at_limit() {
        let mut log: ErrorLog<i32> = ErrorLog::new();

        // Push MAX_ERRORS - 1 errors (should all succeed)
        for i in 0..MAX_ERRORS - 1 {
            assert!(log.error(i as i32).is_ok());
        }

        // The MAX_ERRORS-th error should trigger Bail
        assert!(log.error(99).is_err());
        assert_eq!(log.error_count(), MAX_ERRORS);
    }

    #[test]
    fn test_error_log_fatal() {
        let mut log: ErrorLog<String> = ErrorLog::new();

        // Fatal always returns Err(Bail)
        assert!(log.fatal("fatal error".to_string()).is_err());
        assert_eq!(log.error_count(), 1);
    }

    #[test]
    fn test_error_log_into_result() {
        let mut log: ErrorLog<String> = ErrorLog::new();

        // No errors → Ok
        assert!(log.into_result(42).is_ok());

        // With errors → Err
        log.error("error".to_string()).unwrap();
        let result: Result<i32, Vec<String>> = log.into_result(42);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().len(), 1);
    }

    #[test]
    fn test_error_log_extend() {
        let mut log: ErrorLog<i32> = ErrorLog::new();

        log.extend(vec![1, 2, 3]).unwrap();
        assert_eq!(log.error_count(), 3);
    }

    #[test]
    fn test_fatal_always_logged_even_at_off() {
        let host = InMemoryCompilerHost::new();
        let logger = Logger::new(&host, LogLevel::Off);

        logger.fatal(Code::TypeMismatch, "fatal error");

        let diags = host.diagnostics();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Fatal);
    }
}
