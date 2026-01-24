//! Logger module for structured compiler logging
//!
//! Provides a `Logger` wrapper around `CompilerHost` for convenient logging
//! with severity levels and span tracking.
//!
//! Note: Time tracking is intentionally omitted from the compiler to keep it
//! syscall-free. The CLI or host implementation should add timestamps if needed.

use crate::compiler_host::{Code, CompilerHost, Diagnostic, LogLevel, Severity};

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
            LogLevel::Off => false,
            LogLevel::Error => severity == Severity::Error,
            LogLevel::Warn => matches!(severity, Severity::Error | Severity::Warning),
            LogLevel::Info => {
                matches!(severity, Severity::Error | Severity::Warning | Severity::Info)
            }
            LogLevel::Debug => true,
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
        if self.should_log(Severity::Hint) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Hint, // Use Hint for debug level
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
        // Emit SpanStart
        self.host.emit_diagnostic(Diagnostic {
            severity: Severity::Info,
            code: Code::SpanStart,
            message: name.to_string(),
            span: None,
        });

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
            severity: Severity::Info,
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
            severity: Severity::Info,
            code: Code::SpanEnd,
            message: name.to_string(),
            span: None,
        });
    }

    /// Get a reference to the underlying host
    pub fn host(&self) -> &'a H {
        self.host
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
        self.logger.host.emit_diagnostic(Diagnostic {
            severity: Severity::Info,
            code: Code::SpanEnd,
            message: self.name.clone(),
            span: None,
        });
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
}
