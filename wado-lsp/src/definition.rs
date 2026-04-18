//! Go-to-definition, powered by `wado_compiler::annotate`.
//!
//! Resolution flow:
//! 1. Run `annotate` to produce a fully-resolved [`Annotated`] snapshot.
//! 2. Use `Annotated::ast_id_at` to find the innermost AST node at the cursor.
//! 3. If that node is a use-site (Ident of a local), follow
//!    `Annotated::referenced_symbol` to the binding [`SymbolKey`].
//! 4. Otherwise the cursor AST id itself points at a declared symbol.
//! 5. Translate the resulting [`SymbolKey`] into a [`DefinitionResult`].

use wado_compiler::CompilerHost;
use wado_compiler::annotate::annotate;
use wado_compiler::ast::{self, AstId, Item, Module};
use wado_compiler::token::Span;

use crate::diagnostics::{Position, Range};
use crate::location::{resolve_def_key, span_to_range, symbol_uri, uri_to_filename};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionResult {
    pub uri: String,
    pub range: Range,
}

/// Find the definition of the identifier at `position` in `source`.
///
/// `uri` is the URI of the document being edited; cross-file results carry
/// their own URI (derived from the defining module's `diagnostic_filename`).
pub async fn find_definition<H: CompilerHost>(
    source: &str,
    position: Position,
    uri: &str,
    host: &H,
) -> Option<DefinitionResult> {
    let filename = uri_to_filename(uri);
    let annotated = annotate(source, host, Some(&filename)).await.ok()?;

    let module = annotated.entry_module_source.clone();
    let line = position.line as usize + 1;
    let col = position.character as usize + 1;

    let def_key = resolve_def_key(&annotated, &module, line, col)?;

    let symbol = annotated.symbol_at(&def_key)?;
    let span = annotated
        .name_span_of(&def_key)
        .or(symbol.span)
        .or_else(|| span_of_ast_id(annotated.modules.get(&def_key.module)?, def_key.ast_id))?;
    let def_uri = symbol_uri(&annotated, symbol, uri)?;
    Some(DefinitionResult {
        uri: def_uri,
        range: span_to_range(&span),
    })
}

/// Best-effort span for an arbitrary [`AstId`] — walks module items looking for
/// a matching id. Used only when `name_span_of` has no name-span and the
/// symbol has no declared span (rare).
fn span_of_ast_id(module: &Module, target: AstId) -> Option<Span> {
    for item in &module.items {
        if let Some(span) = item_span_if_match(item, target) {
            return Some(span);
        }
    }
    None
}

fn item_span_if_match(item: &Item, target: AstId) -> Option<Span> {
    match item {
        Item::Function(f) if f.id == target => Some(f.span),
        Item::Struct(s) if s.id == target => Some(s.span),
        Item::Enum(e) if e.id == target => Some(e.span),
        Item::Variant(v) if v.id == target => Some(v.span),
        Item::Flags(fl) if fl.id == target => Some(fl.span),
        Item::Trait(t) if t.id == target => Some(t.span),
        Item::Newtype(n) if n.id == target => Some(n.span),
        Item::Global(g) if g.id == target => Some(g.span),
        _ => None,
    }
}

#[allow(dead_code)]
fn _touch_ast_module_import(module: &ast::Module) {
    let _ = module;
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
        fn new(path: &str, source: &str) -> Self {
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

    async fn def_at(source: &str, line: u32, character: u32) -> Option<DefinitionResult> {
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = TestHost::new(path, source);
        find_definition(source, Position { line, character }, &uri, &host).await
    }

    #[tokio::test]
    async fn param_definition() {
        let source = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n";
        let result = def_at(source, 1, 11)
            .await
            .expect("definition of a in body");
        assert_eq!(result.range.start.line, 0);
        assert_eq!(result.range.start.character, 7);
        assert_eq!(result.range.end.character, 8);
    }

    #[tokio::test]
    async fn local_var_definition() {
        let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x;\n}\n";
        let result = def_at(source, 2, 11)
            .await
            .expect("definition of x in return");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 8);
        assert_eq!(result.range.end.character, 9);
    }

    #[tokio::test]
    async fn shadow_resolution() {
        let source = "fn f() -> i32 {\n    let x = 1;\n    let x = x + 1;\n    return x;\n}\n";
        let result = def_at(source, 2, 12)
            .await
            .expect("RHS x resolves to outer let");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 8);
        assert_eq!(result.range.end.character, 9);
    }

    #[tokio::test]
    async fn item_definition() {
        let source =
            "fn helper() -> i32 {\n    return 1;\n}\nfn run() -> i32 {\n    return helper();\n}\n";
        let result = def_at(source, 4, 11)
            .await
            .expect("call-site resolves to fn helper");
        assert_eq!(result.range.start.line, 0);
        assert_eq!(result.range.start.character, 3);
        assert_eq!(result.range.end.character, 9);
    }

    #[tokio::test]
    async fn struct_destructuring_binding_definition() {
        let source = concat!(
            "struct Point { x: i32, y: i32 }\n",
            "fn f(p: Point) -> i32 {\n",
            "    let { x, y } = p;\n",
            "    return x + y;\n",
            "}\n",
        );
        let result = def_at(source, 3, 11).await.expect("use of destructured x");
        assert_eq!(result.range.start.line, 2);
        assert_eq!(result.range.start.character, 10);
        assert_eq!(result.range.end.character, 11);
    }

    #[tokio::test]
    async fn tuple_destructuring_binding_definition() {
        let source = concat!(
            "fn f() -> i32 {\n",
            "    let [a, b] = [1, 2];\n",
            "    return a + b;\n",
            "}\n",
        );
        let result = def_at(source, 2, 11).await.expect("use of a");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 9);
        assert_eq!(result.range.end.character, 10);
    }

    #[tokio::test]
    async fn closure_param_definition() {
        let source = concat!(
            "fn f() -> i32 {\n",
            "    let g = |x: i32| x + 1;\n",
            "    return g(1);\n",
            "}\n",
        );
        let result = def_at(source, 1, 21).await.expect("use of x in body");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 13);
        assert_eq!(result.range.end.character, 14);
    }

    #[tokio::test]
    async fn closure_capture_definition() {
        let source = concat!(
            "fn f() -> i32 {\n",
            "    let outer = 10;\n",
            "    let g = |x: i32| x + outer;\n",
            "    return g(1);\n",
            "}\n",
        );
        let result = def_at(source, 2, 25).await.expect("capture of outer");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 8);
        assert_eq!(result.range.end.character, 13);
    }

    #[tokio::test]
    async fn if_let_binding_definition() {
        let source = concat!(
            "fn f(opt: Option<i32>) -> i32 {\n",
            "    if let Some(v) = opt {\n",
            "        return v;\n",
            "    }\n",
            "    return 0;\n",
            "}\n",
        );
        let result = def_at(source, 2, 15).await.expect("use of v");
        assert_eq!(result.range.start.line, 1);
        assert_eq!(result.range.start.character, 16);
        assert_eq!(result.range.end.character, 17);
    }

    #[tokio::test]
    async fn match_arm_binding_definition() {
        let source = concat!(
            "fn f(opt: Option<i32>) -> i32 {\n",
            "    return match opt {\n",
            "        Some(v) => v,\n",
            "        None => 0,\n",
            "    };\n",
            "}\n",
        );
        let result = def_at(source, 2, 19).await.expect("use of v in arm");
        assert_eq!(result.range.start.line, 2);
        assert_eq!(result.range.start.character, 13);
        assert_eq!(result.range.end.character, 14);
    }
}
