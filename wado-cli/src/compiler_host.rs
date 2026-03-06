//! Filesystem-based compiler host for CLI usage
//!
//! This module provides `FilesystemCompilerHost` which implements
//! `wado_compiler::CompilerHost` using filesystem I/O.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use wado_compiler::{Code, CompilerHost, Diagnostic, LogLevel, Severity, SourceError};

/// Filesystem-based compiler host
///
/// This is the standard host used by the CLI. It loads sources from the filesystem
/// and prints diagnostics to stderr with optional timestamps for phase tracking.
#[derive(Debug)]
pub struct FilesystemCompilerHost {
    /// Base path for resolving relative imports
    base_path: PathBuf,
    /// Collected diagnostics (for programmatic access)
    diagnostics: Mutex<Vec<Diagnostic>>,
    /// Whether to print diagnostics to stderr
    print_diagnostics: bool,
    /// Log level for filtering output
    log_level: LogLevel,
    /// Start time for timestamp calculation
    start_time: Instant,
}

impl FilesystemCompilerHost {
    /// Create a new filesystem host with the given base path
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            base_path,
            diagnostics: Mutex::new(Vec::new()),
            print_diagnostics: true,
            log_level: LogLevel::Info,
            start_time: Instant::now(),
        }
    }

    /// Create a host that collects diagnostics without printing
    #[must_use]
    pub fn silent(base_path: PathBuf) -> Self {
        Self {
            base_path,
            diagnostics: Mutex::new(Vec::new()),
            print_diagnostics: false,
            log_level: LogLevel::Off,
            start_time: Instant::now(),
        }
    }

    /// Create a host with a specific log level
    #[must_use]
    pub fn with_log_level(base_path: PathBuf, log_level: LogLevel) -> Self {
        Self {
            base_path,
            diagnostics: Mutex::new(Vec::new()),
            print_diagnostics: true,
            log_level,
            start_time: Instant::now(),
        }
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

    /// Get the base path
    pub fn base_path(&self) -> &PathBuf {
        &self.base_path
    }

    /// Check if the given severity should be logged at the current level
    fn should_log(&self, severity: Severity) -> bool {
        match self.log_level {
            LogLevel::Off => false,
            LogLevel::Error => severity == Severity::Error,
            LogLevel::Warn => matches!(severity, Severity::Error | Severity::Warning),
            LogLevel::Info => {
                matches!(
                    severity,
                    Severity::Error | Severity::Warning | Severity::Info
                )
            }
            LogLevel::Debug => true,
        }
    }

    /// Format elapsed time as `hh:mm:ss.mmmm` (fixed-width under 100 minutes).
    ///
    /// Time tracking is done here in the CLI to keep the compiler syscall-free.
    fn format_timestamp(&self) -> String {
        let elapsed = self.start_time.elapsed();
        let total_secs = elapsed.as_secs();
        let hours = total_secs / 3600;
        let minutes = (total_secs % 3600) / 60;
        let seconds = total_secs % 60;
        // 4 decimal places = 0.1ms precision
        let frac = elapsed.subsec_micros() / 100;
        format!("[{hours:02}:{minutes:02}:{seconds:02}.{frac:04}]")
    }

    /// Format diagnostic with timestamp for span tracking
    fn format_diagnostic(&self, diagnostic: &Diagnostic) -> String {
        let timestamp = self.format_timestamp();

        match diagnostic.code {
            Code::SpanStart => {
                format!("{timestamp} >> {}", diagnostic.message)
            }
            Code::SpanEnd => {
                format!("{timestamp} << {}", diagnostic.message)
            }
            _ => {
                if let Some(span) = &diagnostic.span {
                    format!(
                        "{timestamp} {}:{}:{}: {}: {}",
                        span.file, span.line, span.column, diagnostic.severity, diagnostic.message
                    )
                } else {
                    format!(
                        "{timestamp} {}: {}",
                        diagnostic.severity, diagnostic.message
                    )
                }
            }
        }
    }
}

impl CompilerHost for FilesystemCompilerHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        let full_path = self.base_path.join(path);
        std::fs::read(&full_path).map_err(|e| SourceError::IoError {
            path: full_path.display().to_string(),
            message: e.to_string(),
        })
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        // Always collect diagnostics (for errors check)
        self.diagnostics.lock().unwrap().push(diagnostic.clone());

        // Print if enabled and severity passes the log level filter
        if self.print_diagnostics && self.should_log(diagnostic.severity) {
            let formatted = self.format_diagnostic(&diagnostic);
            eprintln!("{formatted}");
        }
    }
}
