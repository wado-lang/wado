//! Typed URI helpers used by both the engine and the stdio server.
//!
//! All URI parsing for the LSP lives here:
//! - `Uri::from_str` recognises the `file:`, `core:`, `wasi:`, and `kiln:`
//!   schemes the wado compiler emits via `module_uri`.
//! - `Uri::to_filename` strips `file://` for compiler-side diagnostic
//!   filenames and falls back to the raw string for non-file schemes
//!   (matching the behaviour of `ModuleSource::source_path`).
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
/// `WEP 2026-04-12 §"The `kiln:` URI scheme"` for `kiln:` redirects.
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

    /// A `file:` URI naming `path`, percent-encoded.
    ///
    /// The counterpart to [`Self::to_filename`], for callers holding a real
    /// path rather than a wire URI — `wado query`, opening a file named on
    /// the command line.
    #[must_use]
    pub fn from_file_path(path: &Path) -> Self {
        Self(format!(
            "file://{}",
            percent_encode_path(&path.to_string_lossy())
        ))
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

    /// Filename suitable for compiler-side diagnostics (`Logger::set_file`,
    /// `DiagnosticSpan::file`).
    ///
    /// `file://` URIs lose the scheme and are percent-decoded: clients send
    /// rfc3986-encoded URIs, and every path this crate derives from one has
    /// to name a real directory.
    ///
    /// Every other scheme returns the raw URI, matching
    /// `ModuleSource::source_path`. Those are compiler-minted and never
    /// encoded, so decoding them would only corrupt a literal `%`.
    #[must_use]
    pub fn to_filename(&self) -> String {
        self.0
            .strip_prefix("file://")
            .map_or_else(|| self.0.clone(), percent_decode)
    }

    /// Path / opaque component after the scheme delimiter `:`.
    ///
    /// Returns `None` when the URI has no `:` (i.e. [`Self::scheme`]
    /// returns [`UriScheme::Other`]). Callers that have already
    /// classified the scheme as one of the known kinds may safely
    /// `.expect()` on the result.
    #[must_use]
    pub fn rest(&self) -> Option<&str> {
        self.0.split_once(':').map(|(_, r)| r)
    }

    /// Directory containing this URI's file, for use as a
    /// `FilesystemCompilerHost` base path.
    ///
    /// Returns `None` for non-`file:` URIs (`core:` / `wasi:` / `kiln:`
    /// modules don't have a workspace root to resolve relative imports
    /// against) AND for malformed `file://` URIs that carry no path —
    /// silently CWD-rooting a bad URI would let buggy clients trigger
    /// reads against the LSP process's working directory. Returns
    /// `Some(PathBuf::from("/"))` for absolute-root paths
    /// (e.g. `file:///foo.wado`).
    #[must_use]
    pub fn workspace_root(&self) -> Option<PathBuf> {
        if self.scheme() != UriScheme::File {
            return None;
        }
        let filename = self.to_filename();
        if filename.is_empty() {
            // `file://` with no path: refuse rather than CWD-root.
            return None;
        }
        Some(
            Path::new(&filename)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
        )
    }
}

/// Decode rfc3986 percent-escapes (`%XX`) in `s`.
///
/// Decoding is byte-level with the result re-validated as UTF-8, so a
/// character spread over several escapes (`%E3%81%82`) round-trips. A
/// malformed escape or non-UTF-8 result leaves the input untouched: a path we
/// cannot decode is better handed to the filesystem verbatim than mangled.
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let Some(hex) = bytes.get(i + 1..i + 3) else {
                return s.to_owned();
            };
            let Some(byte) = hex_pair(hex[0], hex[1]) else {
                return s.to_owned();
            };
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

