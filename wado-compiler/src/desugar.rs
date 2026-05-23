//! AST→AST desugaring of the parsed surface syntax. The output AST is
//! what the resolver sees.
//!
//! What this phase desugars:
//!
//! - `CompoundAssignExpr` (`x += y`) → `AssignExpr` (`x = x + y`)
//! - `while` / `for` / `for-of` loops → explicit `loop` + `break`
//! - template strings → `Display`/`Inspect`-driven concatenation skeletons
//!
//! What this phase deliberately leaves alone, deferring desugar to the
//! resolver (which has typed sub-expressions to work with):
//!
//! - `assert cond[, msg];` → `Resolver::desugar_assert`
//! - `expr matches { pat [&& guard] }` → `Resolver::desugar_matches_expr`
//! - `a < b < c` (`Expr::ComparisonChain`) → `Resolver::desugar_comparison_chain`
//! - `use … namespace` prefixes (e.g. `helper::foo`) — canonicalized at
//!   lookup time by `Resolver::strip_ns_prefix` so the AST keeps the
//!   user's text and LSP cursors land on the prefixed form as written.

use crate::ast::{
    AssignExpr, AstId, BinaryExpr, BinaryOp, Block, BreakStmt, CallExpr, CastExpr, ClosureExpr,
    ComparisonChainExpr, CompoundAssignExpr, CompoundAssignOp, Condition, ConditionElement,
    ContinueStmt, EnumDecl, Expr, ExprStmt, FieldAccessExpr, ForOfStmt, ForStmt, Function,
    GlobalDecl, IfExpr, IfStmt, ImplBlock, IndexExpr, InterfaceDecl, Item, LabeledBlockStmt,
    LetStmt, LoopStmt, MatchArm, MatchExpr, MethodCallExpr, Module, Newtype, Pattern, ReturnStmt,
    StaticMethodCallExpr, Stmt, StructDecl, StructLiteralExpr, StructLiteralField, TaskReturnStmt,
    TemplatePart, TemplateStringExpr, TestDecl, TraitDecl, TupleLiteralExpr, UnaryExpr, UnaryOp,
    WhileStmt,
};

/// Context for desugaring, holding state that needs to be tracked across the process.
struct DesugarContext {
    /// Counter for generating unique loop labels (for break/continue handling)
    loop_counter: u32,
    /// Stack of loop labels for break/continue transformation in For loops
    /// Each entry is `(outer_label, body_label)` where:
    /// - `outer_label`: target for unlabeled break
    /// - `body_label`: target for unlabeled continue (to skip to update)
    for_loop_labels: Vec<(String, String)>,
    /// `AstId` of the AST node currently being desugared. Synthetic nodes produced
    /// during desugaring inherit this id so that `Module::ast_id_count` remains
    /// parser-allocated and `AstIds` stay dense in `0..ast_id_count`. The desugar
    /// phase is slated for removal; after that, every AST node will be parser-owned.
    ///
    /// `None` at module top-level before descending into any item. Must be `Some`
    /// whenever `synth_id` is called — every desugar helper that produces synthetic
    /// nodes saves, sets this to the enclosing AST node's id, recurses, then restores.
    current_parent_id: Option<AstId>,
}

impl DesugarContext {
    /// Returns the `AstId` to use for a synthetic node created during desugaring.
    ///
    /// Synthetic nodes have no source origin, so they inherit the id of the AST
    /// node currently being desugared. This preserves the parse-time density of
    /// `AstIds` without requiring a separate id space for desugar-introduced nodes.
    ///
    /// Panics if called outside of any item body — callers must set
    /// `current_parent_id` before invoking any desugaring that may allocate
    /// synthetic nodes.
    fn synth_id(&self) -> AstId {
        self.current_parent_id
            .expect("synth_id called without an enclosing AST node; set current_parent_id first")
    }
}

