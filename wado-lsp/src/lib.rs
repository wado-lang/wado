mod macros;

mod definition;
mod diagnostics;
mod document_highlight;
pub mod host;
mod hover;
mod inlay_hints;
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
use wado_compiler::kiln::InvocationIndex;
use wado_compiler::semantics::Semantics;
use wado_compiler::{CompilerHost, Diagnostic as CompilerDiagnostic, LogLevel};

use crate::query::QueryContext;

pub use definition::DefinitionResult;
pub use diagnostics::{Diagnostic, DiagnosticTag, Position, Range, Severity};
pub use document_highlight::{DocumentHighlight, HighlightKind};
pub use host::FilesystemCompilerHost;
pub use hover::{HoverResult, MarkupContent, MarkupKind};
pub use inlay_hints::{InlayHint, InlayHintKind};
pub use references::ReferenceLocation;
pub use text::PositionEncoding;
pub use uri::{Uri, UriScheme};

/// Why [`Engine::definition_by_symbol`] could not resolve a notation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolQueryError {
    /// The query context could not be analyzed (loader failure, etc.).
    Unavailable,
    /// The notation's module is not loaded in the analysis context.
    ModuleNotFound,
    /// No matching symbol or member; `available` lists suggestions (the
    /// module's public symbols, or the type's members for a `Type::m` miss).
    NotFound { available: Vec<String> },
    /// The resolved symbol has no usable source location / URI.
    NoLocation,
}

impl std::fmt::Display for SymbolQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("could not analyze the query context"),
            Self::ModuleNotFound => f.write_str("module not found or not loaded"),
            Self::NotFound { .. } => f.write_str("no such symbol in module"),
            Self::NoLocation => f.write_str("symbol has no resolvable source location"),
        }
    }
}

impl std::error::Error for SymbolQueryError {}

/// Language service engine backing both the `wado-lsp` binary and direct
/// library consumers.
///
/// Manages open documents and answers LSP-style queries (diagnostics, hover,
/// definition, references, document highlight, semantic tokens, inlay
/// hints). `Engine` itself performs no I/O: every query takes a `&impl
/// CompilerHost`, so the caller decides how imported modules are loaded.
///
/// ## Snapshot cache
///
/// Each open document keeps a lazily-computed [`Snapshot`] bundling the
/// `Semantics` produced by composing `wado_compiler::parse` →
/// `wado_compiler::load` → `wado_compiler::semantics_of` (see
/// [`build_semantics`]) and the `CompilerDiagnostic`s emitted during
/// that run. Every query —
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
    /// Whether to surface source-level unused / dead-code warnings
    /// (`DeadFunction` / `DeadGlobal` / `TestOnly*`) in `diagnostics`.
    /// On by default, mirroring `CompilerOptions::unused_diagnostics`.
    unused_diagnostics: bool,
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
    /// Precomputed kiln redirect index supplied by a runtime-backed host
    /// (native `wado-cli`, which ran the generators via wasmtime). When
    /// `Some`, [`build_semantics`] uses it verbatim instead of the
    /// consume-only on-disk discovery (`kiln::prepare_invocations`); the
    /// wasm32 LSP leaves it `None` and stays consume-only.
    invocations: Option<InvocationIndex>,
}

impl Document {
    fn new(text: String) -> Self {
        Self {
            text,
            snapshot: RefCell::new(None),
            invocations: None,
        }
    }

    fn with_invocations(text: String, invocations: InvocationIndex) -> Self {
        Self {
            text,
            snapshot: RefCell::new(None),
            invocations: Some(invocations),
        }
    }

    fn replace_text(&mut self, text: String) {
        self.text = text;
        self.snapshot.get_mut().take();
        // The injected index was built for the old text; drop it so a
        // changed document falls back to consume-only rather than
        // redirecting against a stale invocation set.
        self.invocations = None;
    }
}

