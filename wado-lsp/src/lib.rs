mod macros;

mod definition;
mod diagnostics;
mod document_highlight;
pub mod host;
mod hover;
pub mod kiln;
mod location;
mod query;
mod references;
pub mod semantic_tokens;
pub mod server;
#[doc(hidden)]
pub mod test_support;
pub mod text;
pub mod uri;

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;
use wado_compiler::semantics::Semantics;
use wado_compiler::{CompilerHost, Diagnostic as CompilerDiagnostic};

use crate::query::QueryContext;

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
/// Each open document keeps a lazily-computed [`Snapshot`] bundling the
/// `Semantics` produced by `semantics_with_invocations` and the
/// `CompilerDiagnostic`s emitted during that run. Every query —
/// diagnostics included — consumes the cache, so one document version
/// triggers at most one semantics pass. Mutators
/// (`update_document` / `close_document`) invalidate every document's
/// snapshot: cross-file imports mean editing `bar.wado` may have
/// changed what `foo.wado` resolves to, so per-document invalidation
/// would silently return stale answers for `foo.wado`.
pub struct Engine {
    documents: IndexMap<String, Document>,
    /// Position encoding negotiated with the LSP client. Defaults to
    /// `utf-16` per LSP 3.18 §general.positionEncodings; the stdio server
    /// updates this from the `initialize` request before dispatching any
    /// position-bearing query.
    position_encoding: PositionEncoding,
}

/// One semantics pass over a document. Bundles the analysis result with
/// the diagnostics emitted during the same pass so `Engine::diagnostics`
/// returns from cache instead of re-running it.
///
/// Internal to the crate: external consumers reach the underlying
/// `Semantics` through the typed query methods on [`Engine`] (`definition`,
/// `hover`, …) rather than the raw bundle.
pub(crate) struct Snapshot {
    pub(crate) sem: Semantics,
    pub(crate) diagnostics: Vec<CompilerDiagnostic>,
}

struct Document {
    text: String,
    /// Cached snapshot for the current `text`. Cleared whenever any
    /// document is updated/closed, so cross-file edits don't leave a
    /// stale `Semantics` against changed imports.
    snapshot: RefCell<Option<Rc<Snapshot>>>,
}

impl Document {
    fn new(text: String) -> Self {
        Self {
            text,
            snapshot: RefCell::new(None),
        }
    }

    fn replace_text(&mut self, text: String) {
        self.text = text;
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
        self.invalidate_all_snapshots();
        self.documents.insert(uri.to_string(), Document::new(text));
    }

    pub fn update_document(&mut self, uri: &str, text: String) {
        self.invalidate_all_snapshots();
        match self.documents.get_mut(uri) {
            Some(doc) => doc.replace_text(text),
            None => {
                self.documents.insert(uri.to_string(), Document::new(text));
            }
        }
    }

    pub fn close_document(&mut self, uri: &str) {
        self.invalidate_all_snapshots();
        self.documents.shift_remove(uri);
    }

    /// Drop every cached snapshot. Called whenever document state changes,
    /// so a cached `Semantics` never out-lives the imported modules it
    /// resolved against. Cheap when nothing was cached.
    fn invalidate_all_snapshots(&mut self) {
        for (_, doc) in &mut self.documents {
            doc.snapshot.get_mut().take();
        }
    }

    /// Get the source text for an open document.
    #[must_use]
    pub fn get_document(&self, uri: &str) -> Option<&str> {
        self.documents.get(uri).map(|d| d.text.as_str())
    }

