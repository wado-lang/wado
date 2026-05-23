//! Owning, immutable-fold-style AST transformer.
//!
//! Provides the [`AstFolder`] trait used by desugar-style passes that need to
//! rewrite parts of the AST while keeping the surrounding structure intact
//! (namespace stripping, assert lowering, future desugar dismantlement steps).
//! Splitting this out of `ast.rs` keeps the type-definition module focused on
//! the AST node shapes themselves.

use crate::ast::{
    Block, Condition, ConditionElement, Expr, Function, Item, Literal, LiteralExpr, MatchArm,
    Pattern, Stmt, TemplatePart, Type,
};

/// Owning, immutable-fold-style AST transformer.
///
/// Every `fold_*` method consumes a node and returns a (possibly new) node of
/// the same kind. Default implementations delegate to the corresponding
/// `default_fold_*` function, which recursively folds children through the
/// folder's own methods and rebuilds the node. Implementers override the
/// `fold_*` methods for the kinds they want to transform; everything else
/// passes through unchanged thanks to the defaults.
///
/// Use this trait whenever a pass needs to rewrite parts of the AST while
/// keeping the surrounding structure intact (desugar moves, namespace
/// rewriting, power-assert capture, etc.). The exhaustive `match` arms inside
/// `default_fold_*` make the surface area compile-checked: adding a new AST
/// variant fails the build until the relevant `default_fold_*` learns how to
/// recurse through it, eliminating silent missed-recursion bugs.
pub trait AstFolder: Sized {
    fn fold_item(&mut self, item: Item) -> Item {
        default_fold_item(self, item)
    }
    fn fold_function(&mut self, func: Function) -> Function {
        default_fold_function(self, func)
    }
    fn fold_block(&mut self, block: Block) -> Block {
        default_fold_block(self, block)
    }
    fn fold_stmt(&mut self, stmt: Stmt) -> Stmt {
        default_fold_stmt(self, stmt)
    }
    fn fold_condition(&mut self, cond: Condition) -> Condition {
        default_fold_condition(self, cond)
    }
    fn fold_expr(&mut self, expr: Expr) -> Expr {
        default_fold_expr(self, expr)
    }
    fn fold_pattern(&mut self, pat: Pattern) -> Pattern {
        default_fold_pattern(self, pat)
    }
    fn fold_type(&mut self, ty: Type) -> Type {
        default_fold_type(self, ty)
    }
}

/// Recurse into every child of `item` through `folder`, rebuilding the item.
/// Item kinds with no child Expr/Stmt/Type (e.g. `Use`, `World`) pass through.
pub fn default_fold_item<F: AstFolder>(folder: &mut F, item: Item) -> Item {
    match item {
        Item::Function(func) => Item::Function(folder.fold_function(func)),
        Item::Test(mut t) => {
            t.body = folder.fold_block(t.body);
            Item::Test(t)
        }
        Item::Global(mut g) => {
            g.ty = folder.fold_type(g.ty);
            g.initializer = folder.fold_expr(g.initializer);
            Item::Global(g)
        }
        Item::Impl(mut i) => {
            if let Some(trait_ty) = i.trait_type {
                i.trait_type = Some(folder.fold_type(trait_ty));
            }
            i.ty = folder.fold_type(i.ty);
            for binding in &mut i.associated_types {
                let ty = std::mem::replace(&mut binding.ty, Type::Tuple(Vec::new()));
                binding.ty = folder.fold_type(ty);
            }
            for c in &mut i.constants {
                let ty = std::mem::replace(&mut c.ty, Type::Tuple(Vec::new()));
                c.ty = folder.fold_type(ty);
                let value = std::mem::replace(
                    &mut c.value,
                    Expr::Literal(LiteralExpr {
                        id: c.id,
                        value: Literal::Unit,
                        span: c.span,
                    }),
                );
                c.value = folder.fold_expr(value);
            }
            i.methods = i
                .methods
                .into_iter()
                .map(|m| folder.fold_function(m))
                .collect();
            Item::Impl(i)
        }
        Item::Trait(mut t) => {
            t.methods = t
                .methods
                .into_iter()
                .map(|m| folder.fold_function(m))
                .collect();
            Item::Trait(t)
        }
        Item::Struct(mut s) => {
            for f in &mut s.fields {
                let ty = std::mem::replace(&mut f.ty, Type::Tuple(Vec::new()));
                f.ty = folder.fold_type(ty);
                if let Some(default) = f.default.take() {
                    f.default = Some(folder.fold_expr(default));
                }
            }
            Item::Struct(s)
        }
        Item::Newtype(mut n) => {
            n.ty = folder.fold_type(n.ty);
            Item::Newtype(n)
        }
        Item::Variant(mut vr) => {
            for case in &mut vr.cases {
                if let Some(payload) = case.payload.take() {
                    case.payload = Some(folder.fold_type(payload));
                }
            }
            Item::Variant(vr)
        }
        // No child Expr/Stmt/Type — pass through as-is.
        Item::Use(_)
        | Item::Interface(_)
        | Item::Enum(_)
        | Item::Flags(_)
        | Item::TupleTypeDecl(_)
        | Item::Resource(_)
        | Item::World(_) => item,
    }
}

