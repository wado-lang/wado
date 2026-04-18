// Desugaring pass for Wado AST
//
// Transforms high-level AST constructs to simpler forms for codegen:
// - CompoundAssignExpr (x += y) → AssignExpr (x = x + y)
// - ComparisonChainExpr (a < b < c) → BinaryExpr chain ((a < b) && (b < c))
// - Assert (assert cond, msg) → LabeledBlock with intermediates, if, and panic

use crate::ast::{
    AssertStmt, AssignExpr, AstId, BinaryExpr, BinaryOp, Block, BreakStmt, CallExpr, CastExpr,
    ClosureExpr, ComparisonChainExpr, CompoundAssignExpr, CompoundAssignOp, Condition,
    ConditionElement, ContinueStmt, EffectDecl, EnumDecl, Expr, ExprStmt, FieldAccessExpr,
    ForOfStmt, ForStmt, FormatSpec, Function, GlobalDecl, IdentExpr, IfExpr, IfStmt, ImplBlock,
    IndexExpr, Item, LabeledBlockStmt, LetStmt, Literal, LiteralExpr, LoopStmt, MatchArm,
    MatchExpr, MethodCallExpr, Module, Newtype, Pattern, ReturnStmt, StaticMethodCallExpr, Stmt,
    StructDecl, StructLiteralExpr, StructLiteralField, TaskReturnStmt, TemplatePart,
    TemplateStringExpr, TestDecl, TraitDecl, TupleLiteralExpr, Type, UnaryExpr, UnaryOp, UseItem,
    WhileStmt,
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
    /// Namespace import names (e.g., "shapes" from `use shapes from "..."`)
    namespace_names: Vec<String>,
    /// `AstId` of the AST node currently being desugared. Synthetic nodes produced
    /// during desugaring inherit this id so that `Module::ast_id_count` remains
    /// parser-allocated and AstIds stay dense in `0..ast_id_count`. The desugar
    /// phase is slated for removal; after that, every AST node will be parser-owned.
    current_parent_id: AstId,
}

impl DesugarContext {
    /// Returns the `AstId` to use for a synthetic node created during desugaring.
    ///
    /// Synthetic nodes have no source origin, so they inherit the id of the AST
    /// node currently being desugared. This preserves the parse-time density of
    /// AstIds without requiring a separate id space for desugar-introduced nodes.
    fn synth_id(&self) -> AstId {
        self.current_parent_id
    }
}

/// Desugar a module, transforming high-level constructs to simpler forms.
pub fn desugar_module(module: &Module) -> Module {
    let namespace_names: Vec<String> = module
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
    let mut ctx = DesugarContext {
        assert_counter: 0,
        loop_counter: 0,
        for_loop_labels: Vec::new(),
        namespace_names,
        current_parent_id: AstId(0),
    };
    let items: Vec<Item> = module
        .items
        .iter()
        .map(|item| desugar_item(item, &mut ctx))
        .collect();
    let items = if ctx.namespace_names.is_empty() {
        items
    } else {
        items
            .into_iter()
            .map(|item| strip_ns_from_item(item, &ctx))
            .collect()
    };
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

fn desugar_type(ty: &Type, ctx: &DesugarContext) -> Type {
    if ctx.namespace_names.is_empty() {
        return ty.clone();
    }
    match ty {
        Type::Named(n) => {
            let stripped = strip_namespace_prefix(&n.name, &ctx.namespace_names);
            if stripped.len() == n.name.len() {
                Type::Named(n.clone())
            } else {
                Type::Named(crate::ast::NamedType {
                    name: stripped.to_string(),
                    span: n.span,
                })
            }
        }
        Type::Generic(g) => {
            let stripped = strip_namespace_prefix(&g.name, &ctx.namespace_names);
            Type::Generic(crate::ast::GenericType {
                name: if stripped.len() == g.name.len() {
                    g.name.clone()
                } else {
                    stripped.to_string()
                },
                args: g.args.iter().map(|a| desugar_type(a, ctx)).collect(),
                span: g.span,
            })
        }
        Type::Tuple(types) => Type::Tuple(types.iter().map(|t| desugar_type(t, ctx)).collect()),
        Type::Reference(inner) => Type::Reference(Box::new(desugar_type(inner, ctx))),
        Type::MutReference(inner) => Type::MutReference(Box::new(desugar_type(inner, ctx))),
        Type::Function(f) => Type::Function(Box::new(crate::ast::FunctionType {
            params: f.params.iter().map(|p| desugar_type(p, ctx)).collect(),
            return_type: desugar_type(&f.return_type, ctx),
            effects: f.effects.clone(),
            stores: f.stores.clone(),
        })),
        Type::NamespacedGeneric(_) | Type::TypePackSpread(..) => ty.clone(),
    }
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
        Item::Effect(e) => Item::Effect(desugar_effect(e)),
        Item::Use(u) => Item::Use(u.clone()),
        Item::Resource(r) => Item::Resource(r.clone()),
        Item::World(w) => Item::World(w.clone()),
        Item::Test(t) => Item::Test(desugar_test(t, ctx)),
        Item::Global(g) => Item::Global(desugar_global(g, ctx)),
        Item::TupleTypeDecl(d) => Item::TupleTypeDecl(d.clone()),
    }
}

