mod definition;
mod diagnostics;
mod document_highlight;
pub mod host;
mod hover;
pub mod kiln;
mod location;
mod references;
pub mod semantic_tokens;
pub mod server;
pub mod text;
pub mod uri;

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;
use wado_compiler::annotate::Annotated;
use wado_compiler::{CompilerHost, Diagnostic as CompilerDiagnostic};

pub use definition::DefinitionResult;
pub use diagnostics::{Diagnostic, Position, Range, Severity};
pub use document_highlight::{DocumentHighlight, HighlightKind};
pub use host::FilesystemCompilerHost;
pub use hover::{HoverResult, MarkupContent, MarkupKind};
pub use references::ReferenceLocation;
pub use text::PositionEncoding;
pub use uri::{Uri, UriScheme};

/// Language service engine backing both the `wado-lsp` binary and direct
/// library consumers.
///
/// Manages open documents and answers LSP-style queries (diagnostics, hover,
/// definition, references, document highlight, semantic tokens). `Engine`
/// itself performs no I/O: every query takes a `&impl CompilerHost`, so the
/// caller decides how imported modules are loaded.
///
/// ## Snapshot cache
///
/// Each open document keeps a lazily-computed `Rc<Annotated>` produced by
/// `wado_compiler::annotate_with_invocations`. The snapshot is invalidated
/// on `update_document` / `close_document`; back-to-back queries on the
/// same document version share one annotate run. Hand the same `host` to
/// each query — the cache is keyed by document text only, so a fresh host
/// per query is fine but won't make annotate run again.
pub struct Engine {
    documents: IndexMap<String, Document>,
    /// Position encoding negotiated with the LSP client. Defaults to
    /// `utf-16` per LSP 3.18 §general.positionEncodings; the stdio server
    /// updates this from the `initialize` request before dispatching any
    /// position-bearing query.
    position_encoding: PositionEncoding,
}

struct Document {
    text: String,
    /// Last `version` reported by the client (`didOpen` / `didChange`).
    /// Tracked for future incremental sync; not currently consumed.
    #[allow(dead_code)]
    version: Option<i32>,
    /// Cached `Annotated` for the current `text`. Cleared whenever
    /// `text` is replaced, so any borrow held outside the cache (via
    /// `Rc::clone`) survives the next edit without aliasing the old
    /// snapshot's interior `RefCell`s.
    snapshot: RefCell<Option<Rc<Annotated>>>,
}

impl Document {
    fn new(text: String, version: Option<i32>) -> Self {
        Self {
            text,
            version,
            snapshot: RefCell::new(None),
        }
    }

    fn invalidate(&mut self, text: String, version: Option<i32>) {
        self.text = text;
        self.version = version;
        self.snapshot.get_mut().take();
    }
}

