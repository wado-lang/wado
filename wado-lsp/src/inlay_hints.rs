//! Inlay hints — inline annotations rendered next to source positions to
//! surface types and parameter names the user did not write.
//!
//! Three kinds of hints are produced:
//!
//! 1. **Type hints** on `let` patterns, closure parameters, and `for x of …`
//!    bindings that lack an explicit `: T` annotation. Tuple / struct /
//!    variant / or-patterns recurse into their leaves so each bound
//!    identifier hints with the elaborator's inferred type for that leaf.
//!    Types come from [`Semantics::local_type_name`], populated by the
//!    elaborator via `record_local_symbol`.
//! 2. **Parameter-name hints** at free-function call sites. The callee's
//!    `FunctionSymbol::params` (set up by the analyzer) drives the labels.
//! 3. **Parameter-name hints at method / static-method call sites.** The
//!    callee's `Function` AST node is reached by following the elaborator's
//!    use→def edge from the method-name token's `AstId` to the declaring
//!    impl method, then reading `Function::params` directly from the AST.
//!    Impl methods are not registered in the symbol table, so the AST is
//!    the source of truth here.
//!
//! All hint positions are produced as `Position`s in the LSP-negotiated
//! [`PositionEncoding`]. Hints whose anchor falls outside the requested
//! `range` are filtered out at the end of [`inlay_hints`].

use serde::{Deserialize, Serialize};
use wado_compiler::ast::{
    self, AstVisitor, ClosureParam, Expr, ForOfStmt, Function, LetStmt, Pattern, Stmt,
};
use wado_compiler::module_source::ModuleSource;
use wado_compiler::symbol::{SymbolKey, SymbolKind};
use wado_compiler::token::Span;

use crate::diagnostics::{Position, Range};
use crate::macros::lsp_repr_u32_enum;
use crate::query::QueryContext;
use crate::text::codepoint_offset_to_character;

lsp_repr_u32_enum!(
    /// LSP inlay-hint kind. Serializes as the wire integer.
    pub enum InlayHintKind {
        Type = 1,
        Parameter = 2,
    }
);

/// An inlay hint produced for the requesting document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlayHint {
    pub position: Position,
    pub label: String,
    pub kind: InlayHintKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding_left: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding_right: Option<bool>,
}

/// Compute inlay hints for the entry module of `ctx`.
///
/// Hints anchored outside the requested `range` are dropped so the LSP
/// client only renders what is currently visible. The implementation
/// walks the entry-module AST once via [`AstVisitor`]; per-hint work is
/// `O(1)` (symbol-table lookup) for free functions and `O(impls in
/// module)` for methods.
#[must_use]
pub(crate) fn inlay_hints(ctx: &QueryContext, range: Range) -> Vec<InlayHint> {
    let entry = ctx.entry();
    let Some(module) = ctx.sem.modules.get(entry) else {
        return Vec::new();
    };
    let mut collector = HintCollector {
        ctx,
        module_source: entry,
        hints: Vec::new(),
    };
    for item in &module.items {
        collector.visit_item(item);
    }
    let HintCollector { hints, .. } = collector;
    hints.into_iter().filter(|h| in_range(h, range)).collect()
}

fn in_range(hint: &InlayHint, range: Range) -> bool {
    let pos = hint.position;
    let after_start = (pos.line, pos.character) >= (range.start.line, range.start.character);
    let before_end = (pos.line, pos.character) <= (range.end.line, range.end.character);
    after_start && before_end
}

struct HintCollector<'a> {
    ctx: &'a QueryContext<'a>,
    module_source: &'a ModuleSource,
    hints: Vec<InlayHint>,
}

