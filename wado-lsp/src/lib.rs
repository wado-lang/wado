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

use indexmap::IndexMap;
use wado_compiler::{CompilerHost, Diagnostic as CompilerDiagnostic};

pub use definition::DefinitionResult;
pub use diagnostics::{Diagnostic, Position, Range, Severity};
pub use document_highlight::{DocumentHighlight, HighlightKind};
pub use host::FilesystemCompilerHost;
pub use hover::{HoverResult, MarkupContent, MarkupKind};
pub use references::ReferenceLocation;

/// Language service engine backing both the `wado-lsp` binary and direct
/// library consumers.
///
/// Manages open documents and answers LSP-style queries (diagnostics, hover,
/// definition, references, document highlight, semantic tokens). `Engine`
/// itself performs no I/O: every query takes a `&impl CompilerHost`, so the
/// caller decides how imported modules are loaded.
pub struct Engine {
    /// Open documents: URI → source text
    documents: IndexMap<String, String>,
}

impl Engine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            documents: IndexMap::new(),
        }
    }

    pub fn open_document(&mut self, uri: &str, text: String) {
        self.documents.insert(uri.to_string(), text);
    }

    pub fn update_document(&mut self, uri: &str, text: String) {
        self.documents.insert(uri.to_string(), text);
    }

    pub fn close_document(&mut self, uri: &str) {
        self.documents.shift_remove(uri);
    }

    /// Get the source text for an open document.
    #[must_use]
    pub fn get_document(&self, uri: &str) -> Option<&str> {
        self.documents.get(uri).map(String::as_str)
    }

    /// Find the definition of the symbol at the given position.
    ///
    /// Runs the compiler's `annotate` pipeline so cross-file imports resolve
    /// correctly. The provided `host` is used to load imported modules.
    pub async fn definition<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        host: &H,
    ) -> Option<DefinitionResult> {
        let source = self.documents.get(uri)?;
        definition::find_definition(source, position, uri, host).await
    }

    /// Compute hover information for the symbol at the given position.
    ///
    /// Runs the compiler's `annotate` pipeline to access resolved type
    /// information; for symbols from imported modules, the `host` is used to
    /// load their sources.
    pub async fn hover<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        host: &H,
    ) -> Option<HoverResult> {
        let source = self.documents.get(uri)?;
        hover::find_hover(source, position, uri, host).await
    }

    /// Find every reference to the symbol named at the given position.
    ///
    /// When `include_declaration` is true, the defining occurrence is included
    /// in the result. Returns an empty vector when the cursor is not on a
    /// known symbol or when the document is not open.
    pub async fn references<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        include_declaration: bool,
        host: &H,
    ) -> Vec<ReferenceLocation> {
        let Some(source) = self.documents.get(uri) else {
            return Vec::new();
        };
        references::find_references(source, position, uri, include_declaration, host).await
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
        let Some(source) = self.documents.get(uri) else {
            return Vec::new();
        };
        document_highlight::document_highlight(source, position, uri, host).await
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
        let (scheme, rest) = uri.split_once(':')?;
        if scheme != "core" && scheme != "wasi" {
            return None;
        }
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
        let Some(source) = self.documents.get(uri) else {
            return Vec::new();
        };
        let tokens = semantic_tokens::compute(source);
        semantic_tokens::delta_encode(&tokens)
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
        let Some(source) = self.documents.get(uri) else {
            return Vec::new();
        };

        let filename = uri_to_filename(uri);
        let collecting_host = DiagnosticCollector::new(host);
        let invocations = kiln::prepare_invocations(&filename, source, &collecting_host);
        let _ = wado_compiler::annotate_with_invocations(
            source,
            &collecting_host,
            Some(&filename),
            invocations,
        )
        .await;

        collecting_host
            .take_diagnostics()
            .into_iter()
            .filter_map(|d| diagnostics::from_compiler_diagnostic(&d, uri))
            .collect()
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract a filename from a URI for compiler error messages.
///
/// For `file:///path/to/foo.wado`, returns `foo.wado` (or the full path).
/// For non-file URIs, returns the URI as-is.
fn uri_to_filename(uri: &str) -> String {
    if let Some(path) = uri.strip_prefix("file://") {
        path.to_string()
    } else {
        uri.to_string()
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
    fn test_uri_to_filename() {
        assert_eq!(
            uri_to_filename("file:///home/user/test.wado"),
            "/home/user/test.wado"
        );
        assert_eq!(uri_to_filename("untitled:1"), "untitled:1");
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
