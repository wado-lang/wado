//! Per-module structural index over an [`ast::Module`].
//!
//! `AstIndex` is built once per module right after parse/bind and stored on
//! [`crate::semantics::Semantics`]. It collects positional facts that LSP
//! queries and the semantics pipeline used to rediscover with bespoke recursive
//! walkers — `name_span_in_module`, `collect_write_target_ids`, and the
//! `span_of_ast_id` fallback in `wado-lsp/src/definition.rs`. By centralising
//! these tables behind an [`AstVisitor`] traversal we keep extension cheap:
//! when the AST grows a new decl-bearing node, only this builder needs an
//! update instead of every consumer in the LSP layer.
//!
//! The index is intentionally narrow. It holds *only* facts derivable from the
//! AST itself (spans, declaration name spans, assignment write targets); the
//! semantic facts (resolved references, types, symbols) live elsewhere on
//! [`crate::semantics::Semantics`].

use crate::ast::{
    AstId, AstVisitor, Expr, Function, ImplBlock, InterfaceMethod, Item, Module, Pattern,
    walk_expr, walk_function, walk_interface_method, walk_item, walk_pattern,
};
use crate::hashmap::{IndexMap, IndexSet};
use crate::token::Span;

/// Address of a `Function` AST node within its declaring module's
/// `items` list. Stored on [`AstIndex`] so consumers can recover the
/// `&Function` for a given declaring `AstId` in O(1) instead of
/// rescanning every item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FunctionLocation {
    /// `module.items[item_idx]` is `Item::Function`.
    Free { item_idx: usize },
    /// `module.items[item_idx]` is `Item::Impl(b)` or `Item::Trait(t)`;
    /// the function is `b.methods[method_idx]` / `t.methods[method_idx]`.
    Method { item_idx: usize, method_idx: usize },
}

/// Structural facts about an [`ast::Module`].
#[derive(Debug, Default, Clone)]
pub struct AstIndex {
    /// Span attached to every id-bearing AST node, exactly as the
    /// default [`AstVisitor`] traversal emits it via `visit_id`. For
    /// container nodes (functions, items, blocks) this is the full span;
    /// for leaf identifier nodes (patterns, params, use specifiers) it
    /// already coincides with the name span.
    spans: IndexMap<AstId, Span>,
    /// Identifier-only span for declaration nodes — the narrower span the
    /// AST stores in fields like `Function::name_span` or
    /// `StructField::name_span`. Useful for `goto-definition` ranges that
    /// should not cover the entire item body.
    ///
    /// Populated for every named declaration the AST exposes a
    /// `name_span` for. Nodes without a dedicated `name_span` (e.g.
    /// `Item::Resource`, anonymous `Item::Impl`) are absent; consumers
    /// should fall back to [`Self::span_of`].
    name_spans: IndexMap<AstId, Span>,
    /// `IdentExpr.id` values that appear as the direct LHS target of
    /// `=` or a compound-assign. Direct targets only — for `obj.field = v`
    /// only the `field` identifier is recorded (and that one lives on the
    /// `FieldAccess` node, not the `IdentExpr`), so the `IdentExpr` for
    /// `obj` is *not* a write target. This matches the document-highlight
    /// classification: writes to the binding itself, not through a path.
    write_targets: IndexSet<AstId>,
    /// Address of every `Function`-shaped declaration in the module,
    /// keyed by the function's own `AstId`. Covers top-level free
    /// functions and methods inside `Item::Impl` / `Item::Trait` (both
    /// of which carry `methods: Vec<Function>`). Interface and resource
    /// methods carry the `InterfaceMethod` shape and live elsewhere; the
    /// analyzer registers them as `SymbolKind::Function` entries, so
    /// consumers reach them through the symbol table rather than here.
    function_locations: IndexMap<AstId, FunctionLocation>,
}

