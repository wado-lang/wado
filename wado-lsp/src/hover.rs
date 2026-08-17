//! Hover information, powered by `wado_compiler::semantics`.
//!
//! Rendering strategy: `Semantics::cursor_at` places a `Cursor` on the
//! innermost AST node at the request position; `Cursor::def_symbol` chases
//! the use→def edge and returns the binding's `Symbol` (or `None` if the
//! cursor isn't on a recognised name). Locals render as `let` / param
//! signatures (computed from the defining AST node); items delegate to
//! `wado_compiler::unparse`.

use serde::{Deserialize, Serialize};
use wado_compiler::ast::{self, AstId, AstVisitor, Expr, Item, Stmt};
use wado_compiler::semantics::Semantics;
use wado_compiler::symbol::{Symbol, SymbolKind};
use wado_compiler::unparse;

use crate::ast_search::{self, FirstMatch};
use crate::diagnostics::{Position, Range};
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

/// Position-based hover. `public_only` controls how container types render
/// their members (matching `wado doc` when true; everything when false).
#[must_use]
pub(crate) fn find_hover_opts(
    ctx: &QueryContext,
    position: Position,
    public_only: bool,
) -> Option<HoverResult> {
    let cursor = ctx.cursor_at(position)?;
    let symbol = cursor.def_symbol()?;
    let signature = match &symbol.kind {
        SymbolKind::Variable(_) => render_local_binding(ctx.sem, symbol.defined_at, &symbol.name)?,
        _ => render_item_signature(ctx.sem, symbol, public_only)?,
    };

    let cursor_span = cursor.span()?;
    Some(HoverResult {
        contents: MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```wado\n{signature}\n```"),
        },
        range: Some(ctx.range_in_document(&cursor_span)),
    })
}

/// Hover for an item-level [`Symbol`] resolved by name (no cursor). Renders the
/// declaration; for a type, also appends its `impl` blocks (method / associated
/// constant signatures) as Wado, so the result is a usage overview. With
/// `public_only` the rendering matches `wado doc` (private fields elided as
/// `..`, only `pub` inherent members shown); otherwise everything is shown.
/// Locals are not name-resolution targets so they yield `None`. The result
/// carries no `range` — there is no request position to anchor it.
#[must_use]
pub(crate) fn hover_for_item_symbol(
    sem: &Semantics,
    symbol: &Symbol,
    public_only: bool,
) -> Option<HoverResult> {
    if matches!(symbol.kind, SymbolKind::Variable(_)) {
        return None;
    }
    let mut value = render_item_signature(sem, symbol, public_only)?;
    if is_type_symbol(&symbol.kind) {
        append_impl_blocks(sem, symbol, public_only, &mut value);
    }
    Some(fenced_hover(value))
}

/// True for symbol kinds that name a type (and so can carry `impl` blocks).
fn is_type_symbol(kind: &SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Struct(_)
            | SymbolKind::Enum(_)
            | SymbolKind::Variant(_)
            | SymbolKind::Flags(_)
            | SymbolKind::Newtype(_)
            | SymbolKind::Resource(_)
            | SymbolKind::Trait(_)
            | SymbolKind::Effect(_)
    )
}

/// Append the module's `impl` blocks targeting `symbol`'s type, each rendered
/// as Wado with member signatures (bodies omitted).
fn append_impl_blocks(sem: &Semantics, symbol: &Symbol, public_only: bool, out: &mut String) {
    let Some(module) = sem.modules.get(symbol.module_source()) else {
        return;
    };
    for item in &module.items {
        let Item::Impl(b) = item else { continue };
        if b.ty.head_base_name() != Some(symbol.name.as_str()) {
            continue;
        }
        let block = unparse::unparse_impl_block_signature(b, public_only);
        if !block.is_empty() {
            out.push_str("\n\n");
            out.push_str(&block);
        }
    }
}

/// Hover for a method or free function reached by [`AstId`] (e.g. a symbol
/// notation `Type::m`), where there is no item-level [`Symbol`] to render.
#[must_use]
pub(crate) fn hover_for_function(f: &ast::Function) -> HoverResult {
    fenced_hover(unparse::unparse_function_signature(f))
}