/// Recurse into a function's params/return/body through `folder`.
pub fn default_fold_function<F: AstFolder>(folder: &mut F, mut func: Function) -> Function {
    for param in &mut func.params {
        let ty = std::mem::replace(&mut param.ty, Type::Tuple(Vec::new()));
        param.ty = folder.fold_type(ty);
        if let Some(default) = param.default.take() {
            param.default = Some(folder.fold_expr(default));
        }
    }
    if let Some(ret) = func.return_type.take() {
        func.return_type = Some(folder.fold_type(ret));
    }
    if let Some(body) = func.body.take() {
        func.body = Some(folder.fold_block(body));
    }
    func
}

/// Fold every statement in `block` through `folder`.
pub fn default_fold_block<F: AstFolder>(folder: &mut F, mut block: Block) -> Block {
    block.stmts = block
        .stmts
        .into_iter()
        .map(|s| folder.fold_stmt(s))
        .collect();
    block
}

/// Recurse into every child of `stmt` through `folder`, rebuilding the stmt.
pub fn default_fold_stmt<F: AstFolder>(folder: &mut F, stmt: Stmt) -> Stmt {
    match stmt {
        Stmt::Let(mut l) => {
            l.pattern = folder.fold_pattern(l.pattern);
            if let Some(ty) = l.ty.take() {
                l.ty = Some(folder.fold_type(ty));
            }
            if let Some(value) = l.value.take() {
                l.value = Some(folder.fold_expr(value));
            }
            Stmt::Let(l)
        }
        Stmt::Expr(mut e) => {
            e.expr = folder.fold_expr(e.expr);
            Stmt::Expr(e)
        }
        Stmt::Return(mut r) => {
            if let Some(value) = r.value.take() {
                r.value = Some(folder.fold_expr(value));
            }
            Stmt::Return(r)
        }
        Stmt::TaskReturn(mut t) => {
            t.value = folder.fold_expr(t.value);
            Stmt::TaskReturn(t)
        }
        Stmt::If(mut i) => {
            i.condition = folder.fold_condition(i.condition);
            i.then_block = folder.fold_block(i.then_block);
            if let Some(eb) = i.else_block.take() {
                i.else_block = Some(folder.fold_block(eb));
            }
            Stmt::If(i)
        }
        Stmt::While(mut w) => {
            w.condition = folder.fold_condition(w.condition);
            w.body = folder.fold_block(w.body);
            Stmt::While(w)
        }
        Stmt::For(mut f) => {
            if let Some(init) = f.init.take() {
                f.init = Some(Box::new(folder.fold_stmt(*init)));
            }
            if let Some(cond) = f.condition.take() {
                f.condition = Some(folder.fold_condition(cond));
            }
            if let Some(update) = f.update.take() {
                f.update = Some(folder.fold_expr(update));
            }
            f.body = folder.fold_block(f.body);
            Stmt::For(f)
        }
        Stmt::ForOf(mut f) => {
            f.binding = folder.fold_pattern(f.binding);
            f.iterable = folder.fold_expr(f.iterable);
            f.body = folder.fold_block(f.body);
            Stmt::ForOf(f)
        }
        Stmt::Loop(mut l) => {
            l.body = folder.fold_block(l.body);
            Stmt::Loop(l)
        }
        Stmt::Match(mut m) => {
            m.expr = folder.fold_expr(m.expr);
            m.arms = m
                .arms
                .into_iter()
                .map(|arm| fold_match_arm(folder, arm))
                .collect();
            Stmt::Match(m)
        }
        Stmt::Break(mut b) => {
            if let Some(value) = b.value.take() {
                b.value = Some(Box::new(folder.fold_expr(*value)));
            }
            Stmt::Break(b)
        }
        Stmt::Continue(c) => Stmt::Continue(c),
        Stmt::Assert(mut a) => {
            a.condition = folder.fold_expr(a.condition);
            if let Some(msg) = a.message.take() {
                a.message = Some(folder.fold_expr(msg));
            }
            Stmt::Assert(a)
        }
        Stmt::LabeledBlock(mut lb) => {
            lb.block = folder.fold_block(lb.block);
            Stmt::LabeledBlock(lb)
        }
    }
}