impl Engine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            documents: IndexMap::new(),
            position_encoding: PositionEncoding::default(),
            unused_diagnostics: true,
        }
    }

    /// Toggle source-level unused / dead-code warnings in `diagnostics`.
    /// Defaults to `true`; mirrors `CompilerOptions::unused_diagnostics`.
    pub fn set_unused_diagnostics(&mut self, enabled: bool) {
        self.unused_diagnostics = enabled;
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

    /// Open a document together with a precomputed kiln redirect index.
    ///
    /// For native hosts that ran the generators themselves (wasmtime-backed
    /// `wado-cli`): the resulting [`InvocationIndex`] redirects
    /// `use { ... } from "<schema>"` clauses to the freshly generated entry
    /// modules, so queries resolve generated symbols even when no on-disk
    /// artifact existed beforehand. Without this, a document falls back to
    /// consume-only on-disk discovery (see [`kiln::prepare_invocations`]).
    pub fn open_document_with_invocations(
        &mut self,
        uri: &str,
        text: String,
        invocations: InvocationIndex,
    ) {
        self.invalidate_all_snapshots();
        self.documents.insert(
            uri.to_string(),
            Document::with_invocations(text, invocations),
        );
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

    /// Whether `uri` analyzes to a non-empty module graph. `false` when the
    /// loader/analysis hard-failed (e.g. a missing import, or a compile-time
    /// codegen module the LSP path can't produce) — `build_semantics` returns
    /// an empty `Semantics` in that case. Used to probe whether importing a
    /// workspace file would sink a combined analysis.
    pub async fn analyzes<H: CompilerHost>(&self, uri: &str, host: &H) -> bool {
        self.snapshot(uri, host)
            .await
            .is_some_and(|s| !s.sem.modules.is_empty())
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
        let invocations = doc.invocations.clone();
        let collecting_host = DiagnosticCollector::new(host);
        // Three-stage composition: parse once, derive kiln invocations
        // from the parsed entry AST, then drive load + semantics_of with
        // the shared parse result. This avoids the second lex+parse of
        // the entry source that the `wado_compiler::semantics`
        // convenience would otherwise do, and threads the
        // InvocationIndex through without the LSP having to know the
        // loader's source-based entry point.
        let sem = build_semantics(&doc.text, &filename, invocations, &collecting_host).await;
        // The Design-B semantic diagnostics (effect / stores / default-purity)
        // are produced from `Semantics`; the LSP builds no TIR, so this is the
        // only place they surface in the editor. `check_semantics` builds the
        // shared effect index once and runs all three.
        let mut diagnostics = collecting_host.take_diagnostics();
        let semantic = wado_compiler::check_semantics(&sem);
        for error in semantic.effects {
            diagnostics.push(error.into());
        }
        for error in semantic.stores {
            diagnostics.push(error.into());
        }
        for error in semantic.purity {
            diagnostics.push(error.into());
        }
        for error in wado_compiler::check_resource_moves_semantic(&sem) {
            diagnostics.push(error.into());
        }
        let snapshot = Rc::new(Snapshot { sem, diagnostics });
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

    /// Snapshot `uri` and resolve a [symbol
    /// notation](wado_compiler::symbol_notation) to its [`Definition`].
    ///
    /// Shared by the name-based query methods. `uri` must be an open document
    /// that loads the notation's module (typically a synthetic entry that
    /// `use`s it); the module resolves relative to that entry, so relative
    /// paths anchor at its directory while `core:` / `wasi:` modules are
    /// location-independent. On [`SymbolQueryError::NotFound`] the error
    /// carries the module's public symbol names as suggestions.
    async fn resolve_symbol_def<H: CompilerHost>(
        &self,
        uri: &str,
        notation: &wado_compiler::symbol_notation::SymbolNotation,
        public_only: bool,
        host: &H,
    ) -> Result<(Rc<Snapshot>, wado_compiler::Definition), SymbolQueryError> {
        use wado_compiler::SymbolResolveError;
        let snapshot = self
            .snapshot(uri, host)
            .await
            .ok_or(SymbolQueryError::Unavailable)?;
        match snapshot.sem.resolve_symbol_notation(notation) {
            Ok(def) => Ok((snapshot, def)),
            Err(SymbolResolveError::ModuleNotLoaded) => Err(SymbolQueryError::ModuleNotFound),
            Err(SymbolResolveError::SymbolNotFound) => {
                let module = snapshot.sem.resolve_notation_module(&notation.module);
                let available = match &notation.receiver {
                    Some(receiver) => {
                        snapshot
                            .sem
                            .type_member_names(&module, receiver, public_only)
                    }
                    None => snapshot.sem.module_symbol_names(&module, public_only),
                };
                Err(SymbolQueryError::NotFound { available })
            }
        }
    }

    /// Build a [`QueryContext`] over `uri`'s cached snapshot. The caller owns
    /// the `Rc<Snapshot>` so the borrow outlives the context.
    fn query_ctx_over<'a>(&'a self, snapshot: &'a Snapshot, uri: &'a str) -> QueryContext<'a> {
        QueryContext {
            sem: &snapshot.sem,
            source: self.documents.get(uri).map_or("", |d| d.text.as_str()),
            uri,
            encoding: self.position_encoding,
        }
    }

    /// Resolve a symbol notation to a definition location. `public_only` only
    /// affects the suggestions on a miss (public symbols vs. everything).
    pub async fn definition_by_symbol<H: CompilerHost>(
        &self,
        uri: &str,
        notation: &wado_compiler::symbol_notation::SymbolNotation,
        public_only: bool,
        host: &H,
    ) -> Result<DefinitionResult, SymbolQueryError> {
        let (snapshot, def) = self
            .resolve_symbol_def(uri, notation, public_only, host)
            .await?;
        let sem = &snapshot.sem;
        let span = def.span.ok_or(SymbolQueryError::NoLocation)?;
        let entry = &sem.entry_module_source;
        let def_uri = match sem.symbol_at(def.ast_id) {
            Some(symbol) => location::symbol_uri(entry, symbol, uri),
            None => sem
                .module_of_id(def.ast_id)
                .and_then(|m| location::module_uri(entry, m, uri)),
        }
        .ok_or(SymbolQueryError::NoLocation)?;
        Ok(DefinitionResult {
            uri: def_uri,
            range: location::span_to_range(&span, None, self.position_encoding),
        })
    }

    /// Compute hover info for a symbol named by notation. With `public_only`,
    /// a type's rendering matches `wado doc` (private fields elided, only `pub`
    /// inherent members listed); otherwise everything is shown.
    pub async fn hover_by_symbol<H: CompilerHost>(
        &self,
        uri: &str,
        notation: &wado_compiler::symbol_notation::SymbolNotation,
        public_only: bool,
        host: &H,
    ) -> Result<HoverResult, SymbolQueryError> {
        let (snapshot, def) = self
            .resolve_symbol_def(uri, notation, public_only, host)
            .await?;
        let sem = &snapshot.sem;
        // Module-level symbols render from the symbol table; methods and free
        // functions reached by AstId (e.g. `Type::m`) render from the AST.
        if let Some(symbol) = sem.symbol_at(def.ast_id) {
            return hover::hover_for_item_symbol(sem, symbol, public_only)
                .ok_or(SymbolQueryError::NoLocation);
        }
        if let Some(function) = sem.function_at(def.ast_id) {
            return Ok(hover::hover_for_function(function));
        }
        Err(SymbolQueryError::NoLocation)
    }

    /// Find every reference to a symbol named by notation, across the loaded
    /// module graph. `include_declaration` adds the declaration site.
    pub async fn references_by_symbol<H: CompilerHost>(
        &self,
        uri: &str,
        notation: &wado_compiler::symbol_notation::SymbolNotation,
        include_declaration: bool,
        public_only: bool,
        host: &H,
    ) -> Result<Vec<ReferenceLocation>, SymbolQueryError> {
        let (snapshot, def) = self
            .resolve_symbol_def(uri, notation, public_only, host)
            .await?;
        let ctx = self.query_ctx_over(&snapshot, uri);
        Ok(references::references_for_def(
            &ctx,
            def.ast_id,
            include_declaration,
        ))
    }

    /// Highlight every occurrence (Read/Write) of a symbol named by notation
    /// inside its own defining module. Returns the defining module's URI
    /// alongside the highlights (they all live in that one file).
    pub async fn document_highlight_by_symbol<H: CompilerHost>(
        &self,
        uri: &str,
        notation: &wado_compiler::symbol_notation::SymbolNotation,
        public_only: bool,
        host: &H,
    ) -> Result<(String, Vec<DocumentHighlight>), SymbolQueryError> {
        let (snapshot, def) = self
            .resolve_symbol_def(uri, notation, public_only, host)
            .await?;
        let sem = &snapshot.sem;
        let def_uri = location::module_uri(&sem.entry_module_source, &def.module, uri)
            .ok_or(SymbolQueryError::NoLocation)?;
        let ctx = self.query_ctx_over(&snapshot, uri);
        let highlights = document_highlight::highlights_for_def(&ctx, def.ast_id, &def.module);
        Ok((def_uri, highlights))
    }

    /// Compute hover information for the symbol at the given position
    /// (public-API view: container types render like `wado doc`).
    pub async fn hover<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        host: &H,
    ) -> Option<HoverResult> {
        self.hover_with(uri, position, true, host).await
    }

    /// [`Engine::hover`] with explicit member visibility: `public_only = false`
    /// shows private fields too (the `wado query --all` view).
    pub async fn hover_with<H: CompilerHost>(
        &self,
        uri: &str,
        position: Position,
        public_only: bool,
        host: &H,
    ) -> Option<HoverResult> {
        self.with_query_ctx(uri, host, None, |ctx| {
            hover::find_hover_opts(ctx, position, public_only)
        })
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

    /// Compute inlay hints in the requested `range` of the document.
    ///
    /// Returns inferred-type hints for `let` / closure / `for-of` bindings
    /// that lack an explicit annotation, plus parameter-name hints for
    /// free-function and method call sites. Hints anchored outside the
    /// requested `range` are filtered out.
    pub async fn inlay_hints<H: CompilerHost>(
        &self,
        uri: &str,
        range: Range,
        host: &H,
    ) -> Vec<InlayHint> {
        self.with_query_ctx(uri, host, Vec::new(), |ctx| {
            inlay_hints::inlay_hints(ctx, range)
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
    /// Reuses the cached [`Semantics`] snapshot so identifiers are classified by
    /// their resolved symbol kind. When the snapshot cannot be built (loader
    /// failure), classification falls back to lexer/AST heuristics so
    /// highlighting still appears — see [`semantic_tokens::compute`].
    pub async fn semantic_tokens<H: CompilerHost>(&self, uri: &str, host: &H) -> Vec<u32> {
        // Take the snapshot first (an owned `Rc`) so no borrow of `self` is
        // held across the `.await`; then borrow the document text.
        let snapshot = self.snapshot(uri, host).await;
        let Some(doc) = self.documents.get(uri) else {
            return Vec::new();
        };
        let sem = snapshot.as_ref().map(|s| &s.sem);
        let tokens = semantic_tokens::compute(&doc.text, sem);
        semantic_tokens::delta_encode(&tokens, &doc.text, self.position_encoding)
    }

    /// Compute diagnostics for the given document.
    ///
    /// Reads from the snapshot cache populated by [`Engine::snapshot`].
    /// The semantics pipeline runs at most once per document version
    /// regardless of which queries the client issued first. Each
    /// diagnostic's column is re-encoded against the source whose file
    /// matches its
    /// `span.file` — cross-file diagnostics keep the compiler's codepoint
    /// columns, the entry document is re-expressed in the negotiated
    /// position encoding.
    ///
    /// Source-level unused / dead-code warnings are computed here rather
    /// than baked into the snapshot: the classification is a cheap walk
    /// over the liveness lists the compiler already produced, and applying
    /// it at query time keeps [`Engine::set_unused_diagnostics`] live
    /// without invalidating the cache. Only items in the entry document
    /// are reported — dead code in an imported module belongs to that
    /// module's own `publishDiagnostics`, not this one.
    pub async fn diagnostics<H: CompilerHost>(&self, uri: &str, host: &H) -> Vec<Diagnostic> {
        let Some(snapshot) = self.snapshot(uri, host).await else {
            return Vec::new();
        };
        let filename = Uri::new(uri).to_filename();
        let encoding = self.position_encoding;
        let entry_text = self.documents.get(uri).map(|d| d.text.as_str());
        let reencode = |d: &CompilerDiagnostic| {
            // Only re-encode against the entry document's text when the
            // diagnostic actually points at it. Diagnostics from imported
            // modules carry codepoint columns relative to the OTHER
            // module's source, which we don't have on hand; passing `None`
            // keeps them as raw codepoint indices (correct under UTF-32 /
            // ASCII).
            let source = d
                .span
                .as_ref()
                .filter(|s| s.file == filename)
                .and(entry_text);
            diagnostics::from_compiler_diagnostic(d, uri, source, encoding)
        };
        let mut out: Vec<Diagnostic> = snapshot.diagnostics.iter().filter_map(&reencode).collect();
        if self.unused_diagnostics {
            out.extend(
                wado_compiler::unused_diagnostics(&snapshot.sem, false)
                    .iter()
                    .filter(|d| d.span.as_ref().is_some_and(|s| s.file == filename))
                    .filter_map(&reencode),
            );
        }
        out
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
        // Recover from poisoning: a panic inside the inner host's
        // `emit_diagnostic` would otherwise propagate via `unwrap()`
        // and kill every subsequent snapshot build for the rest of the
        // process — fatal for a long-running LSP server.
        self.diagnostics
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<H: CompilerHost> CompilerHost for DiagnosticCollector<'_, H> {
    async fn load_source(&self, path: &str) -> Result<Vec<u8>, wado_compiler::SourceError> {
        self.inner.load_source(path).await
    }

    fn emit_diagnostic(&self, diagnostic: CompilerDiagnostic) {
        // Capture for the snapshot cache, then forward so the inner host's
        // own side effects (e.g. CLI stderr logging) still happen.
        // Recover from a poisoned lock so a panic in a prior `inner.emit_diagnostic`
        // call doesn't take down every subsequent capture.
        self.diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(diagnostic.clone());
        self.inner.emit_diagnostic(diagnostic);
    }

    // Delegate every non-diagnostic capability to the wrapped host — the
    // collector only intercepts diagnostics. Falling back to the trait defaults
    // here would silently drop the host's dependency index (path *and* component
    // deps), so `use { … } from "<dep>"` would fail to resolve on the LSP path.
    fn dependency_index(&self) -> wado_compiler::DependencyIndex {
        self.inner.dependency_index()
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.inner.env_var(name)
    }

    fn run_generator(
        &self,
        component_wasm: &[u8],
        request: wado_compiler::GeneratorRequest,
    ) -> impl std::future::Future<
        Output = Result<wado_compiler::GeneratorResponse, wado_compiler::GeneratorRunnerError>,
    > + Send {
        self.inner.run_generator(component_wasm, request)
    }
}

/// Drive the compiler frontend's three stages — `parse`, `load`,
/// `semantics_of` — over `source`, interleaving kiln invocation
/// discovery between parse and load so the entry AST is shared instead
/// of parsed twice.
///
/// `override_invocations` short-circuits kiln discovery: a runtime-backed
/// host (native `wado-cli`) supplies an [`InvocationIndex`] it built by
/// running the generators, and it is used verbatim. When `None`, the
/// consume-only on-disk discovery (`kiln::prepare_invocations`) runs.
///
/// Every failure path emits its diagnostic directly through `host` and
/// returns [`Semantics::empty`], so callers see a uniformly-shaped
/// (possibly empty) snapshot regardless of which stage bailed.
async fn build_semantics<H: CompilerHost>(
    source: &str,
    filename: &str,
    override_invocations: Option<InvocationIndex>,
    host: &H,
) -> Semantics {
    let parsed = wado_compiler::parse(source);
    // Report each recovered lex/parse error, then continue with the partial
    // AST so position queries resolve in the regions outside the error. A
    // load/bind failure on the partial AST still degrades to
    // `Semantics::empty()` below; recovery helps only when binding succeeds.
    for e in &parsed.lex_errors {
        host.emit_diagnostic(wado_compiler::lex_error_diagnostic(e, Some(filename)));
    }
    for e in &parsed.errors {
        host.emit_diagnostic(wado_compiler::parse_error_diagnostic(e, Some(filename)));
    }
    let invocations = match override_invocations {
        Some(index) => index,
        None => kiln::prepare_invocations(filename, &parsed.ast, host),
    };
    match wado_compiler::load(
        parsed,
        Some(filename),
        host,
        invocations,
        LogLevel::default(),
    )
    .await
    {
        // LSP engine: facts only, no TIR (queries never read `tir_modules`).
        Ok(loaded) => wado_compiler::semantics_of(loaded, host, LogLevel::default(), false),
        Err(e) => {
            host.emit_diagnostic(e.into());
            Semantics::empty()
        }
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
