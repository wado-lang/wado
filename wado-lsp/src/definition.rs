//! Go-to-definition, powered by `wado_compiler::annotate`.
//!
//! Resolution flow:
//! 1. Run `annotate` to produce a fully-resolved [`Annotated`] snapshot.
//! 2. Use `Annotated::ast_id_at` to find the innermost AST node at the cursor.
//! 3. If that node is a use-site (Ident of a local), follow
//!    `Annotated::referenced_symbol` to the binding [`SymbolKey`].
//! 4. Otherwise, fall back to per-module name lookup for item-level symbols.
//! 5. Translate the resulting [`SymbolKey`] into a [`DefinitionResult`].

use wado_compiler::CompilerHost;
use wado_compiler::annotate::{Annotated, annotate};
use wado_compiler::ast::{self, AstId, Item, Module};
use wado_compiler::lexer::Lexer;
use wado_compiler::name::ModuleSource;
use wado_compiler::symbol::{Symbol, SymbolKey};
use wado_compiler::token::{Span, TokenKind};

use crate::diagnostics::{Position, Range};

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

    let def_key = resolve_def_key(&annotated, &module, source, position, line, col)?;

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

/// Resolve the defining [`SymbolKey`] for the identifier at the cursor.
///
/// Tries in order:
/// 1. `ast_id_at` → `referenced_symbol` (local variable / parameter)
/// 2. `ast_id_at` → key points directly to a declared symbol (item)
/// 3. Lexer-driven name lookup in the entry module (imports, cross-module)
fn resolve_def_key(
    annotated: &Annotated,
    module: &ModuleSource,
    source: &str,
    position: Position,
    line: usize,
    col: usize,
) -> Option<SymbolKey> {
    if let Some(ast_id) = annotated.ast_id_at(module, line, col) {
        let cursor_key = SymbolKey::new(module.clone(), ast_id);
        if let Some(def) = annotated.referenced_symbol(&cursor_key) {
            return Some(def);
        }
        if annotated.symbol_at(&cursor_key).is_some() {
            return Some(cursor_key);
        }
    }

    let ident = find_ident_at_position(source, position)?;
    let entry = &annotated.entry_module_source;
    if let Some(sym) = annotated.symbols.lookup_in_module(entry, &ident) {
        return Some(sym.defined_at.clone());
    }
    annotated
        .modules
        .keys()
        .find_map(|ms| annotated.symbols.lookup_in_module(ms, &ident))
        .map(|s| s.defined_at.clone())
}

/// Scan `tokens` for an identifier whose span covers `position`. Used as a
/// fallback when the AST-id cursor lookup does not yield a resolvable symbol.
fn find_ident_at_position(source: &str, position: Position) -> Option<String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().ok()?;
    let target_line = position.line as usize + 1;
    let target_col = position.character as usize + 1;

    for token in &tokens {
        if !token_contains(&token.span, target_line, target_col) {
            continue;
        }
        if let TokenKind::Ident(name) = &token.kind {
            return Some(name.clone());
        }
        if token.kind.as_ident_name().is_some()
            && matches!(
                token.kind,
                TokenKind::Flags | TokenKind::Type | TokenKind::Of | TokenKind::From
            )
        {
            return token.kind.as_ident_name().map(str::to_string);
        }
    }
    None
}

fn token_contains(span: &Span, target_line: usize, target_col: usize) -> bool {
    if target_line < span.line || target_line > span.end_line {
        return false;
    }
    if target_line == span.line && target_col < span.column {
        return false;
    }
    if target_line == span.end_line && target_col >= span.end_column {
        return false;
    }
    true
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

/// Derive the URI for a symbol's defining module.
fn symbol_uri(annotated: &Annotated, symbol: &Symbol, request_uri: &str) -> Option<String> {
    let module = &symbol.defined_at.module;
    if module == &annotated.entry_module_source {
        return Some(request_uri.to_string());
    }
    match module {
        ModuleSource::EntryPoint { filename } => Some(filename_to_uri(filename)),
        ModuleSource::Local { path } => Some(resolve_local_uri(path, request_uri)),
        _ => None,
    }
}

fn uri_to_filename(uri: &str) -> String {
    if let Some(path) = uri.strip_prefix("file://") {
        path.to_string()
    } else {
        uri.to_string()
    }
}

fn filename_to_uri(filename: &str) -> String {
    if filename.starts_with("file://") {
        filename.to_string()
    } else if filename.starts_with('/') {
        format!("file://{filename}")
    } else {
        filename.to_string()
    }
}

fn resolve_local_uri(module_path: &str, request_uri: &str) -> String {
    if module_path.starts_with('/') || module_path.starts_with("file://") {
        return filename_to_uri(module_path);
    }
    let request_path = uri_to_filename(request_uri);
    let base_dir = request_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    let normalized = module_path.strip_prefix("./").unwrap_or(module_path);
    if base_dir.is_empty() {
        filename_to_uri(normalized)
    } else {
        filename_to_uri(&format!("{base_dir}/{normalized}"))
    }
}

fn span_to_range(span: &Span) -> Range {
    Range {
        start: Position {
            line: span.line.saturating_sub(1) as u32,
            character: span.column.saturating_sub(1) as u32,
        },
        end: Position {
            line: span.end_line.saturating_sub(1) as u32,
            character: span.end_column.saturating_sub(1) as u32,
        },
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
