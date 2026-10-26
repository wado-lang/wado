//! Filesystem-based compiler host for CLI usage. Wraps
//! [`wado_lsp::FilesystemCompilerHost`] with CLI decorations: phase-tracking
//! timestamps, log-level filtering, and stderr printing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use indexmap::IndexMap;
use wado_compiler::remote::{Integrity, is_remote};
use wado_compiler::{
    Code, CompilerHost, DependencyIndex, Diagnostic, GeneratorRequest, GeneratorResponse,
    GeneratorRunnerError, LogLevel, ProbeRequest, Severity, SourceError, SourceReader,
};
use wado_manifest::LOCK_FILENAME;

use crate::args::DEFAULT_LOG_LEVEL;
use crate::dep_component::Acquisition;
use crate::manifest::{package_root, resolution_inputs};
use crate::remote::{self, RemoteReader};
use crate::rss::RssSample;
use crate::run_cache::RunCache;
use wado_host::kiln::driver::KilnSpan;
use wado_host::kiln::runtime::{generate_for, probe_for};
use wado_host::sync::lock;

pub struct FilesystemCompilerHost {
    inner: Arc<wado_lsp::FilesystemCompilerHost>,
    print_diagnostics: bool,
    log_level: LogLevel,
    start_time: Instant,
    /// Per-host by default; a `wado test` run shares one across all
    /// fixtures via [`Self::with_shared_run_cache`].
    run: Arc<RunCache>,
    /// Precomputed `[dependencies]` index. When set, `dependency_index`
    /// returns it instead of having the inner host re-read the manifest —
    /// the caller already parsed it for the Kiln pipeline.
    dep_index: Option<wado_compiler::DependencyIndex>,
    /// How far a build may go to acquire what the packages it reaches depend
    /// on. `None` where nothing is to be acquired, so every index is offline.
    acquisition: Option<Acquisition>,
    /// Each package a walk placed modules in that no manifest declares. Shared
    /// with every sibling host, so the compile reads the index the Kiln walk's
    /// acquisition left.
    undeclared: Arc<Mutex<IndexMap<PackageRead, UndeclaredPackage>>>,
    /// The resident set at each open span's start, innermost last, so a span's
    /// end line says how far it moved.
    span_rss: Mutex<Vec<Option<RssSample>>>,
}

/// The package at `root` as a host based at `base` reads it: its index spells
/// each dependency from that base.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PackageRead {
    pub base: PathBuf,
    pub root: String,
}

/// The index an undeclared package is answered with: offline until acquired.
enum UndeclaredPackage {
    Offline(DependencyIndex),
    Acquired(DependencyIndex),
}