impl Engine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            documents: IndexMap::new(),
            position_encoding: PositionEncoding::default(),
        }
    }

    /// Set the LSP position encoding negotiated during `initialize`.
    /// The stdio server calls this once before dispatching position-bearing
    /// requests; library consumers that drive `Engine` directly default to
    /// `utf-16` to match the spec.
    pub fn set_position_encoding(&mut self, encoding: PositionEncoding) {
        self.position_encoding = encoding;
    }

    #[must_use]
    pub fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    pub fn open_document(&mut self, uri: &str, text: String) {
        self.open_document_versioned(uri, text, None);
    }

    pub fn open_document_versioned(&mut self, uri: &str, text: String, version: Option<i32>) {
        self.documents
            .insert(uri.to_string(), Document::new(text, version));
    }

    pub fn update_document(&mut self, uri: &str, text: String) {
        self.update_document_versioned(uri, text, None);
    }

    pub fn update_document_versioned(&mut self, uri: &str, text: String, version: Option<i32>) {
        match self.documents.get_mut(uri) {
            Some(doc) => doc.invalidate(text, version),
            None => {
                self.documents
                    .insert(uri.to_string(), Document::new(text, version));
            }
        }
    }

    pub fn close_document(&mut self, uri: &str) {
        self.documents.shift_remove(uri);
    }

    /// Get the source text for an open document.
    #[must_use]
    pub fn get_document(&self, uri: &str) -> Option<&str> {
        self.documents.get(uri).map(|d| d.text.as_str())
    }

    /// Compute (or reuse) an `Annotated` snapshot for the given document.
    ///
    /// On a cache hit returns the same `Rc` so call sites that issue
    /// back-to-back queries on the same document version pay annotate's
    /// cost only once.
    pub async fn snapshot<H: CompilerHost>(&self, uri: &str, host: &H) -> Option<Rc<Annotated>> {
        let doc = self.documents.get(uri)?;
        if let Some(cached) = doc.snapshot.borrow().as_ref() {
            return Some(cached.clone());
        }
        let filename = Uri::new(uri).to_filename();
        let invocations = kiln::prepare_invocations(&filename, &doc.text, host);
        let annotated = wado_compiler::annotate::annotate_with_invocations(
            &doc.text,
            host,
            Some(&filename),
            invocations,
        )
        .await;
        let rc = Rc::new(annotated);
        // The borrow returned above was dropped before the `await`. Any
        // concurrently-arrived snapshot call would have raced us, but the
        // dispatcher is single-tasked so there is no real contention; if a
        // race ever does happen the worst case is one redundant annotate.
        *doc.snapshot.borrow_mut() = Some(rc.clone());
        Some(rc)
    }

    /// Find the definition of the symbol at the given position.
    pub async fn definition<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        host: &H,
    ) -> Option<DefinitionResult> {
        let annotated = self.snapshot(uri, host).await?;
        let doc_text = self.documents.get(uri)?.text.as_str();
        definition::find_definition(&annotated, doc_text, position, uri, self.position_encoding)
    }

    /// Compute hover information for the symbol at the given position.
    pub async fn hover<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        host: &H,
    ) -> Option<HoverResult> {
        let annotated = self.snapshot(uri, host).await?;
        let doc_text = self.documents.get(uri)?.text.as_str();
        hover::find_hover(&annotated, doc_text, position, uri, self.position_encoding)
    }

    /// Find every reference to the symbol named at the given position.
    pub async fn references<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        include_declaration: bool,
        host: &H,
    ) -> Vec<ReferenceLocation> {
        let Some(annotated) = self.snapshot(uri, host).await else {
            return Vec::new();
        };
        let Some(doc_text) = self.documents.get(uri).map(|d| d.text.as_str()) else {
            return Vec::new();
        };
        references::find_references(
            &annotated,
            doc_text,
            position,
            uri,
            include_declaration,
            self.position_encoding,
        )
    }

    /// Find every occurrence of the symbol named at the given position
    /// **inside the requested document**. References from other files are
    /// filtered out — see [`Engine::references`] for the cross-file lookup.
    pub async fn document_highlight<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        host: &H,
    ) -> Vec<DocumentHighlight> {
        let Some(annotated) = self.snapshot(uri, host).await else {
            return Vec::new();
        };
        let Some(doc_text) = self.documents.get(uri).map(|d| d.text.as_str()) else {
            return Vec::new();
        };
        document_highlight::document_highlight(
            &annotated,
            doc_text,
            position,
            uri,
            self.position_encoding,
        )
    }

    /// Resolve a `core:` / `wasi:` URI to its bundled stdlib source.
    ///
    /// Powers `workspace/textDocumentContent` so editors can open
    /// `core:cli` / `wasi:filesystem/types.wado` jump-to-definition targets
    /// even though the source lives only in the compiler's binary.
    /// Tolerates the rfc3986-normalised form `core:/cli` that some clients
    /// emit when round-tripping the URI through their parser.
    #[must_use]
    pub fn text_document_content(&self, uri: &str) -> Option<&'static str> {
        let parsed = Uri::new(uri);
        let scheme = match parsed.scheme() {
            UriScheme::Core => "core",
            UriScheme::Wasi => "wasi",
            _ => return None,
        };
        let (_, rest) = uri.split_once(':')?;
        // Canonical form (`core:cli`) hits get_stdlib_module without an
        // intermediate allocation; only the normalised form (`core:/cli`)
        // needs its slash stripped and the URI re-formed.
        match rest.strip_prefix('/') {
            None => wado_compiler::stdlib::get_stdlib_module(uri),
            Some(path) => wado_compiler::stdlib::get_stdlib_module(&format!("{scheme}:{path}")),
        }
    }

    /// Compute semantic tokens for the given document.
    ///
    /// Returns delta-encoded token data for LSP `textDocument/semanticTokens/full`.
    /// This is a lightweight operation (lex + parse only, no compilation).
    #[must_use]
    pub fn semantic_tokens(&self, uri: &str) -> Vec<u32> {
        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };
        let tokens = semantic_tokens::compute(&doc.text);
        semantic_tokens::delta_encode(&tokens, &doc.text, self.position_encoding)
    }

    /// Compute diagnostics for the given document.
    ///
    /// Runs the compiler's `annotate` pipeline (parse → bind → load → analyze
    /// → resolve) with a silent host that collects diagnostics
    /// without printing. Codegen and downstream phases are intentionally
    /// skipped: they can panic on compiler-internal bugs (e.g. invalid Wasm
    /// emitted from an unusual entry module) and produce nothing useful for
    /// editor feedback even when they succeed. All user-actionable
    /// diagnostics — type errors, undefined symbols, prelude collisions,
    /// effect violations — surface during `annotate`.
    pub async fn diagnostics<H: CompilerHost>(&self, uri: &str, host: &H) -> Vec<Diagnostic> {
        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };

        let filename = Uri::new(uri).to_filename();
        let collecting_host = DiagnosticCollector::new(host);
        let invocations = kiln::prepare_invocations(&filename, &doc.text, &collecting_host);
        wado_compiler::annotate_with_invocations(
            &doc.text,
            &collecting_host,
            Some(&filename),
            invocations,
        )
        .await;

        let encoding = self.position_encoding;
        let text = doc.text.as_str();
        collecting_host
            .take_diagnostics()
            .into_iter()
            .filter_map(|d| diagnostics::from_compiler_diagnostic(&d, uri, Some(text), encoding))
            .collect()
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

