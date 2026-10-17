//! A filesystem `CompilerHost` that answers from stubs what a real host would
//! ask its environment, for compiling the e2e fixtures outside `wado`.

use std::path::PathBuf;
use std::sync::Arc;

use indexmap::IndexMap;
use wado_compiler::{
    CompilerHost, DependencyIndex, Diagnostic, GeneratorRequest, GeneratorResponse,
    GeneratorRunnerError, SourceError,
};
use wado_lsp::FilesystemCompilerHost;

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
    /// The `[build-dependencies]` a clause's `module: "lib:name"` names: the
    /// nickname → the generator's `.wado` file, relative to the host's base.
    pub build_dependencies: IndexMap<String, String>,
}

/// A [`FilesystemCompilerHost`] that answers the dependency index and the
/// environment from [`HostStubs`] rather than from `wado.toml` and the process.
pub struct StubHost {
    filesystem: FilesystemCompilerHost,
    stubs: HostStubs,
    /// Where generators run, once [`Self::with_generators`] gives a cache.
    /// Without one the host cannot run a generator, as the trait's default.
    generators: Option<Arc<ComponentCache>>,
}

impl StubHost {
    /// A host loading sources relative to `base_path`, with nothing stubbed.
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        wado_lsp::install_stderr_trace_sink();
        Self {
            filesystem: FilesystemCompilerHost::new(base_path),
            stubs: HostStubs::default(),
            generators: None,
        }
    }

    /// This host, running Kiln generators on the components `cache` holds.
    #[must_use]
    pub fn with_generators(mut self, cache: Arc<ComponentCache>) -> Self {
        self.generators = Some(cache);
        self
    }

    /// The generator component for `wasm` and its engine, or `Unsupported`
    /// when this host was given no cache.
    fn component(
        &self,
        wasm: &[u8],
    ) -> Result<(wasmtime::Engine, wasmtime::component::Component), GeneratorRunnerError> {
        self.generators
            .as_ref()
            .ok_or(GeneratorRunnerError::Unsupported)?
            .get_or_compile(wasm)
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
}

impl CompilerHost for StubHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        self.filesystem.load_source(path).await
    }

    async fn source_exists(&self, path: &str) -> bool {
        self.filesystem.source_exists(path).await
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

    fn dependency_index(&self) -> DependencyIndex {
        let mut index = DependencyIndex::default();
        for (name, lib) in &self.stubs.dependencies {
            index.resolved.insert(name.clone(), lib.clone());
        }
        index
    }

    async fn run_generator(
        &self,
        component_wasm: &[u8],
        request: GeneratorRequest,
    ) -> Result<GeneratorResponse, GeneratorRunnerError> {
        let (engine, component) = self.component(component_wasm)?;
        generate_for(self, &engine, &component, request).await
    }

    async fn probe_generator(
        &self,
        component_wasm: &[u8],
        request: &GeneratorRequest,
    ) -> Result<Vec<Option<u64>>, GeneratorRunnerError> {
        let (engine, component) = self.component(component_wasm)?;
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
