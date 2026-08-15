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
//! - a way to land an LSP [`Position`] on a compiler [`Cursor`],
//! - a [`LineIndex`] over the document, so the many spans one query
//!   answers share a single scan.
//!
//! [`QueryContext`] bundles them so feature functions take a single
//! argument instead of threading the tuple by hand at every call site.

use wado_compiler::Cursor;
use wado_compiler::ast::AstId;
use wado_compiler::module_source::ModuleSource;
use wado_compiler::semantics::Semantics;
use wado_compiler::token::Span;

use crate::diagnostics::{Position, Range};
use crate::text::{self, LineIndex, PositionEncoding, lsp_position_to_line_col};

/// Constant inputs to one LSP query against an open document.
///
/// Built by [`crate::Engine::with_query_ctx`] from a cached snapshot;
/// feature functions consume it by reference.
///
/// Fields are `pub(crate)` so feature files can reach the underlying
/// [`Semantics`] (and the source / URI / encoding) directly for
/// queries that don't need a positional conversion. Positional lookups
/// (`Position` → `Cursor`) **must** go through [`Self::cursor_at`] or
/// [`Self::cursor_at_line_col`] so that any future invariant added to
/// the conversion (e.g. clamping, normalisation, tracing) reaches every
/// feature.
pub(crate) struct QueryContext<'a> {
    pub(crate) sem: &'a Semantics,
    pub(crate) source: &'a str,
    pub(crate) uri: &'a str,
    pub(crate) encoding: PositionEncoding,
    /// Line table over `source`, built once per query — a query answers many
    /// spans and each converts two columns.
    lines: LineIndex<'a>,
}

impl<'a> QueryContext<'a> {
    pub fn new(
        sem: &'a Semantics,
        source: &'a str,
        uri: &'a str,
        encoding: PositionEncoding,
    ) -> Self {
        Self {
            sem,
            source,
            uri,
            encoding,
            lines: LineIndex::new(source),
        }
    }

    /// Convert `span` — belonging to node `id` — to an LSP [`Range`].
    ///
    /// Spans in the entry document are re-encoded against its text; spans in
    /// other modules keep the compiler's codepoint columns, since their
    /// source is not on hand (correct under UTF-32 / ASCII).
    pub fn range_of(&self, span: &Span, id: AstId) -> Range {
        match self.source_for_id(id) {
            Some(_) => text::span_to_range_indexed(span, &self.lines, self.encoding),
            None => text::span_to_range(span, None, self.encoding),
        }
    }

    /// [`Self::range_of`] for a span already known to be in the entry
    /// document (a cursor's own span).
    pub fn range_in_document(&self, span: &Span) -> Range {
        text::span_to_range_indexed(span, &self.lines, self.encoding)
    }

    /// An LSP [`Position`] from a 0-based line and codepoint column in the
    /// entry document. For anchors that are one edge of a span, not a range.
    pub fn position_at(&self, line: u32, codepoint_col: u32) -> Position {
        Position {
            line,
            character: self.lines.to_character(line, codepoint_col, self.encoding),
        }
    }

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
        self.cursor_at_line_col(line, col)
    }

    /// Variant of [`Self::cursor_at`] that takes pre-converted compiler
    /// `(line, col)` coordinates. Callers that need `(line, col)` for
    /// other lookups too (e.g. file-path matchers operating on raw
    /// `Span`s) should call [`Self::line_col`] once and share the result
    /// with this method, instead of bypassing the wrapper by reaching
    /// for `sem.cursor_at` directly.
    pub fn cursor_at_line_col(&self, line: usize, col: usize) -> Option<Cursor<'a>> {
        self.sem.cursor_at(self.entry(), line, col)
    }

    /// Whether node `id` lives in the entry document, carrying its source
    /// when so. `None` for spans in other modules: their text is not
    /// available here, so [`Self::range_of`] emits the compiler's codepoint
    /// columns verbatim (correct under UTF-32 / ASCII).
    pub fn source_for_id(&self, id: AstId) -> Option<&'a str> {
        (self.sem.module_of_id(id) == Some(self.entry())).then_some(self.source)
    }
}