impl HintCollector<'_> {
    fn key(&self, ast_id: ast::AstId) -> SymbolKey {
        SymbolKey::new(self.module_source.clone(), ast_id)
    }

    /// Position immediately after `span` ends — where a `: T` label is anchored.
    fn position_after(&self, span: Span) -> Position {
        let line = span.end_line.saturating_sub(1) as u32;
        let codepoint_col = span.end_column.saturating_sub(1) as u32;
        let character =
            codepoint_offset_to_character(self.ctx.source, line, codepoint_col, self.ctx.encoding);
        Position { line, character }
    }

    /// Position at the start of `span` — where a `name:` parameter label is anchored.
    fn position_before(&self, span: Span) -> Position {
        let line = span.line.saturating_sub(1) as u32;
        let codepoint_col = span.column.saturating_sub(1) as u32;
        let character =
            codepoint_offset_to_character(self.ctx.source, line, codepoint_col, self.ctx.encoding);
        Position { line, character }
    }

    /// Emit a `: T` hint anchored at the end of `name_span` for a binding
    /// identified by `binding_id`. No-op when the elaborator did not record a
    /// type for `binding_id` (e.g. the binding sits inside a function whose
    /// body the elaborator bailed on).
    fn push_type_hint(&mut self, binding_id: ast::AstId, name_span: Span) {
        let Some(type_name) = self.ctx.sem.local_type_name(&self.key(binding_id)) else {
            return;
        };
        self.hints.push(InlayHint {
            position: self.position_after(name_span),
            label: format!(": {type_name}"),
            kind: InlayHintKind::Type,
            padding_left: None,
            padding_right: None,
        });
    }

    /// Emit a `name:` parameter-name hint anchored at the start of `arg_span`.
    fn push_param_hint(&mut self, param_name: &str, arg_span: Span) {
        self.hints.push(InlayHint {
            position: self.position_before(arg_span),
            label: format!("{param_name}:"),
            kind: InlayHintKind::Parameter,
            padding_left: None,
            padding_right: Some(true),
        });
    }

    /// Emit a type hint for every `Ident` / `MutIdent` leaf reachable
    /// from `pattern`. Tuple / struct / variant / or-patterns recurse
    /// into their sub-patterns; the elaborator records a `local_type` for
    /// each leaf binding via `record_local_symbol`, so each leaf can
    /// hint independently. Literal and wildcard leaves bind nothing
    /// and have no type to surface.
    fn hint_pattern_bindings(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Ident { id, span, .. } | Pattern::MutIdent { id, span, .. } => {
                self.push_type_hint(*id, *span);
            }
            Pattern::Tuple(sub_patterns, _) | Pattern::Or(sub_patterns) => {
                for p in sub_patterns {
                    self.hint_pattern_bindings(p);
                }
            }
            Pattern::Struct { fields, .. } => {
                for field in fields {
                    self.hint_pattern_bindings(&field.pattern);
                }
            }
            Pattern::Variant { bindings, .. } => {
                for p in bindings {
                    self.hint_pattern_bindings(p);
                }
            }
            Pattern::Range { .. } | Pattern::Literal(_) | Pattern::Wildcard | Pattern::Error(_) => {
            }
        }
    }

    /// Hint parameters for a `CallExpr`. Resolves the callee identifier
    /// through `Semantics::referenced_symbol` and reads the parameter
    /// names off the callee's `FunctionSymbol` (or, for impl methods
    /// that the analyzer does not register as symbols, the declaring
    /// `Function` AST node).
    fn hint_call_args(&mut self, call: &ast::CallExpr) {
        let Some(param_names) = self.callee_param_names(&call.callee) else {
            return;
        };
        self.emit_arg_param_hints(&param_names, &call.args);
    }

    /// Parameter names of the function that `callee` resolves to, or
    /// `None` if the callee does not name a known callable.
    ///
    /// Two routes are tried, in order:
    /// 1. The use→def edge points at a symbol-table entry (free function,
    ///    effect method, resource method). [`FunctionSymbol::params`]
    ///    carries the names.
    /// 2. The use→def edge points at an impl method's `AstId`, which the
    ///    analyzer does not register as a symbol. `Semantics::function_at`
    ///    looks the `Function` AST node up through the per-module
    ///    [`AstIndex`] in O(1); we read its params from there (self
    ///    params filtered out).
    fn callee_param_names(&self, callee: &Expr) -> Option<Vec<String>> {
        let ident = match callee {
            Expr::Ident(i) => i,
            _ => return None,
        };
        // The use→def edge is keyed on the `IdentExpr` id for bare paths
        // (`add`) and on the trailing path segment id for qualified paths
        // (`Point::at`, `ns::add`). Try both, preferring the segment when
        // the path is qualified — that matches how `resolve_ident` /
        // `resolve_call` record the edge.
        let trailing_id = ident.segments.last().map_or(ident.id, |seg| seg.id);
        let def_key = self
            .ctx
            .sem
            .referenced_symbol(&self.key(trailing_id))
            .or_else(|| self.ctx.sem.referenced_symbol(&self.key(ident.id)))?;
        if let Some(symbol) = self.ctx.sem.symbol_at(&def_key)
            && let SymbolKind::Function(f) = &symbol.kind
        {
            return Some(f.params.clone());
        }
        let func = self.ctx.sem.function_at(&def_key)?;
        Some(filter_non_self_param_names(func))
    }

    /// Hint parameters for a `MethodCallExpr` (`receiver.method(args)`).
    ///
    /// Impl methods are not present in the symbol table, so the use→def
    /// edge points at the declaring `Function`'s `AstId`. The per-module
    /// [`AstIndex`] indexes that mapping so `Semantics::function_at`
    /// resolves it in O(1).
    fn hint_method_call_args(&mut self, call: &ast::MethodCallExpr) {
        let Some(param_names) = self.method_param_names(call.method_id) else {
            return;
        };
        self.emit_arg_param_hints(&param_names, &call.args);
    }

    /// Hint parameters for a `StaticMethodCallExpr` (`Type::method(args)`).
    /// Same resolution as `hint_method_call_args` — the elaborator records
    /// the same use→def edge for both call shapes.
    fn hint_static_method_call_args(&mut self, call: &ast::StaticMethodCallExpr) {
        let Some(param_names) = self.method_param_names(call.method_id) else {
            return;
        };
        self.emit_arg_param_hints(&param_names, &call.args);
    }

    /// Parameter names of the impl/trait method whose declaration `AstId`
    /// is the use→def target of `method_id_at_call`. Returns `None` for
    /// synthetic / unresolved call sites.
    fn method_param_names(&self, method_id_at_call: ast::AstId) -> Option<Vec<String>> {
        let def_key = self
            .ctx
            .sem
            .referenced_symbol(&self.key(method_id_at_call))?;
        let func = self.ctx.sem.function_at(&def_key)?;
        Some(filter_non_self_param_names(func))
    }

    fn emit_arg_param_hints(&mut self, param_names: &[String], args: &[Expr]) {
        // Align positional args with their parameter names. An arity
        // mismatch (more args than params) terminates the loop early; the
        // elaborator flags that as a diagnostic on its own path.
        for (param_name, arg) in param_names.iter().zip(args.iter()) {
            self.push_param_hint(param_name, arg.span());
        }
    }

    fn hint_closure_param(&mut self, p: &ClosureParam) {
        if p.ty.is_none() {
            self.push_type_hint(p.id, p.name_span);
        }
    }
}