fn fenced_hover(signature: String) -> HoverResult {
    HoverResult {
        contents: MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```wado\n{signature}\n```"),
        },
        range: None,
    }
}

/// Render a signature for the given item-level symbol.
///
/// Matching is by [`AstId`], never by name: a module may declare one name
/// twice — a struct field beside a free function, an enum case beside a
/// method — and only `Symbol::defined_at` picks out which one.
fn render_item_signature(sem: &Semantics, symbol: &Symbol, public_only: bool) -> Option<String> {
    let module = sem.modules.get(symbol.module_source())?;
    let target = symbol.defined_at;
    module
        .items
        .iter()
        .find_map(|item| item_info(item, target, public_only))
}

/// Render a hover line for a local binding (`let x: T` / `fn f(x: T)`).
fn render_local_binding(sem: &Semantics, def_id: AstId, name: &str) -> Option<String> {
    let module = sem.modules.get(sem.module_of_id(def_id)?)?;
    ast_search::find_in_module(
        module,
        LocalRenderer {
            target: def_id,
            name,
            result: None,
        },
    )
}

/// Locates the AST node that binds `target` and renders its declaration.
///
/// Traversal is [`AstVisitor`]'s, so every shape that can hold a binding is
/// reached by construction.
///
/// Only the shapes carrying extra syntax are intercepted: `Stmt::Let` (for
/// `mut` and the type annotation), function and closure parameters. Every
/// other binding site — match arms, `if let` / `while let`, `for … of` —
/// reaches [`Self::visit_pattern`] and renders as a bare `let name`.
struct LocalRenderer<'a> {
    target: AstId,
    name: &'a str,
    result: Option<String>,
}

impl LocalRenderer<'_> {
    /// The declaring param of `params`, rendered as `name: T`.
    fn render_param(&self, params: &[ast::Param]) -> Option<String> {
        let param = params.iter().find(|p| p.id == self.target)?;
        let mut out = String::new();
        unparse::unparse_param_into(param, &mut out);
        Some(out)
    }

    /// `let [mut ]name[: T]`. The annotation is shown only for a simple
    /// binding — for a destructuring pattern the annotation describes the
    /// whole scrutinee, not the leaf the cursor sits on.
    fn render_let(&self, l: &ast::LetStmt) -> Option<String> {
        if !pattern_binds(&l.pattern, self.target) {
            return None;
        }
        let mut out = String::new();
        out.push_str(if l.is_mut { "let mut " } else { "let " });
        out.push_str(self.name);
        if let Some(ty) = &l.ty
            && matches!(
                &l.pattern,
                ast::Pattern::Ident { .. } | ast::Pattern::MutIdent { .. }
            )
        {
            out.push_str(": ");
            unparse::unparse_type_into(ty, &mut out);
        }
        Some(out)
    }

    /// `|name: T|` for the closure parameter declaring `target`.
    fn render_closure_param(&self, params: &[ast::ClosureParam]) -> Option<String> {
        let param = params.iter().find(|p| p.id == self.target)?;
        let mut out = String::from("|");
        out.push_str(&param.name);
        if let Some(ty) = &param.ty {
            out.push_str(": ");
            unparse::unparse_type_into(ty, &mut out);
        }
        out.push('|');
        Some(out)
    }
}

impl FirstMatch for LocalRenderer<'_> {
    type Output = String;

    fn found(&self) -> bool {
        self.result.is_some()
    }

    fn take(self) -> Option<String> {
        self.result
    }
}

