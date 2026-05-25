//! LSP position ↔ compiler `(line, column)` conversion with negotiated
//! position encoding.
//!
//! LSP positions are `(line, character)` with `character` counted in code
//! units of the negotiated encoding (UTF-16 by default per the spec).
//! The wado compiler's `Span` uses 1-based line and **1-based codepoint**
//! columns (`lexer.rs::Lexer::advance` increments `column` per Unicode
//! scalar value, not per byte and not per UTF-16 code unit). The
//! conversion lives here so every LSP query reaches the same compiler
//! `(line, col)` for a given cursor regardless of which code-unit space
//! the client speaks.
//!
//! Previously each query did
//!
//! ```ignore
//! let line = position.line as usize + 1;
//! let col = position.character as usize + 1;
//! ```
//!
//! which silently assumed both 1-based-ness and ASCII. Non-ASCII source
//! (multi-byte characters, multi-code-unit grapheme clusters) drifted
//! the column by an unbounded amount, breaking hover / definition /
//! references at every cursor past an emoji or a CJK character.
//!
//! See LSP 3.18 §general.positionEncodings.

use crate::diagnostics::{Position, Range};
use wado_compiler::token::Span;

/// Position encoding negotiated with the LSP client.
///
/// Per LSP 3.18 §general.positionEncodings the default — when the client
/// does not advertise the capability or sends an empty list — is
/// `utf-16`. The server must respond with exactly one of the encodings
/// the client offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PositionEncoding {
    /// `character` counts UTF-8 bytes from the start of the line.
    Utf8,
    /// `character` counts UTF-16 code units from the start of the line.
    /// LSP default.
    #[default]
    Utf16,
    /// `character` counts UTF-32 code points (= Unicode scalar values).
    Utf32,
}

impl PositionEncoding {
    /// Wire-format string used in `initialize` / `initializeResult`.
    #[must_use]
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
    }

    /// Pick the most-preferred encoding the client advertises.
    ///
    /// Preference order: utf-32 > utf-8 > utf-16. UTF-32 wins because
    /// the compiler's `Span::column` is a 1-based codepoint index, so a
    /// UTF-32 session is a direct passthrough with no per-line decoding
    /// work. UTF-16 is the LSP default and only kicks in when the
    /// client offers nothing else.
    #[must_use]
    pub fn negotiate(client_offer: &[String]) -> Self {
        if client_offer.iter().any(|s| s == "utf-32") {
            Self::Utf32
        } else if client_offer.iter().any(|s| s == "utf-8") {
            Self::Utf8
        } else {
            Self::Utf16
        }
    }
}

/// Convert an LSP `Position` (0-based, code units of `encoding`) into a
/// compiler 1-based `(line, column)` where `column` is a 1-based
/// codepoint index within the line — matching what `lexer.rs` records
/// on each [`Span`].
///
/// Returns an out-of-range sentinel `(usize::MAX, 1)` for any position
/// whose line is past the end of `source`. The compiler-side
/// `ast_id_at` searches for nodes containing the position; no real
/// span has line `usize::MAX`, so the lookup correctly returns `None`
/// — call sites can keep using `?` to bail. (Returning `(1, 1)` here
/// would map every past-EOF cursor onto the first AST node, silently
/// resolving hover / definition to whatever symbol starts at the
/// document head.)
#[must_use]
pub fn lsp_position_to_line_col(
    source: &str,
    position: Position,
    encoding: PositionEncoding,
) -> (usize, usize) {
    for (current_line, line) in source.split_inclusive('\n').enumerate() {
        let current_line = current_line as u32;
        if current_line == position.line {
            // `split_inclusive` keeps the trailing '\n' on every line
            // except possibly the last. The cursor's `character` is
            // measured against the line's content; strip the line
            // terminator before locating the column so a cursor at the
            // end-of-line does not slide into the next line on UTF-16
            // counting.
            let line_content = line_without_terminator(line);
            let codepoint_col =
                character_to_codepoint_offset(line_content, position.character, encoding);
            // Compiler convention: 1-based line + 1-based codepoint column.
            return (current_line as usize + 1, codepoint_col + 1);
        }
    }
    // Cursor past EOF: out-of-range sentinel so the compiler's
    // `ast_id_at` returns `None`. See doc-comment above.
    (usize::MAX, 1)
}

/// Strip a trailing `\n` (and optional preceding `\r`) from a single
/// line slice. Every line lookup in this crate uses this idiom because
/// `split_inclusive('\n')` keeps the terminator on each line.
#[must_use]
pub(crate) fn line_without_terminator(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(line)
}

