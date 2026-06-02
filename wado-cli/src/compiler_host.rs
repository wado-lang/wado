//! Filesystem-based compiler host for CLI usage. Wraps
//! [`wado_lsp::FilesystemCompilerHost`] with CLI decorations: phase-tracking
//! timestamps, log-level filtering, and stderr printing.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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
    /// In-memory only: when N invocations in one pipeline run share a
    /// generator they share the same wasm bytes, so caching the
    /// `Component` (which is internally `Arc`) turns N×cranelift-AOT
    /// into 1×AOT + (N-1) cheap clones. Not persisted to disk —
    /// caching a serialized `.cwasm` would expose a trust-the-disk
    /// code-injection vector that this in-memory cache does not.
    kiln_components: Mutex<Vec<([u8; 32], wasmtime::component::Component)>>,
    /// Cache misses on `kiln_components`. Tests assert against this
    /// rather than wall-clock timing.
    kiln_component_compile_count: AtomicUsize,
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
            kiln_components: Mutex::new(Vec::new()),
            kiln_component_compile_count: AtomicUsize::new(0),
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
            kiln_components: Mutex::new(Vec::new()),
            kiln_component_compile_count: AtomicUsize::new(0),
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
            kiln_components: Mutex::new(Vec::new()),
            kiln_component_compile_count: AtomicUsize::new(0),
        }
    }

    #[must_use]
    pub fn kiln_component_compile_count(&self) -> usize {
        self.kiln_component_compile_count.load(Ordering::SeqCst)
    }

    /// Concurrent callers with the same key may each compile once and
    /// overwrite each other in the map — that's wasted but correct
    /// (the resulting `Component`s are equivalent). Holding the Mutex
    /// across the multi-second cranelift call would serialize unrelated
    /// generators, which is the worse tradeoff.
    fn get_or_compile_kiln_component(
        &self,
        engine: &wasmtime::Engine,
        wasm: &[u8],
    ) -> Result<wasmtime::component::Component, GeneratorRunnerError> {
        let key = wado_compiler::kiln::content_hash(wasm);
        if let Ok(guard) = self.kiln_components.lock()
            && let Some((_, component)) = guard.iter().find(|(k, _)| k == &key)
        {
            return Ok(component.clone());
        }
        let component = kiln_runtime::compile_component(engine, wasm)?;
        self.kiln_component_compile_count
            .fetch_add(1, Ordering::SeqCst);
        if let Ok(mut guard) = self.kiln_components.lock() {
            if let Some(slot) = guard.iter_mut().find(|(k, _)| k == &key) {
                slot.1 = component.clone();
            } else {
                guard.push((key, component.clone()));
            }
        }
        Ok(component)
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
        let component = self.get_or_compile_kiln_component(&engine, component_wasm)?;
        let (response, diagnostics) = kiln_runtime::run_generator(
            &engine,
            self.inner.clone(),
            &component,
            request,
            KilnRunPolicy::default(),
        )
        .await?;
        // Relay generator-emitted diagnostics through `self` so they are
        // both collected and printed. `run_generator` only has the
        // collect-only inner host on hand, so it hands the diagnostics back
        // for the printing wrapper here to surface them — this is what makes
        // e.g. Gale's prediction warnings visible at build time.
        for diag in diagnostics {
            kiln_runtime::relay_diagnostic(self, diag);
        }
        Ok(response)
    }
}
