//! Find-references, powered by `wado_compiler::semantics`.
//!
//! Resolution flow mirrors `definition.rs`:
//! 1. Run `semantics` to produce a fully-resolved snapshot.
//! 2. Resolve the cursor to a defining [`SymbolKey`].
//! 3. Walk `Semantics::iter_references` and collect every use-site whose
//!    target equals the def key.
//! 4. Translate each use-site into a [`ReferenceLocation`].
//! 5. Optionally prepend the defining occurrence itself.

use serde::{Deserialize, Serialize};
use wado_compiler::symbol::SymbolKey;

use crate::diagnostics::{Position, Range};
use crate::location::{module_uri, span_to_range, symbol_uri};
use crate::query::QueryContext;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceLocation {
    pub uri: String,
    pub range: Range,
}

/// Find every reference to the symbol named at `position` in `ctx.source`.
///
/// When `include_declaration` is true, the defining occurrence (the
/// identifier at the symbol's declaration site) is included in the result.
/// The result is deduplicated and sorted by `(uri, range.start)`.
#[must_use]
pub(crate) fn find_references(
    ctx: &QueryContext,
    position: Position,
    include_declaration: bool,
) -> Vec<ReferenceLocation> {
    let Some(cursor) = ctx.cursor_at(position) else {
        return Vec::new();
    };
    let Some(def_key) = cursor.def_key() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    if include_declaration && let Some(loc) = declaration_location(ctx, &def_key) {
        out.push(loc);
    }
    for use_key in cursor.references_to_def() {
        if let Some(loc) = use_site_location(ctx, &use_key) {
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
    ctx: &QueryContext,
    def_key: &SymbolKey,
) -> Option<ReferenceLocation> {
    let symbol = ctx.sem.symbol_at(def_key)?;
    let span = ctx.sem.name_span_of(def_key).or(symbol.span)?;
    let uri = symbol_uri(ctx.entry(), symbol, ctx.uri)?;
    Some(ReferenceLocation {
        uri,
        range: span_to_range(&span, ctx.source_for_key(def_key), ctx.encoding),
    })
}

pub(crate) fn use_site_location(
    ctx: &QueryContext,
    use_key: &SymbolKey,
) -> Option<ReferenceLocation> {
    let span = ctx.sem.span_of_key(use_key)?;
    let uri = module_uri(ctx.entry(), &use_key.module, ctx.uri)?;
    Some(ReferenceLocation {
        uri,
        range: span_to_range(&span, ctx.source_for_key(use_key), ctx.encoding),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MapHost;
    use crate::text::PositionEncoding;

    async fn refs_at(
        source: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Vec<ReferenceLocation> {
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = MapHost::single(path, source);
        let sem = wado_compiler::semantics(source, &host, Some(path)).await;
        let ctx = QueryContext {
            sem: &sem,
            source,
            uri: &uri,
            encoding: PositionEncoding::Utf16,
        };
        find_references(&ctx, Position { line, character }, include_declaration)
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
