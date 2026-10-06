//! Shared test fixtures for `wado-lsp`.
//!
//! `#[doc(hidden)] pub` so both unit tests (inside `src/`) and integration
//! tests (`tests/*.rs`) reach it without inflating the documented surface.
//!
//! The in-memory host is the compiler's own `InMemoryCompilerHost`, built by
//! [`in_memory_host`]. Add shared helpers here rather than growing a per-file
//! `TestHost`: a second host drifts in constructor names and
//! diagnostic-capture behaviour, and then a test silently asserts against the
//! non-canonical one.

use wado_compiler::InMemoryCompilerHost;

use crate::host::install_dev_stdlib;
use crate::{Engine, install_stderr_trace_sink};

/// Path a single-file fixture is analysed under.
pub const TEST_PATH: &str = "/test.wado";

/// An [`Engine`] with one document open, and the host serving the fixture.
pub struct Opened {
    pub engine: Engine,
    pub host: InMemoryCompilerHost,
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
        host: in_memory_host(files),
        uri,
    }
}

/// An [`InMemoryCompilerHost`] serving `files`, set up as a native binary sets
/// up its own: the dev stdlib installed, for a test that compiles without an
/// [`Engine`], and traces sent to stderr. `wado-compiler-tests` takes it too.
#[must_use]
pub fn in_memory_host(files: &[(&str, &str)]) -> InMemoryCompilerHost {
    install_stderr_trace_sink();
    install_dev_stdlib();
    InMemoryCompilerHost::with_files(files)
}
