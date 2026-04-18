//! Hover information, powered by `wado_compiler::annotate`.
//!
//! Rendering strategy: `Annotated::ast_id_at` finds the innermost node at the
//! cursor. If the node is an `IdentExpr` that resolves to a local binding, we
//! render the let/param signature directly from the AST. Otherwise we fall
//! back to item-level lookup through the symbol table and delegate signature
//! rendering to `wado_compiler::unparse`.

use wado_compiler::CompilerHost;
use wado_compiler::annotate::{Annotated, annotate};
use wado_compiler::ast::{self, AstId, Expr, Item, Module, Stmt};
use wado_compiler::lexer::Lexer;
use wado_compiler::name::ModuleSource;
use wado_compiler::symbol::{Symbol, SymbolKey, SymbolKind};
use wado_compiler::token::{Span, Token, TokenKind};
use wado_compiler::unparse;

use crate::diagnostics::{Position, Range};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverResult {
    pub contents: String,
    pub range: Range,
}

pub async fn find_hover<H: CompilerHost>(
    source: &str,
    position: Position,
    uri: &str,
    host: &H,
) -> Option<HoverResult> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().ok()?;
    let (_ident_name, token_range, token_span) = find_ident_at_position(&tokens, position)?;

    let filename = uri_to_filename(uri);
    let annotated = annotate(source, host, Some(&filename)).await.ok()?;

    let module = annotated.entry_module_source.clone();
    let line = position.line as usize + 1;
    let col = position.character as usize + 1;

    let signature = resolve_hover_signature(&annotated, &module, &tokens, line, col)
        .or_else(|| resolve_hover_signature_by_name(&annotated, source, position))?;
    let _ = token_span;

    Some(HoverResult {
        contents: format!("```wado\n{signature}\n```"),
        range: token_range,
    })
}

fn resolve_hover_signature(
    annotated: &Annotated,
    module: &ModuleSource,
    _tokens: &[Token],
    line: usize,
    col: usize,
) -> Option<String> {
    let cursor_id = annotated.ast_id_at(module, line, col)?;
    let cursor_key = SymbolKey::new(module.clone(), cursor_id);
    let def_key = annotated
        .referenced_symbol(&cursor_key)
        .unwrap_or_else(|| cursor_key.clone());

    let symbol = annotated.symbol_at(&def_key)?;
    match &symbol.kind {
        SymbolKind::Variable(_) => render_local_binding(annotated, &def_key, &symbol.name),
        _ => render_item_signature(annotated, symbol),
    }
}

fn resolve_hover_signature_by_name(
    annotated: &Annotated,
    source: &str,
    position: Position,
) -> Option<String> {
    let mut lexer = Lexer::new(source);
    let tokens = lexer.tokenize().ok()?;
    let (name, _range, _span) = find_ident_at_position(&tokens, position)?;
    let symbol = annotated
        .symbols
        .lookup_in_module(&annotated.entry_module_source, &name)
        .or_else(|| {
            annotated
                .modules
                .keys()
                .find_map(|ms| annotated.symbols.lookup_in_module(ms, &name))
        })?;
    render_item_signature(annotated, symbol)
}

fn find_ident_at_position(
    tokens: &[Token],
    position: Position,
) -> Option<(String, Range, Span)> {
    let target_line = position.line as usize + 1;
    let target_col = position.character as usize + 1;

    for token in tokens {
        let TokenKind::Ident(name) = &token.kind else {
            continue;
        };
        if target_line < token.span.line || target_line > token.span.end_line {
            continue;
        }
        if target_line == token.span.line && target_col < token.span.column {
            continue;
        }
        if target_line == token.span.end_line && target_col >= token.span.end_column {
            continue;
        }
        let range = Range {
            start: Position {
                line: (token.span.line - 1) as u32,
                character: (token.span.column - 1) as u32,
            },
            end: Position {
                line: (token.span.end_line - 1) as u32,
                character: (token.span.end_column - 1) as u32,
            },
        };
        return Some((name.clone(), range, token.span));
    }
    None
}