impl AstVisitor for LocalRenderer<'_> {
    fn visit_function(&mut self, func: &ast::Function) {
        if self.result.is_some() {
            return;
        }
        self.result = self.render_param(&func.params);
        if self.result.is_none() {
            ast::walk_function(self, func);
        }
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        if self.result.is_some() {
            return;
        }
        // `Stmt::Let` is intercepted before `walk_stmt` would hand its
        // pattern to `visit_pattern`, which renders the annotation-less form.
        if let Stmt::Let(l) = stmt
            && let Some(rendered) = self.render_let(l)
        {
            self.result = Some(rendered);
            return;
        }
        ast::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if self.result.is_some() {
            return;
        }
        if let Expr::Closure(c) = expr
            && let Some(rendered) = self.render_closure_param(&c.params)
        {
            self.result = Some(rendered);
            return;
        }
        ast::walk_expr(self, expr);
    }

    fn visit_pattern(&mut self, pat: &ast::Pattern) {
        if self.result.is_some() {
            return;
        }
        if pattern_binds(pat, self.target) {
            self.result = Some(format!("let {}", self.name));
            return;
        }
        ast::walk_pattern(self, pat);
    }
}

/// Whether `pattern` itself is the identifier leaf binding `target`.
/// Recursion into sub-patterns is [`ast::walk_pattern`]'s job.
fn pattern_binds(pattern: &ast::Pattern, target: AstId) -> bool {
    match pattern {
        ast::Pattern::Ident { id, .. } | ast::Pattern::MutIdent { id, .. } => *id == target,
        ast::Pattern::Tuple(..)
        | ast::Pattern::Or(_)
        | ast::Pattern::Struct { .. }
        | ast::Pattern::Variant { .. }
        | ast::Pattern::Range { .. }
        | ast::Pattern::Literal(_)
        | ast::Pattern::Wildcard
        | ast::Pattern::Error(_) => false,
    }
}

