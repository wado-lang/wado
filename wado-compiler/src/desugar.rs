// Desugaring pass for Wado AST
//
// Transforms high-level AST constructs to simpler forms for codegen:
// - CompoundAssignExpr (x += y) → AssignExpr (x = x + y)
// - ComparisonChainExpr (a < b < c) → BinaryExpr chain ((a < b) && (b < c))

use crate::ast::{
    AssertStmt, AssignExpr, BinaryExpr, BinaryOp, Block, BreakStmt, CallExpr, CastExpr,
    ClosureExpr, ComparisonChainExpr, CompoundAssignExpr, CompoundAssignOp, ContinueStmt,
    EffectDecl, EnumDecl, Expr, FieldAccessExpr, ForOfStmt, ForStmt, Function, IfExpr, IfStmt,
    ImplBlock, IndexExpr, Item, LetStmt, LoopStmt, MatchArm, MatchExpr, MethodCallExpr, Module,
    ReturnStmt, StaticMethodCallExpr, Stmt, StructDecl, StructLiteralExpr, StructLiteralField,
    TemplateStringExpr, TupleLiteralExpr, TypeAlias, UnaryExpr, WhileStmt,
};

/// Desugar a module, transforming high-level constructs to simpler forms.
pub fn desugar_module(module: &Module) -> Module {
    Module::with_metadata(
        module.items.iter().map(desugar_item).collect(),
        module.shebang().map(String::from),
        module.data_section().map(String::from),
    )
}

fn desugar_item(item: &Item) -> Item {
    match item {
        Item::Function(f) => Item::Function(desugar_function(f)),
        Item::Impl(i) => Item::Impl(desugar_impl(i)),
        Item::Struct(s) => Item::Struct(desugar_struct(s)),
        Item::Enum(e) => Item::Enum(desugar_enum(e)),
        Item::Type(t) => Item::Type(desugar_type_alias(t)),
        Item::Effect(e) => Item::Effect(desugar_effect(e)),
        Item::Use(u) => Item::Use(u.clone()),
        Item::Resource(r) => Item::Resource(r.clone()),
        Item::World(w) => Item::World(w.clone()),
    }
}

fn desugar_function(func: &Function) -> Function {
    Function {
        name: func.name.clone(),
        is_pub: func.is_pub,
        type_params: func.type_params.clone(),
        attrs: func.attrs.clone(),
        params: func.params.clone(),
        return_type: func.return_type.clone(),
        effects: func.effects.clone(),
        body: func.body.as_ref().map(desugar_block),
        span: func.span,
    }
}

fn desugar_impl(impl_block: &ImplBlock) -> ImplBlock {
    ImplBlock {
        type_params: impl_block.type_params.clone(),
        ty: impl_block.ty.clone(),
        methods: impl_block.methods.iter().map(desugar_function).collect(),
        span: impl_block.span,
    }
}

fn desugar_struct(s: &StructDecl) -> StructDecl {
    s.clone()
}

fn desugar_enum(e: &EnumDecl) -> EnumDecl {
    e.clone()
}

fn desugar_type_alias(t: &TypeAlias) -> TypeAlias {
    t.clone()
}

fn desugar_effect(e: &EffectDecl) -> EffectDecl {
    e.clone()
}

fn desugar_block(block: &Block) -> Block {
    Block {
        stmts: block.stmts.iter().map(desugar_stmt).collect(),
        span: block.span,
    }
}

fn desugar_let_stmt(l: &LetStmt) -> LetStmt {
    LetStmt {
        name: l.name.clone(),
        is_mut: l.is_mut,
        is_reactive: l.is_reactive,
        ty: l.ty.clone(),
        value: desugar_expr(&l.value),
        span: l.span,
    }
}

