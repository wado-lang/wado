mod definition;
mod diagnostics;
mod hover;
pub mod semantic_tokens;

use indexmap::IndexMap;
use wado_compiler::{CompilerHost, CompilerOptions, Diagnostic as CompilerDiagnostic, LogLevel};

pub use definition::DefinitionResult;
pub use diagnostics::{Diagnostic, Position, Range, Severity};
pub use hover::HoverResult;

/// Protocol-agnostic language service engine.
///
/// Manages open documents and provides language service queries (diagnostics, etc.).
/// IO-free: all file access goes through `CompilerHost`. Compatible with `wasm32-unknown-unknown`.
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
    /// Returns the location of the definition, or `None` if not found.
    /// Currently resolves definitions within the same file only (AST-level).
    #[must_use]
    pub fn definition(&self, uri: &str, position: Position) -> Option<DefinitionResult> {
        let source = self.documents.get(uri)?;
        definition::find_definition(source, position, uri)
    }

    /// Compute hover information for the symbol at the given position.
    ///
    /// Returns markdown content describing the symbol, or `None` if not found.
    #[must_use]
    pub fn hover(&self, uri: &str, position: Position) -> Option<HoverResult> {
        let source = self.documents.get(uri)?;
        hover::find_hover(source, position)
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
    /// Runs the compiler with a silent host that collects diagnostics without printing.
    /// The `host` is used to load imported modules from the filesystem (or any other source).
    pub async fn diagnostics<H: CompilerHost>(&self, uri: &str, host: &H) -> Vec<Diagnostic> {
        let Some(source) = self.documents.get(uri) else {
            return Vec::new();
        };

        let filename = uri_to_filename(uri);
        let options = CompilerOptions {
            log_level: Some(LogLevel::Off),
            ..CompilerOptions::default()
        };

        // Compile and collect diagnostics. We don't care about the result —
        // errors are captured by the host via emit_diagnostic.
        let collecting_host = DiagnosticCollector::new(host);
        let _ =
            wado_compiler::compile_with_options(source, &collecting_host, Some(&filename), options)
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
}