/// Recurse into the children of `cond` through `folder`, rebuilding it.
pub fn default_fold_condition<F: AstFolder>(folder: &mut F, cond: Condition) -> Condition {
    match cond {
        Condition::Expr(e) => Condition::Expr(folder.fold_expr(e)),
        Condition::LetChain { elements, span } => Condition::LetChain {
            elements: elements
                .into_iter()
                .map(|el| match el {
                    ConditionElement::Let {
                        pattern,
                        expr,
                        span,
                    } => ConditionElement::Let {
                        pattern: Box::new(folder.fold_pattern(*pattern)),
                        expr: folder.fold_expr(expr),
                        span,
                    },
                    ConditionElement::Expr(e) => ConditionElement::Expr(folder.fold_expr(e)),
                })
                .collect(),
            span,
        },
    }
}

/// Fold the children of a single match arm through `folder`.
fn fold_match_arm<F: AstFolder>(folder: &mut F, arm: MatchArm) -> MatchArm {
    MatchArm {
        id: arm.id,
        pattern: folder.fold_pattern(arm.pattern),
        guard: arm.guard.map(|g| folder.fold_expr(g)),
        body: folder.fold_expr(arm.body),
        span: arm.span,
    }
}

/// Recurse into every child of `expr` through `folder`, rebuilding the expr.
pub fn default_fold_expr<F: AstFolder>(folder: &mut F, expr: Expr) -> Expr {
    match expr {
        // Leaves.
        Expr::Ident(_) | Expr::Literal(_) => expr,
        Expr::Binary(mut b) => {
            b.left = folder.fold_expr(b.left);
            b.right = folder.fold_expr(b.right);
            Expr::Binary(b)
        }
        Expr::Unary(mut u) => {
            u.expr = folder.fold_expr(u.expr);
            Expr::Unary(u)
        }
        Expr::Assign(mut a) => {
            a.target = folder.fold_expr(a.target);
            a.value = folder.fold_expr(a.value);
            Expr::Assign(a)
        }
        Expr::CompoundAssign(mut c) => {
            c.target = folder.fold_expr(c.target);
            c.value = folder.fold_expr(c.value);
            Expr::CompoundAssign(c)
        }
        Expr::ComparisonChain(mut c) => {
            c.first = folder.fold_expr(c.first);
            c.comparisons = c
                .comparisons
                .into_iter()
                .map(|mut cmp| {
                    cmp.right = folder.fold_expr(cmp.right);
                    cmp
                })
                .collect();
            Expr::ComparisonChain(c)
        }
        Expr::Call(mut c) => {
            c.callee = folder.fold_expr(c.callee);
            c.type_args = c
                .type_args
                .into_iter()
                .map(|t| folder.fold_type(t))
                .collect();
            c.args = c.args.into_iter().map(|a| folder.fold_expr(a)).collect();
            Expr::Call(c)
        }
        Expr::MethodCall(mut m) => {
            m.receiver = folder.fold_expr(m.receiver);
            m.type_args = m
                .type_args
                .into_iter()
                .map(|t| folder.fold_type(t))
                .collect();
            m.args = m.args.into_iter().map(|a| folder.fold_expr(a)).collect();
            Expr::MethodCall(m)
        }
        Expr::StaticMethodCall(mut s) => {
            s.target_type = folder.fold_type(s.target_type);
            s.type_args = s
                .type_args
                .into_iter()
                .map(|t| folder.fold_type(t))
                .collect();
            s.args = s.args.into_iter().map(|a| folder.fold_expr(a)).collect();
            Expr::StaticMethodCall(s)
        }
        Expr::FieldAccess(mut f) => {
            f.expr = folder.fold_expr(f.expr);
            Expr::FieldAccess(f)
        }
        Expr::Index(mut i) => {
            i.expr = folder.fold_expr(i.expr);
            i.index = folder.fold_expr(i.index);
            Expr::Index(i)
        }
        Expr::Block(b) => Expr::Block(Box::new(folder.fold_block(*b))),
        Expr::If(mut i) => {
            i.condition = folder.fold_condition(i.condition);
            i.then_block = folder.fold_block(i.then_block);
            if let Some(eb) = i.else_block.take() {
                i.else_block = Some(folder.fold_block(eb));
            }
            Expr::If(i)
        }
        Expr::Match(mut m) => {
            m.expr = folder.fold_expr(m.expr);
            m.arms = m
                .arms
                .into_iter()
                .map(|arm| fold_match_arm(folder, arm))
                .collect();
            Expr::Match(m)
        }
        Expr::Matches(mut m) => {
            m.expr = folder.fold_expr(m.expr);
            m.pattern = folder.fold_pattern(m.pattern);
            if let Some(guard) = m.guard.take() {
                m.guard = Some(folder.fold_expr(guard));
            }
            Expr::Matches(m)
        }
        Expr::Closure(mut c) => {
            for p in &mut c.params {
                if let Some(ty) = p.ty.take() {
                    p.ty = Some(folder.fold_type(ty));
                }
                if let Some(default) = p.default.take() {
                    p.default = Some(folder.fold_expr(default));
                }
            }
            c.body = folder.fold_expr(c.body);
            Expr::Closure(c)
        }
        Expr::TemplateString(mut t) => {
            t.parts = t
                .parts
                .into_iter()
                .map(|part| match part {
                    TemplatePart::Interpolation { expr, format } => TemplatePart::Interpolation {
                        expr: Box::new(folder.fold_expr(*expr)),
                        format,
                    },
                    other => other,
                })
                .collect();
            Expr::TemplateString(t)
        }
        Expr::Cast(mut c) => {
            c.expr = folder.fold_expr(c.expr);
            c.target_type = folder.fold_type(c.target_type);
            Expr::Cast(c)
        }
        Expr::StructLiteral(mut s) => {
            s.fields = s
                .fields
                .into_iter()
                .map(|mut f| {
                    f.value = folder.fold_expr(f.value);
                    f
                })
                .collect();
            Expr::StructLiteral(s)
        }
        Expr::TupleLiteral(mut t) => {
            t.elements = t
                .elements
                .into_iter()
                .map(|e| folder.fold_expr(e))
                .collect();
            Expr::TupleLiteral(t)
        }
        Expr::LabeledBlock(mut lb) => {
            lb.block = folder.fold_block(lb.block);
            Expr::LabeledBlock(lb)
        }
        Expr::TryOp(mut t) => {
            t.expr = folder.fold_expr(t.expr);
            Expr::TryOp(t)
        }
        Expr::Spread(mut inner, span) => {
            *inner = folder.fold_expr(*inner);
            Expr::Spread(inner, span)
        }
        Expr::Range(mut r) => {
            r.start = folder.fold_expr(r.start);
            r.end = folder.fold_expr(r.end);
            Expr::Range(r)
        }
        Expr::WithHandler(mut w) => {
            w.handlers = w
                .handlers
                .into_iter()
                .map(|mut b| {
                    if let Some(effect) = b.effect.take() {
                        b.effect = Some(folder.fold_type(effect));
                    }
                    b.handler = folder.fold_expr(b.handler);
                    b
                })
                .collect();
            w.body = folder.fold_block(w.body);
            Expr::WithHandler(w)
        }
        Expr::Resume(mut r) => {
            r.value = folder.fold_expr(r.value);
            Expr::Resume(r)
        }
    }
}

