//! Shared test fixtures for `wado-lsp`.
//!
//! Centralises the in-memory `CompilerHost` implementation that every test
//! file used to redeclare. The module is publicly exposed but marked
//! `#[doc(hidden)]` so it is reachable from both unit tests (inside
//! `src/`) and integration tests (`tests/*.rs`) without inflating the
//! crate's documented surface.
//!
//! Adding a new shared helper here is preferable to growing yet another
//! `TestHost` per file — the previous duplication drifted across files
//! (different constructor names, different diagnostic-capture behaviour)
//! and made it easy to accidentally test against a non-canonical host.
//!
//! Typical usage in tests:
//!
//! ```ignore
//! use wado_lsp::test_support::MapHost;
//!
//! let host = MapHost::single("/test.wado", source);
//! ```
//!
//! For multi-file fixtures use [`MapHost::with_files`]; for tests that
//! expect lookups to fail, use [`MapHost::empty`].
//!
//! Diagnostics emitted via `CompilerHost::emit_diagnostic` are captured
//! in an internal buffer and observable through
//! [`MapHost::emitted`] — useful for asserting that a wrapper host
//! (e.g. `DiagnosticCollector`) actually forwards to the inner host.

use std::sync::Mutex;

use indexmap::IndexMap;
use wado_compiler::{CompilerHost, Diagnostic as CompilerDiagnostic, SourceError};

/// In-memory `CompilerHost` backed by an `IndexMap` of path → source bytes.
///
/// Emitted diagnostics are captured into an internal buffer so tests can
/// assert on forwarding without supplying a separate observer host.
pub struct MapHost {
    sources: IndexMap<String, Vec<u8>>,
    emitted: Mutex<Vec<CompilerDiagnostic>>,
}

impl MapHost {
    /// A host with no source files; every `load_source` call returns
    /// [`SourceError::NotFound`].
    #[must_use]
    pub fn empty() -> Self {
        Self {
            sources: IndexMap::new(),
            emitted: Mutex::new(Vec::new()),
        }
    }

    /// A host serving exactly one file.
    #[must_use]
    pub fn single(path: &str, source: &str) -> Self {
        Self::with_files(&[(path, source)])
    }

    /// A host serving each `(path, source)` pair. Later entries with the
    /// same path overwrite earlier ones (matches `IndexMap::insert`).
    #[must_use]
    pub fn with_files(files: &[(&str, &str)]) -> Self {
        let mut sources = IndexMap::new();
        for (path, body) in files {
            sources.insert((*path).to_string(), body.as_bytes().to_vec());
        }
        Self {
            sources,
            emitted: Mutex::new(Vec::new()),
        }
    }

    /// Snapshot of diagnostics this host has received via
    /// `CompilerHost::emit_diagnostic`.
    #[must_use]
    pub fn emitted(&self) -> Vec<CompilerDiagnostic> {
        self.emitted.lock().unwrap().clone()
    }
}

impl CompilerHost for MapHost {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
        self.sources
            .get(path)
            .cloned()
            .ok_or_else(|| SourceError::NotFound {
                path: path.to_string(),
            })
    }

    fn emit_diagnostic(&self, diagnostic: CompilerDiagnostic) {
        self.emitted.lock().unwrap().push(diagnostic);
    }
}
