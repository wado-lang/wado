// Desugaring pass for Wado AST
//
// Transforms high-level AST constructs to simpler forms for codegen:
// - CompoundAssignExpr (x += y) → AssignExpr (x = x + y)
// - ComparisonChainExpr (a < b < c) → BinaryExpr chain ((a < b) && (b < c))
// - Assert (assert cond, msg) → LabeledBlock with intermediates, if, and panic

use crate::ast::{
    AssertStmt, AssignExpr, BinaryExpr, BinaryOp, Block, BreakStmt, CallExpr, CastExpr,
    ClosureExpr, ComparisonChainExpr, CompoundAssignExpr, CompoundAssignOp, Condition,
    ContinueStmt, EffectDecl, EnumDecl, Expr, ExprStmt, FieldAccessExpr, ForOfStmt, ForStmt,
    Function, GlobalDecl, IdentExpr, IfExpr, IfStmt, ImplBlock, IndexExpr, Item, LabeledBlockStmt,
    LetStmt, Literal, LiteralExpr, LoopStmt, MatchArm, MatchExpr, MethodCallExpr, Module, Newtype,
    Pattern, ReturnStmt, StaticMethodCallExpr, Stmt, StructDecl, StructLiteralExpr,
    StructLiteralField, TaskReturnStmt, TemplatePart, TemplateStringExpr, TestDecl, TraitDecl,
    TupleLiteralExpr, UnaryExpr, UnaryOp, WhileStmt,
};
use crate::unparse::unparse_expr_simple;

/// Context for desugaring, holding state that needs to be tracked across the process.
struct DesugarContext {
    /// Counter for generating unique assert block labels
    assert_counter: u32,
    /// Counter for generating unique loop labels (for break/continue handling)
    loop_counter: u32,
    /// Stack of loop labels for break/continue transformation in For loops
    /// Each entry is `(outer_label, body_label)` where:
    /// - `outer_label`: target for unlabeled break
    /// - `body_label`: target for unlabeled continue (to skip to update)
    for_loop_labels: Vec<(String, String)>,
}

/// Desugar a module, transforming high-level constructs to simpler forms.
pub fn desugar_module(module: &Module) -> Module {
    let mut ctx = DesugarContext {
        assert_counter: 0,
        loop_counter: 0,
        for_loop_labels: Vec::new(),
    };
    Module::with_metadata(
        module
            .items
            .iter()
            .map(|item| desugar_item(item, &mut ctx))
            .collect(),
        module.inner_attributes().to_vec(),
        module.shebang().map(String::from),
        module.data_section().map(String::from),
    )
}

fn desugar_item(item: &Item, ctx: &mut DesugarContext) -> Item {
    match item {
        Item::Function(f) => Item::Function(desugar_function(f, ctx)),
        Item::Impl(i) => Item::Impl(desugar_impl(i, ctx)),
        Item::Trait(t) => Item::Trait(desugar_trait(t, ctx)),
        Item::Struct(s) => Item::Struct(desugar_struct(s)),
        Item::Enum(e) => Item::Enum(desugar_enum(e)),
        Item::Variant(v) => Item::Variant(v.clone()),
        Item::Flags(f) => Item::Flags(f.clone()),
        Item::Type(t) => Item::Type(desugar_newtype(t)),
        Item::Effect(e) => Item::Effect(desugar_effect(e)),
        Item::Use(u) => Item::Use(u.clone()),
        Item::Resource(r) => Item::Resource(r.clone()),
        Item::World(w) => Item::World(w.clone()),
        Item::Test(t) => Item::Test(desugar_test(t, ctx)),
        Item::Global(g) => Item::Global(desugar_global(g, ctx)),
    }
}

fn desugar_global(global: &GlobalDecl, _ctx: &mut DesugarContext) -> GlobalDecl {
    GlobalDecl {
        name: global.name.clone(),
        ty: global.ty.clone(),
        initializer: desugar_expr(&global.initializer),
        mutable: global.mutable,
        is_pub: global.is_pub,
        attributes: global.attributes.clone(),
        span: global.span,
    }
}

fn desugar_function(func: &Function, ctx: &mut DesugarContext) -> Function {
    Function {
        name: func.name.clone(),
        is_pub: func.is_pub,
        is_export: func.is_export,
        is_async: func.is_async,
        type_params: func.type_params.clone(),
        attrs: func.attrs.clone(),
        params: func.params.clone(),
        return_type: func.return_type.clone(),
        effects: func.effects.clone(),
        body: func.body.as_ref().map(|b| desugar_block(b, ctx)),
        span: func.span,
    }
}