/// Desugar a module, transforming high-level constructs to simpler forms.
pub fn desugar_module(module: &Module) -> Module {
    let mut ctx = DesugarContext {
        loop_counter: 0,
        for_loop_labels: Vec::new(),
        current_parent_id: None,
    };
    let items: Vec<Item> = module
        .items
        .iter()
        .map(|item| desugar_item(item, &mut ctx))
        .collect();
    Module::with_metadata(
        items,
        module.inner_attributes().to_vec(),
        module.shebang().map(String::from),
        module.data_section().map(String::from),
        module.include_paths().clone(),
        module.ast_id_count(),
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
        Item::Newtype(t) => Item::Newtype(desugar_newtype(t)),
        Item::Interface(e) => Item::Interface(desugar_interface(e)),
        Item::Use(u) => Item::Use(u.clone()),
        Item::Resource(r) => Item::Resource(r.clone()),
        Item::World(w) => Item::World(w.clone()),
        Item::Test(t) => Item::Test(desugar_test(t, ctx)),
        Item::Global(g) => Item::Global(desugar_global(g, ctx)),
        Item::TupleTypeDecl(d) => Item::TupleTypeDecl(d.clone()),
    }
}

fn desugar_global(global: &GlobalDecl, ctx: &mut DesugarContext) -> GlobalDecl {
    let saved = ctx.current_parent_id;
    ctx.current_parent_id = Some(global.id);
    let result = GlobalDecl {
        id: global.id,
        name: global.name.clone(),
        name_span: global.name_span,
        ty: global.ty.clone(),
        initializer: desugar_expr(&global.initializer),
        mutable: global.mutable,
        is_pub: global.is_pub,
        attributes: global.attributes.clone(),
        span: global.span,
    };
    ctx.current_parent_id = saved;
    result
}

fn desugar_function(func: &Function, ctx: &mut DesugarContext) -> Function {
    let saved = ctx.current_parent_id;
    ctx.current_parent_id = Some(func.id);
    let result = Function {
        id: func.id,
        name: func.name.clone(),
        name_span: func.name_span,
        is_pub: func.is_pub,
        is_export: func.is_export,
        is_async: func.is_async,
        type_params: func.type_params.clone(),
        attrs: func.attrs.clone(),
        params: func.params.clone(),
        return_type: func.return_type.clone(),
        effects: func.effects.clone(),
        effect_ids: func.effect_ids.clone(),
        stores: func.stores.clone(),
        body: func.body.as_ref().map(|b| desugar_block(b, ctx)),
        span: func.span,
    };
    ctx.current_parent_id = saved;
    result
}

fn desugar_test(test: &TestDecl, ctx: &mut DesugarContext) -> TestDecl {
    let saved = ctx.current_parent_id;
    ctx.current_parent_id = Some(test.id);
    let result = TestDecl {
        id: test.id,
        attributes: test.attributes.clone(),
        name: test.name.clone(),
        body: desugar_block(&test.body, ctx),
        span: test.span,
    };
    ctx.current_parent_id = saved;
    result
}

fn desugar_impl(impl_block: &ImplBlock, ctx: &mut DesugarContext) -> ImplBlock {
    ImplBlock {
        id: impl_block.id,
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
        has_rest: impl_block.has_rest,
        span: impl_block.span,
    }
}

