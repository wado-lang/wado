//! Filesystem-based compiler host for CLI usage. Wraps
//! [`wado_lsp::FilesystemCompilerHost`] with CLI decorations: phase-tracking
//! timestamps, log-level filtering, and stderr printing.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use wado_compiler::{
    Code, CompilerHost, Diagnostic, GeneratorRequest, GeneratorResponse, GeneratorRunnerError,
    LogLevel, Severity, SourceError,
};

use crate::kiln_runtime::{self, KilnRunPolicy};
use crate::runtime::create_kiln_engine;

pub struct FilesystemCompilerHost {
    inner: Arc<wado_lsp::FilesystemCompilerHost>,
    print_diagnostics: bool,
    log_level: LogLevel,
    start_time: Instant,
    kiln_engine: OnceLock<wasmtime::Engine>,
}

impl FilesystemCompilerHost {
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(wado_lsp::FilesystemCompilerHost::new(base_path)),
            print_diagnostics: true,
            log_level: LogLevel::Info,
            start_time: Instant::now(),
            kiln_engine: OnceLock::new(),
        }
    }

    #[must_use]
    pub fn silent(base_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(wado_lsp::FilesystemCompilerHost::new(base_path)),
            print_diagnostics: false,
            log_level: LogLevel::Off,
            start_time: Instant::now(),
            kiln_engine: OnceLock::new(),
        }
    }

    #[must_use]
    pub fn with_log_level(base_path: PathBuf, log_level: LogLevel) -> Self {
        Self {
            inner: Arc::new(wado_lsp::FilesystemCompilerHost::new(base_path)),
            print_diagnostics: true,
            log_level,
            start_time: Instant::now(),
            kiln_engine: OnceLock::new(),
        }
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.inner.diagnostics()
    }

    pub fn has_errors(&self) -> bool {
        self.inner.has_errors()
    }

    pub fn base_path(&self) -> &PathBuf {
        self.inner.base_path()
    }

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

    // Timestamps live here, not in the compiler, to keep the compiler syscall-free.
    fn format_timestamp(&self) -> String {
        let elapsed = self.start_time.elapsed();
        let total_secs = elapsed.as_secs();
        let hours = total_secs / 3600;
        let minutes = (total_secs % 3600) / 60;
        let seconds = total_secs % 60;
        let frac = elapsed.subsec_micros() / 100;
        format!("[{hours:02}:{minutes:02}:{seconds:02}.{frac:04}]")
    }

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
        self.inner.load_source(path).await
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        if self.print_diagnostics && self.should_log(diagnostic.severity) {
            let formatted = self.format_diagnostic(&diagnostic);
            eprintln!("{formatted}");
        }
        self.inner.collect_diagnostic(diagnostic);
    }

    async fn run_generator(
        &self,
        component_wasm: &[u8],
        request: GeneratorRequest,
    ) -> Result<GeneratorResponse, GeneratorRunnerError> {
        let engine = if let Some(engine) = self.kiln_engine.get() {
            engine.clone()
        } else {
            let engine = create_kiln_engine(wasmtime::OptLevel::Speed).map_err(|error| {
                GeneratorRunnerError::Host(format!(
                    "failed to create kiln wasmtime engine: {error}"
                ))
            })?;
            // Racing callers: first `set` wins; we always clone from the stored value.
            let _ = self.kiln_engine.set(engine);
            self.kiln_engine
                .get()
                .expect("kiln_engine was set above or by a racing caller")
                .clone()
        };
        kiln_runtime::run_generator(
            &engine,
            self.inner.clone(),
            component_wasm,
            request,
            KilnRunPolicy::default(),
        )
        .await
    }
}
