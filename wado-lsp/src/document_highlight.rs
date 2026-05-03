//! Document highlight, powered by `wado_compiler::annotate`.
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
//! Write classification consults `Annotated::is_write_target`, which is
//! populated by the per-module `AstIndex` during the annotate phase. The
//! highlight pass therefore performs no AST walks of its own.

use serde::{Deserialize, Serialize};
use wado_compiler::CompilerHost;
use wado_compiler::annotate::annotate;

use crate::diagnostics::{Position, Range};
use crate::location::{span_to_range, uri_to_filename};

/// LSP `DocumentHighlightKind` values. Serializes as the 1..=3 integer
/// defined by the LSP wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "u32", try_from = "u32")]
pub enum HighlightKind {
    Text = 1,
    Read = 2,
    Write = 3,
}

impl From<HighlightKind> for u32 {
    fn from(k: HighlightKind) -> Self {
        k as Self
    }
}

impl TryFrom<u32> for HighlightKind {
    type Error = String;

    fn try_from(n: u32) -> Result<Self, <Self as TryFrom<u32>>::Error> {
        match n {
            1 => Ok(Self::Text),
            2 => Ok(Self::Read),
            3 => Ok(Self::Write),
            _ => Err(format!("invalid HighlightKind: {n}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentHighlight {
    pub range: Range,
    pub kind: HighlightKind,
}

pub async fn document_highlight<H: CompilerHost>(
    source: &str,
    position: Position,
    uri: &str,
    host: &H,
) -> Vec<DocumentHighlight> {
    let filename = uri_to_filename(uri);
    let Ok(annotated) = annotate(source, host, Some(&filename)).await else {
        return Vec::new();
    };

    let module = annotated.entry_module_source.clone();
    let line = position.line as usize + 1;
    let col = position.character as usize + 1;

    let Some(cursor) = annotated.cursor_at(&module, line, col) else {
        return Vec::new();
    };
    let Some(def_key) = cursor.def_key() else {
        return Vec::new();
    };

    let mut out = Vec::new();

    if def_key.module == module
        && let Some(span) = annotated
            .name_span_of(&def_key)
            .or_else(|| annotated.symbol_at(&def_key).and_then(|s| s.span))
    {
        out.push(DocumentHighlight {
            range: span_to_range(&span),
            kind: HighlightKind::Write,
        });
    }

    for use_key in cursor.references_to_def() {
        if use_key.module != module {
            continue;
        }
        let Some(span) = annotated.span_of_key(&use_key) else {
            continue;
        };
        let kind = if annotated.is_write_target(&use_key) {
            HighlightKind::Write
        } else {
            HighlightKind::Read
        };
        out.push(DocumentHighlight {
            range: span_to_range(&span),
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
    use indexmap::IndexMap;
    use wado_compiler::{Diagnostic as CompilerDiagnostic, SourceError};

    struct TestHost {
        sources: IndexMap<String, Vec<u8>>,
    }

    impl TestHost {
        fn single(path: &str, source: &str) -> Self {
            let mut sources = IndexMap::new();
            sources.insert(path.to_string(), source.as_bytes().to_vec());
            Self { sources }
        }
    }

    impl CompilerHost for TestHost {
        async fn load_source(&self, path: &str) -> Result<Vec<u8>, SourceError> {
            self.sources
                .get(path)
                .cloned()
                .ok_or_else(|| SourceError::NotFound {
                    path: path.to_string(),
                })
        }

        fn emit_diagnostic(&self, _diagnostic: CompilerDiagnostic) {}
    }

    async fn highlights_at(source: &str, line: u32, character: u32) -> Vec<DocumentHighlight> {
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = TestHost::single(path, source);
        document_highlight(source, Position { line, character }, &uri, &host).await
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