impl FilesystemCompilerHost {
    /// The one constructor; the three below name the combinations callers want.
    fn configured(base_path: PathBuf, print_diagnostics: bool, log_level: LogLevel) -> Self {
        wado_lsp::install_stderr_trace_sink();
        Self {
            inner: Arc::new(wado_lsp::FilesystemCompilerHost::new(base_path)),
            print_diagnostics,
            log_level,
            start_time: Instant::now(),
            run: Arc::new(RunCache::new()),
            dep_index: None,
            acquisition: None,
            undeclared: Arc::default(),
            span_rss: Mutex::default(),
        }
    }

    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        Self::configured(base_path, true, DEFAULT_LOG_LEVEL)
    }

    #[must_use]
    pub fn silent(base_path: PathBuf) -> Self {
        Self::configured(base_path, false, LogLevel::Off)
    }

    #[must_use]
    pub fn with_log_level(base_path: PathBuf, log_level: LogLevel) -> Self {
        Self::configured(base_path, true, log_level)
    }

    /// Supply a precomputed `[dependencies]` index so the inner host does not
    /// re-read the manifest the caller already parsed for the Kiln pipeline.
    #[must_use]
    pub fn with_dependency_index(mut self, index: wado_compiler::DependencyIndex) -> Self {
        self.dep_index = Some(index);
        self
    }

    /// Acquire per `tier` what an undeclared package a walk reaches depends on,
    /// as the declared ones were.
    #[must_use]
    pub fn acquiring(mut self, tier: Acquisition) -> Self {
        self.acquisition = Some(tier);
        self
    }

    /// The tier to acquire at, and each undeclared package a walk reached that
    /// is not yet acquired, with the offline index it was answered with.
    #[must_use]
    pub fn unacquired_packages(
        &self,
    ) -> Option<(Acquisition, Vec<(PackageRead, DependencyIndex)>)> {
        let tier = self.acquisition?;
        let unacquired = lock(&self.undeclared)
            .iter()
            .filter_map(|(read, package)| match package {
                UndeclaredPackage::Offline(index) => Some((read.clone(), index.clone())),
                UndeclaredPackage::Acquired(_) => None,
            })
            .collect();
        Some((tier, unacquired))
    }

    /// Answer `index` for the undeclared package `read` from now on.
    pub fn adopt_acquired(&self, read: PackageRead, index: DependencyIndex) {
        lock(&self.undeclared).insert(read, UndeclaredPackage::Acquired(index));
    }

    /// Share one [`RunCache`] across hosts, so a `wado test` run resolves and
    /// AOT-compiles each generator once instead of once per fixture, and reads
    /// one view of the source tree.
    #[must_use]
    pub fn with_shared_run_cache(mut self, cache: Arc<RunCache>) -> Self {
        self.run = cache;
        self
    }

    /// How loud this host is, so work it delegates reports at the same level.
    #[must_use]
    pub fn log_level(&self) -> LogLevel {
        self.log_level
    }

    /// The run-scoped state this host shares.
    #[must_use]
    pub fn run_cache(&self) -> Arc<RunCache> {
        Arc::clone(&self.run)
    }

    #[must_use]
    pub fn kiln_component_compile_count(&self) -> usize {
        self.run.components().compile_count()
    }

    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.inner.diagnostics()
    }

    pub fn has_errors(&self) -> bool {
        self.inner.has_errors()
    }

    pub fn base_path(&self) -> &Path {
        self.inner.base_path()
    }

    /// A sibling host based at `base_path`, sharing the AOT generator cache,
    /// the dependency index, and the diagnostics buffer (via
    /// `wado_lsp::FilesystemCompilerHost::rebased`). The Kiln pipeline uses
    /// it to read schemas relative to the manifest root, where `Invocation`
    /// paths are anchored, while the main compile host stays at the entry
    /// file's directory.
    #[must_use]
    pub fn rebased(&self, base_path: PathBuf) -> Self {
        Self {
            inner: Arc::new(self.inner.rebased(base_path)),
            print_diagnostics: self.print_diagnostics,
            log_level: self.log_level,
            start_time: self.start_time,
            run: Arc::clone(&self.run),
            dep_index: self.dep_index.clone(),
            acquisition: self.acquisition,
            undeclared: Arc::clone(&self.undeclared),
            span_rss: Mutex::default(),
        }
    }

    /// The cached AOT compile, under a span of its own so a slow Kiln build
    /// separates native compilation from what the generator then does.
    fn aot(
        &self,
        component_wasm: &[u8],
    ) -> Result<(wasmtime::Engine, wasmtime::component::Component), GeneratorRunnerError> {
        let _span = KilnSpan::new(self, "kiln/aot");
        self.run.components().get_or_compile(component_wasm)
    }

    /// Record that reading `path` failed with `error`: a missing file changes
    /// the outcome once it appears, and any other failure is no file's to
    /// vouch for.
    fn observe_failure(&self, path: &str, error: &SourceError) {
        match error {
            SourceError::NotFound { .. } => {
                self.run.observe_absent(&self.inner.base_path().join(path));
            }
            SourceError::IoError { .. } | SourceError::NetworkError { .. } => {
                self.run.observe_untracked();
            }
            SourceError::IntegrityMismatch { .. } | SourceError::Unpinned { .. } => {
                unreachable!("only a remote read is held to a pin")
            }
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
                let rss = RssSample::read();
                lock(&self.span_rss).push(rss);
                format!(
                    "{timestamp} >> {}{}",
                    diagnostic.message,
                    span_rss_suffix(rss, None)
                )
            }
            Code::SpanEnd => {
                let start = lock(&self.span_rss).pop().flatten();
                format!(
                    "{timestamp} << {}{}",
                    diagnostic.message,
                    span_rss_suffix(RssSample::read(), start)
                )
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
        self.load_source_prefix(path, None).await
    }

    /// Nothing read through the reader is recorded: the part of the file a
    /// compile depends on is read again through [`Self::load_source_prefix`].
    /// A file that fails to open is recorded, as one that fails to load is.
    async fn open_source(&self, path: &str) -> Result<Arc<dyn SourceReader>, SourceError> {
        if is_remote(path) {
            return match RemoteReader::open(path).await {
                Ok(reader) => Ok(Arc::new(reader)),
                Err(e) => {
                    self.run.observe_untracked();
                    Err(e)
                }
            };
        }
        let opened = self.inner.open_source(path).await;
        if let Err(e) = &opened {
            self.observe_failure(path, e);
        }
        opened
    }

    async fn load_source_prefix(
        &self,
        path: &str,
        extent: Option<u64>,
    ) -> Result<Vec<u8>, SourceError> {
        // Every module and asset a compile reads passes here, which is how the
        // run learns what tree it is describing.
        let loaded = self.inner.load_source_prefix(path, extent).await;
        match &loaded {
            Ok(bytes) => self
                .run
                .observe(&self.inner.base_path().join(path), extent, bytes),
            Err(e) => self.observe_failure(path, e),
        }
        loaded
    }

    /// A pin settles the bytes, so a read records no file but the `wado.lock`
    /// that pinned it, and only a failure leaves the compile untracked.
    async fn load_remote(
        &self,
        url: &str,
        extent: Option<u64>,
        integrity: Option<Integrity>,
    ) -> Result<Vec<u8>, SourceError> {
        let lock_root = package_root(self.base_path());
        let loaded = remote::load(url, extent, integrity, lock_root.as_deref()).await;
        match (&loaded, &lock_root) {
            (Err(_), _) => self.run.observe_untracked(),
            (Ok(_), Some(root)) if integrity.is_none() => {
                self.run.depend_on_file(&root.join(LOCK_FILENAME));
            }
            (Ok(_), _) => {}
        }
        loaded
    }

    async fn source_exists(&self, path: &str) -> bool {
        let exists = self.inner.source_exists(path).await;
        // A present file is recorded once the compile reads it.
        if !exists {
            self.run.observe_absent(&self.inner.base_path().join(path));
        }
        exists
    }

    /// Saves into the OS temp directory, which exists on every platform the
    /// CLI runs on and needs no write permission where the sources live. The
    /// name is opaque: a predictable one there is a symlink target (CWE-59).
    fn save_internal_artifact(&self, file_stem: &str, bytes: &[u8]) -> Option<String> {
        let mut file = tempfile::Builder::new()
            .prefix(&format!("wado-{file_stem}-"))
            .suffix(".wasm")
            .tempfile()
            .ok()?;
        file.write_all(bytes).ok()?;
        let (_, path) = file.keep().ok()?;
        Some(path.display().to_string())
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

    fn serves_core(&self) -> bool {
        self.inner.serves_core()
    }

    fn package_root(&self, path: &str) -> Option<String> {
        self.inner.package_root_probing(path, |manifest, found| {
            if found {
                self.run.depend_on_file(manifest);
            } else {
                self.run.observe_absent(manifest);
            }
        })
    }

    fn package_index(&self, root: &str) -> DependencyIndex {
        for file in resolution_inputs(Path::new(root)) {
            self.run.depend_on_file(&file);
        }
        let read = PackageRead {
            base: self.base_path().to_path_buf(),
            root: root.to_string(),
        };
        let mut undeclared = lock(&self.undeclared);
        if let Some(UndeclaredPackage::Acquired(index)) = undeclared.get(&read) {
            return index.clone();
        }
        let offline = self.inner.package_index(root);
        if self.acquisition.is_some() {
            undeclared.insert(read, UndeclaredPackage::Offline(offline.clone()));
        }
        offline
    }

    fn env_var(&self, name: &str) -> Option<String> {
        let value = std::env::var(name).ok();
        self.run.observe_env(name, value.as_deref());
        value
    }

    /// A dev build compiles against the stdlib it read from disk, which an edit
    /// changes. A release build embeds it, so the binary alone stands for it.
    fn stdlib_module_read(&self, import_path: &str) {
        #[cfg(debug_assertions)]
        {
            use wado_compiler::stdlib::{DEV_STDLIB_ROOT, module_file};

            let file = module_file(import_path)
                .unwrap_or_else(|| panic!("{import_path} is a stdlib module"));
            self.run
                .depend_on_watched(&Path::new(DEV_STDLIB_ROOT).join(file));
        }
        #[cfg(not(debug_assertions))]
        let _ = import_path;
    }

    async fn run_generator(
        &self,
        component_wasm: &[u8],
        request: GeneratorRequest,
    ) -> Result<GeneratorResponse, GeneratorRunnerError> {
        let (engine, component) = self.aot(component_wasm)?;
        let _span = KilnSpan::new(self, "kiln/generate");
        generate_for(self, &engine, &component, request).await
    }

    async fn probe_generator(
        &self,
        component_wasm: &[u8],
        request: &ProbeRequest,
    ) -> Result<Vec<Option<u64>>, GeneratorRunnerError> {
        let (engine, component) = self.aot(component_wasm)?;
        let _span = KilnSpan::new(self, "kiln/probe");
        probe_for(self, &engine, &component, request).await
    }
}

/// A span trace line's RSS suffix, or nothing where no reading exists.
fn span_rss_suffix(rss: Option<RssSample>, start: Option<RssSample>) -> String {
    rss.map(|rss| rss.suffix(start.as_ref()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An index is spelled from the base of the host that read it, so a
    /// sibling at another base never answers one acquired at this one.
    #[test]
    fn an_acquired_index_is_answered_only_at_the_base_it_was_acquired_from() {
        let tmp = tempfile::tempdir().unwrap();
        let package = tmp.path().join("pkg");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("wado.toml"),
            "[package]\nname = \"pkg\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let root = package.display().to_string();
        let host =
            FilesystemCompilerHost::silent(tmp.path().join("a")).acquiring(Acquisition::Build);
        let sibling = host.rebased(tmp.path().join("b"));

        host.package_index(&root);
        let (_, unacquired) = host.unacquired_packages().unwrap();
        let mut acquired = DependencyIndex::default();
        acquired
            .resolved
            .insert("lib:marker".to_string(), "marker.wado".to_string());
        for (package, _) in unacquired {
            host.adopt_acquired(package, acquired.clone());
        }

        assert!(
            host.package_index(&root)
                .resolved
                .contains_key("lib:marker")
        );
        assert!(
            !sibling
                .package_index(&root)
                .resolved
                .contains_key("lib:marker")
        );
    }
}