/// Percent-encode a filesystem path for use as a `file:` URI body — the
/// inverse of [`percent_decode`]. Prefer [`Uri::from_file_path`] when building
/// a whole URI.
///
/// Unreserved characters, sub-delims, and the path-structural `/ : @` stay
/// verbatim (rfc3986 §3.3), matching what clients emit; everything else,
/// including `%` itself, is escaped.
#[must_use]
pub fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if is_path_safe(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(byte >> 4)] as char);
            out.push(HEX[usize::from(byte & 0x0f)] as char);
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn is_path_safe(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        // unreserved
        || matches!(b, b'-' | b'.' | b'_' | b'~')
        // sub-delims
        || matches!(b, b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';' | b'=')
        // path structure
        || matches!(b, b'/' | b':' | b'@')
}

fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    Some(hex_digit(hi)? << 4 | hex_digit(lo)?)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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

    #[test]
    fn malformed_file_uri_without_path_has_no_workspace_root() {
        // `file://` (no path component) used to silently CWD-root the
        // FilesystemCompilerHost. That let a buggy client read
        // arbitrary files under the LSP process's cwd via relative
        // imports off the bad URI. The typed accessor refuses such
        // URIs.
        assert_eq!(Uri::new("file://").workspace_root(), None);
    }

    #[test]
    fn file_uri_percent_escapes_are_decoded() {
        // Clients percent-encode every reserved character.
        let u = Uri::new("file:///home/user/my%20project/main.wado");
        assert_eq!(u.to_filename(), "/home/user/my project/main.wado");
        assert_eq!(
            u.workspace_root(),
            Some(PathBuf::from("/home/user/my project")),
        );
    }

    #[test]
    fn multi_byte_percent_escapes_decode_as_utf8() {
        // A non-ASCII directory arrives as one escape per UTF-8 byte.
        let u = Uri::new("file:///home/%E3%81%82/main.wado");
        assert_eq!(u.to_filename(), "/home/あ/main.wado");
    }

    #[test]
    fn encoded_percent_round_trips() {
        let u = Uri::new("file:///home/100%25/main.wado");
        assert_eq!(u.to_filename(), "/home/100%/main.wado");
    }

    #[test]
    fn malformed_escape_is_left_verbatim() {
        let u = Uri::new("file:///home/%zz/main.wado");
        assert_eq!(u.to_filename(), "/home/%zz/main.wado");
        let truncated = Uri::new("file:///home/trailing%2");
        assert_eq!(truncated.to_filename(), "/home/trailing%2");
    }

    #[test]
    fn non_file_schemes_are_not_decoded() {
        assert_eq!(
            Uri::new("kiln:/tmp/a%20b.wado").to_filename(),
            "kiln:/tmp/a%20b.wado"
        );
    }

    #[test]
    fn encode_escapes_only_what_a_uri_needs() {
        assert_eq!(
            percent_encode_path("/home/my project"),
            "/home/my%20project"
        );
        assert_eq!(percent_encode_path("/home/あ"), "/home/%E3%81%82");
        assert_eq!(percent_encode_path("/home/100%"), "/home/100%25");
        assert_eq!(percent_encode_path("/a?b#c[d]"), "/a%3Fb%23c%5Bd%5D");
        // Path structure and sub-delims stay legible.
        assert_eq!(percent_encode_path("/a/b:c@d+e,f"), "/a/b:c@d+e,f");
    }

    #[test]
    fn encode_and_decode_round_trip() {
        for path in [
            "/plain/main.wado",
            "/my project/main.wado",
            "/あ/main.wado",
            "/100%/main.wado",
            "/weird ?#[]/main.wado",
        ] {
            let uri = Uri::new(format!("file://{}", percent_encode_path(path)));
            assert_eq!(uri.to_filename(), path, "round trip failed for {path:?}");
        }
    }

    #[test]
    fn rest_returns_part_after_colon() {
        assert_eq!(Uri::new("core:cli").rest(), Some("cli"));
        assert_eq!(
            Uri::new("file:///home/x.wado").rest(),
            Some("///home/x.wado"),
        );
        assert_eq!(Uri::new("untitled:1").rest(), Some("1"));
        assert_eq!(Uri::new("no-colon").rest(), None);
    }
}