    /// Compute (or reuse) a [`Snapshot`] for the given document.
    ///
    /// On a cache hit returns the same `Rc` so call sites that issue
    /// back-to-back queries on the same document version pay the
    /// semantics pipeline's cost only once. The snapshot also captures
    /// every `CompilerDiagnostic` emitted during that pass, so
    /// `diagnostics` can answer from the same cache without re-running
    /// the pipeline.
    pub(crate) async fn snapshot<H: CompilerHost>(
        &self,
        uri: &str,
        host: &H,
    ) -> Option<Rc<Snapshot>> {
        let doc = self.documents.get(uri)?;
        // Drop the borrow before the `await` below — the `if let`
        // scrutinee is a temporary that goes out of scope at the end of
        // this block.
        if let Some(cached) = doc.snapshot.borrow().clone() {
            return Some(cached);
        }
        let filename = Uri::new(uri).to_filename();
        let collecting_host = DiagnosticCollector::new(host);
        let invocations = kiln::prepare_invocations(&filename, &doc.text, &collecting_host);
        let sem = wado_compiler::semantics::semantics_with_invocations(
            &doc.text,
            &collecting_host,
            Some(&filename),
            invocations,
        )
        .await;
        let snapshot = Rc::new(Snapshot {
            sem,
            diagnostics: collecting_host.take_diagnostics(),
        });
        *doc.snapshot.borrow_mut() = Some(snapshot.clone());
        Some(snapshot)
    }

    /// Build a [`QueryContext`] for the given document and hand it to
    /// `f`. Returns `default` when the document is closed or its
    /// semantics snapshot cannot be computed.
    ///
    /// Every position-bearing query — `definition`, `hover`, `references`,
    /// `document_highlight` — goes through here. The shared closure body
    /// keeps the snapshot cache lookup and `QueryContext` construction
    /// in one place; feature functions take a single `&QueryContext`
    /// instead of threading the five inputs by hand.
    async fn with_query_ctx<H, F, R>(&self, uri: &str, host: &H, default: R, f: F) -> R
    where
        H: CompilerHost,
        F: FnOnce(&QueryContext<'_>) -> R,
    {
        let Some(snapshot) = self.snapshot(uri, host).await else {
            return default;
        };
        let Some(doc_text) = self.documents.get(uri).map(|d| d.text.as_str()) else {
            return default;
        };
        let ctx = QueryContext {
            sem: &snapshot.sem,
            source: doc_text,
            uri,
            encoding: self.position_encoding,
        };
        f(&ctx)
    }

    /// Find the definition of the symbol at the given position.
    pub async fn definition<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        host: &H,
    ) -> Option<DefinitionResult> {
        self.with_query_ctx(uri, host, None, |ctx| {
            definition::find_definition(ctx, position)
        })
        .await
    }