/// Render a signature for the given item-level symbol.
fn render_item_signature(annotated: &Annotated, symbol: &Symbol) -> Option<String> {
    let module = annotated.modules.get(&symbol.defined_at.module)?;
    for item in &module.items {
        if let Some(rendered) = item_info(item, &symbol.name) {
            return Some(rendered);
        }
    }
    None
}

/// Render a hover line for a local binding (`let x: T` / `fn f(x: T)`).
fn render_local_binding(annotated: &Annotated, def_key: &SymbolKey, name: &str) -> Option<String> {
    let module = annotated.modules.get(&def_key.module)?;
    render_local_in_module(module, def_key.ast_id, name)
}

fn render_local_in_module(module: &Module, target: AstId, name: &str) -> Option<String> {
    for item in &module.items {
        if let Some(s) = render_local_in_item(item, target, name) {
            return Some(s);
        }
    }
    None
}

fn render_local_in_item(item: &Item, target: AstId, name: &str) -> Option<String> {
    match item {
        Item::Function(f) => {
            for p in &f.params {
                if p.id == target {
                    let mut out = String::new();
                    unparse::unparse_param_into(p, &mut out);
                    return Some(out);
                }
            }
            if let Some(body) = &f.body {
                return find_let_in_block(body, target, name);
            }
            None
        }
        Item::Impl(imp) => {
            for m in &imp.methods {
                for p in &m.params {
                    if p.id == target {
                        let mut out = String::new();
                        unparse::unparse_param_into(p, &mut out);
                        return Some(out);
                    }
                }
                if let Some(body) = &m.body
                    && let Some(s) = find_let_in_block(body, target, name)
                {
                    return Some(s);
                }
            }
            None
        }
        Item::Trait(t) => {
            for m in &t.methods {
                for p in &m.params {
                    if p.id == target {
                        let mut out = String::new();
                        unparse::unparse_param_into(p, &mut out);
                        return Some(out);
                    }
                }
                if let Some(body) = &m.body
                    && let Some(s) = find_let_in_block(body, target, name)
                {
                    return Some(s);
                }
            }
            None
        }
        Item::Test(t) => find_let_in_block(&t.body, target, name),
        Item::Global(g) => find_let_in_expr(&g.initializer, target, name),
        _ => None,
    }
}

fn find_let_in_block(block: &ast::Block, target: AstId, name: &str) -> Option<String> {
    for stmt in &block.stmts {
        if let Some(s) = find_let_in_stmt(stmt, target, name) {
            return Some(s);
        }
    }
    None
}

fn find_let_in_stmt(stmt: &Stmt, target: AstId, name: &str) -> Option<String> {
    match stmt {
        Stmt::Let(l) => {
            if l.id == target {
                let mut out = String::new();
                out.push_str(if l.is_mut { "let mut " } else { "let " });
                out.push_str(name);
                if let Some(ty) = &l.ty {
                    out.push_str(": ");
                    unparse::unparse_type_into(ty, &mut out);
                }
                return Some(out);
            }
            if let Some(v) = &l.value {
                return find_let_in_expr(v, target, name);
            }
            None
        }
        Stmt::Expr(s) => find_let_in_expr(&s.expr, target, name),
        Stmt::Return(s) => s.value.as_ref().and_then(|v| find_let_in_expr(v, target, name)),
        Stmt::TaskReturn(s) => find_let_in_expr(&s.value, target, name),
        Stmt::If(s) => find_let_in_block(&s.then_block, target, name)
            .or_else(|| s.else_block.as_ref().and_then(|b| find_let_in_block(b, target, name))),
        Stmt::While(s) => find_let_in_block(&s.body, target, name),
        Stmt::For(s) => {
            if let Some(init) = &s.init
                && let Some(r) = find_let_in_stmt(init, target, name)
            {
                return Some(r);
            }
            find_let_in_block(&s.body, target, name)
        }
        Stmt::ForOf(s) => find_let_in_block(&s.body, target, name),
        Stmt::Loop(s) => find_let_in_block(&s.body, target, name),
        Stmt::Match(m) => {
            for arm in &m.arms {
                if let Some(r) = find_let_in_expr(&arm.body, target, name) {
                    return Some(r);
                }
            }
            None
        }
        Stmt::Break(_) | Stmt::Continue(_) => None,
        Stmt::Assert(_) => None,
        Stmt::LabeledBlock(s) => find_let_in_block(&s.block, target, name),
    }
}

