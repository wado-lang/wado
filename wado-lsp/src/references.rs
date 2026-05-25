//! Find-references, powered by `wado_compiler::annotate`.
//!
//! Resolution flow mirrors `definition.rs`:
//! 1. Run `annotate` to produce a fully-resolved snapshot.
//! 2. Resolve the cursor to a defining [`SymbolKey`].
//! 3. Walk `Annotated::iter_references` and collect every use-site whose
//!    target equals the def key.
//! 4. Translate each use-site into a [`ReferenceLocation`].
//! 5. Optionally prepend the defining occurrence itself.

use serde::{Deserialize, Serialize};
use wado_compiler::annotate::Annotated;
use wado_compiler::symbol::SymbolKey;

use crate::diagnostics::{Position, Range};
use crate::location::{module_uri, span_to_range, symbol_uri};
use crate::text::{PositionEncoding, lsp_position_to_line_col};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceLocation {
    pub uri: String,
    pub range: Range,
}

/// Find every reference to the symbol named at `position` in `source`.
///
/// When `include_declaration` is true, the defining occurrence (the
/// identifier at the symbol's declaration site) is included in the result.
/// The result is deduplicated and sorted by `(uri, range.start)`.
#[must_use]
pub fn find_references(
    annotated: &Annotated,
    source: &str,
    position: Position,
    uri: &str,
    include_declaration: bool,
    encoding: PositionEncoding,
) -> Vec<ReferenceLocation> {
    let module = annotated.entry_module_source.clone();
    let (line, col) = lsp_position_to_line_col(source, position, encoding);

    let Some(cursor) = annotated.cursor_at(&module, line, col) else {
        return Vec::new();
    };
    let Some(def_key) = cursor.def_key() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    if include_declaration
        && let Some(loc) = declaration_location(annotated, &def_key, uri, source, encoding)
    {
        out.push(loc);
    }
    for use_key in cursor.references_to_def() {
        if let Some(loc) = use_site_location(annotated, &use_key, uri, source, encoding) {
            out.push(loc);
        }
    }

    out.sort_by(|a, b| {
        a.uri.cmp(&b.uri).then_with(|| {
            (a.range.start.line, a.range.start.character)
                .cmp(&(b.range.start.line, b.range.start.character))
        })
    });
    out.dedup();
    out
}

pub(crate) fn declaration_location(
    annotated: &Annotated,
    def_key: &SymbolKey,
    request_uri: &str,
    source: &str,
    encoding: PositionEncoding,
) -> Option<ReferenceLocation> {
    let symbol = annotated.symbol_at(def_key)?;
    let span = annotated.name_span_of(def_key).or(symbol.span)?;
    let uri = symbol_uri(annotated, symbol, request_uri)?;
    // Spans for declarations in other modules are byte-indexed against
    // *that* module's source, not the request document. Only re-encode
    // against `source` when the def lives in the entry module; otherwise
    // pass `None` so the conversion stays byte-accurate. This is the
    // same conservative choice `span_to_range` documents for non-ASCII
    // cross-file results — the value carries the compiler's byte column
    // verbatim, which is correct for ASCII and the only choice we can
    // make without loading the other module's text on every query.
    let source_for_range = (def_key.module == annotated.entry_module_source).then_some(source);
    Some(ReferenceLocation {
        uri,
        range: span_to_range(&span, source_for_range, encoding),
    })
}

pub(crate) fn use_site_location(
    annotated: &Annotated,
    use_key: &SymbolKey,
    request_uri: &str,
    source: &str,
    encoding: PositionEncoding,
) -> Option<ReferenceLocation> {
    let span = annotated.span_of_key(use_key)?;
    let uri = module_uri(annotated, &use_key.module, request_uri)?;
    let source_for_range = (use_key.module == annotated.entry_module_source).then_some(source);
    Some(ReferenceLocation {
        uri,
        range: span_to_range(&span, source_for_range, encoding),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use wado_compiler::CompilerHost;
    use wado_compiler::annotate::annotate_with_invocations;
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

    async fn refs_at(
        source: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Vec<ReferenceLocation> {
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = TestHost::single(path, source);
        let invocations = wado_compiler::kiln::InvocationIndex::new();
        let annotated = annotate_with_invocations(source, &host, Some(path), invocations).await;
        find_references(
            &annotated,
            source,
            Position { line, character },
            &uri,
            include_declaration,
            PositionEncoding::Utf16,
        )
    }

    fn ranges(refs: &[ReferenceLocation]) -> Vec<(u32, u32, u32, u32)> {
        refs.iter()
            .map(|r| {
                (
                    r.range.start.line,
                    r.range.start.character,
                    r.range.end.line,
                    r.range.end.character,
                )
            })
            .collect()
    }

    #[test]
    fn local_var_uses() {
        futures::executor::block_on(async {
            let source =
                "fn f() -> i32 {\n    let x: i32 = 1;\n    let y = x + x;\n    return y + x;\n}\n";
            let refs = refs_at(source, 1, 8, false).await;
            assert_eq!(
                ranges(&refs),
                vec![(2, 12, 2, 13), (2, 16, 2, 17), (3, 15, 3, 16),]
            );
        });
    }

    #[test]
    fn includes_declaration_when_requested() {
        futures::executor::block_on(async {
            let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x;\n}\n";
            let refs = refs_at(source, 1, 8, true).await;
            assert_eq!(ranges(&refs), vec![(1, 8, 1, 9), (2, 11, 2, 12),]);
        });
    }

    #[test]
    fn function_references_from_call_site() {
        futures::executor::block_on(async {
            let source = "fn helper() -> i32 {\n    return 1;\n}\nfn run() -> i32 {\n    let a = helper();\n    let b = helper();\n    return a + b;\n}\n";
            // cursor on the declaration of `helper`
            let refs = refs_at(source, 0, 4, false).await;
            assert_eq!(ranges(&refs), vec![(4, 12, 4, 18), (5, 12, 5, 18),]);
        });
    }

    #[test]
    fn cursor_on_use_site_finds_all_uses() {
        futures::executor::block_on(async {
            let source = "fn helper() -> i32 {\n    return 1;\n}\nfn run() -> i32 {\n    let a = helper();\n    let b = helper();\n    return a + b;\n}\n";
            // cursor on the first call to `helper`
            let refs = refs_at(source, 4, 13, false).await;
            assert_eq!(ranges(&refs), vec![(4, 12, 4, 18), (5, 12, 5, 18),]);
        });
    }

    #[test]
    fn param_references() {
        futures::executor::block_on(async {
            let source = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n";
            let refs = refs_at(source, 0, 7, false).await;
            assert_eq!(ranges(&refs), vec![(1, 11, 1, 12)]);
        });
    }

    #[test]
    fn no_match_outside_identifier() {
        futures::executor::block_on(async {
            let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x;\n}\n";
            let refs = refs_at(source, 0, 0, false).await;
            assert!(refs.is_empty(), "got: {refs:?}");
        });
    }
}