/// Render `item` when it — or one of its members — is the declaration
/// identified by `target`.
fn item_info(item: &Item, target: AstId, public_only: bool) -> Option<String> {
    match item {
        Item::Function(f) => (f.id == target).then(|| unparse::unparse_function_signature(f)),
        // Struct fields follow `public_only` (matching `wado doc`'s `..`
        // elision when set); enum cases are always public.
        Item::Struct(s) => {
            if s.id == target {
                return Some(unparse::unparse_struct_signature(s, public_only));
            }
            s.fields
                .iter()
                .find(|f| f.id == target)
                .map(|f| unparse::unparse_struct_field(&s.name, f))
        }
        Item::Enum(e) => {
            if e.id == target {
                return Some(unparse::unparse_enum_signature(e));
            }
            e.cases
                .iter()
                .find(|c| c.id == target)
                .map(|c| unparse::unparse_enum_case(&e.name, c))
        }
        Item::Variant(v) => {
            if v.id == target {
                return Some(unparse::unparse_variant_header(v));
            }
            v.cases
                .iter()
                .find(|c| c.id == target)
                .map(|c| unparse::unparse_variant_case(&v.name, c))
        }
        Item::Trait(t) => {
            if t.id == target {
                return Some(unparse::unparse_trait_header(t));
            }
            t.methods
                .iter()
                .find(|m| m.id == target)
                .map(unparse::unparse_function_signature)
        }
        Item::Impl(imp) => imp
            .methods
            .iter()
            .find(|m| m.id == target)
            .map(unparse::unparse_function_signature),
        Item::Flags(fl) => (fl.id == target).then(|| unparse::unparse_flags_header(fl)),
        Item::Newtype(n) => (n.id == target).then(|| unparse::unparse_newtype_signature(n)),
        Item::BuiltinTypeDecl(d) => {
            (d.id == target).then(|| unparse::unparse_builtin_type_decl_signature(d))
        }
        Item::Interface(e) => (e.id == target).then(|| format!("interface {}", e.name)),
        Item::Global(g) => (g.id == target).then(|| unparse::unparse_global_signature(g)),
        Item::Use(_)
        | Item::Resource(_)
        | Item::World(_)
        | Item::Test(_)
        | Item::TupleTypeDecl(_)
        | Item::Error(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_ctx::with_ctx;
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
    fn builtin_type_decl_hover() {
        futures::executor::block_on(async {
            // A named definition-less type (`pub type Buf<T>;`) renders its
            // signature on hover, like a newtype/struct declaration.
            let source = "pub type Buf<T>;\nfn run() {}\n";
            let result = hover_at(source, 0, 9).await.expect("hover on Buf");
            assert!(
                result.contents.value.contains("type Buf<T>"),
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
        // never surface the elaborator's synthetic `__iter_N` local. A hover
        // here should return None (the cursor lands between recognised
        // names) or, at most, surface the user's loop variable — never
        // a `let __iter_N` line that exposes compiler internals.
        futures::executor::block_on(async {
            let source = concat!(
                "fn f(items: List<i32>) -> i32 {\n",
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
        with_ctx(source, encoding, |ctx| {
            find_hover_opts(ctx, Position { line, character }, true)
        })
        .await
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
        // elaborator bails on the bad body, but the LSP should still surface
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

    #[test]
    fn item_hover_ignores_a_member_sharing_the_name() {
        // A name scan would answer with the struct field, declared first.
        futures::executor::block_on(async {
            let source = concat!(
                "struct Wrap { helper: i32 }\n",
                "fn helper() -> i32 { return 1; }\n",
                "fn run() -> i32 { return helper(); }\n",
            );
            let result = hover_at(source, 2, 26).await.expect("hover on helper()");
            assert_eq!(result.contents.value, "```wado\nfn helper() -> i32\n```");
        });
    }

    #[test]
    fn hover_on_local_inside_a_closure_call_argument() {
        futures::executor::block_on(async {
            let source = concat!(
                "fn apply(f: fn(i32) -> i32) -> i32 { return f(1); }\n",
                "fn run() -> i32 {\n",
                "    return apply(|n: i32| -> i32 { let doubled = n * 2; return doubled; });\n",
                "}\n",
            );
            let line =
                "    return apply(|n: i32| -> i32 { let doubled = n * 2; return doubled; });";
            let col = line.rfind("doubled").unwrap() as u32;
            let result = hover_at(source, 2, col).await.expect("hover on doubled");
            assert_eq!(result.contents.value, "```wado\nlet doubled\n```");
        });
    }

    #[test]
    fn hover_on_local_inside_a_nested_call_argument_block() {
        futures::executor::block_on(async {
            let source = concat!(
                "fn take(v: i32) -> i32 { return v; }\n",
                "fn run() -> i32 {\n",
                "    return take(1 + b: { let inner: i32 = 2; break b: inner; });\n",
                "}\n",
            );
            let line = "    return take(1 + b: { let inner: i32 = 2; break b: inner; });";
            let col = line.rfind("inner").unwrap() as u32;
            let result = hover_at(source, 2, col).await.expect("hover on inner");
            assert_eq!(result.contents.value, "```wado\nlet inner: i32\n```");
        });
    }

    #[test]
    fn hover_on_local_inside_a_test_block_closure() {
        futures::executor::block_on(async {
            let source = concat!(
                "fn apply(f: fn(i32) -> i32) -> i32 { return f(1); }\n",
                "fn run() {}\n",
                "test \"t\" {\n",
                "    let _ = apply(|n: i32| -> i32 { let scaled = n * 3; return scaled; });\n",
                "}\n",
            );
            let line = "    let _ = apply(|n: i32| -> i32 { let scaled = n * 3; return scaled; });";
            let col = line.rfind("scaled").unwrap() as u32;
            let result = hover_at(source, 3, col).await.expect("hover on scaled");
            assert_eq!(result.contents.value, "```wado\nlet scaled\n```");
        });
    }

    #[test]
    fn hover_on_local_in_let_else_block() {
        futures::executor::block_on(async {
            let source = concat!(
                "fn f() -> i32 {\n",
                "    let opt: Option<i32> = Option::Some(1);\n",
                "    let Some(x) = opt else {\n",
                "        let msg: i32 = -1;\n",
                "        return msg;\n",
                "    };\n",
                "    return x;\n",
                "}\n",
            );
            let result = hover_at(source, 4, 16).await.expect("hover on msg");
            assert_eq!(result.contents.value, "```wado\nlet msg: i32\n```");
        });
    }
}