/// Convert a compiler `Span` to an LSP `Range` in the negotiated
/// `encoding`. The compiler's `column` / `end_column` are 1-based
/// codepoint indices (see `lexer.rs::Lexer::advance`); the LSP
/// `character` field needs them re-expressed in the client's chosen
/// code-unit measure.
///
/// When `source` is `None` we degrade to "treat codepoint indices as
/// LSP code units" — correct for ASCII and UTF-32, drifts for UTF-16 on
/// non-ASCII content. Pass `Some(source)` whenever the source text is
/// on hand (the snapshot always provides it) so non-ASCII spans
/// round-trip correctly.
#[must_use]
pub fn span_to_range(span: &Span, source: Option<&str>, encoding: PositionEncoding) -> Range {
    let start_line = span.line.saturating_sub(1) as u32;
    let start_col_codepoints = span.column.saturating_sub(1) as u32;
    let end_line = span.end_line.saturating_sub(1) as u32;
    let end_col_codepoints = span.end_column.saturating_sub(1) as u32;

    let (start_char, end_char) = match source {
        Some(src) => (
            codepoint_offset_to_character(src, start_line, start_col_codepoints, encoding),
            codepoint_offset_to_character(src, end_line, end_col_codepoints, encoding),
        ),
        None => (start_col_codepoints, end_col_codepoints),
    };

    Range {
        start: Position {
            line: start_line,
            character: start_char,
        },
        end: Position {
            line: end_line,
            character: end_char,
        },
    }
}

/// Translate an LSP `character` (0-based, in `encoding` code units) into
/// a 0-based codepoint offset inside `line`. Saturates at the end of
/// the line if the index runs past it — clients sometimes emit
/// positions slightly past the line end (e.g. cursor at the trailing
/// newline after a soft-wrap).
fn character_to_codepoint_offset(line: &str, character: u32, encoding: PositionEncoding) -> usize {
    let target = character as usize;
    match encoding {
        PositionEncoding::Utf8 => {
            // `character` is a UTF-8 byte index; walk the line's chars
            // until we reach that byte offset.
            let mut bytes = 0usize;
            for (i, ch) in line.chars().enumerate() {
                if bytes >= target {
                    return i;
                }
                bytes += ch.len_utf8();
            }
            line.chars().count()
        }
        PositionEncoding::Utf16 => {
            let mut units = 0usize;
            for (i, ch) in line.chars().enumerate() {
                if units >= target {
                    return i;
                }
                units += ch.len_utf16();
            }
            line.chars().count()
        }
        PositionEncoding::Utf32 => target.min(line.chars().count()),
    }
}

/// Translate a 0-based codepoint offset at `line` (of `source`) into a
/// 0-based `character` in `encoding` code units. Looks the line up in
/// `source` on each call; callers that already have the line slice on
/// hand (e.g. a delta-encoding loop) should use
/// [`codepoints_to_code_units`] directly to skip the lookup.
#[must_use]
pub(crate) fn codepoint_offset_to_character(
    source: &str,
    line: u32,
    codepoint_col: u32,
    encoding: PositionEncoding,
) -> u32 {
    let Some(line_text) = source.split_inclusive('\n').nth(line as usize) else {
        return codepoint_col;
    };
    let line_content = line_without_terminator(line_text);
    let line_codepoints = line_content.chars().count() as u32;
    codepoints_to_code_units(line_content, line_codepoints, codepoint_col, encoding)
}