fn desugar_test(test: &TestDecl, ctx: &mut DesugarContext) -> TestDecl {
    TestDecl {
        attributes: test.attributes.clone(),
        name: test.name.clone(),
        body: desugar_block(&test.body, ctx),
        span: test.span,
    }
}

fn desugar_impl(impl_block: &ImplBlock, ctx: &mut DesugarContext) -> ImplBlock {
    ImplBlock {
        type_params: impl_block.type_params.clone(),
        trait_type: impl_block.trait_type.clone(),
        ty: impl_block.ty.clone(),
        associated_types: impl_block.associated_types.clone(),
        constants: impl_block.constants.clone(),
        methods: impl_block
            .methods
            .iter()
            .map(|m| desugar_function(m, ctx))
            .collect(),
        is_synthesize_request: impl_block.is_synthesize_request,
        span: impl_block.span,
    }
}

fn desugar_trait(trait_decl: &TraitDecl, ctx: &mut DesugarContext) -> TraitDecl {
    TraitDecl {
        name: trait_decl.name.clone(),
        is_pub: trait_decl.is_pub,
        type_params: trait_decl.type_params.clone(),
        associated_types: trait_decl.associated_types.clone(),
        methods: trait_decl
            .methods
            .iter()
            .map(|m| desugar_function(m, ctx))
            .collect(),
        span: trait_decl.span,
    }
}

fn desugar_struct(s: &StructDecl) -> StructDecl {
    s.clone()
}

fn desugar_enum(e: &EnumDecl) -> EnumDecl {
    e.clone()
}

fn desugar_newtype(t: &Newtype) -> Newtype {
    t.clone()
}

fn desugar_effect(e: &EffectDecl) -> EffectDecl {
    e.clone()
}

fn desugar_block(block: &Block, ctx: &mut DesugarContext) -> Block {
    Block {
        stmts: block.stmts.iter().map(|s| desugar_stmt(s, ctx)).collect(),
        span: block.span,
    }
}

fn desugar_let_stmt(l: &LetStmt) -> LetStmt {
    LetStmt {
        pattern: desugar_pattern(&l.pattern),
        is_mut: l.is_mut,
        is_reactive: l.is_reactive,
        ty: l.ty.clone(),
        value: desugar_expr(&l.value),
        span: l.span,
    }
}

fn desugar_pattern(p: &Pattern) -> Pattern {
    match p {
        Pattern::Ident(name) => Pattern::Ident(name.clone()),
        Pattern::MutIdent(name) => Pattern::MutIdent(name.clone()),
        Pattern::Literal(lit) => Pattern::Literal(lit.clone()),
        Pattern::Wildcard => Pattern::Wildcard,
        Pattern::Tuple(patterns) => Pattern::Tuple(patterns.iter().map(desugar_pattern).collect()),
        Pattern::Variant {
            variant_name,
            bindings,
            span,
        } => Pattern::Variant {
            variant_name: variant_name.clone(),
            bindings: bindings.iter().map(desugar_pattern).collect(),
            span: *span,
        },
        Pattern::Struct {
            type_name,
            fields,
            has_rest,
            span,
        } => Pattern::Struct {
            type_name: type_name.clone(),
            fields: fields
                .iter()
                .map(|f| crate::ast::StructPatternField {
                    field_name: f.field_name.clone(),
                    pattern: desugar_pattern(&f.pattern),
                    span: f.span,
                })
                .collect(),
            has_rest: *has_rest,
            span: *span,
        },
    }
}