/// A `CompilerHost` wrapper that delegates file loading to an inner host
/// while silently collecting all diagnostics.
struct DiagnosticCollector<'a, H> {
    inner: &'a H,
    diagnostics: std::sync::Mutex<Vec<CompilerDiagnostic>>,
}

impl<'a, H> DiagnosticCollector<'a, H> {
    fn new(inner: &'a H) -> Self {
        Self {
            inner,
            diagnostics: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn take_diagnostics(self) -> Vec<CompilerDiagnostic> {
        self.diagnostics.into_inner().unwrap()
    }
}

impl<H: CompilerHost> CompilerHost for DiagnosticCollector<'_, H> {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, wado_compiler::SourceError> {
        self.inner.load_source(path).await
    }

    fn emit_diagnostic(&self, diagnostic: CompilerDiagnostic) {
        self.diagnostics.lock().unwrap().push(diagnostic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_and_close_document() {
        let mut engine = Engine::new();
        engine.open_document("file:///test.wado", "fn run() {}".to_string());
        assert_eq!(
            engine.get_document("file:///test.wado"),
            Some("fn run() {}")
        );
        engine.close_document("file:///test.wado");
        assert_eq!(engine.get_document("file:///test.wado"), None);
    }

    #[test]
    fn test_update_document() {
        let mut engine = Engine::new();
        engine.open_document("file:///test.wado", "let x = 1;".to_string());
        engine.update_document("file:///test.wado", "let x = 2;".to_string());
        assert_eq!(engine.get_document("file:///test.wado"), Some("let x = 2;"));
    }

    #[test]
    fn test_update_invalidates_snapshot_cache() {
        // Snapshot cache survives across queries on the same text but
        // must clear on `update_document`. Without invalidation the next
        // query would see stale `Annotated` against the new text.
        let mut engine = Engine::new();
        engine.open_document("file:///t.wado", "fn a() {}".to_string());
        engine.documents.get("file:///t.wado").unwrap();
        engine.update_document("file:///t.wado", "fn b() {}".to_string());
        let doc = engine.documents.get("file:///t.wado").unwrap();
        assert!(doc.snapshot.borrow().is_none());
    }

    #[test]
    fn text_document_content_resolves_core_module() {
        let engine = Engine::new();
        let text = engine.text_document_content("core:cli").unwrap();
        assert!(text.contains("println"));
    }

    #[test]
    fn text_document_content_resolves_wasi_interface() {
        let engine = Engine::new();
        let text = engine
            .text_document_content("wasi:filesystem/types.wado")
            .unwrap();
        assert!(text.contains("Descriptor"));
    }

    #[test]
    fn text_document_content_tolerates_normalized_uri() {
        let engine = Engine::new();
        assert!(engine.text_document_content("core:/cli").is_some());
    }

    #[test]
    fn text_document_content_rejects_unknown_scheme() {
        let engine = Engine::new();
        assert!(engine.text_document_content("file:///etc/passwd").is_none());
    }

    #[test]
    fn text_document_content_rejects_unknown_module() {
        let engine = Engine::new();
        assert!(engine.text_document_content("core:nonexistent").is_none());
    }
}
