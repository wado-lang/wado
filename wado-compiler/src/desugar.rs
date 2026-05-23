// Desugaring pass for Wado AST
//
// Transforms high-level AST constructs to simpler forms for codegen:
// - CompoundAssignExpr (x += y) → AssignExpr (x = x + y)
// - ComparisonChainExpr (a < b < c) → BinaryExpr chain ((a < b) && (b < c))
//
// `assert` is preserved by this phase; the power-assert expansion lives in
// `resolver::Resolver::lower_assert`, which keeps the source AST untouched so
// LSP queries land on the user's original `assert cond, msg;` text.

use crate::ast::{
    AssertStmt, AssignExpr, AstId, BinaryExpr, BinaryOp, Block, BreakStmt, CallExpr, CastExpr,
    ClosureExpr, ComparisonChainExpr, CompoundAssignExpr, CompoundAssignOp, Condition,
    ConditionElement, ContinueStmt, EnumDecl, Expr, ExprStmt, FieldAccessExpr, ForOfStmt, ForStmt,
    Function, GlobalDecl, IfExpr, IfStmt, ImplBlock, IndexExpr, InterfaceDecl, Item,
    LabeledBlockStmt, LetStmt, Literal, LiteralExpr, LoopStmt, MatchArm, MatchExpr, MethodCallExpr,
    Module, Newtype, Pattern, ReturnStmt, StaticMethodCallExpr, Stmt, StructDecl,
    StructLiteralExpr, StructLiteralField, TaskReturnStmt, TemplatePart, TemplateStringExpr,
    TestDecl, TraitDecl, TupleLiteralExpr, Type, UnaryExpr, UnaryOp, UseItem, WhileStmt,
};
use crate::ast_folder::{AstFolder, default_fold_expr, default_fold_pattern, default_fold_type};

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
    let items = NsStripper::for_module(module).strip(items);
    Module::with_metadata(
        items,
        module.inner_attributes().to_vec(),
        module.shebang().map(String::from),
        module.data_section().map(String::from),
        module.include_paths().clone(),
        module.ast_id_count(),
    )
}