fn find_let_in_expr(expr: &Expr, target: AstId, name: &str) -> Option<String> {
    match expr {
        Expr::Block(b) => find_let_in_block(b, target, name),
        Expr::If(e) => find_let_in_block(&e.then_block, target, name)
            .or_else(|| e.else_block.as_ref().and_then(|b| find_let_in_block(b, target, name))),
        Expr::Closure(c) => {
            for p in &c.params {
                if p.id == target {
                    let mut out = String::from("|");
                    out.push_str(&p.name);
                    if let Some(ty) = &p.ty {
                        out.push_str(": ");
                        unparse::unparse_type_into(ty, &mut out);
                    }
                    out.push('|');
                    return Some(out);
                }
            }
            find_let_in_expr(&c.body, target, name)
        }
        Expr::LabeledBlock(lb) => find_let_in_block(&lb.block, target, name),
        Expr::Match(m) => {
            for arm in &m.arms {
                if let Some(r) = find_let_in_expr(&arm.body, target, name) {
                    return Some(r);
                }
            }
            None
        }
        _ => None,
    }
}

fn item_info(item: &Item, name: &str) -> Option<String> {
    match item {
        Item::Function(f) if f.name == name => Some(unparse::unparse_function_signature(f)),
        Item::Struct(s) if s.name == name => Some(unparse::unparse_struct_header(s)),
        Item::Enum(e) if e.name == name => Some(unparse::unparse_enum_header(e)),
        Item::Variant(v) if v.name == name => Some(unparse::unparse_variant_header(v)),
        Item::Flags(fl) if fl.name == name => Some(unparse::unparse_flags_header(fl)),
        Item::Trait(t) if t.name == name => Some(unparse::unparse_trait_header(t)),
        Item::Newtype(n) if n.name == name => Some(unparse::unparse_newtype_signature(n)),
        Item::Effect(e) if e.name == name => Some(format!("effect {name}")),
        Item::Global(g) if g.name == name => Some(unparse::unparse_global_signature(g)),
        Item::Impl(imp) => {
            for method in &imp.methods {
                if method.name == name {
                    return Some(unparse::unparse_function_signature(method));
                }
            }
            None
        }
        Item::Enum(e) => e
            .cases
            .iter()
            .find(|c| c.name == name)
            .map(|c| unparse::unparse_enum_case(&e.name, c)),
        Item::Variant(v) => v
            .cases
            .iter()
            .find(|c| c.name == name)
            .map(|c| unparse::unparse_variant_case(&v.name, c)),
        Item::Struct(s) => s
            .fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| unparse::unparse_struct_field(&s.name, f)),
        Item::Trait(t) => {
            for method in &t.methods {
                if method.name == name {
                    return Some(unparse::unparse_function_signature(method));
                }
            }
            None
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use wado_compiler::{Diagnostic as CompilerDiagnostic, SourceError};

    struct TestHost {
        sources: HashMap<String, Vec<u8>>,
    }

    impl TestHost {
        fn new(path: &str, source: &str) -> Self {
            let mut sources = HashMap::new();
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

    async fn hover_at(source: &str, line: u32, character: u32) -> Option<HoverResult> {
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = TestHost::new(path, source);
        find_hover(source, Position { line, character }, &uri, &host).await
    }

    #[tokio::test]
    async fn local_var_hover() {
        let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x;\n}\n";
        let result = hover_at(source, 2, 11).await.expect("hover on x");
        assert_eq!(result.contents, "```wado\nlet x: i32\n```");
    }

    #[tokio::test]
    async fn param_hover() {
        let source = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n";
        let result = hover_at(source, 1, 11).await.expect("hover on a");
        assert_eq!(result.contents, "```wado\na: i32\n```");
    }

    #[tokio::test]
    async fn fn_hover() {
        let source = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\nfn run() -> i32 {\n    return add(1, 2);\n}\n";
        let result = hover_at(source, 4, 12).await.expect("hover on add call");
        assert!(
            result.contents.contains("fn add(a: i32, b: i32) -> i32"),
            "got: {}",
            result.contents
        );
    }
}