/// Codepoint offset → LSP `character` (code-unit count) inside a single
/// line slice. The caller supplies `line_codepoints` (the line's
/// pre-computed codepoint count) so hot loops can hoist the
/// `chars().count()` walk out — see `semantic_tokens::delta_encode`,
/// which converts twice per emitted token.
///
/// `codepoint_col` is saturated against `line_codepoints` so callers
/// don't need a separate clamp.
#[must_use]
pub(crate) fn codepoints_to_code_units(
    line: &str,
    line_codepoints: u32,
    codepoint_col: u32,
    encoding: PositionEncoding,
) -> u32 {
    let codepoint_col = codepoint_col.min(line_codepoints);
    match encoding {
        PositionEncoding::Utf8 => line
            .chars()
            .take(codepoint_col as usize)
            .map(|c| c.len_utf8() as u32)
            .sum(),
        PositionEncoding::Utf16 => line
            .chars()
            .take(codepoint_col as usize)
            .map(|c| c.len_utf16() as u32)
            .sum(),
        PositionEncoding::Utf32 => codepoint_col,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn past_eof_returns_out_of_range_sentinel() {
        // A stale cursor (line beyond source) used to fall back to
        // (1, 1) — a valid in-range position that silently bound LSP
        // queries to the first AST node. The sentinel must be a line
        // no real `Span` can have, so the compiler's `ast_id_at`
        // returns `None` and the LSP query bails cleanly.
        let src = "fn f() {}\nfn g() {}\n";
        let (line, col) = lsp_position_to_line_col(src, pos(99, 0), PositionEncoding::Utf16);
        assert_eq!(
            (line, col),
            (usize::MAX, 1),
            "past-EOF cursor must yield an out-of-range sentinel",
        );
    }

    #[test]
    fn ascii_round_trip_in_every_encoding() {
        let src = "fn f() {}\nlet x = 1;\n";
        for enc in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            assert_eq!(lsp_position_to_line_col(src, pos(0, 3), enc), (1, 4));
            assert_eq!(lsp_position_to_line_col(src, pos(1, 4), enc), (2, 5));
        }
    }

    #[test]
    fn utf8_position_is_byte_indexed_into_chars() {
        // 'あ' is 3 UTF-8 bytes / 1 UTF-16 unit / 1 codepoint. After
        // "let " (4 bytes) the cursor at byte 7 lands on the codepoint
        // following 'あ' (codepoint index 5 → compiler col 6).
        let src = "let あ = 1;\n";
        assert_eq!(
            lsp_position_to_line_col(src, pos(0, 7), PositionEncoding::Utf8),
            (1, 6)
        );
    }

    #[test]
    fn utf16_position_counts_code_units_into_codepoints() {
        // UTF-16 unit 5 in "let あ = 1;" sits at codepoint 5 (the space
        // after 'あ'), so compiler column = 5 + 1 = 6.
        let src = "let あ = 1;\n";
        assert_eq!(
            lsp_position_to_line_col(src, pos(0, 5), PositionEncoding::Utf16),
            (1, 6)
        );
    }

    #[test]
    fn utf32_position_passes_codepoints_through() {
        // 🦀 is 4 UTF-8 bytes / 2 UTF-16 units / 1 codepoint. UTF-32
        // index 5 is the space after 🦀 — codepoint index 5 → compiler
        // column 6.
        let src = "let 🦀 = 1;\n";
        assert_eq!(
            lsp_position_to_line_col(src, pos(0, 5), PositionEncoding::Utf32),
            (1, 6)
        );
    }

    #[test]
    fn span_to_range_re_encodes_ascii_unchanged() {
        let src = "fn add() {}\n";
        let span = Span {
            line: 1,
            column: 4,
            end_line: 1,
            end_column: 7,
            start: 3,
            end: 6,
        };
        let r = span_to_range(&span, Some(src), PositionEncoding::Utf16);
        assert_eq!(r.start, pos(0, 3));
        assert_eq!(r.end, pos(0, 6));
    }

    #[test]
    fn span_to_range_re_encodes_non_ascii_for_utf16() {
        // The compiler reports `あ` at codepoint column 4..5; under
        // UTF-16 that maps to character index 3..4 (each codepoint here
        // is 1 UTF-16 unit, but the test exercises that the conversion
        // path runs).
        let src = "fn あ() {}\n";
        let span = Span {
            line: 1,
            column: 4,
            end_line: 1,
            end_column: 5,
            start: 3,
            end: 6,
        };
        let r = span_to_range(&span, Some(src), PositionEncoding::Utf16);
        assert_eq!(r.start, pos(0, 3));
        assert_eq!(r.end, pos(0, 4));
    }

    #[test]
    fn span_to_range_utf8_emits_byte_columns() {
        let src = "fn あ() {}\n";
        let span = Span {
            line: 1,
            column: 4,
            end_line: 1,
            end_column: 5,
            start: 3,
            end: 6,
        };
        let r = span_to_range(&span, Some(src), PositionEncoding::Utf8);
        // Codepoint 3 = byte 3, codepoint 4 = byte 3 + 3 (length of 'あ') = 6.
        assert_eq!(r.start, pos(0, 3));
        assert_eq!(r.end, pos(0, 6));
    }

    #[test]
    fn span_to_range_re_encodes_supplementary_pair_for_utf16() {
        // 🦀 is one codepoint but two UTF-16 units. A span whose
        // start/end straddles it must end up two UTF-16 units apart.
        let src = "let 🦀 = 1;\n";
        let span = Span {
            line: 1,
            column: 5, // codepoint index 4 (start of 🦀) → 1-based 5
            end_line: 1,
            end_column: 6, // codepoint index 5 (after 🦀)
            start: 4,
            end: 8,
        };
        let r = span_to_range(&span, Some(src), PositionEncoding::Utf16);
        assert_eq!(r.start, pos(0, 4));
        // "let " = 4 UTF-16 units, '🦀' = 2 UTF-16 units → end at 6.
        assert_eq!(r.end, pos(0, 6));
    }

    #[test]
    fn negotiate_prefers_utf32_when_offered() {
        let off = [
            "utf-16".to_string(),
            "utf-8".to_string(),
            "utf-32".to_string(),
        ];
        assert_eq!(PositionEncoding::negotiate(&off), PositionEncoding::Utf32);
    }

    #[test]
    fn negotiate_falls_back_to_utf8_when_only_utf8_and_utf16() {
        let off = ["utf-16".to_string(), "utf-8".to_string()];
        assert_eq!(PositionEncoding::negotiate(&off), PositionEncoding::Utf8);
    }

    #[test]
    fn negotiate_falls_back_to_utf16_when_only_utf16_offered() {
        let off = ["utf-16".to_string()];
        assert_eq!(PositionEncoding::negotiate(&off), PositionEncoding::Utf16);
    }

    #[test]
    fn negotiate_falls_back_to_utf16_when_offer_empty() {
        // LSP spec: missing capability ⇒ default utf-16.
        assert_eq!(PositionEncoding::negotiate(&[]), PositionEncoding::Utf16);
    }

    #[test]
    fn negotiate_falls_back_to_utf16_for_unknown_encodings() {
        let off = ["utf-7".to_string(), "utf-1024".to_string()];
        assert_eq!(PositionEncoding::negotiate(&off), PositionEncoding::Utf16);
    }
}