/// Recurse into the children of `pat` through `folder`, rebuilding it.
pub fn default_fold_pattern<F: AstFolder>(folder: &mut F, pat: Pattern) -> Pattern {
    match pat {
        Pattern::Ident { .. }
        | Pattern::MutIdent { .. }
        | Pattern::Literal(_)
        | Pattern::Wildcard
        | Pattern::Range { .. } => pat,
        Pattern::Tuple(ps, has_rest) => Pattern::Tuple(
            ps.into_iter().map(|p| folder.fold_pattern(p)).collect(),
            has_rest,
        ),
        Pattern::Or(ps) => Pattern::Or(ps.into_iter().map(|p| folder.fold_pattern(p)).collect()),
        Pattern::Struct {
            type_name,
            fields,
            has_rest,
            span,
        } => Pattern::Struct {
            type_name,
            fields: fields
                .into_iter()
                .map(|mut f| {
                    f.pattern = folder.fold_pattern(f.pattern);
                    f
                })
                .collect(),
            has_rest,
            span,
        },
        Pattern::Variant {
            variant_name,
            variant_qualifier,
            name_id,
            name_span,
            bindings,
            span,
        } => Pattern::Variant {
            variant_name,
            variant_qualifier: variant_qualifier.map(|t| folder.fold_type(t)),
            name_id,
            name_span,
            bindings: bindings
                .into_iter()
                .map(|p| folder.fold_pattern(p))
                .collect(),
            span,
        },
    }
}