impl AstIndex {
    /// Build the index by walking `module` once.
    #[must_use]
    pub fn build(module: &Module) -> Self {
        let mut builder = IndexBuilder {
            index: Self::default(),
        };
        for item in &module.items {
            builder.visit_item(item);
        }
        record_function_locations(&mut builder.index, &module.items);
        builder.index
    }

    /// Span attached to `id`, or `None` if the id was never observed during
    /// the build walk.
    #[must_use]
    pub fn span_of(&self, id: AstId) -> Option<Span> {
        self.spans.get(&id).copied()
    }

    /// Identifier-only span for a declaration node, or `None` if `id` does
    /// not name a decl with a dedicated name span.
    #[must_use]
    pub fn name_span_of(&self, id: AstId) -> Option<Span> {
        self.name_spans.get(&id).copied()
    }

    /// True iff `id` is the `IdentExpr.id` of a direct assignment target.
    #[must_use]
    pub fn is_write_target(&self, id: AstId) -> bool {
        self.write_targets.contains(&id)
    }

    /// Address of the `Function` AST node whose declaring `AstId` is `id`,
    /// or `None` if `id` does not name an indexed function. Crate-private:
    /// `Semantics::function_at` is the entry point external consumers use.
    pub(crate) fn function_location(&self, id: AstId) -> Option<FunctionLocation> {
        self.function_locations.get(&id).copied()
    }

    /// `AstId` of the smallest id-bearing AST node whose span contains the
    /// given 1-based `(line, column)`. Equivalent to
    /// [`crate::ast::Module::ast_id_at`] but answered from the cached
    /// [`Self::span_of`] table without re-walking the AST.
    #[must_use]
    pub fn ast_id_at(&self, line: usize, column: usize) -> Option<AstId> {
        let mut best: Option<(AstId, Span)> = None;
        for (&id, &span) in &self.spans {
            if !span_contains(span, line, column) {
                continue;
            }
            match best {
                None => best = Some((id, span)),
                Some((_, current)) if span_byte_len(span) <= span_byte_len(current) => {
                    best = Some((id, span));
                }
                _ => {}
            }
        }
        best.map(|(id, _)| id)
    }
}

/// Returns true if `(line, column)` lies in
/// `[span.line:column, span.end_line:end_column)`. Mirrors the helper
/// in `ast.rs` (kept module-private there); duplicated here to keep the
/// position-lookup logic colocated with the data.
fn span_contains(span: Span, line: usize, column: usize) -> bool {
    if line < span.line || line > span.end_line {
        return false;
    }
    if line == span.line && column < span.column {
        return false;
    }
    if line == span.end_line && column >= span.end_column {
        return false;
    }
    true
}

fn span_byte_len(span: Span) -> usize {
    span.end.saturating_sub(span.start)
}

struct IndexBuilder {
    index: AstIndex,
}

impl IndexBuilder {
    fn record_name_span(&mut self, id: AstId, span: Span) {
        self.index.name_spans.insert(id, span);
    }
}

impl AstVisitor for IndexBuilder {
    fn visit_id(&mut self, id: AstId, span: Span) {
        // First write wins: the same id can be visited from a leaf walker
        // path (which emits the name span) and from a containing node
        // (which emits the full span). Preferring the first observation
        // matches the current `Module::ast_id_at` / `Module::span_of_ast_id`
        // semantics, which also use the first emission.
        self.index.spans.entry(id).or_insert(span);
    }

    fn visit_item(&mut self, item: &Item) {
        record_item_name_spans(&mut self.index, item);
        walk_item(self, item);
    }

    fn visit_function(&mut self, func: &Function) {
        // Record the function's own name span. Top-level `Item::Function`
        // already gets this via `record_item_name_spans`, but impl-block
        // and trait methods reach this visitor through `walk_item ->
        // visit_function` without that recording. Inserting here covers
        // both paths uniformly; the duplicate write for `Item::Function`
        // is harmless (same key, same value).
        self.record_name_span(func.id, func.name_span);
        for p in &func.params {
            self.record_name_span(p.id, p.name_span);
        }
        for tp in &func.type_params {
            self.record_name_span(tp.id, tp.name_span);
        }
        walk_function(self, func);
    }

