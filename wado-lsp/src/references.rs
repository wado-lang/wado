//! Find-references, powered by `wado_compiler::annotate`.
//!
//! Resolution flow mirrors `definition.rs`:
//! 1. Run `annotate` to produce a fully-resolved snapshot.
//! 2. Resolve the cursor to a defining [`SymbolKey`].
//! 3. Walk `Annotated::iter_references` and collect every use-site whose
//!    target equals the def key.
//! 4. Translate each use-site into a [`ReferenceLocation`].
//! 5. Optionally prepend the defining occurrence itself.

use wado_compiler::CompilerHost;
use wado_compiler::annotate::{Annotated, annotate};
use wado_compiler::symbol::SymbolKey;

use crate::diagnostics::{Position, Range};
use crate::location::{module_uri, resolve_def_key, span_to_range, symbol_uri, uri_to_filename};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceLocation {
    pub uri: String,
    pub range: Range,
}

/// Find every reference to the symbol named at `position` in `source`.
///
/// When `include_declaration` is true, the defining occurrence (the
/// identifier at the symbol's declaration site) is included in the result.
/// The result is deduplicated and sorted by `(uri, range.start)`.
pub async fn find_references<H: CompilerHost>(
    source: &str,
    position: Position,
    uri: &str,
    include_declaration: bool,
    host: &H,
) -> Vec<ReferenceLocation> {
    let filename = uri_to_filename(uri);
    let Ok(annotated) = annotate(source, host, Some(&filename)).await else {
        return Vec::new();
    };

    let module = annotated.entry_module_source.clone();
    let line = position.line as usize + 1;
    let col = position.character as usize + 1;

    let Some(def_key) = resolve_def_key(&annotated, &module, line, col) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    if include_declaration
        && let Some(loc) = declaration_location(&annotated, &def_key, uri)
    {
        out.push(loc);
    }
    for use_key in annotated.references_to(&def_key) {
        if let Some(loc) = use_site_location(&annotated, &use_key, uri) {
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
) -> Option<ReferenceLocation> {
    let symbol = annotated.symbol_at(def_key)?;
    let span = annotated.name_span_of(def_key).or(symbol.span)?;
    let uri = symbol_uri(annotated, symbol, request_uri)?;
    Some(ReferenceLocation {
        uri,
        range: span_to_range(&span),
    })
}

pub(crate) fn use_site_location(
    annotated: &Annotated,
    use_key: &SymbolKey,
    request_uri: &str,
) -> Option<ReferenceLocation> {
    let span = annotated.span_of_key(use_key)?;
    let uri = module_uri(annotated, &use_key.module, request_uri)?;
    Some(ReferenceLocation {
        uri,
        range: span_to_range(&span),
    })
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

    async fn refs_at(
        source: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Vec<ReferenceLocation> {
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = TestHost::single(path, source);
        find_references(
            source,
            Position { line, character },
            &uri,
            include_declaration,
            &host,
        )
        .await
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

    #[tokio::test]
    async fn local_var_uses() {
        let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    let y = x + x;\n    return y + x;\n}\n";
        let refs = refs_at(source, 1, 8, false).await;
        assert_eq!(
            ranges(&refs),
            vec![(2, 12, 2, 13), (2, 16, 2, 17), (3, 15, 3, 16),]
        );
    }

    #[tokio::test]
    async fn includes_declaration_when_requested() {
        let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x;\n}\n";
        let refs = refs_at(source, 1, 8, true).await;
        assert_eq!(ranges(&refs), vec![(1, 8, 1, 9), (2, 11, 2, 12),]);
    }

    #[tokio::test]
    async fn function_references_from_call_site() {
        let source = "fn helper() -> i32 {\n    return 1;\n}\nfn run() -> i32 {\n    let a = helper();\n    let b = helper();\n    return a + b;\n}\n";
        // cursor on the declaration of `helper`
        let refs = refs_at(source, 0, 4, false).await;
        assert_eq!(ranges(&refs), vec![(4, 12, 4, 18), (5, 12, 5, 18),]);
    }

    #[tokio::test]
    async fn cursor_on_use_site_finds_all_uses() {
        let source = "fn helper() -> i32 {\n    return 1;\n}\nfn run() -> i32 {\n    let a = helper();\n    let b = helper();\n    return a + b;\n}\n";
        // cursor on the first call to `helper`
        let refs = refs_at(source, 4, 13, false).await;
        assert_eq!(ranges(&refs), vec![(4, 12, 4, 18), (5, 12, 5, 18),]);
    }

    #[tokio::test]
    async fn param_references() {
        let source = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n";
        let refs = refs_at(source, 0, 7, false).await;
        assert_eq!(ranges(&refs), vec![(1, 11, 1, 12)]);
    }

    #[tokio::test]
    async fn no_match_outside_identifier() {
        let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x;\n}\n";
        let refs = refs_at(source, 0, 0, false).await;
        assert!(refs.is_empty(), "got: {refs:?}");
    }
}
