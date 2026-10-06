//! A filesystem `CompilerHost` that answers from stubs what a real host would
//! ask its environment, for compiling the e2e fixtures outside `wado`.

use std::path::PathBuf;
use std::sync::Mutex;

use indexmap::IndexMap;
use wado_compiler::{CompilerHost, DependencyIndex, Diagnostic, SourceError};

/// What a [`StubHost`] answers in place of the environment.
#[derive(Debug, Default, Clone)]
pub struct HostStubs {
    /// The compile-time environment `#[param(from_env = ...)]` reads.
    pub env: IndexMap<String, String>,
    /// The `[dependencies]` a bare `use ... from "name"` binds to: name → the
    /// dependency's `[package].lib` path, relative to the host's base.
    pub dependencies: IndexMap<String, String>,
}

/// Loads sources relative to a base directory and collects every diagnostic
/// without printing it.
#[derive(Debug)]
pub struct StubHost {
    base_path: PathBuf,
    diagnostics: Mutex<Vec<Diagnostic>>,
    stubs: HostStubs,
}

impl StubHost {
    /// A host loading sources relative to `base_path`, with nothing stubbed.
    #[must_use]
    pub fn new(base_path: PathBuf) -> Self {
        wado_lsp::install_stderr_trace_sink();
        wado_lsp::host::install_dev_stdlib();
        Self {
            base_path,
            diagnostics: Mutex::default(),
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

    /// The diagnostics emitted since this host was created.
    ///
    /// # Panics
    /// If a panic poisoned the buffer.
    #[must_use]
    pub fn diagnostics(&self) -> Vec<Diagnostic> {
        self.diagnostics.lock().unwrap().clone()
    }
}

impl CompilerHost for StubHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        let full_path = self.base_path.join(path);
        std::fs::read(&full_path).map_err(|e| SourceError::IoError {
            path: full_path.display().to_string(),
            message: e.to_string(),
        })
    }

    fn emit_diagnostic(&self, diagnostic: Diagnostic) {
        assert_diagnostic_is_attributed(&diagnostic);
        self.diagnostics.lock().unwrap().push(diagnostic);
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.stubs.env.get(name).cloned()
    }

    fn dependency_index(&self) -> DependencyIndex {
        let mut index = DependencyIndex::default();
        for (name, lib) in &self.stubs.dependencies {
            index.resolved.insert(name.clone(), lib.clone());
        }
        index
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