fn desugar_stmt(stmt: &Stmt, ctx: &mut DesugarContext) -> Stmt {
    match stmt {
        Stmt::Let(l) => Stmt::Let(LetStmt {
            pattern: desugar_pattern(&l.pattern),
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
        Stmt::TaskReturn(tr) => Stmt::TaskReturn(TaskReturnStmt {
            value: desugar_expr(&tr.value),
            span: tr.span,
        }),
        Stmt::If(i) => Stmt::If(IfStmt {
            init: i.init.as_ref().map(|ls| Box::new(desugar_let_stmt(ls))),
            condition: desugar_condition(&i.condition),
            then_block: desugar_block(&i.then_block, ctx),
            else_block: i.else_block.as_ref().map(|b| desugar_block(b, ctx)),
            span: i.span,
        }),
        Stmt::While(w) => desugar_while(w, ctx),
        Stmt::For(f) => desugar_for(f, ctx),
        Stmt::ForOf(f) => desugar_for_of(f, ctx),
        Stmt::Assert(a) => desugar_assert(a, ctx),
        Stmt::Loop(l) => {
            // Save and clear for_loop_labels - breaks inside this loop should
            // target this loop, not an outer for loop
            let saved_labels = std::mem::take(&mut ctx.for_loop_labels);
            let body = desugar_block(&l.body, ctx);
            ctx.for_loop_labels = saved_labels;
            Stmt::Loop(LoopStmt { body, span: l.span })
        }
        Stmt::Break(b) => {
            // If we're inside a For loop and this is an unlabeled break,
            // transform it to break the outer loop label
            if b.label.is_none()
                && let Some((outer_label, _)) = ctx.for_loop_labels.last()
            {
                return Stmt::Break(BreakStmt {
                    label: Some(outer_label.clone()),
                    value: b.value.as_ref().map(|v| Box::new(desugar_expr(v))),
                    span: b.span,
                });
            }
            Stmt::Break(BreakStmt {
                label: b.label.clone(),
                value: b.value.as_ref().map(|v| Box::new(desugar_expr(v))),
                span: b.span,
            })
        }
        Stmt::Continue(c) => {
            // If we're inside a For loop, transform continue to break the body label
            // (this will skip to the update expression)
            if let Some((_, body_label)) = ctx.for_loop_labels.last() {
                return Stmt::Break(BreakStmt {
                    label: Some(body_label.clone()),
                    value: None,
                    span: c.span,
                });
            }
            Stmt::Continue(ContinueStmt { span: c.span })
        }
        Stmt::LabeledBlock(lb) => Stmt::LabeledBlock(LabeledBlockStmt {
            label: lb.label.clone(),
            block: desugar_block(&lb.block, ctx),
            span: lb.span,
        }),
    }
}

fn desugar_expr(expr: &Expr) -> Expr {
    // Desugar expressions. Block/If expressions that can contain statements
    // use a temporary context since they need unique assert IDs within their scope.
    desugar_expr_impl(expr, None)
}

fn desugar_condition(cond: &Condition) -> Condition {
    match cond {
        Condition::Expr(expr) => Condition::Expr(desugar_expr(expr)),
        Condition::Pattern {
            pattern,
            expr,
            span,
        } => Condition::Pattern {
            pattern: pattern.clone(),
            expr: desugar_expr(expr),
            span: *span,
        },
    }
}

fn desugar_expr_impl(expr: &Expr, ctx: Option<&mut DesugarContext>) -> Expr {
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
            has_trailing_comma: c.has_trailing_comma,
            span: c.span,
        })),
        Expr::MethodCall(m) => Expr::MethodCall(Box::new(MethodCallExpr {
            receiver: desugar_expr(&m.receiver),
            method: m.method.clone(),
            type_args: m.type_args.clone(),
            args: m.args.iter().map(desugar_expr).collect(),
            has_trailing_comma: m.has_trailing_comma,
            span: m.span,
        })),
        Expr::StaticMethodCall(s) => Expr::StaticMethodCall(Box::new(StaticMethodCallExpr {
            target_type: s.target_type.clone(),
            method: s.method.clone(),
            args: s.args.iter().map(desugar_expr).collect(),
            has_trailing_comma: s.has_trailing_comma,
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
        Expr::Block(b) => {
            // Block expressions can contain statements including asserts
            if let Some(ctx) = ctx {
                Expr::Block(Box::new(desugar_block(b, ctx)))
            } else {
                // No context - create a temporary one (rare case)
                let mut temp_ctx = DesugarContext {
                    assert_counter: 0,
                    loop_counter: 0,
                    for_loop_labels: Vec::new(),
                };
                Expr::Block(Box::new(desugar_block(b, &mut temp_ctx)))
            }
        }
        Expr::If(i) => {
            if let Some(ctx) = ctx {
                Expr::If(Box::new(IfExpr {
                    init: i.init.as_ref().map(|ls| Box::new(desugar_let_stmt(ls))),
                    condition: desugar_condition(&i.condition),
                    then_block: desugar_block(&i.then_block, ctx),
                    else_block: i.else_block.as_ref().map(|b| desugar_block(b, ctx)),
                    span: i.span,
                }))
            } else {
                let mut temp_ctx = DesugarContext {
                    assert_counter: 0,
                    loop_counter: 0,
                    for_loop_labels: Vec::new(),
                };
                Expr::If(Box::new(IfExpr {
                    init: i.init.as_ref().map(|ls| Box::new(desugar_let_stmt(ls))),
                    condition: desugar_condition(&i.condition),
                    then_block: desugar_block(&i.then_block, &mut temp_ctx),
                    else_block: i
                        .else_block
                        .as_ref()
                        .map(|b| desugar_block(b, &mut temp_ctx)),
                    span: i.span,
                }))
            }
        }
        Expr::Match(m) => Expr::Match(Box::new(MatchExpr {
            expr: desugar_expr(&m.expr),
            arms: m
                .arms
                .iter()
                .map(|arm| MatchArm {
                    pattern: arm.pattern.clone(),
                    guard: arm.guard.as_ref().map(desugar_expr),
                    body: desugar_expr(&arm.body),
                    span: arm.span,
                })
                .collect(),
            span: m.span,
        })),
        Expr::Closure(c) => {
            let source_text = crate::unparse::unparse_expr_simple(&Expr::Closure(c.clone()));
            Expr::Closure(Box::new(ClosureExpr {
                params: c.params.clone(),
                body: desugar_expr(&c.body),
                source_text: Some(source_text),
                span: c.span,
            }))
        }
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
            has_trailing_comma: s.has_trailing_comma,
            span: s.span,
        })),
        Expr::TupleLiteral(t) => Expr::TupleLiteral(Box::new(TupleLiteralExpr {
            elements: t.elements.iter().map(desugar_expr).collect(),
            span: t.span,
        })),
        Expr::LabeledBlock(lb) => {
            // Labeled block expressions can contain statements including asserts
            if let Some(ctx) = ctx {
                Expr::LabeledBlock(Box::new(crate::ast::LabeledBlockExpr {
                    label: lb.label.clone(),
                    block: desugar_block(&lb.block, ctx),
                    span: lb.span,
                }))
            } else {
                let mut ctx = DesugarContext {
                    assert_counter: 0,
                    loop_counter: 0,
                    for_loop_labels: Vec::new(),
                };
                Expr::LabeledBlock(Box::new(crate::ast::LabeledBlockExpr {
                    label: lb.label.clone(),
                    block: desugar_block(&lb.block, &mut ctx),
                    span: lb.span,
                }))
            }
        }
        // Desugar matches expression: `expr matches { pattern && guard }`
        // becomes: `if let pattern = expr { guard } else { false }`
        // or if no guard: `if let pattern = expr { true } else { false }`
        Expr::Matches(m) => desugar_matches_expr(m),
    }
}