fn strip_namespace_prefix<'a>(name: &'a str, namespace_names: &[String]) -> &'a str {
    for ns in namespace_names {
        if let Some(rest) = name.strip_prefix(ns.as_str())
            && let Some(rest) = rest.strip_prefix("::")
        {
            // Keep the namespace prefix for type-qualified names (e.g., "ns::Type::method")
            // so the resolver can identify which module the type belongs to.
            if rest.contains("::") {
                return name;
            }
            return rest;
        }
    }
    name
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
        // Assert is preserved through desugar; the power-assert expansion
        // runs at TIR lowering time in `resolver::Resolver::lower_assert`.
        // We still recurse into the condition / message so other Expr-level
        // desugars (compound assign, comparison chain, `matches!`, …) apply
        // and the resolver sees a form it can handle. The user's original
        // condition source is captured *before* rewriting so power-assert's
        // failure message reads in the user's own words.
        Stmt::Assert(a) => {
            let original_condition_source = crate::unparse::unparse_expr_simple(&a.condition);
            Stmt::Assert(AssertStmt {
                id: a.id,
                condition: desugar_expr(&a.condition),
                message: a.message.as_ref().map(desugar_expr),
                span: a.span,
                original_condition_source: Some(original_condition_source),
            })
        }
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

        // Desugar comparison chain: a < b < c → (a < b) && (b < c)
        Expr::ComparisonChain(chain) => desugar_comparison_chain(chain),

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
        // Desugar matches expression: `expr matches { pattern && guard }`
        // becomes: `if let pattern = expr { guard } else { false }`
        // or if no guard: `if let pattern = expr { true } else { false }`
        Expr::Matches(m) => desugar_matches_expr(m),
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
            id: m.id,
            value: Literal::Bool(true),
            span: m.span,
        })
    };

    // The wildcard arm: `false`
    let wildcard_body = Expr::Literal(LiteralExpr {
        id: m.id,
        value: Literal::Bool(false),
        span: m.span,
    });

    // Build a match expression
    Expr::Match(Box::new(MatchExpr {
        id: m.id,
        expr: scrutinee,
        arms: vec![
            MatchArm {
                id: m.id,
                pattern: m.pattern.clone(),
                guard: None,
                body: match_body,
                span: m.span,
            },
            MatchArm {
                id: m.id,
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
fn desugar_comparison_chain(chain: &ComparisonChainExpr) -> Expr {
    let first = desugar_expr(&chain.first);

    if chain.comparisons.is_empty() {
        return first;
    }

    if chain.comparisons.len() == 1 {
        // Single comparison, just a binary expr
        let cmp = &chain.comparisons[0];
        return Expr::Binary(Box::new(BinaryExpr {
            id: chain.id,
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
            id: chain.id,
            left: prev.clone(),
            op: cmp.op,
            right: right.clone(),
            span: cmp.op_span,
        }));

        result = Some(match result {
            None => comparison,
            Some(acc) => Expr::Binary(Box::new(BinaryExpr {
                id: chain.id,
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

/// Second pass: strip `use … namespace` prefixes from identifiers, types,
/// patterns, and struct-literal names so the resolver sees the canonical
/// (unprefixed) name. Implemented as an [`AstFolder`] so the structural
/// recursion through every node kind is generated once by
/// `default_fold_*`; this struct only overrides the leaf nodes where a
/// name actually needs stripping.
///
/// Selectivity at the item level mirrors the historical
/// `strip_ns_from_item`: only Function / Impl / Trait / Test / Global
/// bodies are visited. Struct / Enum / Variant / etc. field types are
/// left untouched because the resolver already accepts the prefixed
/// form for those declarations.
struct NsStripper {
    namespace_names: Vec<String>,
}

impl NsStripper {
    /// Collect every `use <name> from "..."` (the namespace-style import)
    /// declared in `module`. The result is the prefix set we strip.
    fn for_module(module: &Module) -> Self {
        let namespace_names = module
            .items
            .iter()
            .filter_map(|item| {
                if let Item::Use(u) = item {
                    u.items.iter().find_map(|ui| {
                        if let UseItem::Namespace { name } = ui {
                            Some(name.clone())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            })
            .collect();
        Self { namespace_names }
    }

    fn strip(mut self, items: Vec<Item>) -> Vec<Item> {
        if self.namespace_names.is_empty() {
            return items;
        }
        items.into_iter().map(|item| self.fold_item(item)).collect()
    }

    /// Try to strip a namespace prefix from `name`, returning the new
    /// owned `String` only when a prefix actually matched. Returning
    /// `Option<String>` lets call sites short-circuit on the common
    /// no-match path without an extra allocation.
    fn maybe_strip(&self, name: &str) -> Option<String> {
        let stripped = strip_namespace_prefix(name, &self.namespace_names);
        if stripped.len() == name.len() {
            None
        } else {
            Some(stripped.to_string())
        }
    }
}

impl AstFolder for NsStripper {
    fn fold_item(&mut self, item: Item) -> Item {
        // Mirrors the original `strip_ns_from_item`: only recurse into the
        // items that have user-written expressions / statements / types
        // exposed to namespace use.
        match item {
            Item::Function(f) => Item::Function(self.fold_function(f)),
            Item::Impl(mut i) => {
                i.ty = self.fold_type(i.ty);
                if let Some(t) = i.trait_type.take() {
                    i.trait_type = Some(self.fold_type(t));
                }
                i.methods = i
                    .methods
                    .into_iter()
                    .map(|m| self.fold_function(m))
                    .collect();
                Item::Impl(i)
            }
            Item::Trait(mut t) => {
                t.methods = t
                    .methods
                    .into_iter()
                    .map(|m| self.fold_function(m))
                    .collect();
                Item::Trait(t)
            }
            Item::Test(mut t) => {
                t.body = self.fold_block(t.body);
                Item::Test(t)
            }
            Item::Global(mut g) => {
                g.initializer = self.fold_expr(g.initializer);
                Item::Global(g)
            }
            other => other,
        }
    }

    fn fold_function(&mut self, mut f: Function) -> Function {
        for param in &mut f.params {
            let ty = std::mem::replace(&mut param.ty, Type::Tuple(Vec::new()));
            param.ty = self.fold_type(ty);
            // Param defaults intentionally left untouched (matches historical
            // strip_ns_from_function behavior).
        }
        if let Some(ret) = f.return_type.take() {
            f.return_type = Some(self.fold_type(ret));
        }
        if let Some(body) = f.body.take() {
            f.body = Some(self.fold_block(body));
        }
        f
    }

    fn fold_expr(&mut self, expr: Expr) -> Expr {
        match expr {
            Expr::Ident(mut i) => {
                if let Some(stripped) = self.maybe_strip(&i.name) {
                    i.name = stripped;
                }
                Expr::Ident(i)
            }
            Expr::StructLiteral(mut s) => {
                if let Some(name) = s.name.as_ref()
                    && let Some(stripped) = self.maybe_strip(name)
                {
                    s.name = Some(stripped);
                }
                s.fields = s
                    .fields
                    .into_iter()
                    .map(|mut f| {
                        f.value = self.fold_expr(f.value);
                        f
                    })
                    .collect();
                Expr::StructLiteral(s)
            }
            other => default_fold_expr(self, other),
        }
    }

    fn fold_pattern(&mut self, pat: Pattern) -> Pattern {
        match pat {
            Pattern::Variant {
                variant_name,
                variant_qualifier,
                name_id,
                name_span,
                bindings,
                span,
            } => {
                let new_name = self.maybe_strip(&variant_name).unwrap_or(variant_name);
                Pattern::Variant {
                    variant_name: new_name,
                    variant_qualifier: variant_qualifier.map(|t| self.fold_type(t)),
                    name_id,
                    name_span,
                    bindings: bindings.into_iter().map(|p| self.fold_pattern(p)).collect(),
                    span,
                }
            }
            Pattern::Struct {
                type_name,
                fields,
                has_rest,
                span,
            } => {
                let new_type_name = type_name.map(|n| self.maybe_strip(&n).unwrap_or(n));
                Pattern::Struct {
                    type_name: new_type_name,
                    fields: fields
                        .into_iter()
                        .map(|mut f| {
                            f.pattern = self.fold_pattern(f.pattern);
                            f
                        })
                        .collect(),
                    has_rest,
                    span,
                }
            }
            other => default_fold_pattern(self, other),
        }
    }

    fn fold_type(&mut self, ty: Type) -> Type {
        match ty {
            Type::Named(mut n) => {
                if let Some(stripped) = self.maybe_strip(&n.name) {
                    n.name = stripped;
                    // Reset the (cached) source interface — the resolver
                    // re-derives it from the unprefixed name.
                    n.source_interface = None;
                }
                Type::Named(n)
            }
            Type::Generic(mut g) => {
                if let Some(stripped) = self.maybe_strip(&g.name) {
                    g.name = stripped;
                }
                g.args = g.args.into_iter().map(|a| self.fold_type(a)).collect();
                Type::Generic(g)
            }
            other => default_fold_type(self, other),
        }
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

    #[test]
    fn test_desugar_comparison_chain() {
        use crate::ast::ChainedComparison;

        // 0 < x < 10
        let chain = ComparisonChainExpr {
            id: crate::ast::AstId::fresh(),
            first: Expr::Literal(LiteralExpr {
                id: crate::ast::AstId::fresh(),
                value: crate::ast::Literal::Number("0".to_string()),
                span: dummy_span(),
            }),
            comparisons: vec![
                ChainedComparison {
                    op: BinaryOp::Lt,
                    right: Expr::Ident(IdentExpr {
                        id: crate::ast::AstId::fresh(),
                        name: "x".to_string(),
                        segments: Vec::new(),
                        type_args: Vec::new(),
                        span: dummy_span(),
                    }),
                    op_span: dummy_span(),
                },
                ChainedComparison {
                    op: BinaryOp::Lt,
                    right: Expr::Literal(LiteralExpr {
                        id: crate::ast::AstId::fresh(),
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