    /// Compute hover information for the symbol at the given position.
    pub async fn hover<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        host: &H,
    ) -> Option<HoverResult> {
        self.with_query_ctx(uri, host, None, |ctx| hover::find_hover(ctx, position))
            .await
    }

    /// Find every reference to the symbol named at the given position.
    pub async fn references<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        include_declaration: bool,
        host: &H,
    ) -> Vec<ReferenceLocation> {
        self.with_query_ctx(uri, host, Vec::new(), |ctx| {
            references::find_references(ctx, position, include_declaration)
        })
        .await
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
        self.with_query_ctx(uri, host, Vec::new(), |ctx| {
            document_highlight::document_highlight(ctx, position)
        })
        .await
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
        // `Uri::scheme` returned Core/Wasi, so the URI is guaranteed to
        // contain `:`; the helper unwraps the same split rather than
        // doing it twice.
        let rest = parsed
            .rest()
            .expect("scheme matched, so `:` is present in the URI");
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
    /// Reads from the snapshot cache populated by [`Engine::snapshot`].
    /// Annotate runs at most once per document version regardless of which
    /// queries the client issued first. Each diagnostic's column is
    /// re-encoded against the source whose file matches its
    /// `span.file` — cross-file diagnostics keep the compiler's codepoint
    /// columns, the entry document is re-expressed in the negotiated
    /// position encoding.
    pub async fn diagnostics<H: CompilerHost>(&self, uri: &str, host: &H) -> Vec<Diagnostic> {
        let Some(snapshot) = self.snapshot(uri, host).await else {
            return Vec::new();
        };
        let filename = Uri::new(uri).to_filename();
        let encoding = self.position_encoding;
        let entry_text = self.documents.get(uri).map(|d| d.text.as_str());
        snapshot
            .diagnostics
            .iter()
            .filter_map(|d| {
                // Only re-encode against the entry document's text when
                // the diagnostic actually points at it. Diagnostics from
                // imported modules carry codepoint columns relative to
                // the OTHER module's source, which we don't have on
                // hand; passing `None` keeps them as raw codepoint
                // indices (correct under UTF-32 / ASCII).
                let source = d
                    .span
                    .as_ref()
                    .filter(|s| s.file == filename)
                    .and(entry_text);
                diagnostics::from_compiler_diagnostic(d, uri, source, encoding)
            })
            .collect()
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

/// A `CompilerHost` wrapper that forwards file loading and diagnostic
/// emission to an inner host, while also capturing every emitted
/// diagnostic into an internal buffer. Forwarding preserves the inner
/// host's side effects (logging, error counting) so wrapping is
/// observationally invisible to it.
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
        // Capture for the snapshot cache, then forward so the inner host's
        // own side effects (e.g. CLI stderr logging) still happen.
        self.diagnostics.lock().unwrap().push(diagnostic.clone());
        self.inner.emit_diagnostic(diagnostic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MapHost;
    use futures::executor::block_on;

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
        // Populate the cache via snapshot() first, then verify
        // update_document drops it. Without populating, a tautological
        // assertion would pass even if the invalidation logic were
        // removed.
        let mut engine = Engine::new();
        engine.open_document("file:///t.wado", "fn a() {}".to_string());
        let host = MapHost::empty();
        let _ = block_on(engine.snapshot("file:///t.wado", &host)).expect("snapshot");
        assert!(
            engine
                .documents
                .get("file:///t.wado")
                .unwrap()
                .snapshot
                .borrow()
                .is_some(),
            "snapshot should populate the cache",
        );
        engine.update_document("file:///t.wado", "fn b() {}".to_string());
        assert!(
            engine
                .documents
                .get("file:///t.wado")
                .unwrap()
                .snapshot
                .borrow()
                .is_none(),
            "update should invalidate the cache",
        );
    }

    #[test]
    fn test_update_invalidates_cross_document_snapshots() {
        // Cross-file imports mean a cached Semantics for foo.wado may
        // depend on bar.wado's text. Editing bar.wado must invalidate
        // foo.wado's cache, otherwise hover/definition return stale
        // type info indefinitely.
        let mut engine = Engine::new();
        engine.open_document("file:///foo.wado", "fn a() {}".to_string());
        engine.open_document("file:///bar.wado", "fn b() {}".to_string());
        let host = MapHost::empty();
        let _ = block_on(engine.snapshot("file:///foo.wado", &host)).expect("foo snapshot");
        let _ = block_on(engine.snapshot("file:///bar.wado", &host)).expect("bar snapshot");
        engine.update_document("file:///bar.wado", "fn bb() {}".to_string());
        assert!(
            engine
                .documents
                .get("file:///foo.wado")
                .unwrap()
                .snapshot
                .borrow()
                .is_none(),
            "editing bar.wado must invalidate foo.wado's snapshot",
        );
    }

    #[test]
    fn diagnostic_collector_forwards_to_inner_host() {
        // The collector wraps the user-provided host. Inner host's
        // emit_diagnostic must still receive every diagnostic — its
        // side effects (logging, error counting) would otherwise vanish
        // silently when Engine::snapshot wraps the host.
        let text = "fn f() -> i32 { return \"oops\"; }";
        let host = MapHost::single("/t.wado", text);
        let mut engine = Engine::new();
        engine.open_document("file:///t.wado", text.to_string());
        let _ = block_on(engine.snapshot("file:///t.wado", &host)).expect("snapshot");
        assert!(
            !host.emitted().is_empty(),
            "inner host should have received forwarded diagnostics",
        );
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
