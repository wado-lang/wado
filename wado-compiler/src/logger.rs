//! Logger module for structured compiler logging
//!
//! Provides a `Logger` wrapper around `CompilerHost` for convenient logging
//! with severity levels and phase span tracking.

use std::time::Instant;

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
    ///
    /// Uses `Code::PhaseStart` as a generic info code since info messages
    /// don't typically have specific error codes.
    pub fn info(&self, message: impl Into<String>) {
        if self.should_log(Severity::Info) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Info,
                code: Code::PhaseStart, // Generic code for info
                message: message.into(),
                span: None,
            });
        }
    }

    /// Log a debug message
    ///
    /// Uses `Code::PhaseStart` as a generic debug code since debug messages
    /// don't typically have specific error codes.
    pub fn debug(&self, message: impl Into<String>) {
        if self.should_log(Severity::Hint) {
            self.host.emit_diagnostic(Diagnostic {
                severity: Severity::Hint, // Use Hint for debug level
                code: Code::PhaseStart,   // Generic code for debug
                message: message.into(),
                span: None,
            });
        }
    }

    /// Start a phase span for timing
    ///
    /// Returns a `SpanGuard` that emits `PhaseEnd` when dropped.
    /// The CLI can add timestamps to track compilation phase timing.
    ///
    /// # Example
    ///
    /// ```ignore
    /// {
    ///     let _span = logger.span("parse");
    ///     // ... parsing ...
    /// } // PhaseEnd emitted here
    /// ```
    pub fn span(&self, name: &str) -> SpanGuard<'_, 'a, H> {
        // Emit PhaseStart
        self.host.emit_diagnostic(Diagnostic {
            severity: Severity::Info,
            code: Code::PhaseStart,
            message: name.to_string(),
            span: None,
        });

        SpanGuard {
            logger: self,
            name: name.to_string(),
            start: Instant::now(),
        }
    }

    /// Get a reference to the underlying host
    pub fn host(&self) -> &'a H {
        self.host
    }
}

/// RAII guard for phase span tracking
///
/// When dropped, emits a `PhaseEnd` diagnostic with the phase name.
/// The CLI can use timestamps to measure phase duration.
pub struct SpanGuard<'l, 'a, H: CompilerHost> {
    logger: &'l Logger<'a, H>,
    name: String,
    #[allow(dead_code)]
    start: Instant,
}

impl<H: CompilerHost> Drop for SpanGuard<'_, '_, H> {
    fn drop(&mut self) {
        self.logger.host.emit_diagnostic(Diagnostic {
            severity: Severity::Info,
            code: Code::PhaseEnd,
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
        assert_eq!(diags[0].code, Code::PhaseStart);
        assert_eq!(diags[0].message, "test_phase");
        assert_eq!(diags[1].code, Code::PhaseEnd);
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