fn desugar_stmt(stmt: &Stmt) -> Stmt {
    match stmt {
        Stmt::Let(l) => Stmt::Let(LetStmt {
            name: l.name.clone(),
            is_mut: l.is_mut,
            is_reactive: l.is_reactive,
            ty: l.ty.clone(),
            value: desugar_expr(&l.value),
            span: l.span,
        }),
        Stmt::Expr(e) => Stmt::Expr(crate::ast::ExprStmt {
            expr: desugar_expr(&e.expr),
            span: e.span,
        }),
        Stmt::Return(r) => Stmt::Return(ReturnStmt {
            value: r.value.as_ref().map(desugar_expr),
            span: r.span,
        }),
        Stmt::If(i) => Stmt::If(IfStmt {
            init: i.init.as_ref().map(|ls| Box::new(desugar_let_stmt(ls))),
            condition: desugar_expr(&i.condition),
            then_block: desugar_block(&i.then_block),
            else_block: i.else_block.as_ref().map(desugar_block),
            span: i.span,
        }),
        Stmt::While(w) => Stmt::While(WhileStmt {
            condition: desugar_expr(&w.condition),
            body: desugar_block(&w.body),
            span: w.span,
        }),
        Stmt::For(f) => Stmt::For(ForStmt {
            init: f.init.as_ref().map(|s| Box::new(desugar_stmt(s))),
            condition: f.condition.as_ref().map(desugar_expr),
            update: f.update.as_ref().map(desugar_expr),
            body: desugar_block(&f.body),
            span: f.span,
        }),
        Stmt::ForOf(f) => Stmt::ForOf(ForOfStmt {
            binding: f.binding.clone(),
            is_mut: f.is_mut,
            iterable: desugar_expr(&f.iterable),
            body: desugar_block(&f.body),
            span: f.span,
        }),
        Stmt::Assert(a) => Stmt::Assert(AssertStmt {
            condition: desugar_expr(&a.condition),
            message: a.message.as_ref().map(desugar_expr),
            span: a.span,
        }),
        Stmt::Loop(l) => Stmt::Loop(LoopStmt {
            body: desugar_block(&l.body),
            span: l.span,
        }),
        Stmt::Break(b) => Stmt::Break(BreakStmt { span: b.span }),
        Stmt::Continue(c) => Stmt::Continue(ContinueStmt { span: c.span }),
    }
}

