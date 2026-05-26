//! Per-query context shared by every position-bearing LSP feature.
//!
//! Every feature — `definition` / `hover` / `references` /
//! `document_highlight` — needs the same five inputs against an open
//! document:
//!
//! - the [`Semantics`] snapshot,
//! - the document text,
//! - the document URI (for relative-import URI resolution),
//! - the negotiated [`PositionEncoding`],
//! - a way to land an LSP [`Position`] on a compiler [`Cursor`].
//!
//! [`QueryContext`] bundles them so feature functions take a single
//! argument instead of threading the tuple by hand at every call site.

use wado_compiler::Cursor;
use wado_compiler::module_source::ModuleSource;
use wado_compiler::semantics::Semantics;
use wado_compiler::symbol::SymbolKey;

use crate::diagnostics::Position;
use crate::location::source_for_key;
use crate::text::{PositionEncoding, lsp_position_to_line_col};

/// Constant inputs to one LSP query against an open document.
///
/// Built by [`crate::Engine::with_query_ctx`] from a cached snapshot;
/// feature functions consume it by reference.
pub(crate) struct QueryContext<'a> {
    pub sem: &'a Semantics,
    pub source: &'a str,
    pub uri: &'a str,
    pub encoding: PositionEncoding,
}

impl<'a> QueryContext<'a> {
    /// The entry module — the one whose source text lives in `self.source`.
    pub fn entry(&self) -> &'a ModuleSource {
        &self.sem.entry_module_source
    }

    /// Convert an LSP [`Position`] to compiler 1-based codepoint
    /// `(line, col)` against `self.source` in the negotiated encoding.
    pub fn line_col(&self, position: Position) -> (usize, usize) {
        lsp_position_to_line_col(self.source, position, self.encoding)
    }

    /// Resolve an LSP [`Position`] to a [`Cursor`] over the entry module.
    /// Returns `None` when no AST node covers the position.
    pub fn cursor_at(&self, position: Position) -> Option<Cursor<'a>> {
        let (line, col) = self.line_col(position);
        self.sem.cursor_at(self.entry(), line, col)
    }

    /// Source text to feed [`crate::location::span_to_range`] when
    /// re-encoding a span at `key`. `Some(self.source)` for spans inside
    /// the entry document; `None` for spans in other modules (their
    /// source isn't available here, and `span_to_range` falls back to
    /// "codepoint columns as code units" — correct under UTF-32 / ASCII).
    pub fn source_for_key(&self, key: &SymbolKey) -> Option<&'a str> {
        source_for_key(self.entry(), key, self.source)
    }
}
