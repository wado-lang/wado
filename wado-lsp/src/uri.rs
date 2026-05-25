//! Typed URI helpers used by both the engine and the stdio server.
//!
//! All URI parsing for the LSP lives here:
//! - `Uri::from_str` recognises the `file:`, `core:`, `wasi:`, and `kiln:`
//!   schemes the wado compiler emits via `module_uri`.
//! - `Uri::to_filename` strips `file://` for compiler-side diagnostic
//!   filenames and falls back to the raw string for non-file schemes
//!   (matching the behaviour of `ModuleSource::diagnostic_filename`).
//! - `Uri::workspace_root` extracts the directory of a `file:` URI for
//!   the per-document `FilesystemCompilerHost` base path.
//!
//! Centralising this lets `Engine`, the dispatcher, and
//! `workspace/textDocumentContent` agree on what counts as a URI scheme
//! rather than re-parsing strings inline. Previously the codebase had
//! two copies of `uri_to_filename` (in `lib.rs` and in `location.rs`)
//! plus ad-hoc `strip_prefix("file://")` calls scattered across
//! `host_for_uri`, `text_document_content`, and the LSP query
//! plumbing.

use std::path::{Path, PathBuf};

/// LSP-side URI scheme classification.
///
/// The wado compiler emits the schemes documented at
/// `wado-lsp/CLAUDE.md#bundled-stdlib-content` plus
/// `WEP 2026-04-12 §"URI scheme"` for `kiln:` redirects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UriScheme {
    /// `file:` — disk-backed user source.
    File,
    /// `core:` — bundled standard library module (e.g. `core:cli`).
    Core,
    /// `wasi:` — bundled WASI interface module (e.g. `wasi:filesystem/types.wado`).
    Wasi,
    /// `kiln:` — Kiln-generator output module (see WEP 2026-04-12).
    Kiln,
    /// Other / unrecognised — passed through verbatim to consumers.
    Other,
}

/// A document URI as received from the LSP client.
///
/// Kept as a thin wrapper around `String` so equality / hashing match the
/// raw URI seen on the wire. Methods on this type are the **only** approved
/// way to extract a filename, scheme, or workspace root from a URI in this
/// crate.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Uri(String);

impl Uri {
    /// Wrap a URI string. No normalisation is performed; the value is
    /// stored as-is so cache keys round-trip without loss.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Raw URI string as received from the client.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Scheme classification. Returns [`UriScheme::Other`] when the URI
    /// has no `:` or its scheme is not one of the four wado supports.
    #[must_use]
    pub fn scheme(&self) -> UriScheme {
        match self.0.split_once(':').map(|(s, _)| s) {
            Some("file") => UriScheme::File,
            Some("core") => UriScheme::Core,
            Some("wasi") => UriScheme::Wasi,
            Some("kiln") => UriScheme::Kiln,
            _ => UriScheme::Other,
        }
    }

    /// Filename string suitable for compiler-side diagnostics
    /// (`Logger::set_file`, `DiagnosticSpan::file`).
    ///
    /// For `file://` URIs returns the absolute path with the scheme
    /// stripped. For every other scheme returns the raw URI — matching
    /// `ModuleSource::diagnostic_filename` so cross-file diagnostic
    /// rendering stays consistent.
    #[must_use]
    pub fn to_filename(&self) -> String {
        self.0
            .strip_prefix("file://")
            .map_or_else(|| self.0.clone(), str::to_owned)
    }

    /// Directory containing this URI's file, for use as a
    /// `FilesystemCompilerHost` base path.
    ///
    /// Returns `None` for non-`file:` URIs (`core:` / `wasi:` / `kiln:`
    /// modules don't have a workspace root to resolve relative imports
    /// against). Returns `Some(".")` when the URI's path has no
    /// directory component.
    #[must_use]
    pub fn workspace_root(&self) -> Option<PathBuf> {
        if self.scheme() != UriScheme::File {
            return None;
        }
        let filename = self.to_filename();
        Some(
            Path::new(&filename)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
        )
    }
}

impl From<&str> for Uri {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for Uri {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl std::fmt::Display for Uri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_scheme_and_filename() {
        let u = Uri::new("file:///home/user/foo.wado");
        assert_eq!(u.scheme(), UriScheme::File);
        assert_eq!(u.to_filename(), "/home/user/foo.wado");
        assert_eq!(u.workspace_root(), Some(PathBuf::from("/home/user")));
    }

    #[test]
    fn core_uri_scheme_passthrough() {
        let u = Uri::new("core:cli");
        assert_eq!(u.scheme(), UriScheme::Core);
        assert_eq!(u.to_filename(), "core:cli");
        assert_eq!(u.workspace_root(), None);
    }

    #[test]
    fn wasi_uri_scheme_passthrough() {
        let u = Uri::new("wasi:filesystem/types.wado");
        assert_eq!(u.scheme(), UriScheme::Wasi);
        assert_eq!(u.to_filename(), "wasi:filesystem/types.wado");
        assert_eq!(u.workspace_root(), None);
    }

    #[test]
    fn kiln_uri_scheme_passthrough() {
        let u = Uri::new("kiln:/tmp/.wado-cache/gen.wado");
        assert_eq!(u.scheme(), UriScheme::Kiln);
        assert_eq!(u.to_filename(), "kiln:/tmp/.wado-cache/gen.wado");
        assert_eq!(u.workspace_root(), None);
    }

    #[test]
    fn unknown_scheme_classified_as_other() {
        let u = Uri::new("untitled:1");
        assert_eq!(u.scheme(), UriScheme::Other);
        // `to_filename` falls back to the raw URI for non-file schemes.
        assert_eq!(u.to_filename(), "untitled:1");
        assert_eq!(u.workspace_root(), None);
    }

    #[test]
    fn file_uri_at_root_has_root_workspace() {
        // Pre-existing bug class: `host_for_uri` for `file:///foo.wado`
        // used to yield `PathBuf::from(".")` because `Path::parent` of
        // `/foo.wado` returns `Some("/")` but the prior implementation
        // treated the empty string oddly. Verify the typed accessor.
        let u = Uri::new("file:///foo.wado");
        assert_eq!(u.workspace_root(), Some(PathBuf::from("/")));
    }
}