fn desugar_expr(expr: &Expr) -> Expr {
    match expr {
        // Desugar compound assignment: x += y → x = x + y
        Expr::CompoundAssign(ca) => desugar_compound_assign(ca),

        // Desugar comparison chain: a < b < c → (a < b) && (b < c)
        Expr::ComparisonChain(chain) => desugar_comparison_chain(chain),

        // Recursively desugar other expressions
        Expr::Ident(i) => Expr::Ident(i.clone()),
        Expr::Literal(l) => Expr::Literal(l.clone()),
        Expr::Binary(b) => Expr::Binary(Box::new(BinaryExpr {
            left: desugar_expr(&b.left),
            op: b.op,
            right: desugar_expr(&b.right),
            span: b.span,
        })),
        Expr::Unary(u) => Expr::Unary(Box::new(UnaryExpr {
            op: u.op,
            expr: desugar_expr(&u.expr),
            span: u.span,
        })),
        Expr::Assign(a) => Expr::Assign(Box::new(AssignExpr {
            target: desugar_expr(&a.target),
            value: desugar_expr(&a.value),
            span: a.span,
        })),
        Expr::Call(c) => Expr::Call(Box::new(CallExpr {
            callee: desugar_expr(&c.callee),
            type_args: c.type_args.clone(),
            args: c.args.iter().map(desugar_expr).collect(),
            span: c.span,
        })),
        Expr::MethodCall(m) => Expr::MethodCall(Box::new(MethodCallExpr {
            receiver: desugar_expr(&m.receiver),
            method: m.method.clone(),
            type_args: m.type_args.clone(),
            args: m.args.iter().map(desugar_expr).collect(),
            span: m.span,
        })),
        Expr::StaticMethodCall(s) => Expr::StaticMethodCall(Box::new(StaticMethodCallExpr {
            target_type: s.target_type.clone(),
            method: s.method.clone(),
            args: s.args.iter().map(desugar_expr).collect(),
            span: s.span,
        })),
        Expr::FieldAccess(f) => Expr::FieldAccess(Box::new(FieldAccessExpr {
            expr: desugar_expr(&f.expr),
            field: f.field.clone(),
            span: f.span,
        })),
        Expr::Index(i) => Expr::Index(Box::new(IndexExpr {
            expr: desugar_expr(&i.expr),
            index: desugar_expr(&i.index),
            span: i.span,
        })),
        Expr::Block(b) => Expr::Block(Box::new(desugar_block(b))),
        Expr::If(i) => Expr::If(Box::new(IfExpr {
            init: i.init.as_ref().map(|ls| Box::new(desugar_let_stmt(ls))),
            condition: desugar_expr(&i.condition),
            then_block: desugar_block(&i.then_block),
            else_block: i.else_block.as_ref().map(desugar_block),
            span: i.span,
        })),
        Expr::Match(m) => Expr::Match(Box::new(MatchExpr {
            expr: desugar_expr(&m.expr),
            arms: m
                .arms
                .iter()
                .map(|arm| MatchArm {
                    pattern: arm.pattern.clone(),
                    body: desugar_expr(&arm.body),
                    span: arm.span,
                })
                .collect(),
            span: m.span,
        })),
        Expr::Closure(c) => Expr::Closure(Box::new(ClosureExpr {
            params: c.params.clone(),
            body: desugar_expr(&c.body),
            span: c.span,
        })),
        Expr::TemplateString(t) => Expr::TemplateString(Box::new(desugar_template_string(t))),
        Expr::Cast(c) => Expr::Cast(Box::new(CastExpr {
            expr: desugar_expr(&c.expr),
            target_type: c.target_type.clone(),
            span: c.span,
        })),
        Expr::StructLiteral(s) => Expr::StructLiteral(Box::new(StructLiteralExpr {
            name: s.name.clone(),
            fields: s
                .fields
                .iter()
                .map(|f| StructLiteralField {
                    name: f.name.clone(),
                    value: desugar_expr(&f.value),
                    is_shorthand: f.is_shorthand,
                    span: f.span,
                })
                .collect(),
            span: s.span,
        })),
        Expr::TupleLiteral(t) => Expr::TupleLiteral(Box::new(TupleLiteralExpr {
            elements: t.elements.iter().map(desugar_expr).collect(),
            span: t.span,
        })),
    }
}

/// Desugar compound assignment: `x += y` → `x = x + y`
fn desugar_compound_assign(ca: &CompoundAssignExpr) -> Expr {
    let target = desugar_expr(&ca.target);
    let value = desugar_expr(&ca.value);

    let op = match ca.op {
        CompoundAssignOp::Add => BinaryOp::Add,
        CompoundAssignOp::Sub => BinaryOp::Sub,
        CompoundAssignOp::Mul => BinaryOp::Mul,
        CompoundAssignOp::Div => BinaryOp::Div,
        CompoundAssignOp::Mod => BinaryOp::Mod,
    };

    let binary_expr = Expr::Binary(Box::new(BinaryExpr {
        left: target.clone(),
        op,
        right: value,
        span: ca.span,
    }));

    Expr::Assign(Box::new(AssignExpr {
        target,
        value: binary_expr,
        span: ca.span,
    }))
}

/// Desugar comparison chain: `a < b < c` → `(a < b) && (b < c)`
fn desugar_comparison_chain(chain: &ComparisonChainExpr) -> Expr {
    let first = desugar_expr(&chain.first);

    if chain.comparisons.is_empty() {
        return first;
    }

    if chain.comparisons.len() == 1 {
        // Single comparison, just a binary expr
        let cmp = &chain.comparisons[0];
        return Expr::Binary(Box::new(BinaryExpr {
            left: first,
            op: cmp.op,
            right: desugar_expr(&cmp.right),
            span: chain.span,
        }));
    }

    // Build chain: (a < b) && (b < c) && ...
    let mut result: Option<Expr> = None;
    let mut prev = first;

    for cmp in &chain.comparisons {
        let right = desugar_expr(&cmp.right);
        let comparison = Expr::Binary(Box::new(BinaryExpr {
            left: prev.clone(),
            op: cmp.op,
            right: right.clone(),
            span: cmp.op_span,
        }));

        result = Some(match result {
            None => comparison,
            Some(acc) => Expr::Binary(Box::new(BinaryExpr {
                left: acc,
                op: BinaryOp::And,
                right: comparison,
                span: chain.span,
            })),
        });

        prev = right;
    }

    result.unwrap()
}