/// Desugar matches expression: `expr matches { pattern && guard }`
/// becomes: `match expr { pattern => guard, _ => false }`
/// or if no guard: `match expr { pattern => true, _ => false }`
fn desugar_matches_expr(m: &crate::ast::MatchesExpr) -> Expr {
    let scrutinee = desugar_expr(&m.expr);

    // The match arm body: guard expression or `true` if no guard
    let match_body = if let Some(ref guard) = m.guard {
        desugar_expr(guard)
    } else {
        Expr::Literal(LiteralExpr {
            value: Literal::Bool(true),
            span: m.span,
        })
    };

    // The wildcard arm: `false`
    let wildcard_body = Expr::Literal(LiteralExpr {
        value: Literal::Bool(false),
        span: m.span,
    });

    // Build a match expression
    Expr::Match(Box::new(MatchExpr {
        expr: scrutinee,
        arms: vec![
            MatchArm {
                pattern: m.pattern.clone(),
                guard: None,
                body: match_body,
                span: m.span,
            },
            MatchArm {
                pattern: Pattern::Wildcard,
                guard: None,
                body: wildcard_body,
                span: m.span,
            },
        ],
        span: m.span,
    }))
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

/// Desugar while loop to loop with if-break.
///
/// `while cond { body }` becomes:
/// ```text
/// loop { if !cond { break; } body }
/// ```
///
/// `while let pattern = expr { body }` becomes:
/// ```text
/// loop { if let pattern = expr { body } else { break; } }
/// ```
fn desugar_while(w: &WhileStmt, ctx: &mut DesugarContext) -> Stmt {
    let span = w.span;

    // Save and clear for_loop_labels - breaks inside this while loop should
    // target the generated loop, not an outer for loop
    let saved_labels = std::mem::take(&mut ctx.for_loop_labels);

    let result = match &w.condition {
        Condition::Expr(cond) => {
            // while cond { body } -> loop { if !cond { break; } body }
            let negated_cond = Expr::Unary(Box::new(UnaryExpr {
                op: UnaryOp::Not,
                expr: desugar_expr(cond),
                span,
            }));

            let break_stmt = Stmt::Break(BreakStmt {
                label: None,
                value: None,
                span,
            });

            let if_break = Stmt::If(IfStmt {
                init: None,
                condition: Condition::Expr(negated_cond),
                then_block: Block {
                    stmts: vec![break_stmt],
                    span,
                },
                else_block: None,
                span,
            });

            let mut loop_stmts = vec![if_break];
            loop_stmts.extend(desugar_block(&w.body, ctx).stmts);

            Stmt::Loop(LoopStmt {
                body: Block {
                    stmts: loop_stmts,
                    span,
                },
                span,
            })
        }
        Condition::Pattern { pattern, expr, .. } => {
            // while let pattern = expr { body } -> loop { if let pattern = expr { body } else { break; } }
            let break_stmt = Stmt::Break(BreakStmt {
                label: None,
                value: None,
                span,
            });

            let if_pattern = Stmt::If(IfStmt {
                init: None,
                condition: Condition::Pattern {
                    pattern: pattern.clone(),
                    expr: desugar_expr(expr),
                    span,
                },
                then_block: desugar_block(&w.body, ctx),
                else_block: Some(Block {
                    stmts: vec![break_stmt],
                    span,
                }),
                span,
            });

            Stmt::Loop(LoopStmt {
                body: Block {
                    stmts: vec![if_pattern],
                    span,
                },
                span,
            })
        }
    };

    // Restore for_loop_labels
    ctx.for_loop_labels = saved_labels;
    result
}

/// Desugar C-style for loop to loop with labeled blocks for break/continue handling.
///
/// `for init; cond; update { body }` becomes:
/// ```text
/// __for_N: {
///     init;
///     loop {
///         if !cond { break __for_N; }
///         __for_N_body: { body }
///         update;
///     }
/// }
/// ```
///
/// Pattern conditions are handled similarly:
/// `for init; let pattern = expr; update { body }` becomes:
/// ```text
/// __for_N: {
///     init;
///     loop {
///         if let pattern = expr {
///             __for_N_body: { body }
///             update;
///         } else {
///             break __for_N;
///         }
///     }
/// }
/// ```
fn desugar_for(f: &ForStmt, ctx: &mut DesugarContext) -> Stmt {
    let span = f.span;
    let loop_id = ctx.loop_counter;
    ctx.loop_counter += 1;

    let outer_label = format!("__for_{loop_id}");
    let body_label = format!("__for_{loop_id}_body");

    // Push labels for break/continue transformation
    ctx.for_loop_labels
        .push((outer_label.clone(), body_label.clone()));

    // Desugar body with the labels in scope
    let desugared_body = desugar_block(&f.body, ctx);

    // Pop labels
    ctx.for_loop_labels.pop();

    // Wrap body in labeled block for continue handling
    let labeled_body = Stmt::LabeledBlock(LabeledBlockStmt {
        label: body_label,
        block: desugared_body,
        span,
    });

    // Build loop body based on condition type
    let loop_body = match &f.condition {
        Some(Condition::Expr(cond)) => {
            // Expr condition: if !cond { break __for_N; }
            let negated_cond = Expr::Unary(Box::new(UnaryExpr {
                op: UnaryOp::Not,
                expr: desugar_expr(cond),
                span,
            }));

            let break_outer = Stmt::Break(BreakStmt {
                label: Some(outer_label.clone()),
                value: None,
                span,
            });

            let if_break = Stmt::If(IfStmt {
                init: None,
                condition: Condition::Expr(negated_cond),
                then_block: Block {
                    stmts: vec![break_outer],
                    span,
                },
                else_block: None,
                span,
            });

            let mut stmts = vec![if_break, labeled_body];
            if let Some(update) = &f.update {
                stmts.push(Stmt::Expr(ExprStmt {
                    expr: desugar_expr(update),
                    span,
                }));
            }
            Block { stmts, span }
        }
        Some(Condition::Pattern { pattern, expr, .. }) => {
            // Pattern condition: if let pattern = expr { body; update; } else { break __for_N; }
            let break_outer = Stmt::Break(BreakStmt {
                label: Some(outer_label.clone()),
                value: None,
                span,
            });

            let mut then_stmts = vec![labeled_body];
            if let Some(update) = &f.update {
                then_stmts.push(Stmt::Expr(ExprStmt {
                    expr: desugar_expr(update),
                    span,
                }));
            }

            let if_pattern = Stmt::If(IfStmt {
                init: None,
                condition: Condition::Pattern {
                    pattern: pattern.clone(),
                    expr: desugar_expr(expr),
                    span,
                },
                then_block: Block {
                    stmts: then_stmts,
                    span,
                },
                else_block: Some(Block {
                    stmts: vec![break_outer],
                    span,
                }),
                span,
            });

            Block {
                stmts: vec![if_pattern],
                span,
            }
        }
        None => {
            // No condition: infinite loop (just body + update)
            let mut stmts = vec![labeled_body];
            if let Some(update) = &f.update {
                stmts.push(Stmt::Expr(ExprStmt {
                    expr: desugar_expr(update),
                    span,
                }));
            }
            Block { stmts, span }
        }
    };

    let loop_stmt = Stmt::Loop(LoopStmt {
        body: loop_body,
        span,
    });

    // Build outer block: init; loop { ... }
    let mut outer_stmts = Vec::new();
    if let Some(init) = &f.init {
        outer_stmts.push(desugar_stmt(init, ctx));
    }
    outer_stmts.push(loop_stmt);

    Stmt::LabeledBlock(LabeledBlockStmt {
        label: outer_label,
        block: Block {
            stmts: outer_stmts,
            span,
        },
        span,
    })
}

/// Desugar for-of loop to loop with iterator methods.
///
/// `for let x of items { body }` becomes:
/// ```text
/// {
///     let mut __iter_N = items.into_iter();
///     loop {
///         if let Some(x) = __iter_N.next() { body } else { break; }
///     }
/// }
/// ```
fn desugar_for_of(f: &ForOfStmt, ctx: &mut DesugarContext) -> Stmt {
    let span = f.span;
    let loop_id = ctx.loop_counter;
    ctx.loop_counter += 1;

    // Save and clear for_loop_labels - breaks inside this for-of loop should
    // target the generated loop, not an outer for loop
    let saved_labels = std::mem::take(&mut ctx.for_loop_labels);

    let iter_var = format!("__iter_{loop_id}");

    // let mut __iter_N = items.into_iter();
    let into_iter_call = Expr::MethodCall(Box::new(MethodCallExpr {
        receiver: desugar_expr(&f.iterable),
        method: "into_iter".to_string(),
        type_args: vec![],
        args: vec![],
        has_trailing_comma: false,
        span,
    }));

    let iter_let = Stmt::Let(LetStmt {
        pattern: Pattern::Ident(iter_var.clone()),
        is_mut: true,
        is_reactive: false,
        ty: None,
        value: into_iter_call,
        span,
    });

    // __iter_N.next()
    let next_call = Expr::MethodCall(Box::new(MethodCallExpr {
        receiver: Expr::Ident(IdentExpr {
            name: iter_var,
            span,
        }),
        method: "next".to_string(),
        type_args: vec![],
        args: vec![],
        has_trailing_comma: false,
        span,
    }));

    // Pattern: Some(binding)
    let some_pattern = Pattern::Variant {
        variant_name: "Some".to_string(),
        bindings: vec![desugar_pattern(&f.binding)],
        span,
    };

    // break;
    let break_stmt = Stmt::Break(BreakStmt {
        label: None,
        value: None,
        span,
    });

    // if let Some(x) = __iter_N.next() { body } else { break; }
    let if_let = Stmt::If(IfStmt {
        init: None,
        condition: Condition::Pattern {
            pattern: some_pattern,
            expr: next_call,
            span,
        },
        then_block: desugar_block(&f.body, ctx),
        else_block: Some(Block {
            stmts: vec![break_stmt],
            span,
        }),
        span,
    });

    // loop { if let ... }
    let loop_stmt = Stmt::Loop(LoopStmt {
        body: Block {
            stmts: vec![if_let],
            span,
        },
        span,
    });

    // Restore for_loop_labels
    ctx.for_loop_labels = saved_labels;

    // Wrap in a labeled block: __for_of_N: { let mut __iter_N = ...; loop { ... } }
    Stmt::LabeledBlock(LabeledBlockStmt {
        label: format!("__for_of_{loop_id}"),
        block: Block {
            stmts: vec![iter_let, loop_stmt],
            span,
        },
        span,
    })
}

/// Desugar an assert statement into a labeled block with intermediate value caching.
///
/// `assert condition, message;` becomes:
/// ```text
/// __assert_N: {
///     let __v0 = <intermediate0>;
///     let __v1 = <intermediate1>;
///     ...
///     let __cond = <reconstructed_condition>;
///     if !__cond {
///         panic(`Assertion failed:
/// condition: <source>
/// <intermediate0_source>: {__v0}
/// ...`);
///     }
/// }
/// ```
fn desugar_assert(assert_stmt: &AssertStmt, ctx: &mut DesugarContext) -> Stmt {
    let assert_id = ctx.assert_counter;
    ctx.assert_counter += 1;
    let span = assert_stmt.span;

    // Collect intermediate expressions and generate substitution
    let mut intermediates: Vec<(String, String, Expr)> = Vec::new(); // (var_name, source, expr)
    let mut var_counter = 0;

    // Desugar the condition first (handles CompoundAssign, ComparisonChain, etc.)
    let desugared_condition = desugar_expr(&assert_stmt.condition);

    // Collect intermediates from the desugared condition
    collect_intermediates(
        &desugared_condition,
        &mut intermediates,
        &mut var_counter,
        true,
    );

    // Build the list of let statements for intermediates
    let mut stmts: Vec<Stmt> = Vec::new();

    for (var_name, _source, expr) in &intermediates {
        stmts.push(Stmt::Let(LetStmt {
            pattern: Pattern::Ident(var_name.clone()),
            is_mut: false,
            is_reactive: false,
            ty: None,
            value: expr.clone(),
            span,
        }));
    }

    // Build the condition expression using the intermediate variables
    let reconstructed_condition =
        reconstruct_with_intermediates(&desugared_condition, &intermediates);

    // Store condition in a variable (scoped to this labeled block)
    let cond_var = "__cond".to_string();
    stmts.push(Stmt::Let(LetStmt {
        pattern: Pattern::Ident(cond_var.clone()),
        is_mut: false,
        is_reactive: false,
        ty: None,
        value: reconstructed_condition,
        span,
    }));

    // Build the error message template string
    let condition_source = unparse_expr_simple(&assert_stmt.condition);
    let mut template_parts: Vec<TemplatePart> = Vec::new();

    // Helper to create #function literal expression
    let function_expr = Expr::Literal(LiteralExpr {
        value: Literal::LocationFunction,
        span,
    });
    // Helper to create #file literal expression
    let file_expr = Expr::Literal(LiteralExpr {
        value: Literal::LocationFile,
        span,
    });
    // Helper to create #line literal expression
    let line_expr = Expr::Literal(LiteralExpr {
        value: Literal::LocationLine,
        span,
    });

    // Format: "Assertion failed in <function> at <file>:<line>: <message (if any)>"
    // All on one line for Sentry issue title compatibility
    template_parts.push(TemplatePart::String("Assertion failed in ".to_string()));
    template_parts.push(TemplatePart::Interpolation {
        expr: Box::new(function_expr),
        format: None,
    });
    template_parts.push(TemplatePart::String(" at ".to_string()));
    template_parts.push(TemplatePart::Interpolation {
        expr: Box::new(file_expr),
        format: None,
    });
    template_parts.push(TemplatePart::String(":".to_string()));
    template_parts.push(TemplatePart::Interpolation {
        expr: Box::new(line_expr),
        format: None,
    });
    if let Some(msg) = &assert_stmt.message {
        template_parts.push(TemplatePart::String(": ".to_string()));
        template_parts.push(TemplatePart::Interpolation {
            expr: Box::new(desugar_expr(msg)),
            format: None,
        });
    }
    template_parts.push(TemplatePart::String(format!(
        "\ncondition: {condition_source}\n"
    )));

    // Add each intermediate value
    for (var_name, source, _) in &intermediates {
        template_parts.push(TemplatePart::String(format!("{source}: ")));
        template_parts.push(TemplatePart::Interpolation {
            expr: Box::new(Expr::Ident(IdentExpr {
                name: var_name.clone(),
                span,
            })),
            format: None,
        });
        template_parts.push(TemplatePart::String("\n".to_string()));
    }

    let error_message = Expr::TemplateString(Box::new(TemplateStringExpr {
        parts: template_parts,
        span,
    }));

    // Build: panic(error_message)
    let panic_call = Expr::Call(Box::new(CallExpr {
        callee: Expr::Ident(IdentExpr {
            name: "panic".to_string(),
            span,
        }),
        type_args: vec![],
        args: vec![error_message],
        has_trailing_comma: false,
        span,
    }));

    // Build: if !__cond { panic(...); }
    let if_stmt = Stmt::If(IfStmt {
        init: None,
        condition: Condition::Expr(Expr::Unary(Box::new(UnaryExpr {
            op: UnaryOp::Not,
            expr: Expr::Ident(IdentExpr {
                name: cond_var,
                span,
            }),
            span,
        }))),
        then_block: Block {
            stmts: vec![Stmt::Expr(ExprStmt {
                expr: panic_call,
                span,
            })],
            span,
        },
        else_block: None,
        span,
    });
    stmts.push(if_stmt);

    // Wrap everything in a labeled block
    Stmt::LabeledBlock(LabeledBlockStmt {
        label: format!("__assert_{assert_id}"),
        block: Block { stmts, span },
        span,
    })
}

/// Collect intermediate expressions that should be cached for power-assert display.
/// Returns (`var_name`, `source_text`, `original_expr`) for each intermediate.
fn collect_intermediates(
    expr: &Expr,
    intermediates: &mut Vec<(String, String, Expr)>,
    counter: &mut u32,
    is_root: bool,
) {
    match expr {
        Expr::Binary(bin) => {
            // Recursively collect from operands
            collect_intermediates(&bin.left, intermediates, counter, false);
            collect_intermediates(&bin.right, intermediates, counter, false);

            // Don't collect the root comparison itself (it's shown as "condition: ...")
            if !is_root {
                let var_name = format!("__v{}", *counter);
                *counter += 1;
                let source = unparse_expr_simple(expr);
                intermediates.push((var_name, source, expr.clone()));
            }
        }
        Expr::Ident(ident) => {
            // Always collect identifiers - they're the most useful values
            let var_name = format!("__v{}", *counter);
            *counter += 1;
            intermediates.push((var_name, ident.name.clone(), expr.clone()));
        }
        Expr::Call(_) | Expr::MethodCall(_) | Expr::FieldAccess(_) | Expr::Index(_) => {
            // Collect these expression types
            let var_name = format!("__v{}", *counter);
            *counter += 1;
            let source = unparse_expr_simple(expr);
            intermediates.push((var_name, source, expr.clone()));
        }
        Expr::Unary(unary) => {
            // Skip negated numeric literals — extracting them into intermediates
            // would lose the literal status needed for bidirectional type coercion
            // (e.g., `x_i64 == -50` should coerce -50 to i64, not default to i32)
            if unary.op == UnaryOp::Neg
                && matches!(&unary.expr, Expr::Literal(lit) if matches!(&lit.value, Literal::Number(_)))
            {
                return;
            }
            // Recurse into operand
            collect_intermediates(&unary.expr, intermediates, counter, false);
            // Also collect the unary expression itself if not root
            if !is_root {
                let var_name = format!("__v{}", *counter);
                *counter += 1;
                let source = unparse_expr_simple(expr);
                intermediates.push((var_name, source, expr.clone()));
            }
        }
        // Literals and other expressions don't need to be cached
        _ => {}
    }
}

/// Reconstruct the condition expression using intermediate variable references.
fn reconstruct_with_intermediates(expr: &Expr, intermediates: &[(String, String, Expr)]) -> Expr {
    // Find if this expression matches an intermediate
    let source = unparse_expr_simple(expr);
    for (var_name, int_source, _) in intermediates {
        if &source == int_source {
            return Expr::Ident(IdentExpr {
                name: var_name.clone(),
                span: expr.span(),
            });
        }
    }

    // Otherwise, recursively reconstruct
    match expr {
        Expr::Binary(bin) => Expr::Binary(Box::new(BinaryExpr {
            left: reconstruct_with_intermediates(&bin.left, intermediates),
            op: bin.op,
            right: reconstruct_with_intermediates(&bin.right, intermediates),
            span: bin.span,
        })),
        Expr::Unary(unary) => Expr::Unary(Box::new(UnaryExpr {
            op: unary.op,
            expr: reconstruct_with_intermediates(&unary.expr, intermediates),
            span: unary.span,
        })),
        // For other expressions, return as-is (they might be intermediates or literals)
        _ => expr.clone(),
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
                value: crate::ast::Literal::Number("1".to_string()),
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
                value: crate::ast::Literal::Number("0".to_string()),
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
                        value: crate::ast::Literal::Number("10".to_string()),
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