fn desugar_trait(trait_decl: &TraitDecl, ctx: &mut DesugarContext) -> TraitDecl {
    TraitDecl {
        id: trait_decl.id,
        name: trait_decl.name.clone(),
        name_span: trait_decl.name_span,
        is_pub: trait_decl.is_pub,
        type_params: trait_decl.type_params.clone(),
        associated_types: trait_decl.associated_types.clone(),
        methods: trait_decl
            .methods
            .iter()
            .map(|m| desugar_function(m, ctx))
            .collect(),
        attrs: trait_decl.attrs.clone(),
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

fn desugar_interface(e: &InterfaceDecl) -> InterfaceDecl {
    e.clone()
}

fn desugar_block(block: &Block, ctx: &mut DesugarContext) -> Block {
    Block {
        id: block.id,
        stmts: block.stmts.iter().map(|s| desugar_stmt(s, ctx)).collect(),
        span: block.span,
    }
}

fn desugar_pattern(p: &Pattern) -> Pattern {
    match p {
        Pattern::Ident { id, name, span } => Pattern::Ident {
            id: *id,
            name: name.clone(),
            span: *span,
        },
        Pattern::MutIdent { id, name, span } => Pattern::MutIdent {
            id: *id,
            name: name.clone(),
            span: *span,
        },
        Pattern::Literal(lit) => Pattern::Literal(lit.clone()),
        Pattern::Wildcard => Pattern::Wildcard,
        Pattern::Tuple(patterns, has_rest) => {
            Pattern::Tuple(patterns.iter().map(desugar_pattern).collect(), *has_rest)
        }
        Pattern::Variant {
            variant_name,
            variant_qualifier,
            name_id,
            name_span,
            bindings,
            span,
        } => Pattern::Variant {
            variant_name: variant_name.clone(),
            variant_qualifier: variant_qualifier.clone(),
            name_id: *name_id,
            name_span: *name_span,
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
        Pattern::Or(alternatives) => {
            Pattern::Or(alternatives.iter().map(desugar_pattern).collect())
        }
        Pattern::Range {
            start,
            end,
            kind,
            span,
        } => Pattern::Range {
            start: Box::new(desugar_pattern(start)),
            end: Box::new(desugar_pattern(end)),
            kind: *kind,
            span: *span,
        },
    }
}

fn desugar_stmt(stmt: &Stmt, ctx: &mut DesugarContext) -> Stmt {
    match stmt {
        Stmt::Let(l) => Stmt::Let(LetStmt {
            id: l.id,
            pattern: desugar_pattern(&l.pattern),
            name_span: l.name_span,
            is_mut: l.is_mut,
            is_reactive: l.is_reactive,
            ty: l.ty.clone(),
            value: l.value.as_ref().map(desugar_expr),
            span: l.span,
        }),
        Stmt::Expr(e) => Stmt::Expr(crate::ast::ExprStmt {
            id: e.id,
            expr: desugar_expr(&e.expr),
            span: e.span,
        }),
        Stmt::Return(r) => Stmt::Return(ReturnStmt {
            id: r.id,
            value: r.value.as_ref().map(desugar_expr),
            span: r.span,
        }),
        Stmt::TaskReturn(tr) => Stmt::TaskReturn(TaskReturnStmt {
            id: tr.id,
            value: desugar_expr(&tr.value),
            span: tr.span,
        }),
        Stmt::If(i) => Stmt::If(IfStmt {
            id: i.id,
            condition: desugar_condition(&i.condition),
            then_block: desugar_block(&i.then_block, ctx),
            else_block: i.else_block.as_ref().map(|b| desugar_block(b, ctx)),
            span: i.span,
        }),
        Stmt::While(w) => desugar_while(w, ctx),
        Stmt::For(f) => desugar_for(f, ctx),
        Stmt::ForOf(f) => desugar_for_of(f, ctx),
        // Desugared by `Resolver::desugar_assert`, which needs typed
        // sub-expressions to pick the right `Inspect` impl for each
        // power-assert capture.
        Stmt::Assert(a) => Stmt::Assert(a.clone()),
        Stmt::Loop(l) => {
            // Save and clear for_loop_labels - breaks inside this loop should
            // target this loop, not an outer for loop
            let saved_labels = std::mem::take(&mut ctx.for_loop_labels);
            let body = desugar_block(&l.body, ctx);
            ctx.for_loop_labels = saved_labels;
            Stmt::Loop(LoopStmt {
                id: l.id,
                body,
                span: l.span,
            })
        }
        Stmt::Match(m) => Stmt::Match(Box::new(MatchExpr {
            id: m.id,
            expr: desugar_expr(&m.expr),
            arms: m
                .arms
                .iter()
                .map(|arm| MatchArm {
                    id: arm.id,
                    pattern: arm.pattern.clone(),
                    guard: arm.guard.as_ref().map(desugar_expr),
                    body: desugar_expr(&arm.body),
                    span: arm.span,
                })
                .collect(),
            span: m.span,
        })),
        Stmt::Break(b) => {
            // If we're inside a For loop and this is an unlabeled break,
            // transform it to break the outer loop label
            if b.label.is_none()
                && let Some((outer_label, _)) = ctx.for_loop_labels.last()
            {
                return Stmt::Break(BreakStmt {
                    id: b.id,
                    label: Some(outer_label.clone()),
                    value: b.value.as_ref().map(|v| Box::new(desugar_expr(v))),
                    span: b.span,
                });
            }
            Stmt::Break(BreakStmt {
                id: b.id,
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
                    id: c.id,
                    label: Some(body_label.clone()),
                    value: None,
                    span: c.span,
                });
            }
            Stmt::Continue(ContinueStmt {
                id: c.id,
                span: c.span,
            })
        }
        Stmt::LabeledBlock(lb) => Stmt::LabeledBlock(LabeledBlockStmt {
            id: lb.id,
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
        Condition::LetChain { elements, span } => Condition::LetChain {
            elements: elements
                .iter()
                .map(|e| match e {
                    ConditionElement::Let {
                        pattern,
                        expr,
                        span,
                    } => ConditionElement::Let {
                        pattern: pattern.clone(),
                        expr: desugar_expr(expr),
                        span: *span,
                    },
                    ConditionElement::Expr(expr) => ConditionElement::Expr(desugar_expr(expr)),
                })
                .collect(),
            span: *span,
        },
    }
}

fn desugar_expr_impl(expr: &Expr, ctx: Option<&mut DesugarContext>) -> Expr {
    match expr {
        // Desugar compound assignment: x += y → x = x + y
        Expr::CompoundAssign(ca) => desugar_compound_assign(ca),

        // Desugared by `Resolver::desugar_comparison_chain` (cross-type
        // literal coercion across the chain needs operand types). We
        // still fold operands so other Expr-level rewrites apply.
        Expr::ComparisonChain(chain) => Expr::ComparisonChain(Box::new(ComparisonChainExpr {
            id: chain.id,
            first: desugar_expr(&chain.first),
            comparisons: chain
                .comparisons
                .iter()
                .map(|c| crate::ast::ChainedComparison {
                    op: c.op,
                    right: desugar_expr(&c.right),
                    op_span: c.op_span,
                })
                .collect(),
            span: chain.span,
        })),

        // Recursively desugar other expressions
        Expr::Ident(i) => Expr::Ident(i.clone()),
        Expr::Literal(l) => Expr::Literal(l.clone()),
        Expr::Binary(b) => Expr::Binary(Box::new(BinaryExpr {
            id: b.id,
            left: desugar_expr(&b.left),
            op: b.op,
            right: desugar_expr(&b.right),
            span: b.span,
        })),
        Expr::Unary(u) => Expr::Unary(Box::new(UnaryExpr {
            id: u.id,
            op: u.op,
            expr: desugar_expr(&u.expr),
            span: u.span,
        })),
        Expr::Assign(a) => Expr::Assign(Box::new(AssignExpr {
            id: a.id,
            target: desugar_expr(&a.target),
            value: desugar_expr(&a.value),
            span: a.span,
        })),
        Expr::Call(c) => Expr::Call(Box::new(CallExpr {
            id: c.id,
            callee: desugar_expr(&c.callee),
            type_args: c.type_args.clone(),
            args: c.args.iter().map(desugar_expr).collect(),
            has_trailing_comma: c.has_trailing_comma,
            span: c.span,
        })),
        Expr::MethodCall(m) => Expr::MethodCall(Box::new(MethodCallExpr {
            id: m.id,
            receiver: desugar_expr(&m.receiver),
            method: m.method.clone(),
            method_id: m.method_id,
            method_span: m.method_span,
            type_args: m.type_args.clone(),
            args: m.args.iter().map(desugar_expr).collect(),
            has_trailing_comma: m.has_trailing_comma,
            span: m.span,
        })),
        Expr::StaticMethodCall(s) => Expr::StaticMethodCall(Box::new(StaticMethodCallExpr {
            id: s.id,
            target_type: s.target_type.clone(),
            method: s.method.clone(),
            method_id: s.method_id,
            method_span: s.method_span,
            type_args: s.type_args.clone(),
            args: s.args.iter().map(desugar_expr).collect(),
            has_trailing_comma: s.has_trailing_comma,
            span: s.span,
        })),
        Expr::FieldAccess(f) => Expr::FieldAccess(Box::new(FieldAccessExpr {
            id: f.id,
            expr: desugar_expr(&f.expr),
            field: f.field.clone(),
            field_id: f.field_id,
            field_span: f.field_span,
            span: f.span,
        })),
        Expr::Index(i) => Expr::Index(Box::new(IndexExpr {
            id: i.id,
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
                    loop_counter: 0,
                    for_loop_labels: Vec::new(),
                    current_parent_id: Some(b.id),
                };
                Expr::Block(Box::new(desugar_block(b, &mut temp_ctx)))
            }
        }
        Expr::If(i) => {
            if let Some(ctx) = ctx {
                Expr::If(Box::new(IfExpr {
                    id: i.id,
                    condition: desugar_condition(&i.condition),
                    then_block: desugar_block(&i.then_block, ctx),
                    else_block: i.else_block.as_ref().map(|b| desugar_block(b, ctx)),
                    span: i.span,
                }))
            } else {
                let mut temp_ctx = DesugarContext {
                    loop_counter: 0,
                    for_loop_labels: Vec::new(),
                    current_parent_id: Some(i.id),
                };
                Expr::If(Box::new(IfExpr {
                    id: i.id,
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
            id: m.id,
            expr: desugar_expr(&m.expr),
            arms: m
                .arms
                .iter()
                .map(|arm| MatchArm {
                    id: arm.id,
                    pattern: arm.pattern.clone(),
                    guard: arm.guard.as_ref().map(desugar_expr),
                    body: desugar_expr(&arm.body),
                    span: arm.span,
                })
                .collect(),
            span: m.span,
        })),
        Expr::Closure(c) => Expr::Closure(Box::new(ClosureExpr {
            id: c.id,
            params: c.params.clone(),
            body: desugar_expr(&c.body),
            span: c.span,
        })),
        Expr::TemplateString(t) => Expr::TemplateString(Box::new(desugar_template_string(t))),
        Expr::Cast(c) => Expr::Cast(Box::new(CastExpr {
            id: c.id,
            expr: desugar_expr(&c.expr),
            target_type: c.target_type.clone(),
            span: c.span,
        })),
        Expr::StructLiteral(s) => Expr::StructLiteral(Box::new(StructLiteralExpr {
            id: s.id,
            name: s.name.clone(),
            name_id: s.name_id,
            name_span: s.name_span,
            fields: s
                .fields
                .iter()
                .map(|f| StructLiteralField {
                    name: f.name.clone(),
                    name_id: f.name_id,
                    name_span: f.name_span,
                    value: desugar_expr(&f.value),
                    is_shorthand: f.is_shorthand,
                    span: f.span,
                })
                .collect(),
            has_trailing_comma: s.has_trailing_comma,
            span: s.span,
        })),
        Expr::TupleLiteral(t) => Expr::TupleLiteral(Box::new(TupleLiteralExpr {
            id: t.id,
            elements: t.elements.iter().map(desugar_expr).collect(),
            span: t.span,
        })),
        Expr::LabeledBlock(lb) => {
            // Labeled block expressions can contain statements including asserts
            if let Some(ctx) = ctx {
                Expr::LabeledBlock(Box::new(crate::ast::LabeledBlockExpr {
                    id: lb.id,
                    label: lb.label.clone(),
                    block: desugar_block(&lb.block, ctx),
                    span: lb.span,
                }))
            } else {
                let mut ctx = DesugarContext {
                    loop_counter: 0,
                    for_loop_labels: Vec::new(),
                    current_parent_id: Some(lb.id),
                };
                Expr::LabeledBlock(Box::new(crate::ast::LabeledBlockExpr {
                    id: lb.id,
                    label: lb.label.clone(),
                    block: desugar_block(&lb.block, &mut ctx),
                    span: lb.span,
                }))
            }
        }
        // Desugared by `Resolver::desugar_matches_expr` so LSP queries
        // land on `s matches { p }` as written. We still fold the
        // scrutinee / guard so other Expr-level rewrites apply.
        Expr::Matches(m) => Expr::Matches(Box::new(crate::ast::MatchesExpr {
            id: m.id,
            expr: desugar_expr(&m.expr),
            pattern: m.pattern.clone(),
            guard: m.guard.as_ref().map(desugar_expr),
            span: m.span,
        })),
        Expr::Spread(inner, span) => Expr::Spread(Box::new(desugar_expr(inner)), *span),
        Expr::TryOp(qm) => Expr::TryOp(Box::new(crate::ast::TryOpExpr {
            id: qm.id,
            expr: desugar_expr(&qm.expr),
            span: qm.span,
        })),
        Expr::Range(range) => Expr::Range(Box::new(crate::ast::RangeExpr {
            id: range.id,
            start: desugar_expr(&range.start),
            end: desugar_expr(&range.end),
            kind: range.kind,
            span: range.span,
        })),
        Expr::WithHandler(w) => {
            let handlers = w
                .handlers
                .iter()
                .map(|b| crate::ast::EffectHandlerBinding {
                    id: b.id,
                    effect: b.effect.clone(),
                    handler: desugar_expr(&b.handler),
                    span: b.span,
                })
                .collect();
            let body = if let Some(ctx) = ctx {
                desugar_block(&w.body, ctx)
            } else {
                let mut ctx = DesugarContext {
                    loop_counter: 0,
                    for_loop_labels: Vec::new(),
                    current_parent_id: Some(w.id),
                };
                desugar_block(&w.body, &mut ctx)
            };
            Expr::WithHandler(Box::new(crate::ast::WithHandlerExpr {
                id: w.id,
                handlers,
                body,
                span: w.span,
            }))
        }
        Expr::Resume(r) => Expr::Resume(Box::new(crate::ast::ResumeExpr {
            id: r.id,
            value: desugar_expr(&r.value),
            span: r.span,
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
        CompoundAssignOp::BitAnd => BinaryOp::BitAnd,
        CompoundAssignOp::BitOr => BinaryOp::BitOr,
        CompoundAssignOp::BitXor => BinaryOp::BitXor,
        CompoundAssignOp::Shl => BinaryOp::Shl,
        CompoundAssignOp::Shr => BinaryOp::Shr,
    };

    let binary_expr = Expr::Binary(Box::new(BinaryExpr {
        id: ca.id,
        left: target.clone(),
        op,
        right: value,
        span: ca.span,
    }));

    Expr::Assign(Box::new(AssignExpr {
        id: ca.id,
        target,
        value: binary_expr,
        span: ca.span,
    }))
}

/// Desugar comparison chain: `a < b < c` → `(a < b) && (b < c)`
fn desugar_template_string(t: &TemplateStringExpr) -> TemplateStringExpr {
    TemplateStringExpr {
        id: t.id,
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
                id: ctx.synth_id(),
                op: UnaryOp::Not,
                expr: desugar_expr(cond),
                span,
            }));

            let break_stmt = Stmt::Break(BreakStmt {
                id: ctx.synth_id(),
                label: None,
                value: None,
                span,
            });

            let if_break = Stmt::If(IfStmt {
                id: ctx.synth_id(),
                condition: Condition::Expr(negated_cond),
                then_block: Block {
                    id: ctx.synth_id(),
                    stmts: vec![break_stmt],
                    span,
                },
                else_block: None,
                span,
            });

            let mut loop_stmts = vec![if_break];
            loop_stmts.extend(desugar_block(&w.body, ctx).stmts);

            Stmt::Loop(LoopStmt {
                id: ctx.synth_id(),
                body: Block {
                    id: ctx.synth_id(),
                    stmts: loop_stmts,
                    span,
                },
                span,
            })
        }
        Condition::LetChain { .. } => {
            // while let ... { body } -> loop { if let ... { body } else { break; } }
            let break_stmt = Stmt::Break(BreakStmt {
                id: ctx.synth_id(),
                label: None,
                value: None,
                span,
            });

            let if_chain = Stmt::If(IfStmt {
                id: ctx.synth_id(),
                condition: desugar_condition(&w.condition),
                then_block: desugar_block(&w.body, ctx),
                else_block: Some(Block {
                    id: ctx.synth_id(),
                    stmts: vec![break_stmt],
                    span,
                }),
                span,
            });

            Stmt::Loop(LoopStmt {
                id: ctx.synth_id(),
                body: Block {
                    id: ctx.synth_id(),
                    stmts: vec![if_chain],
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
        id: ctx.synth_id(),
        label: body_label,
        block: desugared_body,
        span,
    });

    // Build loop body based on condition type
    let loop_body = match &f.condition {
        Some(Condition::Expr(cond)) => {
            // Expr condition: if !cond { break __for_N; }
            let negated_cond = Expr::Unary(Box::new(UnaryExpr {
                id: ctx.synth_id(),
                op: UnaryOp::Not,
                expr: desugar_expr(cond),
                span,
            }));

            let break_outer = Stmt::Break(BreakStmt {
                id: ctx.synth_id(),
                label: Some(outer_label.clone()),
                value: None,
                span,
            });

            let if_break = Stmt::If(IfStmt {
                id: ctx.synth_id(),
                condition: Condition::Expr(negated_cond),
                then_block: Block {
                    id: ctx.synth_id(),
                    stmts: vec![break_outer],
                    span,
                },
                else_block: None,
                span,
            });

            let mut stmts = vec![if_break, labeled_body];
            if let Some(update) = &f.update {
                stmts.push(Stmt::Expr(ExprStmt {
                    id: ctx.synth_id(),
                    expr: desugar_expr(update),
                    span,
                }));
            }
            Block {
                id: ctx.synth_id(),
                stmts,
                span,
            }
        }
        Some(Condition::LetChain { .. }) => {
            // Let chain condition: if let ... { body; update; } else { break __for_N; }
            let break_outer = Stmt::Break(BreakStmt {
                id: ctx.synth_id(),
                label: Some(outer_label.clone()),
                value: None,
                span,
            });

            let mut then_stmts = vec![labeled_body];
            if let Some(update) = &f.update {
                then_stmts.push(Stmt::Expr(ExprStmt {
                    id: ctx.synth_id(),
                    expr: desugar_expr(update),
                    span,
                }));
            }

            let if_chain = Stmt::If(IfStmt {
                id: ctx.synth_id(),
                condition: desugar_condition(f.condition.as_ref().unwrap()),
                then_block: Block {
                    id: ctx.synth_id(),
                    stmts: then_stmts,
                    span,
                },
                else_block: Some(Block {
                    id: ctx.synth_id(),
                    stmts: vec![break_outer],
                    span,
                }),
                span,
            });

            Block {
                id: ctx.synth_id(),
                stmts: vec![if_chain],
                span,
            }
        }
        None => {
            // No condition: infinite loop (just body + update)
            let mut stmts = vec![labeled_body];
            if let Some(update) = &f.update {
                stmts.push(Stmt::Expr(ExprStmt {
                    id: ctx.synth_id(),
                    expr: desugar_expr(update),
                    span,
                }));
            }
            Block {
                id: ctx.synth_id(),
                stmts,
                span,
            }
        }
    };

    let loop_stmt = Stmt::Loop(LoopStmt {
        id: ctx.synth_id(),
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
        id: ctx.synth_id(),
        label: outer_label,
        block: Block {
            id: ctx.synth_id(),
            stmts: outer_stmts,
            span,
        },
        span,
    })
}

/// Pass `ForOf` through to the resolver phase (which has type information).
///
/// The resolver handles two cases:
/// - **Tuple iterable**: unrolls the loop body once per element (compile-time expansion)
/// - **Non-tuple iterable**: desugars to `into_iter()` + `next()` iterator pattern
fn desugar_for_of(f: &ForOfStmt, ctx: &mut DesugarContext) -> Stmt {
    // Save and clear for_loop_labels so that break/continue inside the for-of body
    // target the for-of loop itself (which the resolver will desugar into a loop),
    // not an enclosing C-style for loop's body label.
    let saved_labels = std::mem::take(&mut ctx.for_loop_labels);
    let body = desugar_block(&f.body, ctx);
    ctx.for_loop_labels = saved_labels;

    Stmt::ForOf(ForOfStmt {
        id: ctx.synth_id(),
        binding: desugar_pattern(&f.binding),
        is_mut: f.is_mut,
        iterable: desugar_expr(&f.iterable),
        body,
        span: f.span,
    })
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
            id: crate::ast::AstId::fresh(),
            target: Expr::Ident(IdentExpr {
                id: crate::ast::AstId::fresh(),
                name: "x".to_string(),
                segments: Vec::new(),
                type_args: Vec::new(),
                span: dummy_span(),
            }),
            op: CompoundAssignOp::Add,
            value: Expr::Literal(LiteralExpr {
                id: crate::ast::AstId::fresh(),
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

}