fn desugar_global(global: &GlobalDecl, _ctx: &mut DesugarContext) -> GlobalDecl {
    GlobalDecl {
        id: global.id,
        name: global.name.clone(),
        name_span: global.name_span,
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
        stores: func.stores.clone(),
        body: func.body.as_ref().map(|b| desugar_block(b, ctx)),
        span: func.span,
    }
}

fn desugar_test(test: &TestDecl, ctx: &mut DesugarContext) -> TestDecl {
    TestDecl {
        id: test.id,
        attributes: test.attributes.clone(),
        name: test.name.clone(),
        body: desugar_block(&test.body, ctx),
        span: test.span,
    }
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

fn desugar_effect(e: &EffectDecl) -> EffectDecl {
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
        Pattern::Ident(name) => Pattern::Ident(name.clone()),
        Pattern::MutIdent(name) => Pattern::MutIdent(name.clone()),
        Pattern::Literal(lit) => Pattern::Literal(lit.clone()),
        Pattern::Wildcard => Pattern::Wildcard,
        Pattern::Tuple(patterns, has_rest) => {
            Pattern::Tuple(patterns.iter().map(desugar_pattern).collect(), *has_rest)
        }
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
        Stmt::Assert(a) => desugar_assert(a, ctx),
        Stmt::Loop(l) => {
            // Save and clear for_loop_labels - breaks inside this loop should
            // target this loop, not an outer for loop
            let saved_labels = std::mem::take(&mut ctx.for_loop_labels);
            let body = desugar_block(&l.body, ctx);
            ctx.for_loop_labels = saved_labels;
            Stmt::Loop(LoopStmt { id: l.id, body, span: l.span })
        }
        Stmt::Match(m) => Stmt::Match(Box::new(MatchExpr {
            id: m.id,
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
            Stmt::Continue(ContinueStmt { id: c.id, span: c.span })
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
            type_args: m.type_args.clone(),
            args: m.args.iter().map(desugar_expr).collect(),
            has_trailing_comma: m.has_trailing_comma,
            span: m.span,
        })),
        Expr::StaticMethodCall(s) => Expr::StaticMethodCall(Box::new(StaticMethodCallExpr {
            id: s.id,
            target_type: s.target_type.clone(),
            method: s.method.clone(),
            type_args: s.type_args.clone(),
            args: s.args.iter().map(desugar_expr).collect(),
            has_trailing_comma: s.has_trailing_comma,
            span: s.span,
        })),
        Expr::FieldAccess(f) => Expr::FieldAccess(Box::new(FieldAccessExpr {
            id: f.id,
            expr: desugar_expr(&f.expr),
            field: f.field.clone(),
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
                    assert_counter: 0,
                    loop_counter: 0,
                    for_loop_labels: Vec::new(),
                    namespace_names: Vec::new(),
                    current_parent_id: AstId(0),
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
                    assert_counter: 0,
                    loop_counter: 0,
                    for_loop_labels: Vec::new(),
                    namespace_names: Vec::new(),
                    current_parent_id: AstId(0),
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
                id: c.id,
                params: c.params.clone(),
                body: desugar_expr(&c.body),
                source_text: Some(source_text),
                span: c.span,
            }))
        }
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
                    assert_counter: 0,
                    loop_counter: 0,
                    for_loop_labels: Vec::new(),
                    namespace_names: Vec::new(),
                    current_parent_id: AstId(0),
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
            id: ctx.synth_id(),
            pattern: Pattern::Ident(var_name.clone()),
            name_span: span,
            is_mut: false,
            is_reactive: false,
            ty: None,
            value: Some(expr.clone()),
            span,
        }));
    }

    // Build the condition expression using the intermediate variables
    let reconstructed_condition =
        reconstruct_with_intermediates(&desugared_condition, &intermediates);

    // Store condition in a variable (scoped to this labeled block)
    let cond_var = "__cond".to_string();
    stmts.push(Stmt::Let(LetStmt {
        id: ctx.synth_id(),
        pattern: Pattern::Ident(cond_var.clone()),
        name_span: span,
        is_mut: false,
        is_reactive: false,
        ty: None,
        value: Some(reconstructed_condition),
        span,
    }));

    // Build the error message template string
    let condition_source = unparse_expr_simple(&assert_stmt.condition);
    let mut template_parts: Vec<TemplatePart> = Vec::new();

    // Helper to create #function literal expression
    let function_expr = Expr::Literal(LiteralExpr {
        id: ctx.synth_id(),
        value: Literal::LocationFunction,
        span,
    });
    // Helper to create #file literal expression
    let file_expr = Expr::Literal(LiteralExpr {
        id: ctx.synth_id(),
        value: Literal::LocationFile,
        span,
    });
    // Helper to create #line literal expression
    let line_expr = Expr::Literal(LiteralExpr {
        id: ctx.synth_id(),
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

    // Add each intermediate value using inspect format
    for (var_name, source, _) in &intermediates {
        template_parts.push(TemplatePart::String(format!("{source}: ")));
        template_parts.push(TemplatePart::Interpolation {
            expr: Box::new(Expr::Ident(IdentExpr {
                id: ctx.synth_id(),
                name: var_name.clone(),
                span,
            })),
            format: Some(FormatSpec {
                spec: "?".to_string(),
            }),
        });
        template_parts.push(TemplatePart::String("\n".to_string()));
    }

    let error_message = Expr::TemplateString(Box::new(TemplateStringExpr {
        id: ctx.synth_id(),
        parts: template_parts,
        span,
    }));

    // Build: panic(error_message)
    let panic_call = Expr::Call(Box::new(CallExpr {
        id: ctx.synth_id(),
        callee: Expr::Ident(IdentExpr {
            id: ctx.synth_id(),
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
        id: ctx.synth_id(),
        condition: Condition::Expr(Expr::Unary(Box::new(UnaryExpr {
            id: ctx.synth_id(),
            op: UnaryOp::Not,
            expr: Expr::Ident(IdentExpr {
                id: ctx.synth_id(),
                name: cond_var,
                span,
            }),
            span,
        }))),
        then_block: Block {
            id: ctx.synth_id(),
            stmts: vec![Stmt::Expr(ExprStmt {
                id: ctx.synth_id(),
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
        id: ctx.synth_id(),
        label: format!("__assert_{assert_id}"),
        block: Block {
            id: ctx.synth_id(),
            stmts,
            span,
        },
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
                id: expr.id(),
                name: var_name.clone(),
                span: expr.span(),
            });
        }
    }

    // Otherwise, recursively reconstruct
    match expr {
        Expr::Binary(bin) => Expr::Binary(Box::new(BinaryExpr {
            id: bin.id,
            left: reconstruct_with_intermediates(&bin.left, intermediates),
            op: bin.op,
            right: reconstruct_with_intermediates(&bin.right, intermediates),
            span: bin.span,
        })),
        Expr::Unary(unary) => Expr::Unary(Box::new(UnaryExpr {
            id: unary.id,
            op: unary.op,
            expr: reconstruct_with_intermediates(&unary.expr, intermediates),
            span: unary.span,
        })),
        // For other expressions, return as-is (they might be intermediates or literals)
        _ => expr.clone(),
    }
}

fn strip_ns_from_item(item: Item, ctx: &DesugarContext) -> Item {
    match item {
        Item::Function(f) => Item::Function(strip_ns_from_function(f, ctx)),
        Item::Impl(mut i) => {
            i.ty = desugar_type(&i.ty, ctx);
            if let Some(ref t) = i.trait_type {
                i.trait_type = Some(desugar_type(t, ctx));
            }
            i.methods = i
                .methods
                .into_iter()
                .map(|f| strip_ns_from_function(f, ctx))
                .collect();
            Item::Impl(i)
        }
        Item::Trait(mut t) => {
            t.methods = t
                .methods
                .into_iter()
                .map(|f| strip_ns_from_function(f, ctx))
                .collect();
            Item::Trait(t)
        }
        Item::Test(mut t) => {
            t.body = strip_ns_from_block(t.body, ctx);
            Item::Test(t)
        }
        Item::Global(mut g) => {
            g.initializer = strip_ns_from_expr(g.initializer, ctx);
            Item::Global(g)
        }
        other => other,
    }
}

fn strip_ns_from_function(mut f: Function, ctx: &DesugarContext) -> Function {
    for param in &mut f.params {
        param.ty = desugar_type(&param.ty, ctx);
    }
    if let Some(ref ret) = f.return_type {
        f.return_type = Some(desugar_type(ret, ctx));
    }
    if let Some(body) = f.body {
        f.body = Some(strip_ns_from_block(body, ctx));
    }
    f
}

fn strip_ns_from_block(mut block: Block, ctx: &DesugarContext) -> Block {
    block.stmts = block
        .stmts
        .into_iter()
        .map(|s| strip_ns_from_stmt(s, ctx))
        .collect();
    block
}

fn strip_ns_from_stmt(stmt: Stmt, ctx: &DesugarContext) -> Stmt {
    match stmt {
        Stmt::Let(mut l) => {
            l.ty = l.ty.as_ref().map(|t| desugar_type(t, ctx));
            l.value = l.value.map(|e| strip_ns_from_expr(e, ctx));
            l.pattern = strip_ns_from_pattern(l.pattern, ctx);
            Stmt::Let(l)
        }
        Stmt::Expr(mut e) => {
            e.expr = strip_ns_from_expr(e.expr, ctx);
            Stmt::Expr(e)
        }
        Stmt::Return(mut r) => {
            r.value = r.value.map(|e| strip_ns_from_expr(e, ctx));
            Stmt::Return(r)
        }
        Stmt::If(mut i) => {
            i.condition = strip_ns_from_condition(i.condition, ctx);
            i.then_block = strip_ns_from_block(i.then_block, ctx);
            i.else_block = i.else_block.map(|b| strip_ns_from_block(b, ctx));
            Stmt::If(i)
        }
        Stmt::While(mut w) => {
            w.condition = strip_ns_from_condition(w.condition, ctx);
            w.body = strip_ns_from_block(w.body, ctx);
            Stmt::While(w)
        }
        Stmt::For(mut f) => {
            f.init = f.init.map(|s| Box::new(strip_ns_from_stmt(*s, ctx)));
            f.condition = f.condition.map(|c| strip_ns_from_condition(c, ctx));
            f.update = f.update.map(|e| strip_ns_from_expr(e, ctx));
            f.body = strip_ns_from_block(f.body, ctx);
            Stmt::For(f)
        }
        Stmt::ForOf(mut f) => {
            f.iterable = strip_ns_from_expr(f.iterable, ctx);
            f.body = strip_ns_from_block(f.body, ctx);
            f.binding = strip_ns_from_pattern(f.binding, ctx);
            Stmt::ForOf(f)
        }
        Stmt::Loop(mut l) => {
            l.body = strip_ns_from_block(l.body, ctx);
            Stmt::Loop(l)
        }
        Stmt::LabeledBlock(mut l) => {
            l.block = strip_ns_from_block(l.block, ctx);
            Stmt::LabeledBlock(l)
        }
        Stmt::Match(mut m) => {
            m.expr = strip_ns_from_expr(m.expr, ctx);
            m.arms = m
                .arms
                .into_iter()
                .map(|mut arm| {
                    arm.pattern = strip_ns_from_pattern(arm.pattern, ctx);
                    arm.body = strip_ns_from_expr(arm.body, ctx);
                    arm.guard = arm.guard.map(|g| strip_ns_from_expr(g, ctx));
                    arm
                })
                .collect();
            Stmt::Match(m)
        }
        Stmt::Assert(mut a) => {
            a.condition = strip_ns_from_expr(a.condition, ctx);
            a.message = a.message.map(|e| strip_ns_from_expr(e, ctx));
            Stmt::Assert(a)
        }
        Stmt::TaskReturn(mut t) => {
            t.value = strip_ns_from_expr(t.value, ctx);
            Stmt::TaskReturn(t)
        }
        Stmt::Break(_) | Stmt::Continue(_) => stmt,
    }
}

fn strip_ns_from_condition(cond: Condition, ctx: &DesugarContext) -> Condition {
    match cond {
        Condition::Expr(e) => Condition::Expr(strip_ns_from_expr(e, ctx)),
        Condition::LetChain { elements, span } => Condition::LetChain {
            elements: elements
                .into_iter()
                .map(|e| match e {
                    ConditionElement::Let {
                        pattern,
                        expr,
                        span,
                    } => ConditionElement::Let {
                        pattern: strip_ns_from_pattern(pattern, ctx),
                        expr: strip_ns_from_expr(expr, ctx),
                        span,
                    },
                    ConditionElement::Expr(expr) => {
                        ConditionElement::Expr(strip_ns_from_expr(expr, ctx))
                    }
                })
                .collect(),
            span,
        },
    }
}

fn strip_ns_from_pattern(pattern: Pattern, ctx: &DesugarContext) -> Pattern {
    match pattern {
        Pattern::Variant {
            variant_name,
            bindings,
            span,
        } => {
            let stripped = strip_namespace_prefix(&variant_name, &ctx.namespace_names);
            Pattern::Variant {
                variant_name: if stripped.len() == variant_name.len() {
                    variant_name
                } else {
                    stripped.to_string()
                },
                bindings: bindings
                    .into_iter()
                    .map(|p| strip_ns_from_pattern(p, ctx))
                    .collect(),
                span,
            }
        }
        Pattern::Struct {
            type_name,
            fields,
            has_rest,
            span,
        } => {
            let stripped_type = type_name.as_ref().map(|n| {
                let s = strip_namespace_prefix(n, &ctx.namespace_names);
                if s.len() == n.len() {
                    n.clone()
                } else {
                    s.to_string()
                }
            });
            Pattern::Struct {
                type_name: stripped_type,
                fields: fields
                    .into_iter()
                    .map(|mut f| {
                        f.pattern = strip_ns_from_pattern(f.pattern, ctx);
                        f
                    })
                    .collect(),
                has_rest,
                span,
            }
        }
        Pattern::Tuple(patterns, has_rest) => Pattern::Tuple(
            patterns
                .into_iter()
                .map(|p| strip_ns_from_pattern(p, ctx))
                .collect(),
            has_rest,
        ),
        _ => pattern,
    }
}

fn strip_ns_from_expr(expr: Expr, ctx: &DesugarContext) -> Expr {
    match expr {
        Expr::Ident(mut i) => {
            let stripped = strip_namespace_prefix(&i.name, &ctx.namespace_names);
            if stripped.len() != i.name.len() {
                i.name = stripped.to_string();
            }
            Expr::Ident(i)
        }
        Expr::Binary(mut b) => {
            b.left = strip_ns_from_expr(b.left, ctx);
            b.right = strip_ns_from_expr(b.right, ctx);
            Expr::Binary(b)
        }
        Expr::Unary(mut u) => {
            u.expr = strip_ns_from_expr(u.expr, ctx);
            Expr::Unary(u)
        }
        Expr::Assign(mut a) => {
            a.target = strip_ns_from_expr(a.target, ctx);
            a.value = strip_ns_from_expr(a.value, ctx);
            Expr::Assign(a)
        }
        Expr::Call(mut c) => {
            c.callee = strip_ns_from_expr(c.callee, ctx);
            c.args = c
                .args
                .into_iter()
                .map(|a| strip_ns_from_expr(a, ctx))
                .collect();
            Expr::Call(c)
        }
        Expr::MethodCall(mut m) => {
            m.receiver = strip_ns_from_expr(m.receiver, ctx);
            m.args = m
                .args
                .into_iter()
                .map(|a| strip_ns_from_expr(a, ctx))
                .collect();
            Expr::MethodCall(m)
        }
        Expr::StaticMethodCall(mut s) => {
            s.target_type = desugar_type(&s.target_type, ctx);
            s.type_args = s.type_args.iter().map(|t| desugar_type(t, ctx)).collect();
            s.args = s
                .args
                .into_iter()
                .map(|a| strip_ns_from_expr(a, ctx))
                .collect();
            Expr::StaticMethodCall(s)
        }
        Expr::FieldAccess(mut f) => {
            f.expr = strip_ns_from_expr(f.expr, ctx);
            Expr::FieldAccess(f)
        }
        Expr::Index(mut i) => {
            i.expr = strip_ns_from_expr(i.expr, ctx);
            i.index = strip_ns_from_expr(i.index, ctx);
            Expr::Index(i)
        }
        Expr::Block(mut b) => {
            *b = strip_ns_from_block(*b, ctx);
            Expr::Block(b)
        }
        Expr::If(mut i) => {
            i.condition = strip_ns_from_condition(i.condition, ctx);
            i.then_block = strip_ns_from_block(i.then_block, ctx);
            i.else_block = i.else_block.map(|b| strip_ns_from_block(b, ctx));
            Expr::If(i)
        }
        Expr::Match(mut m) => {
            m.expr = strip_ns_from_expr(m.expr, ctx);
            m.arms = m
                .arms
                .into_iter()
                .map(|mut arm| {
                    arm.pattern = strip_ns_from_pattern(arm.pattern, ctx);
                    arm.body = strip_ns_from_expr(arm.body, ctx);
                    arm.guard = arm.guard.map(|g| strip_ns_from_expr(g, ctx));
                    arm
                })
                .collect();
            Expr::Match(m)
        }
        Expr::Matches(mut m) => {
            m.expr = strip_ns_from_expr(m.expr, ctx);
            m.pattern = strip_ns_from_pattern(m.pattern, ctx);
            m.guard = m.guard.map(|g| strip_ns_from_expr(g, ctx));
            Expr::Matches(m)
        }
        Expr::Closure(mut c) => {
            for p in &mut c.params {
                p.ty = p.ty.as_ref().map(|t| desugar_type(t, ctx));
            }
            c.body = strip_ns_from_expr(c.body, ctx);
            Expr::Closure(c)
        }
        Expr::TemplateString(mut t) => {
            t.parts = t
                .parts
                .into_iter()
                .map(|part| match part {
                    TemplatePart::Interpolation { expr, format } => TemplatePart::Interpolation {
                        expr: Box::new(strip_ns_from_expr(*expr, ctx)),
                        format,
                    },
                    other => other,
                })
                .collect();
            Expr::TemplateString(t)
        }
        Expr::Cast(mut c) => {
            c.expr = strip_ns_from_expr(c.expr, ctx);
            c.target_type = desugar_type(&c.target_type, ctx);
            Expr::Cast(c)
        }
        Expr::StructLiteral(mut s) => {
            if let Some(ref name) = s.name {
                let stripped = strip_namespace_prefix(name, &ctx.namespace_names);
                if stripped.len() != name.len() {
                    s.name = Some(stripped.to_string());
                }
            }
            s.fields = s
                .fields
                .into_iter()
                .map(|mut f| {
                    f.value = strip_ns_from_expr(f.value, ctx);
                    f
                })
                .collect();
            Expr::StructLiteral(s)
        }
        Expr::TupleLiteral(mut t) => {
            t.elements = t
                .elements
                .into_iter()
                .map(|e| strip_ns_from_expr(e, ctx))
                .collect();
            Expr::TupleLiteral(t)
        }
        Expr::LabeledBlock(mut l) => {
            l.block = strip_ns_from_block(l.block, ctx);
            Expr::LabeledBlock(l)
        }
        Expr::TryOp(mut t) => {
            t.expr = strip_ns_from_expr(t.expr, ctx);
            Expr::TryOp(t)
        }
        Expr::Spread(mut inner, span) => {
            *inner = strip_ns_from_expr(*inner, ctx);
            Expr::Spread(inner, span)
        }
        Expr::Range(mut range) => {
            range.start = strip_ns_from_expr(range.start, ctx);
            range.end = strip_ns_from_expr(range.end, ctx);
            Expr::Range(range)
        }
        Expr::Literal(_) | Expr::CompoundAssign(_) | Expr::ComparisonChain(_) => expr,
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
