//! Shared test fixtures for `wado-lsp`.
//!
//! The one in-memory `CompilerHost` for the crate's tests. `#[doc(hidden)]
//! pub` so both unit tests (inside `src/`) and integration tests
//! (`tests/*.rs`) reach it without inflating the documented surface.
//!
//! Add shared helpers here rather than growing a per-file `TestHost`: a
//! second host drifts in constructor names and diagnostic-capture behaviour,
//! and then a test silently asserts against the non-canonical one.
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

use crate::Engine;

/// Path a single-file fixture is analysed under.
pub const TEST_PATH: &str = "/test.wado";

/// An [`Engine`] with one document open, and the host serving the fixture.
///
/// Here so a test that builds its own setup stands out as deliberate.
pub struct Opened {
    pub engine: Engine,
    pub host: MapHost,
    pub uri: String,
}

/// [`Opened`] over a single fixture file at [`TEST_PATH`].
#[must_use]
pub fn open(source: &str) -> Opened {
    open_at(TEST_PATH, source)
}

/// [`Opened`] over a single fixture file at `path`.
#[must_use]
pub fn open_at(path: &str, source: &str) -> Opened {
    open_files(&[(path, source)], path)
}

/// [`Opened`] over a multi-file fixture, with `entry` the open document.
///
/// # Panics
/// If `entry` names no file in `files`.
#[must_use]
pub fn open_files(files: &[(&str, &str)], entry: &str) -> Opened {
    let source = files
        .iter()
        .find(|(path, _)| *path == entry)
        .map(|(_, source)| *source)
        .expect("entry file present in fixture");
    let uri = format!("file://{entry}");
    let mut engine = Engine::new();
    engine.open_document(&uri, source.to_string());
    Opened {
        engine,
        host: MapHost::with_files(files),
        uri,
    }
}

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
