//! Find-references, powered by `wado_compiler::semantics`.
//!
//! Resolution flow mirrors `definition.rs`:
//! 1. Run `semantics` to produce a fully-resolved snapshot.
//! 2. Resolve the cursor to a defining `AstId`.
//! 3. Walk `Semantics::iter_references` and collect every use-site whose
//!    target equals the def key.
//! 4. Translate each use-site into a [`ReferenceLocation`].
//! 5. Optionally prepend the defining occurrence itself.

use serde::{Deserialize, Serialize};

use crate::diagnostics::{Position, Range};
use crate::location::{def_uri, module_uri};
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
    references_for_def(ctx, def_key, include_declaration)
}

/// Collect every reference to the definition `def_key`, optionally including
/// the declaration site. Shared by the position-based [`find_references`] and
/// the name-based `Engine::references_by_symbol`.
#[must_use]
pub(crate) fn references_for_def(
    ctx: &QueryContext,
    def_key: wado_compiler::ast::AstId,
    include_declaration: bool,
) -> Vec<ReferenceLocation> {
    let mut out = Vec::new();
    if include_declaration && let Some(loc) = declaration_location(ctx, def_key) {
        out.push(loc);
    }
    for use_id in ctx.sem.references_to(def_key) {
        if let Some(loc) = use_site_location(ctx, use_id) {
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
    def_id: wado_compiler::ast::AstId,
) -> Option<ReferenceLocation> {
    let span = ctx.declaration_span(def_id)?;
    Some(ReferenceLocation {
        uri: def_uri(ctx.sem, def_id, ctx.uri)?,
        range: ctx.range_of(&span, def_id),
    })
}

pub(crate) fn use_site_location(
    ctx: &QueryContext,
    use_id: wado_compiler::ast::AstId,
) -> Option<ReferenceLocation> {
    let span = ctx.sem.span_of_id(use_id)?;
    let uri = module_uri(ctx.entry(), ctx.sem.module_of_id(use_id)?, ctx.uri);
    Some(ReferenceLocation {
        uri,
        range: ctx.range_of(&span, use_id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_ctx::with_ctx;
    use crate::text::PositionEncoding;

    async fn refs_at(
        source: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Vec<ReferenceLocation> {
        with_ctx(source, PositionEncoding::Utf16, |ctx| {
            find_references(ctx, Position { line, character }, include_declaration)
        })
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

    const MEMBERS: &str = "struct P { x: i32 }\nimpl P {\n    fn get(&self) -> i32 { return self.x; }\n}\nfn f(p: P) -> i32 {\n    return p.get() + p.x;\n}\n";

    #[test]
    fn method_references_from_its_declaration() {
        futures::executor::block_on(async {
            // cursor on `get` in `fn get(&self)`
            let refs = refs_at(MEMBERS, 2, 8, false).await;
            assert_eq!(ranges(&refs), vec![(5, 13, 5, 16)]);
        });
    }

    #[test]
    fn field_references_from_its_declaration() {
        futures::executor::block_on(async {
            // cursor on `x` in `struct P { x: i32 }`
            let refs = refs_at(MEMBERS, 0, 11, false).await;
            assert_eq!(ranges(&refs), vec![(2, 39, 2, 40), (5, 23, 5, 24)]);
        });
    }

    #[test]
    fn member_references_from_its_declaration_include_it() {
        futures::executor::block_on(async {
            let refs = refs_at(MEMBERS, 0, 11, true).await;
            assert_eq!(
                ranges(&refs),
                vec![(0, 11, 0, 12), (2, 39, 2, 40), (5, 23, 5, 24)]
            );
            let refs = refs_at(MEMBERS, 2, 8, true).await;
            assert_eq!(ranges(&refs), vec![(2, 7, 2, 10), (5, 13, 5, 16)]);
        });
    }

    #[test]
    fn no_match_in_a_body_off_any_name() {
        futures::executor::block_on(async {
            // cursor on the whitespace after `{` in `get`'s body
            let refs = refs_at(MEMBERS, 2, 26, false).await;
            assert!(refs.is_empty(), "got: {refs:?}");
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