    fn visit_interface_method(&mut self, method: &InterfaceMethod) {
        self.record_name_span(method.id, method.name_span);
        for p in &method.params {
            self.record_name_span(p.id, p.name_span);
        }
        walk_interface_method(self, method);
    }

    fn visit_pattern(&mut self, pat: &Pattern) {
        if let Pattern::Ident { id, span, .. } | Pattern::MutIdent { id, span, .. } = pat {
            self.record_name_span(*id, *span);
        }
        walk_pattern(self, pat);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Assign(e) => {
                if let Expr::Ident(target) = &e.target {
                    self.index.write_targets.insert(target.id);
                }
            }
            Expr::CompoundAssign(e) => {
                if let Expr::Ident(target) = &e.target {
                    self.index.write_targets.insert(target.id);
                }
            }
            Expr::Closure(c) => {
                for p in &c.params {
                    self.record_name_span(p.id, p.name_span);
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

fn record_item_name_spans(index: &mut AstIndex, item: &Item) {
    match item {
        Item::Function(f) => {
            index.name_spans.insert(f.id, f.name_span);
            for tp in &f.type_params {
                index.name_spans.insert(tp.id, tp.name_span);
            }
        }
        Item::Struct(s) => {
            index.name_spans.insert(s.id, s.name_span);
            for tp in &s.type_params {
                index.name_spans.insert(tp.id, tp.name_span);
            }
            for field in &s.fields {
                index.name_spans.insert(field.id, field.name_span);
            }
        }
        Item::Enum(e) => {
            index.name_spans.insert(e.id, e.name_span);
            for tp in &e.type_params {
                index.name_spans.insert(tp.id, tp.name_span);
            }
            for case in &e.cases {
                index.name_spans.insert(case.id, case.name_span);
            }
        }
        Item::Variant(v) => {
            index.name_spans.insert(v.id, v.name_span);
            for tp in &v.type_params {
                index.name_spans.insert(tp.id, tp.name_span);
            }
            for case in &v.cases {
                index.name_spans.insert(case.id, case.name_span);
            }
        }
        Item::Flags(f) => {
            index.name_spans.insert(f.id, f.name_span);
            for flag in &f.flags {
                index.name_spans.insert(flag.id, flag.name_span);
            }
        }
        Item::Newtype(n) => {
            index.name_spans.insert(n.id, n.name_span);
            for tp in &n.type_params {
                index.name_spans.insert(tp.id, tp.name_span);
            }
        }
        Item::Trait(t) => {
            index.name_spans.insert(t.id, t.name_span);
            for tp in &t.type_params {
                index.name_spans.insert(tp.id, tp.name_span);
            }
            // Methods and their params get their name_spans recorded when
            // `visit_function` dispatches them during the walk.
        }
        Item::Interface(i) => {
            index.name_spans.insert(i.id, i.name_span);
            // Interface method name_spans are recorded by the
            // `visit_interface_method` override.
        }
        Item::Resource(r) => {
            // ResourceDecl has no `name_span`; the table simply has no
            // entry for `r.id` — consumers should fall back to span_of.
            for tp in &r.type_params {
                index.name_spans.insert(tp.id, tp.name_span);
            }
        }
        Item::Global(g) => {
            index.name_spans.insert(g.id, g.name_span);
        }
        Item::Impl(imp) => record_impl_name_spans(index, imp),
        Item::Use(_) | Item::World(_) | Item::Test(_) | Item::TupleTypeDecl(_) => {}
    }
}

/// Record every `Function`-shaped declaration's `(item_idx, [method_idx])`
/// address into `index.function_locations`. Run as a single linear pass
/// over the module's items after the `AstVisitor` walk so consumers can
/// project an `AstId` back to its declaring `&Function` in O(1).
fn record_function_locations(index: &mut AstIndex, items: &[Item]) {
    for (item_idx, item) in items.iter().enumerate() {
        match item {
            Item::Function(f) => {
                index
                    .function_locations
                    .insert(f.id, FunctionLocation::Free { item_idx });
            }
            Item::Impl(b) => {
                for (method_idx, method) in b.methods.iter().enumerate() {
                    index.function_locations.insert(
                        method.id,
                        FunctionLocation::Method {
                            item_idx,
                            method_idx,
                        },
                    );
                }
            }
            Item::Trait(t) => {
                for (method_idx, method) in t.methods.iter().enumerate() {
                    index.function_locations.insert(
                        method.id,
                        FunctionLocation::Method {
                            item_idx,
                            method_idx,
                        },
                    );
                }
            }
            // Interface and resource declarations carry `InterfaceMethod`
            // entries, not `Function` — the analyzer registers those as
            // symbol-table entries, and consumers route through the symbol
            // table for them.
            _ => {}
        }
    }
}

fn record_impl_name_spans(index: &mut AstIndex, imp: &ImplBlock) {
    for tp in &imp.type_params {
        index.name_spans.insert(tp.id, tp.name_span);
    }
    // ImplBlock itself has no own name_span (anonymous decl); methods and
    // params are recorded by `visit_function` during the walk.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Stmt;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> Module {
        let tokens = Lexer::new(source).tokenize().expect("lex");
        let mut parser = Parser::new(tokens);
        parser.parse().expect("parse")
    }

    #[test]
    fn function_name_span_is_indexed() {
        let module = parse("fn add(a: i32, b: i32) -> i32 { return a + b; }\n");
        let index = AstIndex::build(&module);
        let func = match &module.items[0] {
            Item::Function(f) => f,
            _ => unreachable!(),
        };
        assert_eq!(index.name_span_of(func.id), Some(func.name_span));
        for p in &func.params {
            assert_eq!(index.name_span_of(p.id), Some(p.name_span));
        }
    }

    #[test]
    fn struct_field_name_spans_are_indexed() {
        let module = parse("struct Point { x: i32, y: i32 }\n");
        let index = AstIndex::build(&module);
        let s = match &module.items[0] {
            Item::Struct(s) => s,
            _ => unreachable!(),
        };
        assert_eq!(index.name_span_of(s.id), Some(s.name_span));
        for f in &s.fields {
            assert_eq!(index.name_span_of(f.id), Some(f.name_span));
        }
    }

    #[test]
    fn write_target_is_recorded_for_direct_assign() {
        let module = parse("fn f() {\n    let mut x: i32 = 0;\n    x = 1;\n    x += 2;\n}\n");
        let index = AstIndex::build(&module);
        let func = match &module.items[0] {
            Item::Function(f) => f,
            _ => unreachable!(),
        };
        let body = func.body.as_ref().expect("body");
        let mut writes = 0;
        for stmt in &body.stmts {
            if let Stmt::Expr(es) = stmt {
                let target = match &es.expr {
                    Expr::Assign(a) => &a.target,
                    Expr::CompoundAssign(a) => &a.target,
                    _ => continue,
                };
                if let Expr::Ident(id) = target {
                    assert!(index.is_write_target(id.id));
                    writes += 1;
                }
            }
        }
        assert_eq!(writes, 2);
    }

    #[test]
    fn read_only_ident_is_not_a_write_target() {
        let module = parse("fn f() -> i32 {\n    let x: i32 = 1;\n    return x + x;\n}\n");
        let index = AstIndex::build(&module);
        let func = match &module.items[0] {
            Item::Function(f) => f,
            _ => unreachable!(),
        };
        let body = func.body.as_ref().expect("body");
        let mut idents = 0;
        for stmt in &body.stmts {
            if let Stmt::Return(r) = stmt
                && let Some(Expr::Binary(b)) = r.value.as_ref()
            {
                for side in [&b.left, &b.right] {
                    if let Expr::Ident(id) = side {
                        assert!(!index.is_write_target(id.id));
                        idents += 1;
                    }
                }
            }
        }
        assert_eq!(idents, 2);
    }

    #[test]
    fn top_level_items_have_spans() {
        let module = parse(
            "struct P { x: i32 }\nfn f() -> i32 {\n    let p = P { x: 1 };\n    return p.x;\n}\n",
        );
        let index = AstIndex::build(&module);
        for item in &module.items {
            let id = match item {
                Item::Struct(s) => s.id,
                Item::Function(f) => f.id,
                _ => continue,
            };
            assert!(index.span_of(id).is_some(), "missing span for {id:?}");
        }
    }

    #[test]
    fn enum_decl_and_cases_are_indexed() {
        let module = parse("enum Color { Red, Green, Blue }\n");
        let index = AstIndex::build(&module);
        let e = match &module.items[0] {
            Item::Enum(e) => e,
            _ => unreachable!(),
        };
        assert_eq!(index.name_span_of(e.id), Some(e.name_span));
        for case in &e.cases {
            assert_eq!(index.name_span_of(case.id), Some(case.name_span));
        }
    }

    #[test]
    fn variant_decl_cases_and_type_params_are_indexed() {
        let module = parse("variant Maybe<T> { Just(T), Nothing }\n");
        let index = AstIndex::build(&module);
        let v = match &module.items[0] {
            Item::Variant(v) => v,
            _ => unreachable!(),
        };
        assert_eq!(index.name_span_of(v.id), Some(v.name_span));
        for tp in &v.type_params {
            assert_eq!(index.name_span_of(tp.id), Some(tp.name_span));
        }
        for case in &v.cases {
            assert_eq!(index.name_span_of(case.id), Some(case.name_span));
        }
    }

    #[test]
    fn flags_decl_and_members_are_indexed() {
        let module = parse("flags Perms { Read, Write, Execute }\n");
        let index = AstIndex::build(&module);
        let f = match &module.items[0] {
            Item::Flags(f) => f,
            _ => unreachable!(),
        };
        assert_eq!(index.name_span_of(f.id), Some(f.name_span));
        for member in &f.flags {
            assert_eq!(index.name_span_of(member.id), Some(member.name_span));
        }
    }

    #[test]
    fn newtype_and_type_params_are_indexed() {
        let module = parse("type Box<T> = T;\n");
        let index = AstIndex::build(&module);
        let n = match &module.items[0] {
            Item::Newtype(n) => n,
            _ => unreachable!(),
        };
        assert_eq!(index.name_span_of(n.id), Some(n.name_span));
        for tp in &n.type_params {
            assert_eq!(index.name_span_of(tp.id), Some(tp.name_span));
        }
    }

    #[test]
    fn trait_decl_methods_and_type_params_are_indexed() {
        let module = parse(concat!(
            "trait Greet<L> {\n",
            "    fn greet(&self) -> String;\n",
            "    fn farewell(&self) -> String;\n",
            "}\n",
        ));
        let index = AstIndex::build(&module);
        let t = match &module.items[0] {
            Item::Trait(t) => t,
            _ => unreachable!(),
        };
        assert_eq!(index.name_span_of(t.id), Some(t.name_span));
        for tp in &t.type_params {
            assert_eq!(index.name_span_of(tp.id), Some(tp.name_span));
        }
        for method in &t.methods {
            assert_eq!(
                index.name_span_of(method.id),
                Some(method.name_span),
                "method {} missing name_span",
                method.name,
            );
        }
    }

    #[test]
    fn impl_block_methods_and_type_params_are_indexed() {
        let module = parse(concat!(
            "struct Point { x: i32, y: i32 }\n",
            "impl<T> Point {\n",
            "    fn origin() -> Point { return Point { x: 0, y: 0 }; }\n",
            "    fn sum(&self) -> i32 { return self.x + self.y; }\n",
            "}\n",
        ));
        let index = AstIndex::build(&module);
        let imp = match &module.items[1] {
            Item::Impl(i) => i,
            _ => unreachable!(),
        };
        for tp in &imp.type_params {
            assert_eq!(index.name_span_of(tp.id), Some(tp.name_span));
        }
        for method in &imp.methods {
            assert_eq!(
                index.name_span_of(method.id),
                Some(method.name_span),
                "method {} missing name_span",
                method.name,
            );
            for p in &method.params {
                assert_eq!(index.name_span_of(p.id), Some(p.name_span));
            }
        }
    }

    #[test]
    fn interface_decl_and_methods_are_indexed() {
        let module = parse(concat!(
            "interface Stdout {\n",
            "    fn write(s: String);\n",
            "    fn flush();\n",
            "}\n",
        ));
        let index = AstIndex::build(&module);
        let i = match &module.items[0] {
            Item::Interface(i) => i,
            _ => unreachable!(),
        };
        assert_eq!(index.name_span_of(i.id), Some(i.name_span));
        for method in &i.methods {
            assert_eq!(
                index.name_span_of(method.id),
                Some(method.name_span),
                "method {} missing name_span",
                method.name,
            );
            for p in &method.params {
                assert_eq!(index.name_span_of(p.id), Some(p.name_span));
            }
        }
    }

    #[test]
    fn global_decl_is_indexed() {
        let module = parse("global PI: f64 = 3.14;\n");
        let index = AstIndex::build(&module);
        let g = match &module.items[0] {
            Item::Global(g) => g,
            _ => unreachable!(),
        };
        assert_eq!(index.name_span_of(g.id), Some(g.name_span));
    }

    #[test]
    fn closure_param_name_spans_are_indexed() {
        let module = parse(concat!(
            "fn f() -> i32 {\n",
            "    let g = |x: i32, y: i32| x + y;\n",
            "    return g(1, 2);\n",
            "}\n",
        ));
        let index = AstIndex::build(&module);
        // Walk the AST to find the closure and check each param.
        let func = match &module.items[0] {
            Item::Function(f) => f,
            _ => unreachable!(),
        };
        let body = func.body.as_ref().expect("body");
        let Stmt::Let(let_stmt) = &body.stmts[0] else {
            panic!("expected let stmt");
        };
        let value = let_stmt.value.as_ref().expect("let value");
        let Expr::Closure(closure) = value else {
            panic!("expected closure");
        };
        for p in &closure.params {
            assert_eq!(index.name_span_of(p.id), Some(p.name_span));
        }
    }

    #[test]
    fn pattern_ident_name_spans_are_indexed() {
        let module = parse(concat!(
            "fn f(opt: Option<i32>) -> i32 {\n",
            "    if let Some(v) = opt { return v; }\n",
            "    return 0;\n",
            "}\n",
        ));
        let index = AstIndex::build(&module);
        // The cursor on `v` should resolve to the binding's AstId, and
        // its name_span should equal the AST pattern's span. Walk the
        // body to find the `Some(v)` pattern and check `v`.
        let func = match &module.items[0] {
            Item::Function(f) => f,
            _ => unreachable!(),
        };
        let body = func.body.as_ref().expect("body");
        let Stmt::If(if_stmt) = &body.stmts[0] else {
            panic!("expected if-let");
        };
        let crate::ast::Condition::LetChain { elements, .. } = &if_stmt.condition else {
            panic!("expected let-chain");
        };
        let crate::ast::ConditionElement::Let { pattern, .. } = &elements[0] else {
            panic!("expected let element");
        };
        // pattern should be `Some(v)`; the binding pattern is the first
        // child.
        let crate::ast::Pattern::Variant { bindings, .. } = pattern.as_ref() else {
            panic!("expected variant pattern");
        };
        let crate::ast::Pattern::Ident { id, span, .. } = &bindings[0] else {
            panic!("expected ident pattern");
        };
        assert_eq!(index.name_span_of(*id), Some(*span));
    }

    #[test]
    fn ast_id_at_finds_innermost_node() {
        let module = parse("fn add(a: i32, b: i32) -> i32 { return a + b; }\n");
        let index = AstIndex::build(&module);
        // Cursor on the return-statement identifier `a` (line 1, col 41).
        // Should resolve to the IdentExpr's id, not the surrounding stmt.
        let id = index.ast_id_at(1, 41).expect("ast id at cursor");
        let span = index.span_of(id).expect("span for id");
        assert_eq!(span.line, 1);
        assert!(span.column <= 41 && 41 < span.end_column);
    }

    #[test]
    fn ast_id_at_returns_none_outside_module() {
        let module = parse("fn f() {}\n");
        let index = AstIndex::build(&module);
        assert!(index.ast_id_at(99, 1).is_none());
    }

    /// Each `StructField`'s indexed span must reach to the end of its
    /// declared type. The parser previously stored `start_span` only —
    /// just the first token — which left descendant spans (e.g. the
    /// `i32` `NamedType`) extending past the parent. That broke
    /// position-keyed lookups for any cursor inside the field but
    /// outside its descendants (the `:` punctuation, the whitespace
    /// between name and type, etc.). This test pins the contract that
    /// `index.span_of(field.id)` covers the whole `name: type` form.
    #[test]
    fn struct_field_span_reaches_type_end() {
        let module = parse("struct Point { x: i32, y: i32 }\n");
        let index = AstIndex::build(&module);
        let s = match &module.items[0] {
            Item::Struct(s) => s,
            _ => unreachable!(),
        };
        for field in &s.fields {
            let field_span = index.span_of(field.id).expect("field span");
            let ty_span = field.ty.span();
            assert!(
                field_span.end >= ty_span.end,
                "field `{}` indexed span end={} must reach the type end={}",
                field.name,
                field_span.end,
                ty_span.end,
            );
        }
    }

    /// Same contract as [`struct_field_span_reaches_type_end`] for
    /// `VariantCase`: a payload-bearing case must report a span that
    /// covers the payload type, not just the case name.
    #[test]
    fn variant_case_span_covers_payload() {
        let module = parse("variant Maybe<T> { Just(T), Nothing }\n");
        let index = AstIndex::build(&module);
        let v = match &module.items[0] {
            Item::Variant(v) => v,
            _ => unreachable!(),
        };
        let just = v
            .cases
            .iter()
            .find(|c| c.name == "Just")
            .expect("Just case");
        let case_span = index.span_of(just.id).expect("case span");
        let payload_span = just.payload.as_ref().expect("payload").span();
        assert!(
            case_span.end >= payload_span.end,
            "variant case span end={} must cover payload end={}",
            case_span.end,
            payload_span.end,
        );
    }

    /// `MatchArm` gained an `AstId` so arm-level position queries have
    /// a stable key (previously the arm itself had no id, so a cursor
    /// inside the arm resolved only to a descendant pattern/expression
    /// or to the surrounding `MatchExpr`). Verify the visitor records
    /// each arm's id with its full span.
    #[test]
    fn match_arm_id_is_indexed() {
        let module = parse(concat!(
            "fn f(x: i32) -> i32 {\n",
            "    return match x {\n",
            "        1 => 10,\n",
            "        _ => 0,\n",
            "    };\n",
            "}\n",
        ));
        let index = AstIndex::build(&module);
        let func = match &module.items[0] {
            Item::Function(f) => f,
            _ => unreachable!(),
        };
        let body = func.body.as_ref().expect("body");
        let match_expr = body
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Return(r) => match r.value.as_ref()? {
                    Expr::Match(m) => Some(m.as_ref()),
                    _ => None,
                },
                _ => None,
            })
            .expect("return-of-match");
        assert_eq!(match_expr.arms.len(), 2);
        for arm in &match_expr.arms {
            assert_eq!(
                index.span_of(arm.id),
                Some(arm.span),
                "arm id must be indexed with the arm's full span",
            );
        }
    }

    #[test]
    fn function_location_indexes_free_function() {
        let module = parse("fn run() -> i32 { return 1; }\n");
        let index = AstIndex::build(&module);
        let func = match &module.items[0] {
            Item::Function(f) => f,
            _ => unreachable!(),
        };
        assert_eq!(
            index.function_location(func.id),
            Some(FunctionLocation::Free { item_idx: 0 }),
        );
    }

    #[test]
    fn function_location_indexes_impl_methods() {
        let module = parse(concat!(
            "struct Point { x: i32, y: i32 }\n",
            "impl Point {\n",
            "    fn origin() -> Point { return Point { x: 0, y: 0 }; }\n",
            "    fn sum(&self) -> i32 { return self.x + self.y; }\n",
            "}\n",
        ));
        let index = AstIndex::build(&module);
        let imp = match &module.items[1] {
            Item::Impl(i) => i,
            _ => unreachable!(),
        };
        for (method_idx, method) in imp.methods.iter().enumerate() {
            assert_eq!(
                index.function_location(method.id),
                Some(FunctionLocation::Method {
                    item_idx: 1,
                    method_idx,
                }),
                "method `{}` should index to its impl-block address",
                method.name,
            );
        }
    }

    #[test]
    fn function_location_indexes_trait_methods() {
        let module = parse(concat!(
            "trait Greet {\n",
            "    fn greet(&self) -> String;\n",
            "    fn farewell(&self) -> String;\n",
            "}\n",
        ));
        let index = AstIndex::build(&module);
        let t = match &module.items[0] {
            Item::Trait(t) => t,
            _ => unreachable!(),
        };
        for (method_idx, method) in t.methods.iter().enumerate() {
            assert_eq!(
                index.function_location(method.id),
                Some(FunctionLocation::Method {
                    item_idx: 0,
                    method_idx,
                }),
                "trait method `{}` should index to its trait-block address",
                method.name,
            );
        }
    }

    /// Interface methods carry `InterfaceMethod` rather than `Function`,
    /// so the function-location index intentionally skips them. The
    /// analyzer registers interface methods as `SymbolKind::Function`
    /// entries; consumers use the symbol table for those.
    #[test]
    fn function_location_skips_interface_methods() {
        let module = parse(concat!(
            "interface Stdout {\n",
            "    fn write(s: String);\n",
            "}\n",
        ));
        let index = AstIndex::build(&module);
        let iface = match &module.items[0] {
            Item::Interface(i) => i,
            _ => unreachable!(),
        };
        for method in &iface.methods {
            assert_eq!(
                index.function_location(method.id),
                None,
                "interface method `{}` should not be in the function-location index",
                method.name,
            );
        }
    }

    /// User-visible improvement: a cursor on the `:` between a struct
    /// field's name and its type now resolves to the field rather than
    /// to the surrounding struct, because `StructField.span` covers the
    /// punctuation. This is the cursor-accuracy contract LSP features
    /// (hover, document-highlight, …) ride on top of.
    #[test]
    fn ast_id_at_resolves_to_field_on_colon() {
        let source = "struct Point { x: i32 }\n";
        let module = parse(source);
        let index = AstIndex::build(&module);
        let s = match &module.items[0] {
            Item::Struct(s) => s,
            _ => unreachable!(),
        };
        let field = &s.fields[0];
        // Single-line source: byte position + 1 == 1-based column.
        let colon_col = source.find(':').expect("colon present") + 1;
        assert_eq!(
            index.ast_id_at(1, colon_col),
            Some(field.id),
            "cursor on `:` of `x: i32` should resolve to the field id",
        );
    }
}
