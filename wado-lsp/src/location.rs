//! Shared helpers for translating between compiler positions/symbols and LSP
//! types. Used by go-to-definition, find-references, and document highlight.
//!
//! URI handling lives in [`crate::uri`]; position-encoding-aware span
//! conversion lives in [`crate::text`]. This module only retains the
//! `ModuleSource → URI` helpers that read the compiler's per-module
//! metadata.

use wado_compiler::annotate::Annotated;
use wado_compiler::module_source::ModuleSource;
use wado_compiler::symbol::Symbol;
use wado_compiler::token::Span;

use crate::diagnostics::Range;
use crate::text::PositionEncoding;
use crate::uri::{Uri, UriScheme};

/// Resolve the URI of a module relative to the requesting document's URI.
///
/// `request_uri` provides the base directory used to anchor `./relative.wado`
/// imports. Returns the request URI itself when the module is the entry point.
pub(crate) fn module_uri(
    annotated: &Annotated,
    module: &ModuleSource,
    request_uri: &str,
) -> Option<String> {
    if module == &annotated.entry_module_source {
        return Some(request_uri.to_string());
    }
    match module {
        ModuleSource::EntryPoint { filename } => Some(filename_to_uri(filename)),
        ModuleSource::Local { path } => Some(resolve_local_uri(path, request_uri)),
        ModuleSource::Core { name } => Some(format!("core:{name}")),
        ModuleSource::Wasi { interface } => Some(format!("wasi:{interface}")),
        ModuleSource::Remote { url } => Some(url.to_string()),
        // Kiln-redirected modules already carry a fully-qualified URI;
        // hand it to the LSP client unchanged.
        ModuleSource::Redirected { uri } => Some(uri.to_string()),
        // Wasm assets (`.wat`/`.wasm` imported via `with { type: ... }`)
        // expose their canonical path; opening these in the editor isn't
        // useful (binary `.wasm`) and stdlib `.wat` paths can't be served
        // until `workspace/textDocumentContent` lands. Emit the path so
        // CLI consumers can still see the reference site.
        ModuleSource::Wasm { path, .. } => Some(path.to_string()),
    }
}

fn filename_to_uri(filename: &str) -> String {
    if filename.starts_with("file://") {
        filename.to_string()
    } else if filename.starts_with('/') {
        format!("file://{filename}")
    } else {
        filename.to_string()
    }
}

fn resolve_local_uri(module_path: &str, request_uri: &str) -> String {
    if module_path.starts_with('/') || module_path.starts_with("file://") {
        return filename_to_uri(module_path);
    }
    // Local imports anchor at the request URI's directory. Non-file
    // request URIs (`core:`, `wasi:`, `kiln:`, `untitled:`) cannot
    // anchor a relative path; fall back to the literal module path so
    // the result is still navigable in the LSP client even though it
    // won't open a real file.
    let request_uri_typed = Uri::new(request_uri);
    let normalized = module_path.strip_prefix("./").unwrap_or(module_path);
    if request_uri_typed.scheme() != UriScheme::File {
        return filename_to_uri(normalized);
    }
    let request_path = request_uri_typed.to_filename();
    let base_dir = request_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    // When the request path is rooted at "/" (e.g. "/test.wado"), rsplit_once
    // yields an empty base_dir, so preserve the leading slash explicitly.
    if base_dir.is_empty() {
        if request_path.starts_with('/') {
            filename_to_uri(&format!("/{normalized}"))
        } else {
            filename_to_uri(normalized)
        }
    } else {
        filename_to_uri(&format!("{base_dir}/{normalized}"))
    }
}

/// URI of the module defining `symbol`, relative to the requesting document.
pub(crate) fn symbol_uri(
    annotated: &Annotated,
    symbol: &Symbol,
    request_uri: &str,
) -> Option<String> {
    module_uri(annotated, &symbol.defined_at.module, request_uri)
}

/// Convert a compiler `Span` (1-based byte column) to an LSP `Range` in
/// the negotiated `encoding`. Pass `Some(source)` for spans inside the
/// request document so non-ASCII columns survive the round-trip; pass
/// `None` only when the source text is not available for the span's
/// module (cross-file references), and accept the ASCII-only correctness
/// implied by that.
pub(crate) fn span_to_range(
    span: &Span,
    source: Option<&str>,
    encoding: PositionEncoding,
) -> Range {
    crate::text::span_to_range(span, source, encoding)
}
