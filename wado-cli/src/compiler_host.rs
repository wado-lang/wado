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

/// AOT-compiled generator [`Component`]s, keyed by wasm SHA-256.
///
/// AOT dominates generator execution (~7s for a 432KB generator; see
/// [`kiln_runtime::compile_component`]), so a generator must be compiled
/// once, not once per fixture. A `wado test` run shares one cache across
/// every per-fixture host via
/// [`FilesystemCompilerHost::with_shared_kiln_cache`]; standalone hosts get
/// their own. In-memory only — a disk `.cwasm` cache would be a
/// trust-the-disk code-injection vector. Every `Component` comes from `engine`.
#[derive(Default)]
pub struct KilnComponentCache {
    engine: OnceLock<wasmtime::Engine>,
    components: Mutex<Vec<([u8; 32], wasmtime::component::Component)>>,
    /// AOT count (misses); asserted by tests instead of wall-clock timing.
    compile_count: AtomicUsize,
}

impl KilnComponentCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Lazily-created shared engine; run components on the engine that
    /// [`Self::get_or_compile`] returns alongside them.
    fn engine(&self) -> Result<wasmtime::Engine, GeneratorRunnerError> {
        if let Some(engine) = self.engine.get() {
            return Ok(engine.clone());
        }
        let engine = create_kiln_engine(wasmtime::OptLevel::Speed).map_err(|error| {
            GeneratorRunnerError::Host(format!("failed to create kiln wasmtime engine: {error}"))
        })?;
        // First `set` wins; always clone from the stored value.
        let _ = self.engine.set(engine);
        Ok(self
            .engine
            .get()
            .expect("kiln engine was set above or by a racing caller")
            .clone())
    }

    /// AOT-compiled component for `wasm` (plus its engine), compiling on a
    /// miss. Racing callers may each compile and overwrite — wasted but
    /// correct; holding the lock across the multi-second compile would
    /// serialize unrelated generators.
    fn get_or_compile(
        &self,
        wasm: &[u8],
    ) -> Result<(wasmtime::Engine, wasmtime::component::Component), GeneratorRunnerError> {
        let engine = self.engine()?;
        let key = wado_compiler::kiln::content_hash(wasm);
        if let Ok(guard) = self.components.lock()
            && let Some((_, component)) = guard.iter().find(|(k, _)| k == &key)
        {
            return Ok((engine, component.clone()));
        }
        let component = kiln_runtime::compile_component(&engine, wasm)?;
        self.compile_count.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut guard) = self.components.lock() {
            if let Some(slot) = guard.iter_mut().find(|(k, _)| k == &key) {
                slot.1 = component.clone();
            } else {
                guard.push((key, component.clone()));
            }
        }
        Ok((engine, component))
    }

    #[must_use]
    pub fn compile_count(&self) -> usize {
        self.compile_count.load(Ordering::SeqCst)
    }
}

pub struct FilesystemCompilerHost {
    inner: Arc<wado_lsp::FilesystemCompilerHost>,
    print_diagnostics: bool,
    log_level: LogLevel,
    start_time: Instant,
    /// Per-host by default; a `wado test` run shares one across all
    /// fixtures via [`Self::with_shared_kiln_cache`].
    kiln_cache: Arc<KilnComponentCache>,
    /// Precomputed `[dependencies]` index. When set, `dependency_index`
    /// returns it instead of having the inner host re-read the manifest —
    /// the caller already parsed it for the Kiln pipeline.
    dep_index: Option<wado_compiler::DependencyIndex>,
}

impl FilesystemCompilerHost {
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(wado_lsp::FilesystemCompilerHost::new(base_path)),
            print_diagnostics: true,
            log_level: crate::args::DEFAULT_LOG_LEVEL,
            start_time: Instant::now(),
            kiln_cache: Arc::new(KilnComponentCache::new()),
            dep_index: None,
        }
    }

    #[must_use]
    pub fn silent(base_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(wado_lsp::FilesystemCompilerHost::new(base_path)),
            print_diagnostics: false,
            log_level: LogLevel::Off,
            start_time: Instant::now(),
            kiln_cache: Arc::new(KilnComponentCache::new()),
            dep_index: None,
        }
    }

    #[must_use]
    pub fn with_log_level(base_path: PathBuf, log_level: LogLevel) -> Self {
        Self {
            inner: Arc::new(wado_lsp::FilesystemCompilerHost::new(base_path)),
            print_diagnostics: true,
            log_level,
            start_time: Instant::now(),
            kiln_cache: Arc::new(KilnComponentCache::new()),
            dep_index: None,
        }
    }

    /// Supply a precomputed `[dependencies]` index so the inner host does not
    /// re-read the manifest the caller already parsed for the Kiln pipeline.
    #[must_use]
    pub fn with_dependency_index(mut self, index: wado_compiler::DependencyIndex) -> Self {
        self.dep_index = Some(index);
        self
    }

    /// Share one [`KilnComponentCache`] across hosts so a generator is
    /// AOT-compiled once per `wado test` run instead of once per fixture.
    #[must_use]
    pub fn with_shared_kiln_cache(mut self, cache: Arc<KilnComponentCache>) -> Self {
        self.kiln_cache = cache;
        self
    }

    #[must_use]
    pub fn kiln_component_compile_count(&self) -> usize {
        self.kiln_cache.compile_count()
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

    /// A sibling host that loads sources relative to `base_path` instead of
    /// this host's base, while sharing the AOT generator cache, the
    /// dependency index, and — crucially — the diagnostics buffer, and copying
    /// the log/print settings. The Kiln pipeline uses this to read schemas
    /// relative to the manifest root (where `Invocation` paths are anchored),
    /// whereas the main compile host stays anchored at the entry file's
    /// directory; sharing the diagnostics buffer keeps pipeline-emitted
    /// diagnostics visible to the consuming `host.has_errors()` /
    /// `host.diagnostics()` gate.
    #[must_use]
    pub fn rebased(&self, base_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(self.inner.rebased(base_path)),
            print_diagnostics: self.print_diagnostics,
            log_level: self.log_level,
            start_time: self.start_time,
            kiln_cache: Arc::clone(&self.kiln_cache),
            dep_index: self.dep_index.clone(),
        }
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

    fn dependency_index(&self) -> wado_compiler::DependencyIndex {
        match &self.dep_index {
            Some(index) => index.clone(),
            None => self.inner.dependency_index(),
        }
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    async fn run_generator(
        &self,
        component_wasm: &[u8],
        request: GeneratorRequest,
    ) -> Result<GeneratorResponse, GeneratorRunnerError> {
        let (engine, component) = self.kiln_cache.get_or_compile(component_wasm)?;
        let (outcome, diagnostics) = kiln_runtime::run_generator(
            &engine,
            self.inner.clone(),
            &component,
            request,
            KilnRunPolicy::default(),
        )
        .await;
        // Relay generator-emitted diagnostics through `self` so they are both
        // collected and printed. `run_generator` only has the collect-only
        // inner host on hand, so it hands the diagnostics back for the
        // printing wrapper here to surface them — this is what makes e.g.
        // Gale's prediction warnings visible at build time. Relay before
        // propagating `outcome` so diagnostics survive a generator failure.
        for diag in diagnostics {
            kiln_runtime::relay_diagnostic(self, diag);
        }
        outcome
    }
}
