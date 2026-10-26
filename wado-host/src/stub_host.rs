//! A filesystem `CompilerHost` that answers from stubs what a real host would
//! ask its environment, for compiling the e2e fixtures outside `wado`.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use wado_compiler::remote::{Integrity, held_to_use, is_remote};
use wado_compiler::{
    CompilerHost, DependencyIndex, Diagnostic, GeneratorRequest, GeneratorResponse,
    GeneratorRunnerError, ProbeRequest, SourceError, SourceReader,
};
use wado_lsp::FilesystemCompilerHost;
use wado_lsp::host::discovery::normalize_path;

use crate::kiln::component_cache::ComponentCache;
use crate::kiln::runtime::{generate_for, probe_for};

/// What a [`StubHost`] answers in place of the environment.
#[derive(Debug, Default, Clone)]
pub struct HostStubs {
    /// The compile-time environment `#[param(from_env = ...)]` reads.
    pub env: IndexMap<String, String>,
    /// The `[dependencies]` a bare `use ... from "name"` binds to: name → the
    /// dependency's `[package].lib` path, relative to the host's base.
    pub dependencies: IndexMap<String, String>,
    /// The directories, relative to the host's base, that stand where a
    /// `wado.toml` would.
    pub package_roots: Vec<String>,
    /// The `[build-dependencies]` a clause's `module: "lib:name"` names: the
    /// nickname → the generator's `.wado` file, relative to the host's base.
    pub build_dependencies: IndexMap<String, String>,
    /// What each URL serves: the URL → a file, relative to the host's base.
    pub remote: IndexMap<String, String>,
}

/// A [`FilesystemCompilerHost`] that answers the dependency index and the
/// environment from [`HostStubs`] rather than from `wado.toml` and the process,
/// and runs Kiln generators as `wado` does.
#[derive(Debug)]
pub struct StubHost {
    filesystem: FilesystemCompilerHost,
    stubs: HostStubs,
}

/// The generator components every [`StubHost`] shares, so a run of many
/// fixtures compiles each generator once.
static GENERATORS: LazyLock<ComponentCache> = LazyLock::new(ComponentCache::default);

impl StubHost {
    /// A host loading sources relative to `base_path`, with nothing stubbed.
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        wado_lsp::install_stderr_trace_sink();
        Self {
            filesystem: FilesystemCompilerHost::new(base_path),
            stubs: HostStubs::default(),
        }
    }

    /// This host, answering from `stubs` what a real host would ask its
    /// environment.
    #[must_use]
    pub fn with_stubs(mut self, stubs: HostStubs) -> Self {
        self.stubs = stubs;
        self
    }

    /// What this host answers in place of the environment.
    #[must_use]
    pub fn stubs(&self) -> &HostStubs {
        &self.stubs
    }

    /// The diagnostics emitted since this host was created.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.filesystem.diagnostics()
    }

    /// The file stubbed for `url`.
    fn remote_file(&self, url: &str) -> Result<&str, SourceError> {
        self.stubs
            .remote
            .get(url)
            .map(String::as_str)
            .ok_or_else(|| SourceError::NotFound {
                path: url.to_string(),
            })
    }
}

impl CompilerHost for StubHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        self.filesystem.load_source(path).await
    }

    async fn source_exists(&self, path: &str) -> bool {
        self.filesystem.source_exists(path).await
    }

    async fn open_source(&self, path: &str) -> Result<Arc<dyn SourceReader>, SourceError> {
        if is_remote(path) {
            return self.filesystem.open_source(self.remote_file(path)?).await;
        }
        self.filesystem.open_source(path).await
    }

    async fn load_source_prefix(
        &self,
        path: &str,
        extent: Option<u64>,
    ) -> Result<Vec<u8>, SourceError> {
        self.filesystem.load_source_prefix(path, extent).await
    }

    /// The file stubbed for `url`, held to the pin as a host with no lock
    /// file holds it.
    async fn load_remote(
        &self,
        url: &str,
        extent: Option<u64>,
        integrity: Option<Integrity>,
    ) -> Result<Vec<u8>, SourceError> {
        let file = self.remote_file(url)?;
        held_to_use(
            url,
            self.filesystem.load_source_prefix(file, extent).await?,
            integrity,
        )
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        assert_diagnostic_is_attributed(&diagnostic);
        self.filesystem.collect_diagnostic(diagnostic);
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.stubs.env.get(name).cloned()
    }

    fn stdlib_module_read(&self, import_path: &str) {
        self.filesystem.stdlib_module_read(import_path);
    }

    /// The nearest stubbed root above the module, compared lexically: a
    /// fixture directory holds no symlink.
    fn package_root(&self, path: &str) -> Option<String> {
        let base = self.filesystem.base_path();
        let file = normalize_path(&base.join(path));
        let dir = file.parent().expect("a module path names a file");
        self.stubs
            .package_roots
            .iter()
            .map(|root| (root, normalize_path(&base.join(root))))
            .filter(|(_, abs)| dir.starts_with(abs))
            .max_by_key(|(_, abs)| abs.components().count())
            .map(|(root, _)| root.clone())
    }

    /// A stubbed package declares no dependencies of its own.
    fn package_index(&self, _root: &str) -> DependencyIndex {
        DependencyIndex::default()
    }

    fn dependency_index(&self) -> DependencyIndex {
        let mut index = DependencyIndex::default();
        for (name, lib) in &self.stubs.dependencies {
            index.resolved.insert(name.clone(), lib.clone());
            index
                .packages
                .insert(lib.clone(), DependencyIndex::default());
        }
        index
    }

    async fn run_generator(
        &self,
        component_wasm: &[u8],
        request: GeneratorRequest,
    ) -> Result<GeneratorResponse, GeneratorRunnerError> {
        let (engine, component) = GENERATORS.get_or_compile(component_wasm)?;
        generate_for(self, &engine, &component, request).await
    }

    async fn probe_generator(
        &self,
        component_wasm: &[u8],
        request: &ProbeRequest,
    ) -> Result<Vec<Option<u64>>, GeneratorRunnerError> {
        let (engine, component) = GENERATORS.get_or_compile(component_wasm)?;
        probe_for(self, &engine, &component, request).await
    }
}

/// A located diagnostic names the file it is in — what a per-document consumer
/// selects on, and without which the LSP drops it. Checked at the host, so the
/// whole fixture corpus enforces it. A span-less diagnostic is about the
/// compilation rather than a place in it, and is exempt.
fn assert_diagnostic_is_attributed(diagnostic: &Diagnostic) {
    if let Some(span) = diagnostic.span.as_ref() {
        assert!(
            !span.file.is_empty(),
            "diagnostic carries a span but no file: {} ({:?}) at {}:{}\n\
             emit it through `Elaborator::emit` / `Logger::error_in`, not `Logger::error`",
            diagnostic.message,
            diagnostic.code,
            span.line,
            span.column,
        );
    }
}