fn desugar_template_string(t: &TemplateStringExpr) -> TemplateStringExpr {
    use crate::ast::TemplatePart;

    TemplateStringExpr {
        parts: t
            .parts
            .iter()
            .map(|part| match part {
                TemplatePart::String(s) => TemplatePart::String(s.clone()),
                TemplatePart::Interpolation { expr, format } => TemplatePart::Interpolation {
                    expr: Box::new(desugar_expr(expr)),
                    format: format.clone(),
                },
            })
            .collect(),
        span: t.span,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{IdentExpr, LiteralExpr};
    use crate::token::Span;

    fn dummy_span() -> Span {
        Span::new(0, 0, 1, 1)
    }

    #[test]
    fn test_desugar_compound_assign() {
        // x += 1
        let ca = CompoundAssignExpr {
            target: Expr::Ident(IdentExpr {
                name: "x".to_string(),
                span: dummy_span(),
            }),
            op: CompoundAssignOp::Add,
            value: Expr::Literal(LiteralExpr {
                value: crate::ast::Literal::Int(crate::ast::IntLiteral {
                    repr: "1".to_string(),
                }),
                span: dummy_span(),
            }),
            span: dummy_span(),
        };

        let desugared = desugar_compound_assign(&ca);

        // Should be x = x + 1
        match desugared {
            Expr::Assign(assign) => {
                match &assign.target {
                    Expr::Ident(i) => assert_eq!(i.name, "x"),
                    _ => panic!("expected ident"),
                }
                match &assign.value {
                    Expr::Binary(b) => {
                        assert_eq!(b.op, BinaryOp::Add);
                        match &b.left {
                            Expr::Ident(i) => assert_eq!(i.name, "x"),
                            _ => panic!("expected ident"),
                        }
                    }
                    _ => panic!("expected binary"),
                }
            }
            _ => panic!("expected assign"),
        }
    }

    #[test]
    fn test_desugar_comparison_chain() {
        use crate::ast::ChainedComparison;

        // 0 < x < 10
        let chain = ComparisonChainExpr {
            first: Expr::Literal(LiteralExpr {
                value: crate::ast::Literal::Int(crate::ast::IntLiteral {
                    repr: "0".to_string(),
                }),
                span: dummy_span(),
            }),
            comparisons: vec![
                ChainedComparison {
                    op: BinaryOp::Lt,
                    right: Expr::Ident(IdentExpr {
                        name: "x".to_string(),
                        span: dummy_span(),
                    }),
                    op_span: dummy_span(),
                },
                ChainedComparison {
                    op: BinaryOp::Lt,
                    right: Expr::Literal(LiteralExpr {
                        value: crate::ast::Literal::Int(crate::ast::IntLiteral {
                            repr: "10".to_string(),
                        }),
                        span: dummy_span(),
                    }),
                    op_span: dummy_span(),
                },
            ],
            span: dummy_span(),
        };

        let desugared = desugar_comparison_chain(&chain);

        // Should be (0 < x) && (x < 10)
        match desugared {
            Expr::Binary(and) => {
                assert_eq!(and.op, BinaryOp::And);
                // Left should be 0 < x
                match &and.left {
                    Expr::Binary(lt) => {
                        assert_eq!(lt.op, BinaryOp::Lt);
                    }
                    _ => panic!("expected binary"),
                }
                // Right should be x < 10
                match &and.right {
                    Expr::Binary(lt) => {
                        assert_eq!(lt.op, BinaryOp::Lt);
                    }
                    _ => panic!("expected binary"),
                }
            }
            _ => panic!("expected binary (and)"),
        }
    }
}