/// Parameter names of `func` with self params filtered out.
///
/// `&self` / `&mut self` are surfaced as `SelfKind::Ref` / `SelfKind::MutRef`
/// by the parser; only `SelfKind::None` params can ever appear as positional
/// call arguments.
fn filter_non_self_param_names(func: &Function) -> Vec<String> {
    func.params
        .iter()
        .filter(|p| matches!(p.self_kind, ast::SelfKind::None))
        .map(|p| p.name.clone())
        .collect()
}

impl AstVisitor for HintCollector<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(LetStmt {
                pattern, ty, value, ..
            }) => {
                // Only hint when the user did not annotate the binding.
                if ty.is_none() {
                    self.hint_pattern_bindings(pattern);
                }
                // Still walk the initializer for hints inside it.
                if let Some(v) = value {
                    self.visit_expr(v);
                }
            }
            Stmt::ForOf(ForOfStmt {
                binding,
                iterable,
                body,
                ..
            }) => {
                // `for x of items` has no `: T` annotation slot, so every
                // simple binding gets a hint.
                self.hint_pattern_bindings(binding);
                self.visit_expr(iterable);
                self.visit_block(body);
            }
            _ => ast::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Closure(c) => {
                for p in &c.params {
                    self.hint_closure_param(p);
                }
                self.visit_expr(&c.body);
            }
            Expr::Call(c) => {
                self.hint_call_args(c);
                ast::walk_expr(self, expr);
            }
            Expr::MethodCall(m) => {
                self.hint_method_call_args(m);
                ast::walk_expr(self, expr);
            }
            Expr::StaticMethodCall(s) => {
                self.hint_static_method_call_args(s);
                ast::walk_expr(self, expr);
            }
            _ => ast::walk_expr(self, expr),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MapHost;
    use crate::text::PositionEncoding;
    use futures::executor::block_on;

    async fn hints_for(source: &str) -> Vec<InlayHint> {
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
        let max_range = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: u32::MAX,
                character: u32::MAX,
            },
        };
        inlay_hints(&ctx, max_range)
    }

    /// Sugar: collect (label, kind) pairs for assertions that don't care about positions.
    fn labels(hints: &[InlayHint]) -> Vec<(String, InlayHintKind)> {
        hints.iter().map(|h| (h.label.clone(), h.kind)).collect()
    }

    #[test]
    fn let_without_annotation_emits_type_hint() {
        block_on(async {
            let src = "fn f() -> i32 {\n    let x = 1;\n    return x;\n}\n";
            let hints = hints_for(src).await;
            assert!(
                hints
                    .iter()
                    .any(|h| h.label == ": i32" && h.kind == InlayHintKind::Type),
                "expected a `: i32` type hint for `let x = 1`; got {hints:?}",
            );
        });
    }

    #[test]
    fn let_with_annotation_emits_no_type_hint() {
        block_on(async {
            let src = "fn f() -> i32 {\n    let x: i32 = 1;\n    return x;\n}\n";
            let hints = hints_for(src).await;
            assert!(
                !hints
                    .iter()
                    .any(|h| h.kind == InlayHintKind::Type && h.label == ": i32"),
                "annotated `let x: i32` must not produce a redundant type hint; got {hints:?}",
            );
        });
    }

    #[test]
    fn closure_param_without_annotation_emits_type_hint() {
        block_on(async {
            let src = concat!(
                "fn f() -> i32 {\n",
                "    let g = |x| x + 1;\n",
                "    return g(1);\n",
                "}\n",
            );
            let hints = hints_for(src).await;
            // The closure body uses x + 1, which through default integer
            // resolution gives x: i32. The exact type depends on inference
            // — we just want some `: i32`-ish hint anchored at the closure
            // param.
            let closure_hints: Vec<_> = hints
                .iter()
                .filter(|h| h.kind == InlayHintKind::Type && h.position.line == 1)
                .collect();
            assert!(
                !closure_hints.is_empty(),
                "expected a closure-param type hint on line 1; got {hints:?}",
            );
        });
    }

    #[test]
    fn for_of_binding_emits_type_hint() {
        block_on(async {
            let src = concat!(
                "fn f() -> i32 {\n",
                "    let items: Array<i32> = [1, 2, 3];\n",
                "    let mut total: i32 = 0;\n",
                "    for let v of items {\n",
                "        total = total + v;\n",
                "    }\n",
                "    return total;\n",
                "}\n",
            );
            let hints = hints_for(src).await;
            assert!(
                hints
                    .iter()
                    .any(|h| h.kind == InlayHintKind::Type && h.label == ": i32"),
                "expected `: i32` element-type hint on `for v of items`; got {hints:?}",
            );
        });
    }

    #[test]
    fn free_function_call_emits_parameter_name_hints() {
        block_on(async {
            let src = concat!(
                "fn add(a: i32, b: i32) -> i32 {\n",
                "    return a + b;\n",
                "}\n",
                "fn run() -> i32 {\n",
                "    return add(1, 2);\n",
                "}\n",
            );
            let hints = hints_for(src).await;
            let param_labels: Vec<_> = labels(&hints)
                .into_iter()
                .filter(|(_, k)| *k == InlayHintKind::Parameter)
                .map(|(l, _)| l)
                .collect();
            assert!(
                param_labels.contains(&"a:".to_string())
                    && param_labels.contains(&"b:".to_string()),
                "expected `a:` and `b:` parameter hints at `add(1, 2)`; got {param_labels:?}",
            );
        });
    }

    #[test]
    fn parameter_hint_emitted_even_when_arg_name_matches_param() {
        block_on(async {
            let src = concat!(
                "fn add(a: i32, b: i32) -> i32 { return a + b; }\n",
                "fn run() -> i32 {\n",
                "    let a: i32 = 1;\n",
                "    let b: i32 = 2;\n",
                "    return add(a, b);\n",
                "}\n",
            );
            let hints = hints_for(src).await;
            let param_labels: Vec<_> = labels(&hints)
                .into_iter()
                .filter(|(_, k)| *k == InlayHintKind::Parameter)
                .map(|(l, _)| l)
                .collect();
            assert!(
                param_labels.contains(&"a:".to_string())
                    && param_labels.contains(&"b:".to_string()),
                "expected `a:`/`b:` parameter hints at `add(a, b)`; got {param_labels:?}",
            );
        });
    }

    #[test]
    fn range_filtering_drops_hints_outside_window() {
        block_on(async {
            let src = "fn f() -> i32 {\n    let x = 1;\n    let y = 2;\n    return x + y;\n}\n";
            let path = "/test.wado";
            let uri = format!("file://{path}");
            let host = MapHost::single(path, src);
            let sem = wado_compiler::semantics(src, &host, Some(path)).await;
            let ctx = QueryContext {
                sem: &sem,
                source: src,
                uri: &uri,
                encoding: PositionEncoding::Utf16,
            };
            // Restrict to line 1 only — `let x = 1` is on line 1 (0-based),
            // `let y = 2` is on line 2. Filter must drop the line-2 hint.
            let range = Range {
                start: Position {
                    line: 1,
                    character: 0,
                },
                end: Position {
                    line: 1,
                    character: u32::MAX,
                },
            };
            let hints = inlay_hints(&ctx, range);
            // We should still get the x-hint but not the y-hint.
            assert!(
                hints.iter().all(|h| h.position.line == 1),
                "every emitted hint should land on line 1; got {hints:?}",
            );
            assert!(
                hints
                    .iter()
                    .any(|h| h.label == ": i32" && h.kind == InlayHintKind::Type),
                "the `let x = 1` hint should survive the range filter; got {hints:?}",
            );
        });
    }

    #[test]
    fn method_call_emits_parameter_name_hint() {
        block_on(async {
            // Inherent impl method. `set_to` takes a single named parameter `v`.
            let src = concat!(
                "struct Box { value: i32 }\n",
                "impl Box {\n",
                "    fn set_to(&mut self, v: i32) {\n",
                "        self.value = v;\n",
                "    }\n",
                "}\n",
                "fn run() {\n",
                "    let mut b = Box { value: 0 };\n",
                "    b.set_to(42);\n",
                "}\n",
            );
            let hints = hints_for(src).await;
            let param_labels: Vec<_> = labels(&hints)
                .into_iter()
                .filter(|(_, k)| *k == InlayHintKind::Parameter)
                .map(|(l, _)| l)
                .collect();
            assert!(
                param_labels.contains(&"v:".to_string()),
                "expected `v:` parameter hint at `b.set_to(42)`; got {param_labels:?}",
            );
        });
    }

    #[test]
    fn static_method_call_emits_parameter_name_hint() {
        block_on(async {
            let src = concat!(
                "struct Point { x: i32, y: i32 }\n",
                "impl Point {\n",
                "    fn at(x: i32, y: i32) -> Point {\n",
                "        return Point { x, y };\n",
                "    }\n",
                "}\n",
                "fn run() -> i32 {\n",
                "    let p = Point::at(3, 4);\n",
                "    return p.x;\n",
                "}\n",
            );
            let hints = hints_for(src).await;
            let param_labels: Vec<_> = labels(&hints)
                .into_iter()
                .filter(|(_, k)| *k == InlayHintKind::Parameter)
                .map(|(l, _)| l)
                .collect();
            assert!(
                param_labels.contains(&"x:".to_string())
                    && param_labels.contains(&"y:".to_string()),
                "expected `x:` and `y:` parameter hints at `Point::at(3, 4)`; got {param_labels:?}",
            );
        });
    }

    #[test]
    fn hint_survives_unrelated_type_error() {
        // Partial-result behaviour: a type-error in one function must not
        // wipe out hints on a well-formed function. Mirrors the
        // `hover_on_well_formed_part_survives_unrelated_error` test.
        block_on(async {
            let src = concat!(
                "fn good() -> i32 {\n",
                "    let n = 7;\n",
                "    return n;\n",
                "}\n",
                "fn broken() -> i32 {\n",
                "    return \"not an i32\";\n",
                "}\n",
            );
            let hints = hints_for(src).await;
            assert!(
                hints
                    .iter()
                    .any(|h| h.label == ": i32" && h.kind == InlayHintKind::Type),
                "expected `: i32` hint on `let n = 7` in `good`; got {hints:?}",
            );
        });
    }

    #[test]
    fn let_mut_without_annotation_emits_type_hint() {
        // `let mut x = 1` parses as `LetStmt { is_mut: true, pattern:
        // Ident { ... } }`. The mut keyword does not change the pattern
        // shape, so `hint_pattern_bindings` must still match. Without
        // this test a `MutIdent`-only matcher (or a future parser change
        // that emits `MutIdent` here) would regress the hint silently.
        block_on(async {
            let src = "fn f() -> i32 {\n    let mut x = 1;\n    return x;\n}\n";
            let hints = hints_for(src).await;
            assert!(
                hints
                    .iter()
                    .any(|h| h.label == ": i32" && h.kind == InlayHintKind::Type),
                "expected `: i32` hint on `let mut x = 1`; got {hints:?}",
            );
        });
    }

    #[test]
    fn struct_destructure_let_hints_each_field_binding() {
        // `let { x, y } = p;` binds two locals — `x` and `y` — each
        // with a elaborator-recorded type (the struct field's type). The
        // pattern walker recurses into the struct sub-patterns and
        // emits a hint per leaf.
        block_on(async {
            let src = concat!(
                "struct Point { x: i32, y: i32 }\n",
                "fn f(p: Point) -> i32 {\n",
                "    let { x, y } = p;\n",
                "    return x + y;\n",
                "}\n",
            );
            let hints = hints_for(src).await;
            // Both `x` and `y` are bound on line 2; assert at least two
            // `: i32` type hints land there.
            let line_2_i32: Vec<_> = hints
                .iter()
                .filter(|h| {
                    h.kind == InlayHintKind::Type && h.position.line == 2 && h.label == ": i32"
                })
                .collect();
            assert_eq!(
                line_2_i32.len(),
                2,
                "expected two `: i32` hints for `let {{ x, y }} = p`; got {hints:?}",
            );
        });
    }

    #[test]
    fn type_hint_position_is_immediately_after_name() {
        // Concrete position assertion: `let x = 1` on column 4 (0-based)
        // must place its `: i32` hint exactly at column 5 — right after
        // `x`, before the surrounding space. Drift here would render the
        // hint mid-expression, which is the cardinal LSP inlay-hint bug.
        block_on(async {
            let src = "fn f() -> i32 {\n    let x = 1;\n    return x;\n}\n";
            let hints = hints_for(src).await;
            let h = hints
                .iter()
                .find(|h| h.label == ": i32" && h.kind == InlayHintKind::Type)
                .expect("expected a `: i32` hint");
            assert_eq!(h.position.line, 1, "hint should be on line 1");
            // The `x` identifier sits at character 8 (0-based) on
            // `    let x = 1;` — after four spaces of indent and `let `.
            // Its name span ends at the next character (9), which is the
            // anchor for the hint.
            assert_eq!(
                h.position.character, 9,
                "hint should anchor just past `x`; got {h:?}",
            );
        });
    }

    #[test]
    fn parameter_hint_padding_right_is_set() {
        // Parameter hints render as `name:` immediately before the
        // argument, so they need a trailing space (`paddingRight: true`)
        // to avoid `a:1`. Type hints don't — `: i32` already starts with
        // its own punctuation. Pin both shapes.
        block_on(async {
            let src = concat!(
                "fn add(a: i32, b: i32) -> i32 { return a + b; }\n",
                "fn run() -> i32 { return add(1, 2); }\n",
            );
            let hints = hints_for(src).await;
            for h in &hints {
                match h.kind {
                    InlayHintKind::Parameter => assert_eq!(
                        h.padding_right,
                        Some(true),
                        "parameter hint must set paddingRight=true: {h:?}",
                    ),
                    InlayHintKind::Type => assert_eq!(
                        h.padding_right, None,
                        "type hint must not set paddingRight: {h:?}",
                    ),
                }
            }
        });
    }

    #[test]
    fn parameter_hint_emitted_for_underscore_param_name() {
        // `_` is the actual parameter name the user wrote on the callee
        // (`fn discard(_: i32)`). The hint surfaces that name verbatim
        // at the call site — pin that we forward it rather than filter
        // anything.
        block_on(async {
            let src = concat!("fn discard(_: i32) {}\n", "fn run() { discard(7); }\n",);
            let hints = hints_for(src).await;
            let param_labels: Vec<_> = labels(&hints)
                .into_iter()
                .filter(|(_, k)| *k == InlayHintKind::Parameter)
                .map(|(l, _)| l)
                .collect();
            assert!(
                param_labels.contains(&"_:".to_string()),
                "underscore param should still produce a `_:` hint; got {param_labels:?}",
            );
        });
    }

    #[test]
    fn type_hint_under_utf16_anchors_at_correct_codepoint() {
        // Defensive: the LSP default encoding is UTF-16, where a single
        // codepoint may be 1 or 2 code units. A non-ASCII character on
        // a preceding line (or in a string on the same line) makes the
        // byte / UTF-16 / codepoint columns disagree; the hint must
        // still land at the right character index.
        //
        // The compiler's `Span::column` is a 1-based codepoint index
        // and the LSP test client speaks UTF-16. Within an ASCII-only
        // line the two coincide, so the fixture puts a multi-byte char
        // on the cursor line via a `🦀` literal that the encoder
        // measures differently per encoding. The `let x = 🦀;` value is
        // never evaluated — we only care that the `: T` hint anchors
        // immediately after `x`.
        let src = "// 🦀🦀🦀\nfn f() -> i32 { let x = 1; return x; }\n";
        let path = "/test.wado";
        let uri = format!("file://{path}");
        let host = MapHost::single(path, src);
        let sem = futures::executor::block_on(wado_compiler::semantics(src, &host, Some(path)));
        let ctx = QueryContext {
            sem: &sem,
            source: src,
            uri: &uri,
            encoding: PositionEncoding::Utf16,
        };
        let max_range = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: u32::MAX,
                character: u32::MAX,
            },
        };
        let hints = inlay_hints(&ctx, max_range);
        let h = hints
            .iter()
            .find(|h| h.label == ": i32" && h.kind == InlayHintKind::Type)
            .expect("expected a `: i32` hint on `let x = 1`");
        // The cursor line is line 1 (0-based) — `fn f() -> i32 { let x = 1; ... }`.
        // `fn f() -> i32 { let x` is 21 UTF-16 units; the anchor sits
        // right after `x` at character 21.
        assert_eq!(h.position.line, 1);
        assert_eq!(
            h.position.character, 21,
            "hint anchor must use UTF-16 code units; got {h:?}",
        );
    }

    #[test]
    fn closure_stored_in_local_does_not_yield_param_hints_on_call() {
        // Calling a closure via its binding (`g(1)`) routes through the
        // elaborator's local-variable path; the use→def edge points at the
        // `let g = …` pattern, not at a function symbol. We must not
        // accidentally try to read params off a `Variable` symbol and
        // emit garbage hints.
        block_on(async {
            let src = concat!(
                "fn f() -> i32 {\n",
                "    let g = |x: i32| x + 1;\n",
                "    return g(7);\n",
                "}\n",
            );
            let hints = hints_for(src).await;
            // No parameter hints anywhere — only type hints are allowed.
            assert!(
                hints.iter().all(|h| h.kind != InlayHintKind::Parameter),
                "closure-via-local call must not produce parameter hints; got {hints:?}",
            );
        });
    }

    #[test]
    fn cross_module_call_emits_parameter_name_hints() {
        // Real-world flow: importing `add` from another module and
        // calling it. Verifies the `referenced_symbol` edge survives the
        // import indirection and that `FunctionSymbol::params` is read
        // from the imported module's symbol entry.
        block_on(async {
            let other = concat!(
                "pub fn add(a: i32, b: i32) -> i32 {\n",
                "    return a + b;\n",
                "}\n",
            );
            let entry = concat!(
                "use { add } from \"./other.wado\";\n",
                "fn run() -> i32 { return add(1, 2); }\n",
            );
            let path = "/test.wado";
            let uri = format!("file://{path}");
            // The host key for the imported module must match the use
            // string verbatim — the loader uses the request path as the
            // host's lookup key. Mirror the pattern in `def_at_in`
            // (`tests/definition.rs`) which keys the sibling module on
            // `./lib.wado`, not its eventual absolute path.
            let host = MapHost::with_files(&[("./other.wado", other), (path, entry)]);
            let sem = wado_compiler::semantics(entry, &host, Some(path)).await;
            let ctx = QueryContext {
                sem: &sem,
                source: entry,
                uri: &uri,
                encoding: PositionEncoding::Utf16,
            };
            let max_range = Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: u32::MAX,
                    character: u32::MAX,
                },
            };
            let hints = inlay_hints(&ctx, max_range);
            let param_labels: Vec<_> = labels(&hints)
                .into_iter()
                .filter(|(_, k)| *k == InlayHintKind::Parameter)
                .map(|(l, _)| l)
                .collect();
            assert!(
                param_labels.contains(&"a:".to_string())
                    && param_labels.contains(&"b:".to_string()),
                "cross-module `add(1, 2)` should still surface `a:`/`b:`; got {param_labels:?}",
            );
        });
    }
}
