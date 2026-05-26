//! Hover information, powered by `wado_compiler::semantics`.
//!
//! Rendering strategy: `Semantics::cursor_at` places a `Cursor` on the
//! innermost AST node at the request position; `Cursor::def_symbol` chases
//! the use→def edge and returns the binding's `Symbol` (or `None` if the
//! cursor isn't on a recognised name). Locals render as `let` / param
//! signatures (computed from the defining AST node); items delegate to
//! `wado_compiler::unparse`.

use serde::{Deserialize, Serialize};
use wado_compiler::ast::{self, AstId, Expr, Item, Module, Stmt};
use wado_compiler::semantics::Semantics;
use wado_compiler::symbol::{Symbol, SymbolKey, SymbolKind};
use wado_compiler::unparse;

use crate::diagnostics::{Position, Range};
use crate::location::span_to_range;
use crate::query::QueryContext;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoverResult {
    pub contents: MarkupContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkupContent {
    pub kind: MarkupKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkupKind {
    Plaintext,
    Markdown,
}

#[must_use]
pub(crate) fn find_hover(ctx: &QueryContext, position: Position) -> Option<HoverResult> {
    let cursor = ctx.cursor_at(position)?;
    let symbol = cursor.def_symbol()?;
    let signature = match &symbol.kind {
        SymbolKind::Variable(_) => render_local_binding(ctx.sem, &symbol.defined_at, &symbol.name)?,
        _ => render_item_signature(ctx.sem, symbol)?,
    };

    let cursor_span = cursor.span()?;
    Some(HoverResult {
        contents: MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```wado\n{signature}\n```"),
        },
        range: Some(span_to_range(&cursor_span, Some(ctx.source), ctx.encoding)),
    })
}

/// Render a signature for the given item-level symbol.
fn render_item_signature(sem: &Semantics, symbol: &Symbol) -> Option<String> {
    let module = sem.modules.get(&symbol.defined_at.module)?;
    for item in &module.items {
        if let Some(rendered) = item_info(item, &symbol.name) {
            return Some(rendered);
        }
    }
    None
}

/// Render a hover line for a local binding (`let x: T` / `fn f(x: T)`).
fn render_local_binding(sem: &Semantics, def_key: &SymbolKey, name: &str) -> Option<String> {
    let module = sem.modules.get(&def_key.module)?;
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

fn format_binding_name(name: &str) -> String {
    format!("let {name}")
}

fn find_let_in_condition(cond: &ast::Condition, target: AstId, name: &str) -> Option<String> {
    use ast::{Condition, ConditionElement};
    match cond {
        Condition::Expr(e) => find_let_in_expr(e, target, name),
        Condition::LetChain { elements, .. } => {
            for el in elements {
                match el {
                    ConditionElement::Let { pattern, expr, .. } => {
                        if pattern_contains_ident(pattern, target) {
                            return Some(format_binding_name(name));
                        }
                        if let Some(r) = find_let_in_expr(expr, target, name) {
                            return Some(r);
                        }
                    }
                    ConditionElement::Expr(e) => {
                        if let Some(r) = find_let_in_expr(e, target, name) {
                            return Some(r);
                        }
                    }
                }
            }
            None
        }
    }
}

fn pattern_contains_ident(pattern: &ast::Pattern, target: AstId) -> bool {
    match pattern {
        ast::Pattern::Ident { id, .. } | ast::Pattern::MutIdent { id, .. } => *id == target,
        ast::Pattern::Tuple(ps, _) | ast::Pattern::Or(ps) => {
            ps.iter().any(|p| pattern_contains_ident(p, target))
        }
        ast::Pattern::Struct { fields, .. } => fields
            .iter()
            .any(|f| pattern_contains_ident(&f.pattern, target)),
        ast::Pattern::Variant { bindings, .. } => {
            bindings.iter().any(|p| pattern_contains_ident(p, target))
        }
        _ => false,
    }
}

fn find_let_in_stmt(stmt: &Stmt, target: AstId, name: &str) -> Option<String> {
    match stmt {
        Stmt::Let(l) => {
            if pattern_contains_ident(&l.pattern, target) {
                let mut out = String::new();
                out.push_str(if l.is_mut { "let mut " } else { "let " });
                out.push_str(name);
                if let Some(ty) = &l.ty
                    && matches!(
                        &l.pattern,
                        ast::Pattern::Ident { .. } | ast::Pattern::MutIdent { .. }
                    )
                {
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
        Stmt::Return(s) => s
            .value
            .as_ref()
            .and_then(|v| find_let_in_expr(v, target, name)),
        Stmt::TaskReturn(s) => find_let_in_expr(&s.value, target, name),
        Stmt::If(s) => find_let_in_condition(&s.condition, target, name)
            .or_else(|| find_let_in_block(&s.then_block, target, name))
            .or_else(|| {
                s.else_block
                    .as_ref()
                    .and_then(|b| find_let_in_block(b, target, name))
            }),
        Stmt::While(s) => find_let_in_condition(&s.condition, target, name)
            .or_else(|| find_let_in_block(&s.body, target, name)),
        Stmt::For(s) => {
            if let Some(init) = &s.init
                && let Some(r) = find_let_in_stmt(init, target, name)
            {
                return Some(r);
            }
            find_let_in_block(&s.body, target, name)
        }
        Stmt::ForOf(s) => {
            if pattern_contains_ident(&s.binding, target) {
                return Some(format_binding_name(name));
            }
            find_let_in_block(&s.body, target, name)
        }
        Stmt::Loop(s) => find_let_in_block(&s.body, target, name),
        Stmt::Match(m) => {
            for arm in &m.arms {
                if pattern_contains_ident(&arm.pattern, target) {
                    return Some(format_binding_name(name));
                }
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
        Expr::If(e) => find_let_in_block(&e.then_block, target, name).or_else(|| {
            e.else_block
                .as_ref()
                .and_then(|b| find_let_in_block(b, target, name))
        }),
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
                if pattern_contains_ident(&arm.pattern, target) {
                    return Some(format_binding_name(name));
                }
                if let Some(r) = find_let_in_expr(&arm.body, target, name) {
                    return Some(r);
                }
            }
            None
        }
        Expr::Matches(m) => {
            if pattern_contains_ident(&m.pattern, target) {
                return Some(format_binding_name(name));
            }
            if let Some(g) = &m.guard {
                return find_let_in_expr(g, target, name);
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
        Item::Interface(e) if e.name == name => Some(format!("interface {name}")),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MapHost;
    use crate::text::PositionEncoding;

    async fn hover_at(source: &str, line: u32, character: u32) -> Option<HoverResult> {
        hover_at_with_encoding(source, line, character, PositionEncoding::Utf16).await
    }

    #[test]
    fn local_var_hover() {
        futures::executor::block_on(async {
            let source = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x;\n}\n";
            let result = hover_at(source, 2, 11).await.expect("hover on x");
            assert_eq!(result.contents.value, "```wado\nlet x: i32\n```");
        });
    }

    #[test]
    fn param_hover() {
        futures::executor::block_on(async {
            let source = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n";
            let result = hover_at(source, 1, 11).await.expect("hover on a");
            assert_eq!(result.contents.value, "```wado\na: i32\n```");
        });
    }

    #[test]
    fn fn_hover() {
        futures::executor::block_on(async {
            let source = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\nfn run() -> i32 {\n    return add(1, 2);\n}\n";
            let result = hover_at(source, 4, 12).await.expect("hover on add call");
            assert!(
                result
                    .contents
                    .value
                    .contains("fn add(a: i32, b: i32) -> i32"),
                "got: {}",
                result.contents.value
            );
        });
    }

    #[test]
    fn let_mut_hover() {
        futures::executor::block_on(async {
            let source = "fn f() -> i32 {\n    let mut x: i32 = 1;\n    return x;\n}\n";
            let result = hover_at(source, 2, 11).await.expect("hover on x");
            assert_eq!(result.contents.value, "```wado\nlet mut x: i32\n```");
        });
    }

    #[test]
    fn destructured_field_hover() {
        futures::executor::block_on(async {
            let source = concat!(
                "struct Point { x: i32, y: i32 }\n",
                "fn f(p: Point) -> i32 {\n",
                "    let { x, y } = p;\n",
                "    return x;\n",
                "}\n",
            );
            let result = hover_at(source, 3, 11).await.expect("hover on x");
            assert_eq!(result.contents.value, "```wado\nlet x\n```");
        });
    }

    #[test]
    fn closure_param_hover() {
        futures::executor::block_on(async {
            let source = concat!(
                "fn f() -> i32 {\n",
                "    let g = |x: i32| x + 1;\n",
                "    return g(1);\n",
                "}\n",
            );
            let result = hover_at(source, 1, 21).await.expect("hover on x");
            assert_eq!(result.contents.value, "```wado\n|x: i32|\n```");
        });
    }

    #[test]
    fn if_let_binding_hover() {
        futures::executor::block_on(async {
            let source = concat!(
                "fn f(opt: Option<i32>) -> i32 {\n",
                "    if let Some(v) = opt {\n",
                "        return v;\n",
                "    }\n",
                "    return 0;\n",
                "}\n",
            );
            let result = hover_at(source, 2, 15).await.expect("hover on v");
            assert_eq!(result.contents.value, "```wado\nlet v\n```");
        });
    }

    #[test]
    fn for_of_keyword_no_synthetic_iter_hover() {
        // The cursor on the `for` keyword of `for x of items { ... }` must
        // never surface the resolver's synthetic `__iter_N` local. A hover
        // here should return None (the cursor lands between recognised
        // names) or, at most, surface the user's loop variable — never
        // a `let __iter_N` line that exposes compiler internals.
        futures::executor::block_on(async {
            let source = concat!(
                "fn f(items: Array<i32>) -> i32 {\n",
                "    let mut total = 0;\n",
                "    for let item of items {\n",
                "        total = total + item;\n",
                "    }\n",
                "    return total;\n",
                "}\n",
            );
            let result = hover_at(source, 2, 4).await;
            if let Some(r) = &result {
                assert!(
                    !r.contents.value.contains("__iter"),
                    "hover on `for` exposed synthetic iter local: {}",
                    r.contents.value
                );
                // The cursor isn't on a name the user wrote, so we should
                // also not surface unrelated symbols (e.g. dragging the
                // user into `Iterator::next` in core:prelude/array.wado).
                assert!(
                    !r.contents.value.contains("fn next")
                        && !r.contents.value.contains("fn into_iter"),
                    "hover on `for` jumped into the iterator impl: {}",
                    r.contents.value
                );
            }
        });
    }

    #[test]
    fn match_arm_binding_hover() {
        futures::executor::block_on(async {
            let source = concat!(
                "fn f(opt: Option<i32>) -> i32 {\n",
                "    return match opt {\n",
                "        Some(v) => v,\n",
                "        None => 0,\n",
                "    };\n",
                "}\n",
            );
            let result = hover_at(source, 2, 19).await.expect("hover on v");
            assert_eq!(result.contents.value, "```wado\nlet v\n```");
        });
    }

    async fn hover_at_with_encoding(
        source: &str,
        line: u32,
        character: u32,
        encoding: PositionEncoding,
    ) -> Option<HoverResult> {
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = MapHost::single(path, source);
        let sem = wado_compiler::semantics(source, &host, Some(path)).await;
        let ctx = QueryContext {
            sem: &sem,
            source,
            uri: &uri,
            encoding,
        };
        find_hover(&ctx, Position { line, character })
    }

    #[test]
    fn hover_after_non_ascii_identifier_under_utf16() {
        // Pre-existing bug: every query took `position.character + 1` as a
        // compiler byte column, which treated UTF-16 code units as UTF-8
        // bytes. Cursor positions past a multi-byte character on the same
        // line drifted by `byte_len - 1` per character — silently breaking
        // hover on every identifier that followed a non-ASCII string,
        // template, or comment.
        //
        // The fixture puts a use of the parameter `x` on the same line as
        // a comment containing a 3-byte UTF-8 character ('あ'). Under
        // UTF-16 the LSP `character` index of `x` differs from its byte
        // column; the conversion in `text::lsp_position_to_line_col` now
        // bridges them.
        futures::executor::block_on(async {
            // Same-line non-ASCII variant: `あ` lives on the cursor line
            // so the bug fires whether the conversion is wrong or right.
            let source = concat!(
                "fn f(x: i32) -> i32 {\n",
                "    let _s: String = \"あ\"; return x;\n",
                "}\n",
            );
            let line1 = "    let _s: String = \"あ\"; return x;";
            let byte_in_line = line1.rfind('x').unwrap();
            let utf16_in_line: u32 = line1[..byte_in_line]
                .chars()
                .map(|c| c.len_utf16() as u32)
                .sum();
            // Sanity: UTF-16 index differs from byte offset because of `あ`.
            assert_ne!(
                utf16_in_line as usize, byte_in_line,
                "fixture must contain a multi-byte char before the cursor",
            );

            let result = hover_at_with_encoding(source, 1, utf16_in_line, PositionEncoding::Utf16)
                .await
                .expect("hover on return-position x under utf-16 encoding");
            assert_eq!(result.contents.value, "```wado\nx: i32\n```");
        });
    }

    #[test]
    fn hover_on_well_formed_part_survives_unrelated_error() {
        // Partial-result behaviour: a type-error elsewhere in the file must
        // not blank out hover on the unrelated, well-formed function. The
        // resolver bails on the bad body, but the LSP should still surface
        // signatures for whatever was successfully resolved before/around it.
        futures::executor::block_on(async {
            let source = concat!(
                "fn add(a: i32, b: i32) -> i32 {\n",
                "    return a + b;\n",
                "}\n",
                "fn broken() -> i32 {\n",
                "    return \"not an i32\";\n", // type mismatch
                "}\n",
            );
            let result = hover_at(source, 0, 4).await.expect("hover on `add`");
            assert!(
                result
                    .contents
                    .value
                    .contains("fn add(a: i32, b: i32) -> i32"),
                "got: {}",
                result.contents.value,
            );
        });
    }
}