/// Recurse into the children of `ty` through `folder`, rebuilding it.
pub fn default_fold_type<F: AstFolder>(folder: &mut F, ty: Type) -> Type {
    match ty {
        Type::Named(_) | Type::TypePackSpread(_, _) => ty,
        Type::Generic(mut g) => {
            g.args = g.args.into_iter().map(|a| folder.fold_type(a)).collect();
            Type::Generic(g)
        }
        Type::NamespacedGeneric(mut g) => {
            g.args = g.args.into_iter().map(|a| folder.fold_type(a)).collect();
            Type::NamespacedGeneric(g)
        }
        Type::Function(mut ft) => {
            ft.params = ft.params.into_iter().map(|p| folder.fold_type(p)).collect();
            ft.return_type = folder.fold_type(ft.return_type);
            Type::Function(ft)
        }
        Type::Tuple(ts) => Type::Tuple(ts.into_iter().map(|t| folder.fold_type(t)).collect()),
        Type::Reference(t) => Type::Reference(Box::new(folder.fold_type(*t))),
        Type::MutReference(t) => Type::MutReference(Box::new(folder.fold_type(*t))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AstId, AstVisitor, Module};
    use crate::hashmap::IndexSet;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::token::Span;

    fn parse(source: &str) -> Module {
        let tokens = Lexer::new(source).tokenize().expect("lex");
        let mut parser = Parser::new(tokens);
        parser.parse().expect("parse")
    }

    fn collect_ids(items: &[Item]) -> Vec<(AstId, Span)> {
        struct Collector(Vec<(AstId, Span)>);
        impl AstVisitor for Collector {
            fn visit_id(&mut self, id: AstId, span: Span) {
                self.0.push((id, span));
            }
        }
        let mut c = Collector(Vec::new());
        for item in items {
            c.visit_item(item);
        }
        c.0
    }

    const SAMPLE: &str = r#"
use { println } from "core:cli";

global COUNT: i32 = 0;

struct Point { x: i32, y: i32 = 0 }

enum Color { Red, Green, Blue }

variant Option<T> { Some(T), None }

trait Greet { fn greet(&self) -> String; }

impl Greet for Point {
    fn greet(&self) -> String {
        return "hi";
    }
}

fn add(a: i32, b: i32) -> i32 {
    let x: i32 = a + b;
    if let Some(v) = lookup(x) {
        for let item of [1, 2, 3] {
            println("{item}");
        }
        match v {
            0 => return 0,
            n => return n,
        }
    }
    assert x == a + b, "broken";
    return x;
}

test "basic" {
    assert add(1, 2) == 3;
}
"#;

    /// A folder that delegates everything to the defaults should be the
    /// identity on AST ids and spans. This guards against silently dropping
    /// a child when `default_fold_*` forgets to recurse into a new variant.
    #[test]
    fn noop_folder_preserves_all_ids() {
        struct NoopFolder;
        impl AstFolder for NoopFolder {}

        let module = parse(SAMPLE);
        let before = collect_ids(&module.items);
        let id_count_before = module.ast_id_count();

        let mut folder = NoopFolder;
        let folded_items: Vec<Item> = module
            .items
            .iter()
            .cloned()
            .map(|it| folder.fold_item(it))
            .collect();

        let after = collect_ids(&folded_items);
        assert_eq!(before.len(), after.len(), "node count must be preserved");
        assert_eq!(before, after, "every (id, span) pair must survive the fold");

        // AstId density: folded items must reference each id at most as many
        // times as the original (no duplication, no loss).
        let unique_before: IndexSet<AstId> = before.iter().map(|(id, _)| *id).collect();
        let unique_after: IndexSet<AstId> = after.iter().map(|(id, _)| *id).collect();
        assert_eq!(unique_before, unique_after);
        let _ = id_count_before;
    }
}
