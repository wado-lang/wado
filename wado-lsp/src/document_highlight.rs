//! Document highlight, powered by `wado_compiler::semantics`.
//!
//! Returns every occurrence of the symbol named at the cursor that lives
//! inside the requested document. References to the same symbol from other
//! files are filtered out — that's `textDocument/references`' job.
//!
//! Each occurrence is classified:
//! - `Write` for the declaration and for use-sites that are direct targets of
//!   `=`, `+=`, etc.
//! - `Read` for every other use-site.
//!
//! Write classification consults `Semantics::is_write_target`, which is
//! populated by the per-module `AstIndex` during the semantics pass. The
//! highlight pass therefore performs no AST walks of its own.

use serde::{Deserialize, Serialize};
use wado_compiler::semantics::Semantics;

use crate::diagnostics::{Position, Range};
use crate::location::span_to_range;
use crate::macros::lsp_repr_u32_enum;
use crate::text::{PositionEncoding, lsp_position_to_line_col};

lsp_repr_u32_enum!(
    /// LSP `DocumentHighlightKind` values. Serializes as the 1..=3 integer
    /// defined by the LSP wire format.
    pub enum HighlightKind {
        Text = 1,
        Read = 2,
        Write = 3,
    }
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentHighlight {
    pub range: Range,
    pub kind: HighlightKind,
}

#[must_use]
pub fn document_highlight(
    sem: &Semantics,
    source: &str,
    position: Position,
    _uri: &str,
    encoding: PositionEncoding,
) -> Vec<DocumentHighlight> {
    let module = sem.entry_module_source.clone();
    let (line, col) = lsp_position_to_line_col(source, position, encoding);

    let Some(cursor) = sem.cursor_at(&module, line, col) else {
        return Vec::new();
    };
    let Some(def_key) = cursor.def_key() else {
        return Vec::new();
    };

    let mut out = Vec::new();

    if def_key.module == module
        && let Some(span) = sem
            .name_span_of(&def_key)
            .or_else(|| sem.symbol_at(&def_key).and_then(|s| s.span))
    {
        out.push(DocumentHighlight {
            range: span_to_range(&span, Some(source), encoding),
            kind: HighlightKind::Write,
        });
    }

    for use_key in cursor.references_to_def() {
        if use_key.module != module {
            continue;
        }
        let Some(span) = sem.span_of_key(&use_key) else {
            continue;
        };
        let kind = if sem.is_write_target(&use_key) {
            HighlightKind::Write
        } else {
            HighlightKind::Read
        };
        out.push(DocumentHighlight {
            range: span_to_range(&span, Some(source), encoding),
            kind,
        });
    }

    out.sort_by_key(|h| (h.range.start.line, h.range.start.character));
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MapHost;
    use wado_compiler::semantics::semantics_with_invocations;

    async fn highlights_at(source: &str, line: u32, character: u32) -> Vec<DocumentHighlight> {
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = MapHost::single(path, source);
        let invocations = wado_compiler::kiln::InvocationIndex::new();
        let sem = semantics_with_invocations(source, &host, Some(path), invocations).await;
        document_highlight(
            &sem,
            source,
            Position { line, character },
            &uri,
            PositionEncoding::Utf16,
        )
    }

    fn summarize(refs: &[DocumentHighlight]) -> Vec<(u32, u32, HighlightKind)> {
        refs.iter()
            .map(|h| (h.range.start.line, h.range.start.character, h.kind))
            .collect()
    }

    #[test]
    fn read_only_uses() {
        futures::executor::block_on(async {
            let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x + x;\n}\n";
            let hl = highlights_at(source, 1, 8).await;
            assert_eq!(
                summarize(&hl),
                vec![
                    (1, 8, HighlightKind::Write),
                    (2, 11, HighlightKind::Read),
                    (2, 15, HighlightKind::Read),
                ]
            );
        });
    }

    #[test]
    fn assignment_marks_target_as_write() {
        futures::executor::block_on(async {
            let source =
                "fn f() {\n    let mut x: i32 = 0;\n    x = 1;\n    x += 2;\n    let y = x;\n}\n";
            let hl = highlights_at(source, 1, 12).await;
            assert_eq!(
                summarize(&hl),
                vec![
                    (1, 12, HighlightKind::Write),
                    (2, 4, HighlightKind::Write),
                    (3, 4, HighlightKind::Write),
                    (4, 12, HighlightKind::Read),
                ]
            );
        });
    }

    #[test]
    fn cursor_on_use_site_works() {
        futures::executor::block_on(async {
            let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x + x;\n}\n";
            let hl = highlights_at(source, 2, 11).await;
            assert_eq!(
                summarize(&hl),
                vec![
                    (1, 8, HighlightKind::Write),
                    (2, 11, HighlightKind::Read),
                    (2, 15, HighlightKind::Read),
                ]
            );
        });
    }

    #[test]
    fn function_highlights_within_file() {
        futures::executor::block_on(async {
            let source = "fn helper() -> i32 {\n    return 1;\n}\nfn run() -> i32 {\n    return helper() + helper();\n}\n";
            let hl = highlights_at(source, 0, 4).await;
            assert_eq!(
                summarize(&hl),
                vec![
                    (0, 3, HighlightKind::Write),
                    (4, 11, HighlightKind::Read),
                    (4, 22, HighlightKind::Read),
                ]
            );
        });
    }
}
